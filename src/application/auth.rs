use base64::{engine::general_purpose, Engine as _};
use once_cell::sync::OnceCell;
use std::fs;
use std::process::Command;

static GH_TOKEN: OnceCell<String> = OnceCell::new();

pub fn init_gh_token(token_file: Option<&str>) -> std::io::Result<()> {
    let token = get_configured_or_cli_token(token_file)?;
    let _ = GH_TOKEN.set(token);
    Ok(())
}

pub fn github_token() -> Option<&'static str> {
    GH_TOKEN.get().map(|s| s.as_str())
}

pub fn github_auth_header() -> Option<String> {
    github_token().map(|token| {
        let creds = format!("x-access-token:{}", token);
        let encoded = general_purpose::STANDARD.encode(creds);
        format!("Authorization: Basic {}", encoded)
    })
}

pub fn get_gh_token() -> std::io::Result<String> {
    let output = Command::new("gh").arg("auth").arg("token").output()?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Failed to get token from GitHub CLI",
        ))
    }
}

fn get_configured_or_cli_token(token_file: Option<&str>) -> std::io::Result<String> {
    if let Some(path) = token_file {
        if !path.trim().is_empty() {
            match get_token_from_file(path) {
                Ok(token) => return Ok(token),
                Err(file_err) => {
                    return get_gh_token().map_err(|err| {
                        std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!(
                                "Failed to load token from token_file '{}': {}. Fallback to GitHub CLI also failed: {}",
                                path, file_err, err
                            ),
                        )
                    });
                }
            }
        }
    }

    get_gh_token()
}

fn get_token_from_file(path: &str) -> std::io::Result<String> {
    let raw = fs::read_to_string(path)?;
    parse_token(&raw).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("No token found in token file '{}'", path),
        )
    })
}

fn parse_token(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(value) = trimmed.parse::<toml::Value>() {
        if let Some(token) = value.get("token").and_then(toml::Value::as_str) {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
        if let Some(token) = value
            .get("git")
            .and_then(|v| v.get("token"))
            .and_then(toml::Value::as_str)
        {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
        if let Some(token) = value
            .get("github")
            .and_then(|v| v.get("token"))
            .and_then(toml::Value::as_str)
        {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }

    trimmed
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
}
