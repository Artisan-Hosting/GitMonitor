use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use artisan_middleware::{
    aggregator::register_app,
    common::{log_error, update_state},
    config::AppConfig,
    git_actions::{generate_git_project_id, generate_git_project_path, GitCredentials},
    state_persistence::{AppState, StatePersistence},
    timestamp::current_timestamp,
};
use config::get_config;
use dusa_collection_utils::log;
use dusa_collection_utils::log::{set_log_level, LogLevel};
use dusa_collection_utils::{
    errors::{ErrorArrayItem, Errors},
    types::PathType,
    version::SoftwareVersion,
};
use git::{handle_existing_repo, handle_new_repo};
use rand::{rngs::StdRng, seq::SliceRandom, SeedableRng};
use signals::sighup_watch;

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
    let mut state: AppState = load_initial_state(&config, &state_path).await;
    if let Err(err) = register_app(&state).await {
        log!(LogLevel::Error, "Failed to register app: {}", err);
    };
    update_state(&mut state, &state_path, None).await;

    // loading signal handeling
    let reload: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    sighup_watch(reload.clone());

    // Load Git credentials
    let git_credentials: GitCredentials = match get_git_credentials(&state.config).await {
        Ok(credentials) => credentials,
        Err(e) => {
            log_error(&mut state, e, &state_path).await;
            return; // Exit on failure
        }
    };

    // Update state to indicate initialization
    state.is_active = true;
    state.config.git = config.git.clone();
    state.data = String::from("Git monitor is initialized");
    update_state(&mut state, &state_path, None).await;

    if config.debug_mode {
        set_log_level(LogLevel::Debug);
        log!(LogLevel::Debug, "Loaded Initial Config: {}", config);
        log!(LogLevel::Debug, "Git credentials loaded {}", git_credentials);
        set_log_level(state.config.log_level);
    };
    
    log!(LogLevel::Info, "Git monitor initialized");

    // Main loop
    loop {
        // Reloading block
        if reload.load(Ordering::Relaxed) {
            log!(LogLevel::Debug, "Reloading config");

            // Getting the new data
            config = get_config();
            state = load_initial_state(&config, &state_path).await;

            update_state(&mut state, &state_path, None).await;

            log!(LogLevel::Debug, "Reloaded config");
            reload.store(false, Ordering::Relaxed);
        }

        // Application logic
        process_git_repositories(&git_credentials, &mut state, &state_path).await;

        // sleep based on config
        thread::sleep(Duration::from_secs(30));
    }
}

// Load initial state, creating a new state if necessary
async fn load_initial_state(config: &AppConfig, state_path: &PathType) -> AppState {
    match StatePersistence::load_state(state_path).await {
        Ok(mut loaded_data) => {
            log!(LogLevel::Debug, "Previous state data loaded");
            loaded_data.config.debug_mode = config.debug_mode;
            loaded_data.last_updated = current_timestamp();
            // clearing errors from last run
            loaded_data.error_log.clear();
            loaded_data.event_counter = 0;
            loaded_data.is_active = true;
            loaded_data.config.log_level = config.log_level;
            loaded_data.config.aggregator = config.aggregator.clone();
            loaded_data.config.git = config.git.clone();
            loaded_data.config.log_level = config.log_level;
            loaded_data.config.version = config.version.clone();
            set_log_level(loaded_data.config.log_level);
            log!(
                LogLevel::Trace,
                "Initial state has been updated from the config"
            );
            loaded_data
        }
        Err(_) => {
            // this was a weird way to initalize this but it retains the config info
            log!(
                LogLevel::Warn,
                "No previous state file found, creating a new one"
            );
            let state = get_initial_state(config);
            if let Err(err) = StatePersistence::save_state(&state, state_path).await {
                log!(
                    LogLevel::Error,
                    "Error occurred while saving new state: {}",
                    err
                );
            }
            set_log_level(state.config.log_level);
            state
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

// Create an initial state
fn get_initial_state(config: &AppConfig) -> AppState {
    AppState {
        data: String::new(),
        last_updated: current_timestamp(),
        event_counter: 0,
        is_active: false,
        error_log: vec![],
        config: config.clone(),
        name: config.app_name.to_string(),
        version: SoftwareVersion::dummy(),
        system_application: true,
    }
}
