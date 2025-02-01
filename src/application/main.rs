use std::{sync::Arc, time::Duration};

use artisan_middleware::{
    aggregator::Status,
    common::{log_error, update_state},
    config::AppConfig,
    git_actions::{generate_git_project_id, generate_git_project_path, GitCredentials},
    state_persistence::{AppState, StatePersistence},
};
use config::{generate_state, get_config};
use dusa_collection_utils::log;
use dusa_collection_utils::log::{set_log_level, LogLevel};
use dusa_collection_utils::{
    errors::{ErrorArrayItem, Errors},
    types::PathType,
};
use git::{handle_existing_repo, handle_new_repo};
use rand::{rngs::StdRng, seq::SliceRandom, SeedableRng};
use signals::{sighup_watch, sigusr_watch};
use tokio::{sync::Notify, time::sleep};

mod config;
mod git;
mod pull;
mod signals;

#[tokio::main]
async fn main() {
    // Initialization

    // Loading configs
    let mut config: AppConfig = get_config();
    let state_path: PathType = StatePersistence::get_state_path(&config);
    let mut state: AppState = generate_state(&config).await;
    update_state(&mut state, &state_path, None).await;

    // loading signal handeling
    let reload: Arc<Notify> = Arc::new(Notify::new());
    let exit_graceful: Arc<Notify> = Arc::new(Notify::new());

    sighup_watch(reload.clone());
    sigusr_watch(exit_graceful.clone());

    // Load Git credentials
    let mut git_credentials: GitCredentials = match get_git_credentials(&state.config).await {
        Ok(credentials) => credentials,
        Err(e) => {
            log_error(&mut state, e, &state_path).await;
            std::process::exit(100)
        }
    };

    // Update state to indicate initialization
    state.config.git = config.git.clone();
    state.data = String::from("Git monitor is initialized");
    state.status = Status::Idle;
    update_state(&mut state, &state_path, None).await;

    if config.debug_mode {
        set_log_level(LogLevel::Debug);
        log!(LogLevel::Debug, "Loaded Initial Config: {}", config);
        log!(
            LogLevel::Debug,
            "Git credentials loaded {}",
            git_credentials
        );
        set_log_level(state.config.log_level);
    };

    log!(LogLevel::Info, "Git monitor initialized");

    // Main loop
    loop {
        tokio::select! {

            _ = reload.notified() => {
                sleep(Duration::from_secs(1)).await;
                config = get_config();
                state = generate_state(&config).await;

                if let Ok(git) = get_git_credentials(&state.config).await {
                    git_credentials = git;
                }
            }

            _ = exit_graceful.notified() => {
                log!(LogLevel::Info, "Shutting down gracefully");
                process_git_repositories(&git_credentials, &mut state, &state_path).await;
                std::process::exit(0)
            }

            _ = tokio::signal::ctrl_c() => {
                log!(LogLevel::Info, "CTRL + C recieved");
                exit_graceful.notify_one();
            }

            _ = tokio::time::sleep(Duration::from_secs(20)) => {
                state.status = Status::Idle;
                update_state(&mut state, &state_path, None).await;
                process_git_repositories(&git_credentials, &mut state, &state_path).await;
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
async fn process_git_repositories(
    git_credentials: &GitCredentials,
    state: &mut AppState,
    state_path: &PathType,
) {
    let mut credentials_shuffled = git_credentials.clone();
    let mut rng: StdRng = StdRng::from_entropy();
    credentials_shuffled.auth_items.shuffle(&mut rng);

    for git_item in credentials_shuffled.auth_items {
        let git_project_path = generate_git_project_path(&git_item);
        let result = if git_project_path.exists() {
            handle_existing_repo(&git_item, &git_project_path).await
        } else {
            handle_new_repo(&git_item, &git_item.server, &git_project_path).await
        };

        if let Err(err) = result {
            log_error(state, err, state_path).await;
        } else {
            state.event_counter += 1;
            state.data = format!("Updated: {}", generate_git_project_id(&git_item));
            update_state(state, state_path, None).await;
        }
    }
}
