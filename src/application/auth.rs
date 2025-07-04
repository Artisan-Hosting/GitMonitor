use base64::{engine::general_purpose, Engine as _};
use once_cell::sync::OnceCell;
use std::process::Command;

static GH_TOKEN: OnceCell<String> = OnceCell::new();

pub fn init_gh_token() -> std::io::Result<()> {
    let token = get_gh_token()?;
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
