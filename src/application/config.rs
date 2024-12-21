use artisan_middleware::config::AppConfig;
use artisan_middleware::version::{aml_version, str_to_version};
use dusa_collection_utils::version::{SoftwareVersion, Version, VersionCode};
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

    let raw_version: SoftwareVersion = {
        // defining the version
        let library_version: Version = aml_version();
        let software_version: Version = str_to_version(env!("CARGO_PKG_VERSION"), Some(VersionCode::Production));
        
        SoftwareVersion {
            application: software_version,
            library: library_version,
        }
    };

    config.version = match serde_json::to_string(&raw_version) {
        Ok(ver) => ver,
        Err(err) => {
            log!(LogLevel::Error, "{}", err);
            std::process::exit(100);
        },
    };

    config.app_name = Stringy::from(env!("CARGO_PKG_NAME"));
    config.database = None;
    config
}