use std::fmt;
use artisan_middleware::{config::{AppConfig, GitConfig}, log, logger::LogLevel};
use colored::Colorize;
use config::{Config, ConfigError, File};
use serde::Deserialize;


pub fn get_config() -> AppConfig {
    let mut config: AppConfig = match AppConfig::new() {
        Ok(loaded_data) => loaded_data,
        Err(e) => {
            log!(LogLevel::Error, "Couldn't load config: {}", e.to_string());
            std::process::exit(0)
        },
    };
    config.app_name = env!("CARGO_PKG_NAME").to_string();
    config.version = env!("CARGO_PKG_VERSION").to_string();
    config.database = None;
    config.aggregator = None;
    config
}

#[derive(Debug, Deserialize, Clone)]
pub struct AppSpecificConfig {
    pub interval_seconds: u32,
    pub git: Option<GitConfig>,
}


impl fmt::Display for AppSpecificConfig {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let interval_str = format!("Interval Seconds: {}", self.interval_seconds).green();
        let git_str = match &self.git {
            Some(git) => format!(
                "Git Config - Credentials file: {}, Default Server: {}",
                git.credentials_file.yellow(),
                git.default_server.to_string().yellow(),
            ),
            None => "Git Config: None".red().to_string(),
        };

        write!(f, "{}\n{}", interval_str, git_str)
    }
}

pub fn specific_config() -> Result<AppSpecificConfig, ConfigError> {
    let mut builder = Config::builder();
    builder = builder.add_source(File::with_name("/etc/git_monitor/Config").required(false));

    let settings = builder.build()?;
    let app_specific: AppSpecificConfig = settings.get("app_specific")?;

    Ok(app_specific)
}