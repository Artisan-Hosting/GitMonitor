use std::{sync::Arc, time::Duration};

use artisan_middleware::{
    aggregator::Status,
    config::AppConfig,
    dusa_collection_utils::{
        core::{
            errors::{ErrorArrayItem, Errors},
            logger::{set_log_level, LogLevel},
            types::pathtype::PathType,
        },
        log,
    },
    git_actions::{generate_git_project_id, generate_git_project_path, GitAuth, GitCredentials},
    resource_monitor::ResourceMonitorLock,
    state_persistence::{log_error, update_state, AppState, StatePersistence},
};
use config::{generate_state, get_config, get_git_token_file, update_state_wrapper};
use git::{
    cleanup_safe_directory_entries, handle_existing_repo, handle_new_repo, inspect_repo_checkout,
    recreate_repo, set_safe_directory, RepoCheckoutState, RepoSyncOutcome,
};
use git_config::resolve_git_credentials_path;
use rand::{rngs::StdRng, seq::SliceRandom, RngExt, SeedableRng};
use signals::{sighup_watch, sigusr_watch};

use auth::init_gh_token;
// use git_auth_store::{auth_items, init_auth_box};
use tokio::{
    sync::{Mutex, Notify},
    time::sleep,
};

mod auth;
mod config;
mod git;
#[path = "../git_config.rs"]
mod git_config;
// mod git_auth_store;
mod pull;
mod signals;
#[cfg(test)]
mod test_support;

const HEALTHY_REFRESH_MIN_SECS: u64 = 8;
const HEALTHY_REFRESH_MAX_SECS_EXCLUSIVE: u64 = 13;
const ERROR_RETRY_BASE_SECS: u64 = 30;
const ERROR_RETRY_CAP_SECS: u64 = 300;
const INITIAL_WORKER_JITTER_MAX_SECS_EXCLUSIVE: u64 = 3;
const WORKER_SPAWN_STAGGER_MILLIS: u64 = 250;

fn healthy_refresh_delay(rng: &mut StdRng) -> u64 {
    rng.random_range(HEALTHY_REFRESH_MIN_SECS..HEALTHY_REFRESH_MAX_SECS_EXCLUSIVE)
}

fn error_retry_delay(rng: &mut StdRng, consecutive_failures: u32) -> u64 {
    let exponent = consecutive_failures.saturating_sub(1).min(4);
    let base = ERROR_RETRY_BASE_SECS
        .saturating_mul(1_u64 << exponent)
        .min(ERROR_RETRY_CAP_SECS);
    let jitter = rng.random_range(0..=(base / 2));
    base.saturating_add(jitter).min(ERROR_RETRY_CAP_SECS)
}

#[tokio::main]
async fn main() {
    tokio::task::LocalSet::new().run_until(async_main()).await;
}

async fn async_main() {
    // Initialization

    // Loading configs
    let mut config: AppConfig = get_config();
    let token_file: Option<String> = get_git_token_file();

    let token_init_error = init_gh_token(token_file.as_deref()).err().map(|err| {
        log!(
            LogLevel::Error,
            "Failed to load GitHub token; repository workers will keep retrying: {}",
            err
        );
        err.to_string()
    });
    let token_initialized = token_init_error.is_none();
    if let Err(err) = cleanup_safe_directory_entries().await {
        log!(
            LogLevel::Error,
            "Failed to cleanup safe.directory entries: {}",
            err
        );
    }

    let state_path: PathType = StatePersistence::get_state_path(&config);
    let state: Arc<Mutex<AppState>> = Arc::new(Mutex::new(generate_state(&config).await));
    {
        let mut s = state.lock().await;
        update_state(&mut s, &state_path, None).await;
    }

    // Self monitring
    let pid = {
        let s = state.lock().await;
        s.pid
    };
    let monitor: Option<ResourceMonitorLock> = match ResourceMonitorLock::new(pid as i32) {
        Ok(mon) => Some(mon),
        Err(err) => {
            log!(
                LogLevel::Error,
                "Can't get resource monitor: {}",
                err.err_mesg
            );
            None
        }
    };

    // loading signal handeling
    let reload: Arc<Notify> = Arc::new(Notify::new());
    let exit_graceful: Arc<Notify> = Arc::new(Notify::new());

    sighup_watch(reload.clone());
    sigusr_watch(exit_graceful.clone());

    // Load Git credentials
    let git_credentials: GitCredentials = {
        let s = state.lock().await;
        match get_git_credentials(&s.config).await {
            Ok(credentials) => credentials,
            Err(e) => {
                drop(s);
                let mut s = state.lock().await;
                log_error(&mut s, e, &state_path).await;
                std::process::exit(100)
            }
        }
    };

    {
        let mut s = state.lock().await;
        if let Some(error) = token_init_error {
            s.data =
                "Git monitor initialized without a GitHub token; clone/fetch operations will retry"
                    .to_string();
            let error = ErrorArrayItem::new(Errors::Git, error);
            log_error(&mut s, error, &state_path).await;
        } else {
            s.data = "Initialized GitHub token storage".to_string();
            s.event_counter += 1;
        }
    }

    // Update state to indicate initialization
    {
        let mut s = state.lock().await;
        s.config.git = config.git.clone();
        s.data = if token_initialized {
            String::from("Git monitor is initialized")
        } else {
            String::from(
                "Git monitor is initialized without a GitHub token; repository operations will retry",
            )
        };
        s.status = Status::Running;
        update_state_wrapper(&mut s, &state_path, &monitor).await;
    }

    if config.debug_mode {
        set_log_level(LogLevel::Debug);
        log!(LogLevel::Debug, "Loaded Initial Config: {}", config);
        log!(
            LogLevel::Debug,
            "Git credentials loaded {}",
            git_credentials
        );
        let log_level = {
            let s = state.lock().await;
            s.config.log_level
        };
        set_log_level(log_level);
    };

    log!(LogLevel::Info, "Git monitor initialized");
    log!(
        LogLevel::Info,
        "Repository polling interval is {}-{} seconds with exponential error backoff from {} to {} seconds",
        HEALTHY_REFRESH_MIN_SECS,
        HEALTHY_REFRESH_MAX_SECS_EXCLUSIVE - 1,
        ERROR_RETRY_BASE_SECS,
        ERROR_RETRY_CAP_SECS
    );
    log!(
        LogLevel::Info,
        "Repositories removed from git.cf are not deleted automatically; use cli_credential for manual cleanup"
    );

    // Spawn background workers for each repository
    let monitor_clone = monitor.as_ref().map(|m| m.clone());
    spawn_git_workers(
        &git_credentials,
        state.clone(),
        state_path.clone(),
        monitor_clone,
    )
    .await;

    // Main loop
    loop {
        tokio::select! {

            _ = reload.notified() => {
                sleep(Duration::from_secs(1)).await;
                config = get_config();
                let new_state = generate_state(&config).await;
                {
                    let mut s = state.lock().await;
                    *s = new_state;
                }

                let _ = { state.lock().await.config.clone() }; // reload uses current config; repo tasks unchanged
            }

            _ = exit_graceful.notified() => {
                {
                    let mut s = state.lock().await;
                    s.data = String::from("Git monitor exiting");
                    s.status = Status::Stopped;
                    update_state_wrapper(&mut s, &state_path, &monitor).await;
                }
                log!(LogLevel::Info, "Shutting down gracefully");
                std::process::exit(0)
            }

            _ = tokio::signal::ctrl_c() => {
                log!(LogLevel::Info, "CTRL + C recieved");
                exit_graceful.notify_one();
            }

            _ = tokio::time::sleep(Duration::from_secs(5)) => {
                let mut s = state.lock().await;
                s.status = Status::Running;
                update_state_wrapper(&mut s, &state_path, &monitor).await;
            }
        }
    }
}

// Load Git credentials from the configuration
async fn get_git_credentials(config: &AppConfig) -> Result<GitCredentials, ErrorArrayItem> {
    let configured_path = config
        .git
        .as_ref()
        .map(|git_config| git_config.credentials_file.as_str());
    let git_file = resolve_git_credentials_path(configured_path);

    log!(LogLevel::Debug, "Loading git credentials from {}", git_file);
    GitCredentials::new(Some(&git_file)).await
}

// Mutable state carried between worker cycles for a single repo. Split out of
// `repo_worker` so the exact same state machine can be driven directly by
// tests (against a temp checkout path) instead of a hand-rolled re-implementation.
struct WorkerCycleState {
    safe_directory_initialized: bool,
    // Forces one submodule backfill pass for repos that predate submodule
    // support; cleared once a cycle (clone or existing-repo pass) succeeds.
    submodules_backfilled: bool,
    consecutive_failures: u32,
}

impl Default for WorkerCycleState {
    fn default() -> Self {
        Self {
            safe_directory_initialized: false,
            submodules_backfilled: false,
            consecutive_failures: 0,
        }
    }
}

// Runs one inspect -> (clone | recreate | sync) -> report cycle for a single
// repo against an explicit checkout path, and returns how long to wait
// before the next cycle. Used by both `repo_worker` (with the real,
// hardcoded `generate_git_project_path` location) and tests (with a temp
// directory), so the sync/self-heal/backoff logic under test is always the
// exact logic production runs.
async fn run_worker_cycle(
    git_item: &GitAuth,
    git_project_path: &PathType,
    state: &Arc<Mutex<AppState>>,
    state_path: &PathType,
    monitor: &Option<ResourceMonitorLock>,
    rng: &mut StdRng,
    cycle_state: &mut WorkerCycleState,
) -> u64 {
    let repo_id = generate_git_project_id(git_item);
    let result: Result<RepoSyncOutcome, ErrorArrayItem> = match inspect_repo_checkout(
        git_item,
        git_project_path,
    )
    .await
    {
        Err(err) => Err(err),
        Ok(RepoCheckoutState::Missing) => {
            log!(
                LogLevel::Info,
                "{}: checkout state=missing path='{}'; cloning from git.cf",
                repo_id,
                git_project_path
            );
            let result = handle_new_repo(git_item, git_project_path).await;
            if result.is_ok() {
                cycle_state.safe_directory_initialized = true;
                cycle_state.submodules_backfilled = true;
            }
            result
        }
        Ok(RepoCheckoutState::Invalid { reason }) => {
            cycle_state.safe_directory_initialized = false;
            cycle_state.submodules_backfilled = false;
            recreate_repo(git_item, git_project_path, &reason).await
        }
        Ok(RepoCheckoutState::WrongRemote { expected, actual }) => {
            cycle_state.safe_directory_initialized = false;
            cycle_state.submodules_backfilled = false;
            let reason = format!(
                "origin remote identifies the wrong repository (actual: '{}', expected: '{}')",
                actual, expected
            );
            recreate_repo(git_item, git_project_path, &reason).await
        }
        Ok(RepoCheckoutState::Ready { remote }) => {
            log!(
                LogLevel::Debug,
                "{}: checkout state=ready path='{}' remote='{}'",
                repo_id,
                git_project_path,
                remote
            );

            if !cycle_state.safe_directory_initialized {
                match set_safe_directory(git_project_path).await {
                    Ok(_) => cycle_state.safe_directory_initialized = true,
                    Err(err) => {
                        log!(
                            LogLevel::Error,
                            "{}: failed to register safe.directory: {}",
                            repo_id,
                            err.err_mesg
                        );
                        let mut s = state.lock().await;
                        s.data = format!(
                                "[repo={} state=configuration-error] safe.directory registration failed",
                                repo_id
                            );
                        log_error(&mut s, err, state_path).await;
                        drop(s);
                        cycle_state.consecutive_failures =
                            cycle_state.consecutive_failures.saturating_add(1);
                        let wait = error_retry_delay(rng, cycle_state.consecutive_failures);
                        log!(
                            LogLevel::Warn,
                            "{}: retrying in {} seconds after {} consecutive failure(s)",
                            repo_id,
                            wait,
                            cycle_state.consecutive_failures
                        );
                        return wait;
                    }
                }
            }

            match handle_existing_repo(
                git_item,
                git_project_path,
                !cycle_state.submodules_backfilled,
            )
            .await
            {
                Ok(outcome) => {
                    cycle_state.submodules_backfilled = true;
                    Ok(outcome)
                }
                Err(sync_error) if sync_error.recreate_checkout => {
                    cycle_state.safe_directory_initialized = false;
                    cycle_state.submodules_backfilled = false;
                    let reason = format!(
                        "local checkout could not be repaired: {}",
                        sync_error.error.err_mesg
                    );
                    recreate_repo(git_item, git_project_path, &reason).await
                }
                Err(sync_error) => Err(sync_error.error),
            }
        }
    };

    let cycle_failed = result.is_err();
    let mut s = state.lock().await;
    match result {
        Err(err) => {
            s.data = format!(
                "[repo={} state=error path={}] {}",
                repo_id, git_project_path, err.err_mesg
            );
            let contextual_error = ErrorArrayItem::new(
                err.err_type,
                format!("{} at '{}': {}", repo_id, git_project_path, err.err_mesg),
            );
            log_error(&mut s, contextual_error, state_path).await;
        }
        Ok(outcome) => {
            match outcome {
                RepoSyncOutcome::Updated(msg) => {
                    s.event_counter += 1;
                    s.data = format!("[repo={} state=synced] {}", repo_id, msg);
                }
                RepoSyncOutcome::Cloned(msg) => {
                    s.event_counter += 1;
                    s.data = format!("[repo={} state=cloned] {}", repo_id, msg);
                }
                RepoSyncOutcome::Recreated(msg) => {
                    cycle_state.safe_directory_initialized = true;
                    cycle_state.submodules_backfilled = true;
                    s.event_counter += 1;
                    s.data = format!("[repo={} state=recreated] {}", repo_id, msg);
                }
                RepoSyncOutcome::NoChange(msg) => {
                    s.data = format!("[repo={} state=ready] {}", repo_id, msg);
                }
            }
            update_state_wrapper(&mut s, state_path, monitor).await;
        }
    }
    drop(s);

    let wait = if cycle_failed {
        cycle_state.consecutive_failures = cycle_state.consecutive_failures.saturating_add(1);
        error_retry_delay(rng, cycle_state.consecutive_failures)
    } else {
        cycle_state.consecutive_failures = 0;
        healthy_refresh_delay(rng)
    };
    if cycle_failed {
        log!(
            LogLevel::Warn,
            "{}: retrying in {} seconds after {} consecutive failure(s)",
            repo_id,
            wait,
            cycle_state.consecutive_failures
        );
    }
    wait
}

// Load Git credentials from the configuration
async fn repo_worker(
    git_item: GitAuth,
    state: Arc<Mutex<AppState>>,
    state_path: PathType,
    monitor: Option<ResourceMonitorLock>,
    initial_delay: u64,
) {
    sleep(Duration::from_secs(initial_delay)).await;
    let mut rng: StdRng = StdRng::from_rng(&mut rand::rng());
    let mut cycle_state = WorkerCycleState::default();
    loop {
        let git_project_path: PathType = generate_git_project_path(&git_item);
        let wait = run_worker_cycle(
            &git_item,
            &git_project_path,
            &state,
            &state_path,
            &monitor,
            &mut rng,
            &mut cycle_state,
        )
        .await;
        sleep(Duration::from_secs(wait)).await;
    }
}

// Spawn workers for each repository with slight timer offsets
// Older blocking function,
// async fn spawn_git_workers(
//     state: Arc<Mutex<AppState>>,
//     state_path: PathType,
//     monitor: Option<ResourceMonitorLock>,
// ) {
//     let Some(items) = auth_items() else { return };
//     let mut rng: StdRng = StdRng::from_rng(&mut rand::rng());
//     let mut indices: Vec<usize> = (0..items.len()).collect();
//     indices.shuffle(&mut rng);

//     for idx in indices {
//         let git_item = items[idx].clone();
//         let delay = rng.random_range(0..5);
//         let st = state.clone();
//         let path = state_path.clone();
//         let mon = monitor.as_ref().map(|m| m.clone());
//         tokio::task::spawn_blocking(move || {
//             let rt = tokio::runtime::Builder::new_current_thread()
//                 .enable_all()
//                 .build()
//                 .expect("runtime");
//             let local = tokio::task::LocalSet::new();
//             rt.block_on(local.run_until(repo_worker(git_item, st, path, mon, delay)));
//         });
//     }
// }

// Spawn workers for each repository with slight timer offsets
async fn spawn_git_workers(
    git_credentials: &GitCredentials,
    state: Arc<Mutex<AppState>>,
    state_path: PathType,
    monitor: Option<ResourceMonitorLock>,
) {
    let mut credentials_shuffled = git_credentials.clone();
    let mut rng: StdRng = StdRng::from_rng(&mut rand::rng());
    credentials_shuffled.auth_items.shuffle(&mut rng);

    for git_item in credentials_shuffled.auth_items {
        log!(
            LogLevel::Debug,
            "Deploying working thread for: {}",
            generate_git_project_id(&git_item)
        );
        let delay = rng.random_range(0..INITIAL_WORKER_JITTER_MAX_SECS_EXCLUSIVE);
        let st = state.clone();
        let path = state_path.clone();
        let mon = monitor.as_ref().map(|m| m.clone());
        tokio::task::spawn_local(async move { repo_worker(git_item, st, path, mon, delay).await });
        sleep(Duration::from_millis(WORKER_SPAWN_STAGGER_MILLIS)).await;
    }
}

#[cfg(test)]
mod backoff_tests {
    use super::*;

    #[test]
    fn healthy_refresh_delay_stays_in_bounds() {
        let mut rng = StdRng::seed_from_u64(1);
        for _ in 0..500 {
            let wait = healthy_refresh_delay(&mut rng);
            assert!(
                (HEALTHY_REFRESH_MIN_SECS..HEALTHY_REFRESH_MAX_SECS_EXCLUSIVE).contains(&wait),
                "wait {} out of bounds [{}, {})",
                wait,
                HEALTHY_REFRESH_MIN_SECS,
                HEALTHY_REFRESH_MAX_SECS_EXCLUSIVE
            );
        }
    }

    #[test]
    fn error_retry_delay_grows_exponentially_before_capping() {
        let mut rng = StdRng::seed_from_u64(2);
        for _ in 0..200 {
            let wait = error_retry_delay(&mut rng, 1);
            assert!((30..=45).contains(&wait), "1st failure wait was {}", wait);
        }
        for _ in 0..200 {
            let wait = error_retry_delay(&mut rng, 2);
            assert!((60..=90).contains(&wait), "2nd failure wait was {}", wait);
        }
        for _ in 0..200 {
            let wait = error_retry_delay(&mut rng, 3);
            assert!((120..=180).contains(&wait), "3rd failure wait was {}", wait);
        }
    }

    #[test]
    fn error_retry_delay_caps_at_max_once_exponent_saturates() {
        let mut rng = StdRng::seed_from_u64(3);
        // Exponent is clamped at 4 (30 * 2^4 = 480, already above the 300s
        // cap), so from the 5th consecutive failure onward the base alone
        // saturates the cap and jitter can never push it past 300.
        for failures in [5_u32, 6, 20, u32::MAX] {
            for _ in 0..50 {
                let wait = error_retry_delay(&mut rng, failures);
                assert_eq!(
                    wait, ERROR_RETRY_CAP_SECS,
                    "failures={} should always saturate at the cap",
                    failures
                );
            }
        }
    }

    #[test]
    fn error_retry_delay_treats_zero_failures_like_the_first_failure() {
        // consecutive_failures=0 shouldn't occur in practice (run_worker_cycle
        // always increments before calling this), but saturating_sub(1) means
        // it doesn't panic and behaves the same as failures=1.
        let mut rng = StdRng::seed_from_u64(4);
        for _ in 0..200 {
            let wait = error_retry_delay(&mut rng, 0);
            assert!((30..=45).contains(&wait), "0-failures wait was {}", wait);
        }
    }
}

// Long-running tests that drive `run_worker_cycle` -- the exact state
// machine `repo_worker` runs in production -- against a real, live-updating
// local git repo. Both #[ignore]d by default so `cargo test` stays fast;
// run explicitly with `cargo test -- --ignored <name>`.
#[cfg(test)]
mod soak_tests {
    use super::*;
    use crate::test_support::{can_chown_to_www_data, run_git_output, Sandbox};
    use artisan_middleware::{
        aggregator::Status, config::AppConfig,
        dusa_collection_utils::core::version::SoftwareVersion,
    };
    use serial_test::serial;
    use std::path::PathBuf;

    fn test_app_state() -> AppState {
        AppState {
            name: "gitmon-soak-test".to_string(),
            version: SoftwareVersion::dummy(),
            data: String::new(),
            status: Status::Running,
            pid: std::process::id(),
            last_updated: 0,
            stared_at: 0,
            event_counter: 0,
            error_log: Vec::new(),
            config: AppConfig::dummy(),
            system_application: false,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    fn checkout_dir(checkout: &PathType) -> PathBuf {
        PathBuf::from(checkout.to_string())
    }

    /// Runs `run_worker_cycle` in a loop, sleeping `poll_interval` between
    /// cycles (instead of the cycle's own computed backoff/refresh wait --
    /// that's what makes this "fast": it's testing the sync/self-heal state
    /// machine over many cycles, not the real timing), until `condition`
    /// returns true or `max_cycles` is reached. Returns whether it converged.
    async fn run_cycles_until(
        auth: &GitAuth,
        checkout: &PathType,
        state: &Arc<Mutex<AppState>>,
        state_path: &PathType,
        rng: &mut StdRng,
        cycle_state: &mut WorkerCycleState,
        poll_interval: Duration,
        max_cycles: u32,
        condition: impl Fn(&AppState) -> bool,
    ) -> bool {
        for _ in 0..max_cycles {
            run_worker_cycle(auth, checkout, state, state_path, &None, rng, cycle_state).await;
            if condition(&*state.lock().await) {
                return true;
            }
            sleep(poll_interval).await;
        }
        condition(&*state.lock().await)
    }

    #[tokio::test]
    #[serial]
    #[ignore = "long-running soak test; run explicitly with `cargo test -- --ignored soak_tracks_repo_fast`"]
    async fn soak_tracks_repo_fast() {
        let sandbox = Sandbox::new();
        let bare = sandbox
            .seed_origin("acme", "widgets", "main", &[("a.txt", "a")])
            .await;
        let auth = sandbox.git_auth("acme", "widgets", "main");
        let checkout = sandbox.checkout_path("widgets");
        sandbox.clone_checkout(&bare, "main", &checkout).await;

        let state = Arc::new(Mutex::new(test_app_state()));
        let state_path = PathType::from(sandbox.path().join("state.toml"));
        let mut rng = StdRng::seed_from_u64(42);
        let mut cycle_state = WorkerCycleState::default();

        // Cycle 1: freshly cloned, nothing changed upstream yet.
        run_worker_cycle(
            &auth,
            &checkout,
            &state,
            &state_path,
            &None,
            &mut rng,
            &mut cycle_state,
        )
        .await;
        {
            let s = state.lock().await;
            assert!(
                s.data.contains("state=ready"),
                "expected an up-to-date report on the first cycle, got: {}",
                s.data
            );
        }

        // Simulate three separate rounds of upstream activity, each observed
        // within a handful of fast polling cycles -- this is the heart of
        // "track a repo for a while": real commits, made concurrently with
        // the same polling loop production uses, actually get picked up.
        for round in 1..=3 {
            let file = format!("round-{}.txt", round);
            let new_sha = sandbox
                .commit_to_origin(
                    &bare,
                    "main",
                    &[(&file, "content")],
                    &format!("round {}", round),
                )
                .await;

            let converged = run_cycles_until(
                &auth,
                &checkout,
                &state,
                &state_path,
                &mut rng,
                &mut cycle_state,
                Duration::from_millis(100),
                20,
                |s| s.data.contains("state=synced"),
            )
            .await;
            assert!(
                converged,
                "round {}: never observed the upstream commit",
                round
            );

            let head = run_git_output(&checkout_dir(&checkout), &["rev-parse", "HEAD"]).await;
            assert_eq!(
                head.trim(),
                new_sha,
                "round {}: HEAD didn't advance to the new commit",
                round
            );
            assert!(
                checkout_dir(&checkout).join(&file).exists(),
                "round {}: new file wasn't checked out",
                round
            );
        }

        // Self-healing: corrupt the checkout mid-run and confirm the same
        // polling loop notices and recovers, without any special-casing.
        // Needs www-data + chown privileges (see can_chown_to_www_data),
        // which most dev machines won't have -- skip gracefully rather than
        // failing on an environment gap unrelated to the sync logic itself.
        if can_chown_to_www_data() {
            std::fs::remove_file(checkout_dir(&checkout).join(".git").join("HEAD"))
                .expect("corrupt the checkout for the self-heal check");

            let healed = run_cycles_until(
                &auth,
                &checkout,
                &state,
                &state_path,
                &mut rng,
                &mut cycle_state,
                Duration::from_millis(100),
                20,
                |s| s.data.contains("state=recreated"),
            )
            .await;
            assert!(
                healed,
                "worker cycle never self-healed the corrupted checkout"
            );
            assert!(
                checkout_dir(&checkout).join("round-3.txt").exists(),
                "recreated checkout should still have the latest content"
            );
        } else {
            eprintln!(
                "soak_tracks_repo_fast: skipping self-heal portion, cannot chown to www-data on this machine"
            );
        }
    }

    #[tokio::test]
    #[serial]
    #[ignore = "long-running soak test using real production timing (30-90s+); run explicitly with `cargo test -- --ignored soak_tracks_repo_real_timing`"]
    async fn soak_tracks_repo_real_timing() {
        let sandbox = Sandbox::new();
        let bare = sandbox
            .seed_origin("acme", "widgets", "main", &[("a.txt", "a")])
            .await;
        let auth = sandbox.git_auth("acme", "widgets", "main");
        let checkout = sandbox.checkout_path("widgets");
        sandbox.clone_checkout(&bare, "main", &checkout).await;

        let state = Arc::new(Mutex::new(test_app_state()));
        let state_path = PathType::from(sandbox.path().join("state.toml"));
        let mut rng = StdRng::seed_from_u64(7);
        let mut cycle_state = WorkerCycleState::default();

        // A healthy cycle's wait must be the real HEALTHY_REFRESH_* window,
        // not a test-shortened one -- and the loop must actually honor it,
        // exactly like repo_worker's real loop does.
        let wait = run_worker_cycle(
            &auth,
            &checkout,
            &state,
            &state_path,
            &None,
            &mut rng,
            &mut cycle_state,
        )
        .await;
        assert!(
            (HEALTHY_REFRESH_MIN_SECS..HEALTHY_REFRESH_MAX_SECS_EXCLUSIVE).contains(&wait),
            "healthy cycle wait {} outside the real production window",
            wait
        );
        sleep(Duration::from_secs(wait)).await;

        // Induce one real failure (unreadable origin) and confirm the
        // returned wait matches the real first-failure backoff window,
        // then actually wait it out and confirm the next cycle recovers.
        std::fs::remove_dir_all(&bare).expect("break the origin to induce a real failure");
        let wait = run_worker_cycle(
            &auth,
            &checkout,
            &state,
            &state_path,
            &None,
            &mut rng,
            &mut cycle_state,
        )
        .await;
        assert!(
            (ERROR_RETRY_BASE_SECS..=(ERROR_RETRY_BASE_SECS + ERROR_RETRY_BASE_SECS / 2))
                .contains(&wait),
            "first-failure wait {} outside the real production window",
            wait
        );
        {
            let s = state.lock().await;
            assert!(
                s.data.contains("state=error"),
                "expected an error report, got: {}",
                s.data
            );
        }
        sleep(Duration::from_secs(wait)).await;

        // Restore the origin and confirm the very next cycle recovers
        // cleanly (consecutive_failures resets, back to a healthy wait).
        sandbox
            .seed_origin("acme", "widgets", "main", &[("a.txt", "a")])
            .await;
        // seed_origin recreates the bare repo at the same path, but the
        // checkout's fetch needs the origin's objects; a fresh fetch will
        // simply find the same history again since content is identical.
        let wait = run_worker_cycle(
            &auth,
            &checkout,
            &state,
            &state_path,
            &None,
            &mut rng,
            &mut cycle_state,
        )
        .await;
        assert!(
            (HEALTHY_REFRESH_MIN_SECS..HEALTHY_REFRESH_MAX_SECS_EXCLUSIVE).contains(&wait),
            "post-recovery wait {} should be back in the healthy window",
            wait
        );
        assert_eq!(cycle_state.consecutive_failures, 0);
    }
}
