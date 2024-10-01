use std::fmt;
use artisan_middleware::config::AppConfig;
use colored::Colorize;
use config::{Config, ConfigError, File};
use serde::Deserialize;


pub fn get_config() -> AppConfig {
    let mut config: AppConfig = AppConfig::new().unwrap();
    config.app_name = env!("CARGO_PKG_NAME").to_string();
    config.version = env!("CARGO_PKG_VERSION").to_string();
    config.database = None;
    config.environment = "Production".to_owned();
    config
}

#[derive(Debug, Deserialize, Clone)]
pub struct AppSpecificConfig {
    pub interval_seconds: u32,
}

impl fmt::Display for AppSpecificConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}:", "App Specific Configuration".bold().underline().purple())?;
        writeln!(
            f,
            "  {}: {}",
            "Interval Seconds".bold().cyan(),
            self.interval_seconds.to_string().bold().yellow()
        )?;
        Ok(())
    }
}

pub fn specific_config() -> Result<AppSpecificConfig, ConfigError> {
    let mut builder = Config::builder();
    builder = builder.add_source(File::with_name("Settings").required(false));

    let settings = builder.build()?;
    let app_specific: AppSpecificConfig = settings.get("app_specific")?;

    Ok(app_specific)
}