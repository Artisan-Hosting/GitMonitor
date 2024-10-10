use std::{thread, time::Duration};

use artisan_middleware::{
    common::{log_error, update_state}, config::AppConfig, git_actions::{generate_git_project_id, generate_git_project_path, GitCredentials}, log, logger::{set_log_level, LogLevel}, state_persistence::{AppState, StatePersistence}, timestamp::current_timestamp
};
use config::{get_config, specific_config, AppSpecificConfig};
use dusa_collection_utils::{
    errors::{ErrorArrayItem, Errors},
    types::PathType,
};
use git::{handle_existing_repo, handle_new_repo};
use rand::{rngs::StdRng, seq::SliceRandom, SeedableRng};

mod config;
mod git;
mod pull;

#[tokio::main]
async fn main() {
    // Initialization
    let config: AppConfig = get_config();
    let state_path: PathType = StatePersistence::get_state_path(&config);
    let mut state: AppState = load_initial_state(&config, &state_path);

    // Load Override configuration
    let specific_config: AppSpecificConfig = match load_specific_config(&mut state, &state_path) {
        Some(cfg) => cfg,
        None => return, // Exit on failure
    };

    // Load Git credentials
    let git_credentials: GitCredentials = match get_git_credentials(&state.config) {
        Ok(credentials) => credentials,
        Err(e) => {
            log_error(&mut state, e, &state_path);
            return; // Exit on failure
        }
    };

    // Update state to indicate initialization
    set_log_level(state.config.log_level);
    state.is_active = true;
    state.config.git = specific_config.git.clone();
    state.data = String::from("Git monitor is initialized");
    update_state(&mut state, &state_path);

    if config.debug_mode {
        println!("Loaded Initial Config: {}", config);
        println!("Loaded Overrides Config \n{}\n", specific_config);
        println!("Git credentials loaded {}", git_credentials);
        println!("Current state: {}", state);
    };
    simple_pretty::output("GREEN", "Git monitor initialized");

    // Main loop
    loop {
        process_git_repositories(&git_credentials, &mut state, &state_path).await;
        thread::sleep(Duration::from_secs(specific_config.interval_seconds.into()));
    }
}

// Load initial state, creating a new state if necessary
fn load_initial_state(config: &AppConfig, state_path: &PathType) -> AppState {
    match StatePersistence::load_state(state_path) {
        Ok(mut loaded_data) => {
            log!(LogLevel::Debug, "Previous state data loaded");
            loaded_data.config.debug_mode = config.debug_mode;
            loaded_data.last_updated = current_timestamp();
            // clearing errors from last run
            loaded_data.error_log.clear();
            loaded_data.is_active = true;
            loaded_data.config.log_level = config.log_level;
            update_state(&mut loaded_data, state_path);
            log!(LogLevel::Trace, "Initial state has been updated from the config");
            loaded_data
        }
        Err(_) => {
            log!(
                LogLevel::Warn,
                "No previous state file found, creating a new one"
            );
            let state = get_initial_state(config);
            if let Err(err) = StatePersistence::save_state(&state, state_path) {
                log!(
                    LogLevel::Error,
                    "Error occurred while saving new state: {}",
                    err
                );
            }
            state
        }
    }
}

// Load specific configuration and update the state in case of errors
fn load_specific_config(state: &mut AppState, state_path: &PathType) -> Option<AppSpecificConfig> {
    match specific_config() {
        Ok(cfg) => {
            log!(LogLevel::Debug, "Loaded Overrides.toml");
            state.config.git = cfg.clone().git;
            update_state(state, state_path);
            Some(cfg)
        }
        Err(e) => {
            log!(
                LogLevel::Error,
                "Failed to load Overrides.toml: {}",
                e.to_string()
            );
            log_error(
                state,
                ErrorArrayItem::new(Errors::ReadingFile, e.to_string()),
                state_path,
            );
            None
        }
    }
}

// Load Git credentials from the configuration
fn get_git_credentials(config: &AppConfig) -> Result<GitCredentials, ErrorArrayItem> {
    match &config.git {
        Some(git_config) => {
            let git_file: PathType = PathType::Str(git_config.credentials_file.clone().into());
            GitCredentials::new(Some(&git_file))
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
            log_error(state, err, state_path);
        } else {
            state.event_counter += 1;
            state.data = format!("Updated: {}", generate_git_project_id(&git_item));
            update_state(state, state_path);
        }
    }
}

// Create an initial state
fn get_initial_state(config: &AppConfig) -> AppState {
    AppState {
        data: String::new(),
        last_updated: current_timestamp(),
        event_counter: 0,
        is_active: false,
        error_log: vec![],
        config: config.clone(),
    }
}
