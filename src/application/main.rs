use std::{fmt::format, thread, time::Duration};

use artisan_middleware::{
    config::{AppConfig, GitConfig}, git_actions::{generate_git_project_id, generate_git_project_path, GitCredentials}, log, logger::{get_log_level, set_log_level, LogLevel}, state_persistence::{AppState, StatePersistence}, timestamp::current_timestamp
};
use config::{get_config, specific_config};
use dusa_collection_utils::{
    errors::{ErrorArrayItem, Errors},
    types::PathType,
};
use git::{handle_existing_repo, handle_new_repo};
use rand::{rngs::StdRng, seq::SliceRandom, SeedableRng};

mod config;
mod git;

#[tokio::main]
async fn main() {
    let config: AppConfig = get_config();
    let specific_config_result: Result<config::AppSpecificConfig, ::config::ConfigError> =
        specific_config();
    let git_credentials_result: Result<GitCredentials, ErrorArrayItem> =
        get_git_credentials(&config);

    // Loading initial state
    let mut state: AppState = get_initial_state(&config);
    let state_path: PathType = StatePersistence::get_state_path(&config);

    // Setting log level
    set_log_level(config.log_level);
    if config.debug_mode{
        log!(LogLevel::Info, "Loglevel: {}", get_log_level());
    }

    // Loading state information
    match StatePersistence::load_state(&state_path) {
        Ok(loaded_data) => {
            log!(LogLevel::Debug, "Previous state data loaded");
            state = loaded_data
        },
        Err(_) => {
            log!(LogLevel::Warn, "No previous state file found, making new one at {}", state_path);
            if let Err(err) = StatePersistence::save_state(&state, &state_path){
                log!(LogLevel::Error, "Error occurred: {}", err.to_string())
            }    
        },
    };

    // Getting the specific config values
    let specific_config: config::AppSpecificConfig = match specific_config_result {
        Ok(loaded_data) => {
            log!(LogLevel::Debug, "Loaded Settings.toml");
            loaded_data
        },
        Err(e) => {
            log!(LogLevel::Error, "Failed to load GitMonitor.toml: {}", e.to_string());
            state
                .error_log
                .push(ErrorArrayItem::new(Errors::ReadingFile, e.to_string()));
            update_state(&mut state, &state_path);
            return;
        }
    };

    // Ensuring we pulled the git credentials
    let git_credentials = match git_credentials_result {
        Ok(loaded_data) => {
            log!(LogLevel::Debug, "Loaded Git credentials");
            loaded_data
        },
        Err(e) => {
            log!(LogLevel::Error, "Error while loading git credentials: {}", e.to_string());
            state.error_log.push(e);
            update_state(&mut state, &state_path);
            return;
        }
    };

    // Debugging print statment
    if config.debug_mode {
        println!("RUNNING WITH DEBUGGING ENABLED");
        println!("Loaded config: {}", config);
        println!("Loaded specific config {}", specific_config);
        println!("Git credentials loaded {}", git_credentials);
        state.is_active = true;
        println!("Current state: {}", state);
    };

    // Starting the application
    state.is_active = true;
    state.data = String::from("Git monitor is initialized");
    update_state(&mut state, &state_path);
    simple_pretty::output("GREEN", "Git monitor initialized");

    loop {
        // Ensuring that the array we pull is shuffled
        let mut credentials_shuffled: GitCredentials = git_credentials.clone();
        let mut rng: StdRng = StdRng::from_entropy(); // Use a seedable RNG that is Send safe
        credentials_shuffled.auth_items.shuffle(&mut rng);

        for git_item in credentials_shuffled.auth_items {
            let git_item_clone: artisan_middleware::git_actions::GitAuth = git_item.clone();

            let git_project_path = generate_git_project_path(&git_item_clone);
            match git_project_path.exists() {
                true => {
                    if let Err(err) = handle_existing_repo(&git_item_clone, &git_project_path).await
                    {
                        log!(LogLevel::Warn, "{}", err);
                        state.error_log.push(err);
                        update_state(&mut state, &state_path);
                    }
                }
                false => {
                    if let Err(err) =
                        handle_new_repo(&git_item_clone, &git_item_clone.server, &git_project_path)
                            .await
                    {
                        log!(LogLevel::Warn, "{}", err);
                        state.error_log.push(err);
                        update_state(&mut state, &state_path);
                    }
                }
            }

            state.event_counter += 1;
            state.data = format!("Updated: {}", generate_git_project_id(&git_item));
            update_state(&mut state, &state_path);
        }

        thread::sleep(Duration::from_secs(specific_config.interval_seconds.into()));
    }
}

fn get_git_credentials(config: &AppConfig) -> Result<GitCredentials, ErrorArrayItem> {
    let git_config: GitConfig = match <std::option::Option<GitConfig> as Clone>::clone(&config.git){
        Some(loaded_data) => loaded_data,
        None => {
            log!(LogLevel::Error, "Failed to load Git config data");
            std::process::exit(0)
        },
    };
    let git_file: PathType = PathType::Str(git_config.credentials_file.into());
    GitCredentials::new(Some(&git_file))
}

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

fn update_state(state: &mut AppState, path: &PathType) {
    state.last_updated = current_timestamp();
    if let Err(err) = StatePersistence::save_state(&state, path) {
        state.is_active = false;
        state.error_log.push(ErrorArrayItem::new(
            Errors::GeneralError,
            format!("{}", err),
        ));
        return;
    }
}
