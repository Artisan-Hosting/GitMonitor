use artisan_middleware::{
    git_actions::GitAuth,
    users::{get_id, set_file_ownership},
};
use dusa_collection_utils::logger::LogLevel;
use dusa_collection_utils::{
    errors::{ErrorArrayItem, Errors},
    types::pathtype::PathType,
};
use dusa_collection_utils::{functions::truncate, log};
use once_cell::sync::Lazy;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::{
    auth::github_token,
    pull::{checkout_branch, clone_repo, pull_latest_changes},
};

// Handle an existing repo: fetch, pull if upstream is ahead, set tracking, restart if needed
pub async fn handle_existing_repo(
    auth: &GitAuth,
    git_project_path: &PathType,
) -> Result<(), ErrorArrayItem> {
    log!(
        LogLevel::Trace,
        "Working on existing git repo {}",
        auth.generate_id()
    );

    fetch_updates(git_project_path).await?;

    let remote_ahead: bool = match is_remote_ahead(auth, git_project_path).await {
        Ok(b) => Ok(b),
        Err(err) => Err(ErrorArrayItem::new(Errors::Git, err.to_string())),
    }?;

    if remote_ahead {
        checkout_branch(git_project_path.to_str().unwrap(), auth.branch.clone())
            .await
            .map_err(ErrorArrayItem::from)?;

        pull_latest_changes(git_project_path.to_str().unwrap(), auth.branch.clone())
            .await
            .map_err(ErrorArrayItem::from)?;

        log!(
            LogLevel::Info,
            "{} Updated, runner should rebuild this shortly.",
            auth.generate_id()
        );
    } else {
        log!(LogLevel::Info, "{}: Up to date !", auth.generate_id());
    }

    Ok(())
}

pub async fn handle_new_repo(
    auth: &GitAuth,
    git_project_path: &PathType,
) -> Result<(), ErrorArrayItem> {
    // Clone the repository
    let repo_url = auth.assemble_remote_url();
    clone_repo(&repo_url, git_project_path)
        .await
        .map_err(|err| ErrorArrayItem::new(Errors::Git, err.to_string()))?;

    // Set ownership to the web user
    let webuser = get_id("www-data")?;
    set_file_ownership(&git_project_path, webuser.0, webuser.1)?;

    // Set safe directory
    set_safe_directory(git_project_path).await?;

    checkout_branch(git_project_path.to_str().unwrap(), auth.branch.clone())
        .await
        .map_err(ErrorArrayItem::from)?;

    Ok(())
}

static SAFE_DIR_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

// Set the git project as a safe directory
pub async fn set_safe_directory(git_project_path: &PathType) -> Result<(), ErrorArrayItem> {
    log!(
        LogLevel::Trace,
        "Setting safe dir for {}",
        git_project_path.to_string()
    );

    let path = git_project_path.to_string();
    let _guard = SAFE_DIR_LOCK.lock().await;

    // Check if already marked safe
    let check = Command::new("git")
        .arg("config")
        .arg("--global")
        .arg("--get-all")
        .arg("safe.directory")
        .output()
        .await
        .map_err(|e| ErrorArrayItem::new(Errors::Git, e.to_string()))?;

    if check.status.success() {
        let existing = String::from_utf8_lossy(&check.stdout);
        if existing.lines().any(|l| l.trim() == path) {
            return Ok(());
        }
    }

    let status = Command::new("git")
        .arg("config")
        .arg("--global")
        .arg("--add")
        .arg("safe.directory")
        .arg(&path)
        .status()
        .await
        .map_err(|e| ErrorArrayItem::new(Errors::Git, e.to_string()))?;

    if status.success() {
        Ok(())
    } else {
        Err(ErrorArrayItem::new(
            Errors::Git,
            format!("Failed to set safe directory for {}", path),
        ))
    }
}

// Fetch updates from the remote repository
pub async fn fetch_updates(git_project_path: &PathType) -> Result<(), ErrorArrayItem> {
    log!(
        LogLevel::Debug,
        "Fetching updates for, {}",
        git_project_path
    );

    let token: &'static str = match github_token() {
        Some(t) => t,
        None => {
            return Err(ErrorArrayItem::new(
                Errors::Git,
                "GitHub token not initialized".to_string(),
            ));
        }
    };

    let header = format!("Authorization: Bearer {}", token);
    let output = Command::new("git")
        .arg("-C")
        .arg(git_project_path.to_string())
        .arg("-c")
        .arg(format!("http.extraheader={}", header))
        .arg("fetch")
        .arg("origin")
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(ErrorArrayItem::new(
            Errors::Git,
            format!("git fetch failed: {}", String::from_utf8_lossy(&out.stderr)),
        )),
        Err(e) => Err(ErrorArrayItem::new(Errors::Git, e.to_string())),
    }
}

// Check if the upstream branch is ahead of the local branch
async fn is_remote_ahead(
    auth: &GitAuth,
    git_project_path: &PathType,
) -> Result<bool, std::io::Error> {
    let local = Command::new("git")
        .arg("-C")
        .arg(git_project_path.to_string())
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .await?;

    let remote = Command::new("git")
        .arg("-C")
        .arg(git_project_path.to_string())
        .arg("rev-parse")
        .arg(format!("origin/{}", auth.branch))
        .output()
        .await?;

    let local_commit = String::from_utf8_lossy(&local.stdout).trim().to_string();
    let remote_commit = String::from_utf8_lossy(&remote.stdout).trim().to_string();

    log!(
        LogLevel::Trace,
        "Latest commit on remote: {}",
        truncate(remote_commit.clone(), 8)
    );
    log!(
        LogLevel::Trace,
        "Latest local commit: {}",
        truncate(local_commit.clone(), 8)
    );

    Ok(local_commit != remote_commit)
}
