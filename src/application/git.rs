use artisan_middleware::{
    dusa_collection_utils::{
        core::{
            errors::{ErrorArrayItem, Errors},
            logger::LogLevel,
            types::pathtype::PathType,
        },
        log,
        platform::functions::truncate,
    },
    git_actions::GitAuth,
    users::{get_id, set_file_ownership},
};
// use dusa_collection_utils::logger::LogLevel;
// use dusa_collection_utils::{
//     errors::{ErrorArrayItem, Errors},
//     types::pathtype::PathType,
// };
// use dusa_collection_utils::{functions::truncate, log};
use once_cell::sync::Lazy;
use std::{collections::HashSet, fs, path::Path};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::{
    auth::github_auth_header,
    config::{APP_CONFIG_DIR, APP_GIT_CONFIG_PATH},
    pull::{checkout_branch, clone_repo},
};

pub enum RepoSyncOutcome {
    Updated(String),
    NoChange(String),
    Cloned(String),
}

struct UpdateDecision {
    should_update: bool,
    reason: String,
    local_commit: String,
    remote_commit: String,
}

// Handle an existing repo: fetch, pull if upstream is ahead, set tracking, restart if needed
pub async fn handle_existing_repo(
    auth: &GitAuth,
    git_project_path: &PathType,
) -> Result<RepoSyncOutcome, ErrorArrayItem> {
    log!(
        LogLevel::Trace,
        "Working on existing git repo {}",
        auth.generate_id()
    );

    fetch_updates(git_project_path).await?;

    let decision: UpdateDecision = match evaluate_update_decision(auth, git_project_path).await {
        Ok(d) => Ok(d),
        Err(err) => Err(ErrorArrayItem::new(Errors::Git, err.to_string())),
    }?;

    let local_short = truncate(decision.local_commit.clone(), 8);
    let remote_short = truncate(decision.remote_commit.clone(), 8);

    if decision.should_update {
        log!(
            LogLevel::Info,
            "{} requires sync (reason: {}, local: {}, remote: {})",
            auth.generate_id(),
            decision.reason,
            local_short,
            remote_short
        );

        checkout_branch(git_project_path.to_str().unwrap(), auth.branch.clone())
            .await
            .map_err(ErrorArrayItem::from)?;

        log!(
            LogLevel::Info,
            "{} synced, runner should rebuild this shortly.",
            auth.generate_id()
        );
        Ok(RepoSyncOutcome::Updated(format!(
            "Synced to origin/{} ({}) because {} (local was {})",
            auth.branch, remote_short, decision.reason, local_short
        )))
    } else {
        log!(LogLevel::Info, "{}: Up to date !", auth.generate_id());
        Ok(RepoSyncOutcome::NoChange(format!(
            "No sync needed ({} == origin/{})",
            local_short, auth.branch
        )))
    }
}

pub async fn handle_new_repo(
    auth: &GitAuth,
    git_project_path: &PathType,
) -> Result<RepoSyncOutcome, ErrorArrayItem> {
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

    Ok(RepoSyncOutcome::Cloned(format!(
        "Cloned and checked out origin/{}",
        auth.branch
    )))
}

static SAFE_DIR_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

fn git_cmd() -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_CONFIG_GLOBAL", APP_GIT_CONFIG_PATH);
    cmd
}

fn ensure_central_git_config() -> Result<(), ErrorArrayItem> {
    fs::create_dir_all(APP_CONFIG_DIR)
        .map_err(|e| ErrorArrayItem::new(Errors::Git, e.to_string()))?;

    if !Path::new(APP_GIT_CONFIG_PATH).exists() {
        fs::File::create(APP_GIT_CONFIG_PATH)
            .map_err(|e| ErrorArrayItem::new(Errors::Git, e.to_string()))?;
    }

    Ok(())
}

fn normalize_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed == "/" {
        return "/".to_string();
    }
    trimmed.trim_end_matches('/').to_string()
}

fn canonicalize_or_normalize(path: &str) -> String {
    match fs::canonicalize(path) {
        Ok(p) => normalize_path(&p.to_string_lossy()),
        Err(_) => normalize_path(path),
    }
}

pub async fn cleanup_safe_directory_entries() -> Result<(), ErrorArrayItem> {
    ensure_central_git_config()?;

    let output = git_cmd()
        .arg("config")
        .arg("--global")
        .arg("--get-all")
        .arg("safe.directory")
        .output()
        .await
        .map_err(|e| ErrorArrayItem::new(Errors::Git, e.to_string()))?;

    if !output.status.success() {
        if output.status.code() == Some(1) {
            return Ok(());
        }
        return Err(ErrorArrayItem::new(
            Errors::Git,
            format!(
                "Failed to read safe.directory entries: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        ));
    }

    let raw_entries: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();

    if raw_entries.is_empty() {
        return Ok(());
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut deduped: Vec<String> = Vec::new();
    let mut rewrite_needed = false;

    for entry in &raw_entries {
        let canonical = canonicalize_or_normalize(entry);
        if canonical != *entry {
            rewrite_needed = true;
        }
        if seen.insert(canonical.clone()) {
            deduped.push(canonical);
        } else {
            rewrite_needed = true;
        }
    }

    if !rewrite_needed {
        return Ok(());
    }

    let unset_status = git_cmd()
        .arg("config")
        .arg("--global")
        .arg("--unset-all")
        .arg("safe.directory")
        .status()
        .await
        .map_err(|e| ErrorArrayItem::new(Errors::Git, e.to_string()))?;

    if !unset_status.success() && unset_status.code() != Some(5) {
        return Err(ErrorArrayItem::new(
            Errors::Git,
            "Failed to clear existing safe.directory entries".to_string(),
        ));
    }

    for entry in deduped {
        let add_status = git_cmd()
            .arg("config")
            .arg("--global")
            .arg("--add")
            .arg("safe.directory")
            .arg(&entry)
            .status()
            .await
            .map_err(|e| ErrorArrayItem::new(Errors::Git, e.to_string()))?;

        if !add_status.success() {
            return Err(ErrorArrayItem::new(
                Errors::Git,
                format!("Failed to add safe.directory entry '{}'", entry),
            ));
        }
    }

    Ok(())
}

// Set the git project as a safe directory
pub async fn set_safe_directory(git_project_path: &PathType) -> Result<(), ErrorArrayItem> {
    log!(
        LogLevel::Trace,
        "Setting safe dir for {}",
        git_project_path.to_string()
    );

    ensure_central_git_config()?;

    let path = canonicalize_or_normalize(&git_project_path.to_string());
    let _guard = SAFE_DIR_LOCK.lock().await;

    // Check if already marked safe
    let check = git_cmd()
        .arg("config")
        .arg("--global")
        .arg("--get")
        .arg("--fixed-value")
        .arg("safe.directory")
        .arg(&path)
        .output()
        .await
        .map_err(|e| ErrorArrayItem::new(Errors::Git, e.to_string()))?;

    if check.status.success() {
        return Ok(());
    }

    if check.status.code() != Some(1) {
        return Err(ErrorArrayItem::new(
            Errors::Git,
            format!(
                "Failed checking safe.directory for '{}': {}",
                path,
                String::from_utf8_lossy(&check.stderr)
            ),
        ));
    }

    let status = git_cmd()
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

    let header: String = match github_auth_header() {
        Some(h) => h,
        None => {
            return Err(ErrorArrayItem::new(
                Errors::Git,
                "GitHub token not initialized".to_string(),
            ));
        }
    };

    let output = git_cmd()
        .arg("-C")
        .arg(git_project_path.to_string())
        .arg("-c")
        .arg(format!("http.extraheader={}", header))
        .arg("fetch")
        .arg("origin")
        .env("GIT_TERMINAL_PROMPT", "0")
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

async fn rev_parse_ref(
    git_project_path: &PathType,
    reference: &str,
) -> Result<String, std::io::Error> {
    let output = git_cmd()
        .arg("-C")
        .arg(git_project_path.to_string())
        .arg("rev-parse")
        .arg(reference)
        .output()
        .await?;

    if !output.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "git rev-parse '{}' failed: {}",
                reference,
                String::from_utf8_lossy(&output.stderr)
            ),
        ));
    }

    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if commit.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("git rev-parse '{}' returned empty output", reference),
        ));
    }

    Ok(commit)
}

async fn is_ancestor(
    git_project_path: &PathType,
    ancestor: &str,
    descendant: &str,
) -> Result<bool, std::io::Error> {
    let output = git_cmd()
        .arg("-C")
        .arg(git_project_path.to_string())
        .arg("merge-base")
        .arg("--is-ancestor")
        .arg(ancestor)
        .arg(descendant)
        .output()
        .await?;

    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "git merge-base --is-ancestor failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        )),
    }
}

// Decide whether repo should be synced to origin/<branch>.
async fn evaluate_update_decision(
    auth: &GitAuth,
    git_project_path: &PathType,
) -> Result<UpdateDecision, std::io::Error> {
    let local_commit = rev_parse_ref(git_project_path, "HEAD").await?;
    let remote_ref = format!("origin/{}", auth.branch);
    let remote_commit = rev_parse_ref(git_project_path, &remote_ref).await?;

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

    if local_commit == remote_commit {
        return Ok(UpdateDecision {
            should_update: false,
            reason: "already at remote HEAD".to_string(),
            local_commit,
            remote_commit,
        });
    }

    let local_is_ancestor = is_ancestor(git_project_path, &local_commit, &remote_commit).await?;
    let remote_is_ancestor = is_ancestor(git_project_path, &remote_commit, &local_commit).await?;

    let reason = if local_is_ancestor {
        "remote ahead".to_string()
    } else if remote_is_ancestor {
        "local drift detected (local ahead)".to_string()
    } else {
        "history diverged".to_string()
    };

    Ok(UpdateDecision {
        should_update: true,
        reason,
        local_commit,
        remote_commit,
    })
}
