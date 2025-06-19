use dusa_collection_utils::log;
use dusa_collection_utils::logger::LogLevel;
use dusa_collection_utils::types::pathtype::PathType;
use dusa_collection_utils::types::stringy::Stringy;
use tokio::process::Command;

use crate::auth::{github_token, github_auth_header};

/// Pulls the latest changes using `git pull`.
pub async fn pull_latest_changes(repo_path: &str, branch_name: Stringy) -> std::io::Result<()> {
    let header: String = match github_auth_header() {
        Some(h) => h,
        None => {
            let err =
                std::io::Error::new(std::io::ErrorKind::Other, "GitHub token not initialized");
            return Err(err);
        }
    };
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("-c")
        .arg(format!("http.extraheader={}", header))
        .arg("pull")
        .arg("origin")
        .arg(branch_name)
        .arg("--rebase")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await?;

    if output.status.success() {
        log!(
            LogLevel::Info,
            "Successfully pulled latest changes for: {}.",
            repo_path
        );
        Ok(())
    } else {
        log!(LogLevel::Error, "Failed to pull changes: {:?}", output);
        let msg = format!(
            "git pull failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Err(std::io::Error::new(std::io::ErrorKind::Other, msg))
    }
}

/// Clones the repository if it does not exist.
pub async fn clone_repo(repo_url: &str, dest_path: &PathType) -> std::io::Result<()> {
    if dest_path.exists() {
        return Ok(());
    }

    log!(LogLevel::Info, "Cloning repository into {}", dest_path);

    let token: &'static str = match github_token() {
        Some(t) => t,
        None => {
            log!(LogLevel::Error, "GitHub token not initialized");
            return Ok(());
        }
    };

    let url_with_token = repo_url.replace("https://", &format!("https://oauth2:{}@", token));
    let output = Command::new("git")
        .arg("clone")
        .arg(url_with_token)
        .arg(dest_path.to_string())
        .output()
        .await?;

    if output.status.success() {
        Ok(())
    } else {
        let msg = format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Err(std::io::Error::new(std::io::ErrorKind::Other, msg))
    }
}

/// Switches to the specified branch.
pub async fn checkout_branch(repo_path: &str, branch_name: Stringy) -> std::io::Result<()> {
    let branch = branch_name.to_string();
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("checkout")
        .arg("-B")
        .arg(&branch)
        .arg(format!("origin/{}", branch))
        .output()
        .await?;

    if output.status.success() {
        log!(LogLevel::Debug, "Switched to branch '{}'", branch);
        Ok(())
    } else {
        let msg = format!(
            "git checkout failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Err(std::io::Error::new(std::io::ErrorKind::Other, msg))
    }
}
