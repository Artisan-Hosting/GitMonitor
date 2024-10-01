use artisan_middleware::config::AppConfig;
use config::{Config, ConfigError, File};
use dusa_collection_utils::stringy::Stringy;
use serde::Deserialize;


pub fn get_config() -> AppConfig {
    let mut config: AppConfig = AppConfig::new().unwrap();


    config.app_name = env!("CARGO_PKG_NAME").to_string();
    config.version = env!("CARGO_PKG_VERSION").to_string();
    config.database = None;
    config.debug_mode = true;


    config
}

#[derive(Debug, Deserialize, Clone)]
pub struct AppSpecificConfig {
    pub level: Stringy,
    pub interval_seconds: u32,
    pub register_with_aggregator: bool,
}

pub fn specific_config() -> Result<AppSpecificConfig, ConfigError> {
    let mut builder = Config::builder();
    builder = builder.add_source(File::with_name("Settings").required(false));

    let settings = builder.build()?;
    let app_specific: AppSpecificConfig = settings.get("app_specific")?;

    Ok(app_specific)
}