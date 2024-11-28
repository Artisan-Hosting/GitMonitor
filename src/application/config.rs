use artisan_middleware::config::AppConfig;
use dusa_collection_utils::version::SoftwareVersion;
use dusa_collection_utils::{log, stringy::Stringy};
use dusa_collection_utils::log::LogLevel;

pub fn get_config() -> AppConfig {
    let mut config: AppConfig = match AppConfig::new() {
        Ok(loaded_data) => loaded_data,
        Err(e) => {
            log!(LogLevel::Error, "Couldn't load config: {}", e.to_string());
            std::process::exit(0)
        }
    };
    let version = SoftwareVersion::dummy();
    config.app_name = Stringy::from(env!("CARGO_PKG_NAME"));
    config.version = version.to_string();
    config.database = None;
    config
}