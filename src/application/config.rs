use artisan_middleware::aggregator::Status;
use artisan_middleware::config::AppConfig;
use artisan_middleware::state_persistence::{self, update_state, AppState, StatePersistence};
use artisan_middleware::timestamp::current_timestamp;
use dusa_collection_utils::log;
use dusa_collection_utils::logger::{set_log_level, LogLevel};
use dusa_collection_utils::types::pathtype::PathType;
use dusa_collection_utils::types::stringy::Stringy;
use dusa_collection_utils::version::SoftwareVersion;

pub fn get_config() -> AppConfig {
    let mut config: AppConfig = match AppConfig::new() {
        Ok(loaded_data) => loaded_data,
        Err(e) => {
            log!(LogLevel::Error, "Couldn't load config: {}", e.to_string());
            std::process::exit(0)
        }
    };
    config.app_name = Stringy::from(env!("CARGO_PKG_NAME"));
    config.database = None;
    config
}

pub async fn generate_state(config: &AppConfig) -> AppState {
    let state_path: PathType = get_state_path(&config);

    match StatePersistence::load_state(&state_path).await {
        Ok(mut loaded_data) => {
            log!(LogLevel::Info, "Loaded previous state data");
            // log!(LogLevel::Trace, "Previous state data: {:#?}", loaded_data);
            loaded_data.data = String::from("Initializing");
            loaded_data.config.debug_mode = config.debug_mode;
            loaded_data.config.git = config.git.clone();
            loaded_data.last_updated = current_timestamp();
            loaded_data.config.log_level = config.log_level;
            loaded_data.config.aggregator = config.aggregator.clone();
            loaded_data.config.environment = config.environment.clone();
            loaded_data.stared_at = current_timestamp();
            loaded_data.pid = std::process::id();
            set_log_level(loaded_data.config.log_level);
            loaded_data.event_counter = 0;
            if config.debug_mode == true {
                set_log_level(LogLevel::Debug);
            }
            loaded_data.error_log.clear();
            update_state(&mut loaded_data, &state_path, None).await;
            loaded_data
        }
        Err(e) => {
            log!(LogLevel::Warn, "No previous state loaded, creating new one");
            log!(LogLevel::Debug, "Error loading previous state: {}", e);
            let mut state = AppState {
                name: env!("CARGO_PKG_NAME").to_owned(),
                version: SoftwareVersion::dummy(),
                data: String::new(),
                last_updated: current_timestamp(),
                stared_at: current_timestamp(),
                event_counter: 0,
                pid: std::process::id(),
                error_log: vec![],
                config: config.clone(),
                system_application: true,
                status: Status::Starting,
                stdout: Vec::new(),
                stderr: Vec::new(),
            };
            state.data = String::from("Initializing");
            state.config.debug_mode = true;
            state.last_updated = current_timestamp();
            state.config.log_level = config.log_level;
            state.config.environment = config.environment.clone();
            if config.debug_mode == true {
                set_log_level(LogLevel::Debug);
            }
            state.error_log.clear();
            update_state(&mut state, &state_path, None).await;
            state
        }
    }
}

pub fn get_state_path(config: &AppConfig) -> PathType {
    state_persistence::StatePersistence::get_state_path(&config)
}
