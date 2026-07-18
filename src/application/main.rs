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
    let mut safe_directory_initialized = false;
    let mut consecutive_failures = 0_u32;
    // Forces one submodule backfill pass for repos that predate submodule
    // support; cleared once a cycle (clone or existing-repo pass) succeeds.
    let mut submodules_backfilled = false;
    loop {
        let git_project_path: PathType = generate_git_project_path(&git_item);
        let repo_id = generate_git_project_id(&git_item);
        let result: Result<RepoSyncOutcome, ErrorArrayItem> = match inspect_repo_checkout(
            &git_item,
            &git_project_path,
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
                let result = handle_new_repo(&git_item, &git_project_path).await;
                if result.is_ok() {
                    safe_directory_initialized = true;
                    submodules_backfilled = true;
                }
                result
            }
            Ok(RepoCheckoutState::Invalid { reason }) => {
                safe_directory_initialized = false;
                submodules_backfilled = false;
                recreate_repo(&git_item, &git_project_path, &reason).await
            }
            Ok(RepoCheckoutState::WrongRemote { expected, actual }) => {
                safe_directory_initialized = false;
                submodules_backfilled = false;
                let reason = format!(
                    "origin remote identifies the wrong repository (actual: '{}', expected: '{}')",
                    actual, expected
                );
                recreate_repo(&git_item, &git_project_path, &reason).await
            }
            Ok(RepoCheckoutState::Ready { remote }) => {
                log!(
                    LogLevel::Debug,
                    "{}: checkout state=ready path='{}' remote='{}'",
                    repo_id,
                    git_project_path,
                    remote
                );

                if !safe_directory_initialized {
                    match set_safe_directory(&git_project_path).await {
                        Ok(_) => safe_directory_initialized = true,
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
                            log_error(&mut s, err, &state_path).await;
                            drop(s);
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            let wait = error_retry_delay(&mut rng, consecutive_failures);
                            log!(
                                LogLevel::Warn,
                                "{}: retrying in {} seconds after {} consecutive failure(s)",
                                repo_id,
                                wait,
                                consecutive_failures
                            );
                            sleep(Duration::from_secs(wait)).await;
                            continue;
                        }
                    }
                }

                match handle_existing_repo(&git_item, &git_project_path, !submodules_backfilled)
                    .await
                {
                    Ok(outcome) => {
                        submodules_backfilled = true;
                        Ok(outcome)
                    }
                    Err(sync_error) if sync_error.recreate_checkout => {
                        safe_directory_initialized = false;
                        submodules_backfilled = false;
                        let reason = format!(
                            "local checkout could not be repaired: {}",
                            sync_error.error.err_mesg
                        );
                        recreate_repo(&git_item, &git_project_path, &reason).await
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
                log_error(&mut s, contextual_error, &state_path).await;
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
                        safe_directory_initialized = true;
                        submodules_backfilled = true;
                        s.event_counter += 1;
                        s.data = format!("[repo={} state=recreated] {}", repo_id, msg);
                    }
                    RepoSyncOutcome::NoChange(msg) => {
                        s.data = format!("[repo={} state=ready] {}", repo_id, msg);
                    }
                }
                update_state_wrapper(&mut s, &state_path, &monitor).await;
            }
        }
        drop(s);

        let wait = if cycle_failed {
            consecutive_failures = consecutive_failures.saturating_add(1);
            error_retry_delay(&mut rng, consecutive_failures)
        } else {
            consecutive_failures = 0;
            healthy_refresh_delay(&mut rng)
        };
        if cycle_failed {
            log!(
                LogLevel::Warn,
                "{}: retrying in {} seconds after {} consecutive failure(s)",
                repo_id,
                wait,
                consecutive_failures
            );
        }
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
