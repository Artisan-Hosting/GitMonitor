use std::process::Command;

pub fn get_gh_token() -> std::io::Result<String> {
    let output = Command::new("gh")
        .arg("auth")
        .arg("token")
        .output()?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Failed to get token from GitHub CLI",
        ))
    }
}
