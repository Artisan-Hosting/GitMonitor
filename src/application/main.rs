use std::{sync::Arc, time::Duration};

use artisan_middleware::{
    aggregator::Status,
    config::AppConfig,
    git_actions::{generate_git_project_id, generate_git_project_path, GitAuth, GitCredentials},
    resource_monitor::ResourceMonitorLock,
    state_persistence::{log_error, update_state, AppState, StatePersistence},
};
use config::{generate_state, get_config, update_state_wrapper};
use dusa_collection_utils::log;
use dusa_collection_utils::logger::{set_log_level, LogLevel};
use dusa_collection_utils::{
    errors::{ErrorArrayItem, Errors},
    types::pathtype::PathType,
};
use git::{handle_existing_repo, handle_new_repo, set_safe_directory};
use git2::Repository;
use rand::{rngs::StdRng, seq::SliceRandom, Rng, SeedableRng};
use signals::{sighup_watch, sigusr_watch};

use auth::init_gh_token;
use tokio::{sync::{Mutex, Notify}, time::sleep};


mod auth;
mod config;
mod git;
mod pull;
mod signals;

#[tokio::main]
async fn main() {
    tokio::task::LocalSet::new().run_until(async_main()).await;
}

async fn async_main() {
    // Initialization

    if let Err(err) = init_gh_token() {
        log!(LogLevel::Error, "Failed to load GitHub token: {}", err);
    }

    // Loading configs
    let mut config: AppConfig = get_config();
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

    // Update state to indicate initialization
    {
        let mut s = state.lock().await;
        s.config.git = config.git.clone();
        s.data = String::from("Git monitor is initialized");
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

    // Spawn background workers for each repository
    let monitor_clone = monitor.as_ref().map(|m| m.clone());
    spawn_git_workers(&git_credentials, state.clone(), state_path.clone(), monitor_clone).await;

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
    match &config.git {
        Some(git_config) => {
            let git_file: PathType = PathType::Str(git_config.credentials_file.clone().into());
            GitCredentials::new(Some(&git_file)).await
        }
        None => Err(ErrorArrayItem::new(
            Errors::ReadingFile,
            "Git configuration not found".to_string(),
        )),
    }
}

// Process Git repositories, handling existing and new repos
// ! UNUSED RIGHT NOW
async fn _process_git_repositories(
    git_credentials: &GitCredentials,
    state: &mut AppState,
    state_path: &PathType,
    monitor: &Option<ResourceMonitorLock>,
) {
    let mut credentials_shuffled = git_credentials.clone();
    let mut rng: StdRng = StdRng::from_entropy();
    credentials_shuffled.auth_items.shuffle(&mut rng);

    for git_item in credentials_shuffled.auth_items {
        let git_project_path: PathType = generate_git_project_path(&git_item);
        if let Err(err) = set_safe_directory(&git_project_path).await {
            log!(LogLevel::Error, "{}", err.err_mesg)
        }
        // Open the repository directory
        let repo_result = match Repository::open(git_project_path.clone()) {
            Ok(repo) => Ok(repo),
            Err(err) => Err(ErrorArrayItem::new(Errors::Git, err.message())),
        };

        let result = match repo_result {
            Ok(repo) => handle_existing_repo(&git_item, repo, &git_project_path).await,
            Err(err) => {
                log!(
                    LogLevel::Warn,
                    "Failed tp open: {}, Assuming it doesn't exist and clonning. {}",
                    git_project_path,
                    err.err_mesg
                );
                handle_new_repo(&git_item, &git_project_path).await
            }
        };

        if let Err(err) = result {
            log_error(state, err, state_path).await;
        } else {
            state.event_counter += 1;
            state.data = format!("Updated: {}", generate_git_project_id(&git_item));
            update_state_wrapper(state, state_path, monitor).await;
        }

        sleep(Duration::from_secs(1)).await;
    }
}

// Worker that continuously processes a single repository with a random delay
async fn repo_worker(
    git_item: GitAuth,
    state: Arc<Mutex<AppState>>,
    state_path: PathType,
    monitor: Option<ResourceMonitorLock>,
    initial_delay: u64,
) {
    sleep(Duration::from_secs(initial_delay)).await;
    let mut rng: StdRng = StdRng::from_entropy();
    loop {
        let git_project_path: PathType = generate_git_project_path(&git_item);
        if let Err(err) = set_safe_directory(&git_project_path).await {
            log!(LogLevel::Error, "{}", err.err_mesg)
        }

        let repo_result = match Repository::open(git_project_path.clone()) {
            Ok(repo) => Ok(repo),
            Err(err) => Err(ErrorArrayItem::new(Errors::Git, err.message())),
        };

        let result = match repo_result {
            Ok(repo) => handle_existing_repo(&git_item, repo, &git_project_path).await,
            Err(err) => {
                log!(
                    LogLevel::Warn,
                    "Failed tp open: {}, Assuming it doesn't exist and clonning. {}",
                    git_project_path,
                    err.err_mesg
                );
                handle_new_repo(&git_item, &git_project_path).await
            }
        };

        let mut s = state.lock().await;
        if let Err(err) = result {
            log_error(&mut s, err, &state_path).await;
        } else {
            s.event_counter += 1;
            s.data = format!("Updated: {}", generate_git_project_id(&git_item));
            update_state_wrapper(&mut s, &state_path, &monitor).await;
        }
        drop(s);

        let wait = rng.gen_range(25..35);
        sleep(Duration::from_secs(wait)).await;
    }
}

// Spawn workers for each repository with slight timer offsets
async fn spawn_git_workers(
    git_credentials: &GitCredentials,
    state: Arc<Mutex<AppState>>,
    state_path: PathType,
    monitor: Option<ResourceMonitorLock>,
) {
    let mut credentials_shuffled = git_credentials.clone();
    let mut rng: StdRng = StdRng::from_entropy();
    credentials_shuffled.auth_items.shuffle(&mut rng);

    for git_item in credentials_shuffled.auth_items {
		log!(LogLevel::Debug, "Deploying working thread for: {}", generate_git_project_id(&git_item));
        let delay = rng.gen_range(0..5);
        let st = state.clone();
        let path = state_path.clone();
        let mon = monitor.as_ref().map(|m| m.clone());
        tokio::task::spawn_local(async move { repo_worker(git_item, st, path, mon, delay).await });
        sleep(Duration::from_secs(3)).await;
    }
}
