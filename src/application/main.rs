use std::{thread, time::Duration};

use artisan_middleware::{
    config::{AppConfig, GitConfig},
    git_actions::{generate_git_project_path, GitCredentials},
    state_persistence::{AppState, StatePersistence},
    timestamp::current_timestamp,
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
    let mut state: AppState = get_initial_state(&config);
    let state_path: PathType = PathType::Content(format!("/tmp/.{}.state", config.app_name));

    // setting initial state
    if let Err(err) = StatePersistence::save_state(&state, &state_path) {
        state.error_log.push(ErrorArrayItem::new(
            Errors::GeneralError,
            format!("{}", err),
        ));
        println!("{:?}", state);
        return;
    };

    // Getting the specific config values
    let specific_config: config::AppSpecificConfig = match specific_config_result {
        Ok(d) => d,
        Err(e) => {
            eprintln!("An error occoured");
            state
                .error_log
                .push(ErrorArrayItem::new(Errors::ReadingFile, e.to_string()));
            let _ = StatePersistence::save_state(&state, &state_path);
            return;
        }
    };

    // Ensuring we pulled the git credentials
    let git_credentials = match git_credentials_result {
        Ok(d) => d,
        Err(e) => {
            eprintln!("An error has occoured");
            state.error_log.push(e);
            let _ = StatePersistence::save_state(&state, &state_path);
            return;
        }
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
                        state.error_log.push(err);
                        update_state(&mut state, &state_path);
                    }
                }
                false => {
                    if let Err(err) =
                        handle_new_repo(&git_item_clone, &git_item_clone.server, &git_project_path)
                            .await
                    {
                        state.error_log.push(err);
                        update_state(&mut state, &state_path);
                    }
                }
            }

            state.event_counter += 1;
            update_state(&mut state, &state_path);
        }

        thread::sleep(Duration::from_secs(specific_config.interval_seconds.into()));
    }

    // println!("Done");
}

fn get_git_credentials(config: &AppConfig) -> Result<GitCredentials, ErrorArrayItem> {
    let git_config: GitConfig = <std::option::Option<GitConfig> as Clone>::clone(&config.git)
        .expect("Failed to load the git credentials file");
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
