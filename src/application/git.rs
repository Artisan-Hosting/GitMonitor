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
    git_actions::{generate_git_project_path, GitAuth, GitServer},
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
    config::{app_config_dir, app_git_config_path},
    pull::{checkout_branch, clone_repo},
};

pub enum RepoSyncOutcome {
    Updated(String),
    NoChange(String),
    Cloned(String),
    Recreated(String),
}

pub enum RepoCheckoutState {
    Missing,
    Ready { remote: String },
    Invalid { reason: String },
    WrongRemote { expected: String, actual: String },
}

pub struct RepoSyncError {
    pub error: ErrorArrayItem,
    pub recreate_checkout: bool,
}

impl RepoSyncError {
    fn retry(error: ErrorArrayItem) -> Self {
        Self {
            error,
            recreate_checkout: false,
        }
    }

    fn recreate(error: ErrorArrayItem) -> Self {
        Self {
            error,
            recreate_checkout: true,
        }
    }
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
    force_submodule_sync: bool,
) -> Result<RepoSyncOutcome, RepoSyncError> {
    log!(
        LogLevel::Trace,
        "Working on existing git repo {}",
        auth.generate_id()
    );

    // Fetch failures are normally external (network, authentication, or remote
    // service). Recreate only when the local object database also fails fsck.
    if let Err(fetch_error) = fetch_updates(git_project_path).await {
        if let Err(local_error) = validate_local_repository(git_project_path).await {
            return Err(RepoSyncError::recreate(ErrorArrayItem::new(
                Errors::Git,
                format!(
                    "fetch failed ({}) and local repository validation failed ({})",
                    fetch_error.err_mesg, local_error.err_mesg
                ),
            )));
        }
        return Err(RepoSyncError::retry(fetch_error));
    }

    let decision: UpdateDecision = match evaluate_update_decision(auth, git_project_path).await {
        Ok(d) => Ok(d),
        Err(err) => Err(ErrorArrayItem::new(Errors::Git, err.to_string())),
    }
    .map_err(RepoSyncError::recreate)?;

    let local_short = truncate(decision.local_commit.clone(), 8);
    let remote_short = truncate(decision.remote_commit.clone(), 8);

    let outcome = if decision.should_update {
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
            .map_err(ErrorArrayItem::from)
            .map_err(RepoSyncError::recreate)?;

        log!(
            LogLevel::Info,
            "{} synced, runner should rebuild this shortly.",
            auth.generate_id()
        );
        RepoSyncOutcome::Updated(format!(
            "Synced to origin/{} ({}) because {} (local was {})",
            auth.branch, remote_short, decision.reason, local_short
        ))
    } else {
        log!(LogLevel::Info, "{}: Up to date !", auth.generate_id());
        RepoSyncOutcome::NoChange(format!(
            "No sync needed ({} == origin/{})",
            local_short, auth.branch
        ))
    };

    // Only pay for submodule sync when the superproject actually moved, or on
    // the caller-driven one-time backfill pass for older checkouts. Submodule
    // failures are retryable and never trigger superproject recreation.
    if decision.should_update || force_submodule_sync {
        if let Err(err) = sync_submodules(git_project_path).await {
            log!(
                LogLevel::Warn,
                "{}: submodule sync failed, will retry next cycle: {}",
                auth.generate_id(),
                err.err_mesg
            );
            return Err(RepoSyncError::retry(err));
        }
    }

    Ok(outcome)
}

pub async fn handle_new_repo(
    auth: &GitAuth,
    git_project_path: &PathType,
) -> Result<RepoSyncOutcome, ErrorArrayItem> {
    // Clone the repository
    // Build the URL from the configured server/owner/repository identity. The
    // token is supplied as an HTTP header and must never be embedded in a URL.
    let repo_url = expected_remote_url(auth);
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

    sync_submodules(git_project_path).await.map_err(|err| {
        log!(
            LogLevel::Warn,
            "{}: submodule sync failed after clone, will retry next cycle: {}",
            auth.generate_id(),
            err.err_mesg
        );
        err
    })?;

    Ok(RepoSyncOutcome::Cloned(format!(
        "Cloned and checked out origin/{}",
        auth.branch
    )))
}

pub async fn inspect_repo_checkout(
    auth: &GitAuth,
    git_project_path: &PathType,
) -> Result<RepoCheckoutState, ErrorArrayItem> {
    let path = git_project_path.to_string();
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RepoCheckoutState::Missing)
        }
        Err(err) => {
            return Err(ErrorArrayItem::new(
                Errors::Git,
                format!("Failed to inspect managed checkout '{}': {}", path, err),
            ))
        }
    };

    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(RepoCheckoutState::Invalid {
            reason: "managed checkout path is not a directory".to_string(),
        });
    }

    let inspection_safe_directory = canonicalize_or_normalize(&path);
    let top_level = git_cmd()
        .arg("-c")
        .arg(format!("safe.directory={}", inspection_safe_directory))
        .arg("-C")
        .arg(&path)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .await
        .map_err(|err| ErrorArrayItem::new(Errors::Git, err.to_string()))?;

    if !top_level.status.success() {
        return Ok(RepoCheckoutState::Invalid {
            reason: format!(
                "path is not a valid Git worktree: {}",
                String::from_utf8_lossy(&top_level.stderr).trim()
            ),
        });
    }

    let reported_top_level = String::from_utf8_lossy(&top_level.stdout)
        .trim()
        .to_string();
    if canonicalize_or_normalize(&reported_top_level) != canonicalize_or_normalize(&path) {
        return Ok(RepoCheckoutState::Invalid {
            reason: format!(
                "Git worktree root is '{}' instead of the managed path",
                reported_top_level
            ),
        });
    }

    let remote_output = git_cmd()
        .arg("-c")
        .arg(format!("safe.directory={}", inspection_safe_directory))
        .arg("-C")
        .arg(&path)
        .arg("remote")
        .arg("get-url")
        .arg("origin")
        .output()
        .await
        .map_err(|err| ErrorArrayItem::new(Errors::Git, err.to_string()))?;

    if !remote_output.status.success() {
        return Ok(RepoCheckoutState::Invalid {
            reason: format!(
                "origin remote is missing or unreadable: {}",
                String::from_utf8_lossy(&remote_output.stderr).trim()
            ),
        });
    }

    let actual = String::from_utf8_lossy(&remote_output.stdout)
        .trim()
        .to_string();
    let expected = expected_remote_url(auth);
    let actual_identity = canonical_remote_identity(&actual);
    let expected_identity = canonical_remote_identity(&expected);

    if actual_identity != expected_identity {
        return Ok(RepoCheckoutState::WrongRemote {
            expected: redact_remote_url(&expected),
            actual: redact_remote_url(&actual),
        });
    }

    if is_ssh_remote(&actual) && !is_ssh_remote(&expected) {
        let actual_safe = redact_remote_url(&actual);
        let expected_safe = redact_remote_url(&expected);
        log!(
            LogLevel::Warn,
            "{}: SSH origin '{}' conflicts with git.cf; attempting HTTP rewrite to '{}'",
            auth.generate_id(),
            actual_safe,
            expected_safe
        );

        let rewrite = git_cmd()
            .arg("-c")
            .arg(format!("safe.directory={}", inspection_safe_directory))
            .arg("-C")
            .arg(&path)
            .arg("remote")
            .arg("set-url")
            .arg("origin")
            .arg(&expected)
            .output()
            .await
            .map_err(|err| ErrorArrayItem::new(Errors::Git, err.to_string()))?;

        if rewrite.status.success() {
            log!(
                LogLevel::Warn,
                "{}: SSH origin rewrite succeeded; correct the repository URL in git.cf/source configuration",
                auth.generate_id()
            );
        } else {
            let reason = format!(
                "SSH origin rewrite failed: {}",
                String::from_utf8_lossy(&rewrite.stderr).trim()
            );
            log!(LogLevel::Error, "{}: {}", auth.generate_id(), reason);
            return Ok(RepoCheckoutState::Invalid { reason });
        }
    }

    Ok(RepoCheckoutState::Ready {
        remote: redact_remote_url(&expected),
    })
}

pub async fn recreate_repo(
    auth: &GitAuth,
    git_project_path: &PathType,
    reason: &str,
) -> Result<RepoSyncOutcome, ErrorArrayItem> {
    let configured_path = generate_git_project_path(auth).to_string();
    let actual_path = git_project_path.to_string();
    if configured_path != actual_path || actual_path == "/" {
        return Err(ErrorArrayItem::new(
            Errors::DeletingDirectory,
            format!(
                "Refusing to remove unmanaged path '{}' (configured path is '{}')",
                actual_path, configured_path
            ),
        ));
    }

    log!(
        LogLevel::Warn,
        "{}: recreating managed checkout at '{}' because {}; git.cf remains the source of truth",
        auth.generate_id(),
        actual_path,
        reason
    );

    match fs::symlink_metadata(&actual_path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(&actual_path).map_err(|err| {
                ErrorArrayItem::new(
                    Errors::DeletingDirectory,
                    format!("Failed to remove checkout '{}': {}", actual_path, err),
                )
            })?;
        }
        Ok(_) => {
            fs::remove_file(&actual_path).map_err(|err| {
                ErrorArrayItem::new(
                    Errors::DeletingFile,
                    format!("Failed to remove checkout path '{}': {}", actual_path, err),
                )
            })?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(ErrorArrayItem::new(
                Errors::DeletingDirectory,
                format!("Failed to inspect checkout before removal: {}", err),
            ))
        }
    }

    log!(
        LogLevel::Info,
        "{}: removed stale checkout; cloning configured repository",
        auth.generate_id()
    );

    handle_new_repo(auth, git_project_path).await?;
    Ok(RepoSyncOutcome::Recreated(format!(
        "Recreated checkout from git.cf after {}",
        reason
    )))
}

fn expected_remote_url(auth: &GitAuth) -> String {
    let base = match &auth.server {
        GitServer::GitHub => "https://github.com".to_string(),
        GitServer::GitLab => "https://gitlab.com".to_string(),
        GitServer::Custom(url) => ssh_to_http_url(url).unwrap_or_else(|| url.to_string()),
    };

    format!(
        "{}/{}/{}.git",
        base.trim_end_matches('/'),
        auth.user,
        auth.repo
    )
}

fn is_ssh_remote(url: &str) -> bool {
    let trimmed = url.trim();
    trimmed.starts_with("ssh://")
        || (trimmed.contains('@') && !trimmed.contains("://") && trimmed.contains(':'))
}

fn ssh_to_http_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    if let Some(remainder) = trimmed.strip_prefix("ssh://") {
        let without_user = remainder
            .rsplit_once('@')
            .map_or(remainder, |(_, host)| host);
        let (host, path) = without_user.split_once('/')?;
        return Some(format!("https://{}/{}", host, path));
    }

    if !trimmed.contains("://") {
        let (authority, path) = trimmed.split_once(':')?;
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        if !host.is_empty() && !path.is_empty() {
            return Some(format!("https://{}/{}", host, path));
        }
    }

    None
}

fn redact_remote_url(url: &str) -> String {
    let trimmed = url.trim();
    let Some(scheme_index) = trimmed.find("://") else {
        return trimmed.to_string();
    };
    let authority_start = scheme_index + 3;
    let authority_end = trimmed[authority_start..]
        .find('/')
        .map(|index| authority_start + index)
        .unwrap_or(trimmed.len());
    let authority = &trimmed[authority_start..authority_end];
    let safe_authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);

    format!(
        "{}{}{}",
        &trimmed[..authority_start],
        safe_authority,
        &trimmed[authority_end..]
    )
}

fn canonical_remote_identity(url: &str) -> Option<String> {
    let safe = redact_remote_url(url);
    let trimmed = safe.trim().trim_end_matches('/');
    let (host, path) = if let Some(scheme_index) = trimmed.find("://") {
        let remainder = &trimmed[scheme_index + 3..];
        remainder.split_once('/')?
    } else {
        let (authority, path) = trimmed.split_once(':')?;
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        (host, path)
    };

    let normalized_path = path
        .trim_matches('/')
        .strip_suffix(".git")
        .unwrap_or_else(|| path.trim_matches('/'));
    if host.is_empty() || normalized_path.is_empty() {
        return None;
    }

    Some(format!("{}/{}", host.to_ascii_lowercase(), normalized_path))
}

// GitHub host the extraheader credential is scoped to, so it never leaks to a
// third-party host referenced by .gitmodules.
const GITHUB_REMOTE_PREFIX: &str = "https://github.com/";

// Builds the `-c` argument that scopes the auth header to GITHUB_REMOTE_PREFIX
// only. Split out for direct unit testing: a live round trip can't easily
// prove the scope matches (redirecting a github.com-scoped request to a
// local test server changes the request URL, which changes what the scope
// matches against), so this is verified as a pure string-construction check.
fn scoped_extraheader_arg(header: &str) -> String {
    format!("http.{}.extraheader={}", GITHUB_REMOTE_PREFIX, header)
}

// Initialize and update submodules (if any) to match the superproject's recorded commits.
async fn sync_submodules(git_project_path: &PathType) -> Result<(), ErrorArrayItem> {
    if !git_project_path.join(".gitmodules").exists() {
        return Ok(());
    }

    let path = git_project_path.to_string();

    log!(LogLevel::Debug, "Syncing submodules for {}", path);

    // Pick up any URL changes recorded in .gitmodules before updating.
    let sync_status = git_cmd()
        .arg("-C")
        .arg(&path)
        .arg("submodule")
        .arg("sync")
        .arg("--recursive")
        .status()
        .await
        .map_err(|e| ErrorArrayItem::new(Errors::Git, e.to_string()))?;

    if !sync_status.success() {
        return Err(ErrorArrayItem::new(
            Errors::Git,
            format!("git submodule sync failed for {}", path),
        ));
    }

    rewrite_ssh_submodule_urls(git_project_path).await?;

    let header: String = github_auth_header().ok_or_else(|| {
        ErrorArrayItem::new(Errors::Git, "GitHub token not initialized".to_string())
    })?;

    let output = git_cmd()
        .arg("-C")
        .arg(&path)
        .arg("-c")
        .arg(scoped_extraheader_arg(&header))
        .arg("submodule")
        .arg("update")
        .arg("--init")
        .arg("--recursive")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .map_err(|e| ErrorArrayItem::new(Errors::Git, e.to_string()))?;

    if output.status.success() {
        log!(LogLevel::Debug, "Submodules synced for {}", path);
        Ok(())
    } else {
        Err(ErrorArrayItem::new(
            Errors::Git,
            format!(
                "git submodule update failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        ))
    }
}

async fn rewrite_ssh_submodule_urls(git_project_path: &PathType) -> Result<(), ErrorArrayItem> {
    let path = git_project_path.to_string();
    let configured_urls = git_cmd()
        .arg("-C")
        .arg(&path)
        .arg("config")
        .arg("--file")
        .arg(".gitmodules")
        .arg("--get-regexp")
        .arg(r"^submodule\..*\.url$")
        .output()
        .await
        .map_err(|err| ErrorArrayItem::new(Errors::Git, err.to_string()))?;

    if !configured_urls.status.success() {
        if configured_urls.status.code() == Some(1) {
            return Ok(());
        }
        return Err(ErrorArrayItem::new(
            Errors::Git,
            format!(
                "failed to inspect submodule URLs: {}",
                String::from_utf8_lossy(&configured_urls.stderr).trim()
            ),
        ));
    }

    for line in String::from_utf8_lossy(&configured_urls.stdout).lines() {
        let Some((key, configured_url)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let configured_url = configured_url.trim();
        let Some(http_url) = ssh_to_http_url(configured_url) else {
            continue;
        };

        log!(
            LogLevel::Warn,
            "SSH submodule URL '{}' is misconfigured; attempting local HTTP rewrite to '{}'",
            redact_remote_url(configured_url),
            redact_remote_url(&http_url)
        );

        let rewrite = git_cmd()
            .arg("-C")
            .arg(&path)
            .arg("config")
            .arg(key)
            .arg(&http_url)
            .output()
            .await
            .map_err(|err| ErrorArrayItem::new(Errors::Git, err.to_string()))?;

        if rewrite.status.success() {
            log!(
                LogLevel::Warn,
                "SSH submodule URL rewrite succeeded for '{}'; update .gitmodules in the source repository",
                key
            );
        } else {
            let message = format!(
                "SSH submodule URL rewrite failed for '{}': {}",
                key,
                String::from_utf8_lossy(&rewrite.stderr).trim()
            );
            log!(LogLevel::Error, "{}", message);
            return Err(ErrorArrayItem::new(Errors::Git, message));
        }
    }

    Ok(())
}

static SAFE_DIR_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

fn git_cmd() -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_CONFIG_GLOBAL", app_git_config_path());
    cmd
}

fn ensure_central_git_config() -> Result<(), ErrorArrayItem> {
    fs::create_dir_all(app_config_dir())
        .map_err(|e| ErrorArrayItem::new(Errors::Git, e.to_string()))?;

    let git_config_path = app_git_config_path();
    if !Path::new(&git_config_path).exists() {
        fs::File::create(&git_config_path)
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

async fn validate_local_repository(git_project_path: &PathType) -> Result<(), ErrorArrayItem> {
    let output = git_cmd()
        .arg("-C")
        .arg(git_project_path.to_string())
        .arg("fsck")
        .arg("--connectivity-only")
        .arg("--no-dangling")
        .output()
        .await
        .map_err(|err| ErrorArrayItem::new(Errors::Git, err.to_string()))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(ErrorArrayItem::new(
            Errors::Git,
            format!(
                "git fsck failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ))
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

async fn current_branch(git_project_path: &PathType) -> Result<Option<String>, std::io::Error> {
    let output = git_cmd()
        .arg("-C")
        .arg(git_project_path.to_string())
        .arg("symbolic-ref")
        .arg("--quiet")
        .arg("--short")
        .arg("HEAD")
        .output()
        .await?;

    match output.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )),
        Some(1) => Ok(None),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "git symbolic-ref failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        )),
    }
}

async fn has_worktree_drift(git_project_path: &PathType) -> Result<bool, std::io::Error> {
    let output = git_cmd()
        .arg("-C")
        .arg(git_project_path.to_string())
        .arg("status")
        .arg("--porcelain")
        .arg("--untracked-files=normal")
        .output()
        .await?;

    if !output.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "git status failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        ));
    }

    Ok(!output.stdout.is_empty())
}

// Decide whether repo should be synced to origin/<branch>.
async fn evaluate_update_decision(
    auth: &GitAuth,
    git_project_path: &PathType,
) -> Result<UpdateDecision, std::io::Error> {
    let local_commit = rev_parse_ref(git_project_path, "HEAD").await?;
    let remote_ref = format!("origin/{}", auth.branch);
    let remote_commit = rev_parse_ref(git_project_path, &remote_ref).await?;
    let branch = current_branch(git_project_path).await?;
    let configured_branch = auth.branch.to_string();
    let branch_matches = branch.as_deref() == Some(configured_branch.as_str());
    let worktree_drift = has_worktree_drift(git_project_path).await?;

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

    if local_commit == remote_commit && branch_matches && !worktree_drift {
        return Ok(UpdateDecision {
            should_update: false,
            reason: "already at remote HEAD".to_string(),
            local_commit,
            remote_commit,
        });
    }

    let mut reasons = Vec::new();
    if !branch_matches {
        reasons.push(format!(
            "branch drift (current: {}, configured: {})",
            branch.unwrap_or_else(|| "detached HEAD".to_string()),
            configured_branch
        ));
    }
    if worktree_drift {
        reasons.push("working tree drift".to_string());
    }
    if local_commit != remote_commit {
        let local_is_ancestor =
            is_ancestor(git_project_path, &local_commit, &remote_commit).await?;
        let remote_is_ancestor =
            is_ancestor(git_project_path, &remote_commit, &local_commit).await?;
        reasons.push(if local_is_ancestor {
            "remote ahead".to_string()
        } else if remote_is_ancestor {
            "local history drift (local ahead)".to_string()
        } else {
            "history diverged".to_string()
        });
    }

    let reason = reasons.join(", ");

    Ok(UpdateDecision {
        should_update: true,
        reason,
        local_commit,
        remote_commit,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        canonical_remote_identity, expected_remote_url, redact_remote_url, scoped_extraheader_arg,
        ssh_to_http_url, GITHUB_REMOTE_PREFIX,
    };
    use artisan_middleware::{
        dusa_collection_utils::core::types::stringy::Stringy,
        git_actions::{GitAuth, GitServer},
    };

    fn auth(user: &str, repo: &str, branch: &str, server: GitServer) -> GitAuth {
        GitAuth {
            user: Stringy::from(user),
            repo: Stringy::from(repo),
            branch: Stringy::from(branch),
            server,
            token: None,
        }
    }

    #[test]
    fn ssh_and_https_urls_have_the_same_repository_identity() {
        let ssh = canonical_remote_identity("git@github.com:owner/repository.git");
        let https = canonical_remote_identity("https://github.com/owner/repository.git");

        assert_eq!(ssh, https);
    }

    #[test]
    fn identity_ignores_trailing_slash_and_host_case() {
        let a = canonical_remote_identity("https://GitHub.com/owner/repository.git/");
        let b = canonical_remote_identity("https://github.com/owner/repository.git");

        assert_eq!(a, b);
    }

    #[test]
    fn identity_treats_missing_dot_git_suffix_the_same() {
        let with_suffix = canonical_remote_identity("https://github.com/owner/repository.git");
        let without_suffix = canonical_remote_identity("https://github.com/owner/repository");

        assert_eq!(with_suffix, without_suffix);
    }

    #[test]
    fn identity_is_none_for_a_pathless_url() {
        assert_eq!(canonical_remote_identity("https://github.com/"), None);
        assert_eq!(canonical_remote_identity("not-a-url-at-all"), None);
    }

    #[test]
    fn rewrites_common_ssh_url_forms_to_https() {
        assert_eq!(
            ssh_to_http_url("git@github.com:owner/repository.git"),
            Some("https://github.com/owner/repository.git".to_string())
        );
        assert_eq!(
            ssh_to_http_url("ssh://git@github.com/owner/repository.git"),
            Some("https://github.com/owner/repository.git".to_string())
        );
    }

    #[test]
    fn ssh_to_http_url_leaves_already_http_urls_alone() {
        assert_eq!(
            ssh_to_http_url("https://github.com/owner/repository.git"),
            None
        );
    }

    #[test]
    fn removes_credentials_before_remote_urls_are_logged() {
        assert_eq!(
            redact_remote_url("https://oauth2:secret@github.com/owner/repository.git"),
            "https://github.com/owner/repository.git"
        );
    }

    #[test]
    fn redact_is_a_no_op_for_urls_without_credentials() {
        assert_eq!(
            redact_remote_url("https://github.com/owner/repository.git"),
            "https://github.com/owner/repository.git"
        );
    }

    #[test]
    fn expected_remote_url_for_github() {
        let a = auth("owner", "repository", "main", GitServer::GitHub);
        assert_eq!(
            expected_remote_url(&a),
            "https://github.com/owner/repository.git"
        );
    }

    #[test]
    fn expected_remote_url_for_gitlab() {
        let a = auth("owner", "repository", "main", GitServer::GitLab);
        assert_eq!(
            expected_remote_url(&a),
            "https://gitlab.com/owner/repository.git"
        );
    }

    #[test]
    fn expected_remote_url_for_plain_custom_server() {
        let a = auth(
            "owner",
            "repository",
            "main",
            GitServer::Custom("https://git.example.internal".to_string()),
        );
        assert_eq!(
            expected_remote_url(&a),
            "https://git.example.internal/owner/repository.git"
        );
    }

    #[test]
    fn expected_remote_url_rewrites_an_ssh_looking_custom_base() {
        // A Custom server base that looks like an SSH remote gets run through
        // the same ssh_to_http_url conversion as everything else, so git.cf
        // entries pointing at `git@host:path` still resolve to an HTTP(S)
        // URL this app can actually authenticate against.
        let a = auth(
            "owner",
            "repository",
            "main",
            GitServer::Custom("git@git.example.internal:base".to_string()),
        );
        assert_eq!(
            expected_remote_url(&a),
            "https://git.example.internal/base/owner/repository.git"
        );
    }

    #[test]
    fn expected_remote_url_trims_trailing_slash_on_custom_base() {
        let a = auth(
            "owner",
            "repository",
            "main",
            GitServer::Custom("https://git.example.internal/".to_string()),
        );
        assert_eq!(
            expected_remote_url(&a),
            "https://git.example.internal/owner/repository.git"
        );
    }

    #[test]
    fn submodule_extraheader_is_scoped_to_github_only() {
        let arg = scoped_extraheader_arg("Authorization: Basic abc123");
        assert_eq!(
            arg,
            format!(
                "http.{}.extraheader=Authorization: Basic abc123",
                GITHUB_REMOTE_PREFIX
            )
        );
        // Sanity: the scope prefix really is github.com, not some broader
        // pattern that would also match e.g. a GitLab or self-hosted URL.
        assert!(arg.starts_with("http.https://github.com/.extraheader="));
    }
}

#[cfg(test)]
mod integration_tests {
    use super::{
        fetch_updates, handle_existing_repo, inspect_repo_checkout, recreate_repo,
        rewrite_ssh_submodule_urls, sync_submodules, RepoCheckoutState, RepoSyncOutcome,
    };
    use crate::test_support::{run_git, run_git_output, Sandbox};
    use serial_test::serial;

    fn checkout_dir(
        checkout: &artisan_middleware::dusa_collection_utils::core::types::pathtype::PathType,
    ) -> std::path::PathBuf {
        std::path::PathBuf::from(checkout.to_string())
    }

    #[tokio::test]
    #[serial]
    async fn inspect_repo_checkout_reports_missing_for_a_nonexistent_path() {
        let sandbox = Sandbox::new();
        sandbox
            .seed_origin("acme", "widgets", "main", &[("README.md", "hi")])
            .await;
        let auth = sandbox.git_auth("acme", "widgets", "main");
        let checkout = sandbox.checkout_path("widgets");

        let state = inspect_repo_checkout(&auth, &checkout)
            .await
            .expect("inspect should succeed");
        assert!(matches!(state, RepoCheckoutState::Missing));
    }

    #[tokio::test]
    #[serial]
    async fn inspect_repo_checkout_reports_ready_for_a_healthy_clone() {
        let sandbox = Sandbox::new();
        let bare = sandbox
            .seed_origin("acme", "widgets", "main", &[("README.md", "hi")])
            .await;
        let auth = sandbox.git_auth("acme", "widgets", "main");
        let checkout = sandbox.checkout_path("widgets");
        sandbox.clone_checkout(&bare, "main", &checkout).await;

        let state = inspect_repo_checkout(&auth, &checkout)
            .await
            .expect("inspect should succeed");
        assert!(matches!(state, RepoCheckoutState::Ready { .. }));
    }

    #[tokio::test]
    #[serial]
    async fn inspect_repo_checkout_reports_invalid_for_a_corrupted_git_dir() {
        let sandbox = Sandbox::new();
        let bare = sandbox
            .seed_origin("acme", "widgets", "main", &[("README.md", "hi")])
            .await;
        let auth = sandbox.git_auth("acme", "widgets", "main");
        let checkout = sandbox.checkout_path("widgets");
        sandbox.clone_checkout(&bare, "main", &checkout).await;

        std::fs::remove_file(checkout_dir(&checkout).join(".git").join("HEAD"))
            .expect("corrupt the checkout");

        let state = inspect_repo_checkout(&auth, &checkout)
            .await
            .expect("inspect should succeed");
        assert!(matches!(state, RepoCheckoutState::Invalid { .. }));
    }

    #[tokio::test]
    #[serial]
    async fn inspect_repo_checkout_reports_wrong_remote_when_origin_points_elsewhere() {
        use crate::test_support::AuthHeaderProbeServer;

        let sandbox = Sandbox::new();
        sandbox
            .seed_origin("acme", "widgets", "main", &[("a.txt", "a")])
            .await;
        sandbox
            .seed_origin("acme", "gadgets", "main", &[("b.txt", "b")])
            .await;
        // canonical_remote_identity compares host+path, so it needs a
        // proper hostful URL to tell two repos apart -- a bare local path
        // or `file:///abs/path` URL has no authority component at all, so
        // identity comparison degenerates to None == None for any two local
        // repos (see git_auth's file:// helper; not exercised by this test).
        let probe = AuthHeaderProbeServer::start(&sandbox.remotes_root());
        let checkout = sandbox.checkout_path("widgets");
        run_git(
            &[],
            sandbox.path(),
            &[
                "clone",
                "-q",
                "--branch",
                "main",
                &format!("{}/acme/widgets.git", probe.base_url()),
                &checkout.to_string(),
            ],
        )
        .await;

        // git.cf says this checkout should be "gadgets", but it's actually widgets.
        let auth_expecting_gadgets =
            sandbox.git_auth_with_server("acme", "gadgets", "main", &probe.base_url());

        let state = inspect_repo_checkout(&auth_expecting_gadgets, &checkout)
            .await
            .expect("inspect should succeed");
        assert!(matches!(state, RepoCheckoutState::WrongRemote { .. }));
    }

    #[tokio::test]
    #[serial]
    async fn handle_existing_repo_reports_no_change_when_nothing_moved() {
        let sandbox = Sandbox::new();
        let bare = sandbox
            .seed_origin("acme", "widgets", "main", &[("a.txt", "a")])
            .await;
        let auth = sandbox.git_auth("acme", "widgets", "main");
        let checkout = sandbox.checkout_path("widgets");
        sandbox.clone_checkout(&bare, "main", &checkout).await;

        let outcome = handle_existing_repo(&auth, &checkout, false)
            .await
            .map_err(|e| e.error)
            .expect("sync should succeed");
        assert!(matches!(outcome, RepoSyncOutcome::NoChange(_)));
    }

    #[tokio::test]
    #[serial]
    async fn handle_existing_repo_reports_updated_after_a_new_upstream_commit() {
        let sandbox = Sandbox::new();
        let bare = sandbox
            .seed_origin("acme", "widgets", "main", &[("a.txt", "a")])
            .await;
        let auth = sandbox.git_auth("acme", "widgets", "main");
        let checkout = sandbox.checkout_path("widgets");
        sandbox.clone_checkout(&bare, "main", &checkout).await;

        let new_sha = sandbox
            .commit_to_origin(&bare, "main", &[("b.txt", "b")], "add b")
            .await;

        let outcome = handle_existing_repo(&auth, &checkout, false)
            .await
            .map_err(|e| e.error)
            .expect("sync should succeed");
        assert!(matches!(outcome, RepoSyncOutcome::Updated(_)));

        let head = run_git_output(&checkout_dir(&checkout), &["rev-parse", "HEAD"]).await;
        assert_eq!(head.trim(), new_sha);
        assert!(checkout_dir(&checkout).join("b.txt").exists());
    }

    #[tokio::test]
    #[serial]
    async fn handle_existing_repo_resets_branch_drift() {
        let sandbox = Sandbox::new();
        let bare = sandbox
            .seed_origin("acme", "widgets", "main", &[("a.txt", "a")])
            .await;
        let auth = sandbox.git_auth("acme", "widgets", "main");
        let checkout = sandbox.checkout_path("widgets");
        sandbox.clone_checkout(&bare, "main", &checkout).await;

        // Detach HEAD locally so the current branch no longer matches git.cf.
        run_git(
            &[],
            &checkout_dir(&checkout),
            &["checkout", "--detach", "HEAD"],
        )
        .await;

        let outcome = handle_existing_repo(&auth, &checkout, false)
            .await
            .map_err(|e| e.error)
            .expect("sync should succeed");
        assert!(matches!(outcome, RepoSyncOutcome::Updated(_)));

        let branch = run_git_output(
            &checkout_dir(&checkout),
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
        )
        .await;
        assert_eq!(branch.trim(), "main");
    }

    #[tokio::test]
    #[serial]
    async fn handle_existing_repo_resets_worktree_drift() {
        let sandbox = Sandbox::new();
        let bare = sandbox
            .seed_origin("acme", "widgets", "main", &[("a.txt", "a")])
            .await;
        let auth = sandbox.git_auth("acme", "widgets", "main");
        let checkout = sandbox.checkout_path("widgets");
        sandbox.clone_checkout(&bare, "main", &checkout).await;

        std::fs::write(checkout_dir(&checkout).join("a.txt"), "modified locally")
            .expect("simulate local drift");

        let outcome = handle_existing_repo(&auth, &checkout, false)
            .await
            .map_err(|e| e.error)
            .expect("sync should succeed");
        assert!(matches!(outcome, RepoSyncOutcome::Updated(_)));

        let contents = std::fs::read_to_string(checkout_dir(&checkout).join("a.txt")).unwrap();
        assert_eq!(
            contents, "a",
            "checkout should have been reset to upstream content"
        );
    }

    #[tokio::test]
    #[serial]
    async fn recreate_repo_refuses_to_touch_an_unmanaged_path() {
        let sandbox = Sandbox::new();
        let auth = sandbox.git_auth("acme", "widgets", "main");
        // Definitely not equal to generate_git_project_path(&auth) (which is
        // hardcoded to /var/www/ais/<hash>).
        let checkout = sandbox.checkout_path("widgets");
        std::fs::create_dir_all(checkout_dir(&checkout)).unwrap();
        std::fs::write(checkout_dir(&checkout).join("keepme.txt"), "important").unwrap();

        let result = recreate_repo(&auth, &checkout, "test-induced").await;
        assert!(
            result.is_err(),
            "recreate_repo should refuse an unmanaged path"
        );
        assert!(
            checkout_dir(&checkout).join("keepme.txt").exists(),
            "recreate_repo must not have deleted anything"
        );
    }

    #[tokio::test]
    #[serial]
    async fn rewrite_ssh_submodule_urls_rewrites_gitmodules_entries_locally() {
        let sandbox = Sandbox::new();
        let bare = sandbox
            .seed_origin("acme", "widgets", "main", &[("a.txt", "a")])
            .await;
        let checkout = sandbox.checkout_path("widgets");
        sandbox.clone_checkout(&bare, "main", &checkout).await;

        // Manually seed a .gitmodules entry with an SSH-style URL -- this
        // doesn't need a real, initialized submodule; rewrite_ssh_submodule_urls
        // only reads .gitmodules and rewrites the checkout's local config.
        run_git(
            &[],
            &checkout_dir(&checkout),
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.url",
                "git@example.internal:owner/sub.git",
            ],
        )
        .await;
        run_git(
            &[],
            &checkout_dir(&checkout),
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.path",
                "sub",
            ],
        )
        .await;

        rewrite_ssh_submodule_urls(&checkout)
            .await
            .expect("rewrite should succeed");

        let rewritten = run_git_output(
            &checkout_dir(&checkout),
            &["config", "--get", "submodule.sub.url"],
        )
        .await;
        assert_eq!(
            rewritten.trim(),
            "https://example.internal/owner/sub.git",
            "local submodule config should now point at the HTTPS form"
        );
    }

    #[tokio::test]
    #[serial]
    async fn sync_submodules_initializes_a_reachable_submodule() {
        use crate::test_support::AuthHeaderProbeServer;

        let sandbox = Sandbox::new();
        let sub_bare = sandbox
            .seed_origin("acme", "sublib", "main", &[("lib.txt", "lib contents")])
            .await;
        let probe = AuthHeaderProbeServer::start(sub_bare.parent().unwrap());
        let submodule_url = format!("{}/sublib.git", probe.base_url());

        let super_bare = sandbox
            .seed_origin("acme", "widgets", "main", &[("a.txt", "a")])
            .await;
        sandbox
            .add_submodule(&super_bare, "main", &submodule_url, "libsub")
            .await;

        let checkout = sandbox.checkout_path("widgets");
        sandbox.clone_checkout(&super_bare, "main", &checkout).await;

        sync_submodules(&checkout)
            .await
            .expect("submodule sync should succeed");

        assert!(
            checkout_dir(&checkout)
                .join("libsub")
                .join("lib.txt")
                .exists(),
            "submodule content should have been fetched and checked out"
        );

        // This submodule URL is http://127.0.0.1:<port>/..., which never
        // matches the GITHUB_REMOTE_PREFIX scope -- proving the auth header
        // doesn't leak to non-GitHub submodule hosts, not just that *some*
        // fetch succeeded.
        assert!(
            probe.received_auth_headers().iter().all(Option::is_none),
            "no Authorization header should have been sent to a non-GitHub submodule host: {:?}",
            probe.received_auth_headers()
        );
        assert!(
            probe.request_count() > 0,
            "the probe server should have seen requests"
        );
    }

    #[tokio::test]
    #[serial]
    async fn fetch_updates_sends_the_configured_auth_header() {
        use crate::auth::github_auth_header;
        use crate::test_support::AuthHeaderProbeServer;

        let sandbox = Sandbox::new();
        let bare = sandbox
            .seed_origin("acme", "widgets", "main", &[("a.txt", "a")])
            .await;
        let checkout = sandbox.checkout_path("widgets");
        sandbox.clone_checkout(&bare, "main", &checkout).await;

        // Point the checkout's origin at the probe server instead of the
        // file:// bare repo, so fetch_updates' real HTTP request (and its
        // -c http.extraheader=...) actually goes over the wire where it can
        // be observed, instead of file:// transport (which never touches
        // HTTP headers at all).
        let probe = AuthHeaderProbeServer::start(&sandbox.remotes_root());
        run_git(
            &[],
            &checkout_dir(&checkout),
            &[
                "remote",
                "set-url",
                "origin",
                &format!("{}/acme/widgets.git", probe.base_url()),
            ],
        )
        .await;

        fetch_updates(&checkout)
            .await
            .expect("fetch should succeed");

        let expected_header = github_auth_header().expect("token should be initialized");
        let expected_value = expected_header
            .strip_prefix("Authorization: ")
            .expect("header should be an Authorization line");
        assert!(
            probe
                .received_auth_headers()
                .iter()
                .any(|h| h.as_deref() == Some(expected_value)),
            "expected an Authorization header matching {:?}, got {:?}",
            expected_value,
            probe.received_auth_headers()
        );
    }

    // handle_new_repo (and recreate_repo, which reclones via handle_new_repo)
    // unconditionally chown the fresh clone to `www-data`, which needs both
    // that user to exist and root/CAP_CHOWN -- neither holds on an arbitrary
    // dev box. These are correct and will run for real wherever the daemon
    // actually deploys; run them explicitly there with
    // `cargo test -- --ignored`.

    #[tokio::test]
    #[serial]
    #[ignore = "requires a www-data user and chown privileges (root)"]
    async fn handle_new_repo_clones_and_checks_out_the_configured_branch() {
        use super::handle_new_repo;
        use crate::test_support::can_chown_to_www_data;

        if !can_chown_to_www_data() {
            eprintln!("skipping: cannot chown to www-data on this machine");
            return;
        }

        let sandbox = Sandbox::new();
        sandbox
            .seed_origin("acme", "widgets", "main", &[("README.md", "hello")])
            .await;
        let auth = sandbox.git_auth("acme", "widgets", "main");
        let checkout = sandbox.checkout_path("widgets");

        let outcome = handle_new_repo(&auth, &checkout)
            .await
            .expect("clone should succeed");
        assert!(matches!(outcome, RepoSyncOutcome::Cloned(_)));
        assert!(checkout_dir(&checkout).join("README.md").exists());
    }

    #[tokio::test]
    #[serial]
    #[ignore = "requires a www-data user and chown privileges (root)"]
    async fn recreate_repo_wipes_and_reclones_a_corrupted_checkout() {
        use crate::test_support::can_chown_to_www_data;

        if !can_chown_to_www_data() {
            eprintln!("skipping: cannot chown to www-data on this machine");
            return;
        }

        let sandbox = Sandbox::new();
        let bare = sandbox
            .seed_origin("acme", "widgets", "main", &[("a.txt", "a")])
            .await;
        let auth = sandbox.git_auth("acme", "widgets", "main");
        let checkout = sandbox.checkout_path("widgets");
        sandbox.clone_checkout(&bare, "main", &checkout).await;
        std::fs::remove_file(checkout_dir(&checkout).join(".git").join("HEAD")).unwrap();
        std::fs::write(checkout_dir(&checkout).join("stale-marker.txt"), "old").unwrap();

        let outcome = recreate_repo(&auth, &checkout, "corrupted for test")
            .await
            .expect("recreate should succeed");
        assert!(matches!(outcome, RepoSyncOutcome::Recreated(_)));
        assert!(!checkout_dir(&checkout).join("stale-marker.txt").exists());
        assert!(checkout_dir(&checkout).join("a.txt").exists());
    }
}
