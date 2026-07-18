use artisan_middleware::{
    cli::{get_user_input, get_user_selection, get_yes_no},
    config::AppConfig,
    dusa_collection_utils::{
        core::{
            errors::ErrorArrayItem,
            logger::{set_log_level, LogLevel},
            types::{pathtype::PathType, stringy::Stringy},
        },
        log,
    },
    git_actions::{generate_git_project_path, GitAuth, GitCredentials, GitServer},
};
use git_config::resolve_git_credentials_path;
use std::{
    collections::HashSet,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

const AIS_REPO_ROOT: &str = "/var/www/ais";

#[path = "../application/auth.rs"]
mod auth;
#[allow(dead_code)]
#[path = "../application/config.rs"]
mod config;
#[allow(dead_code)]
#[path = "../application/git.rs"]
mod git;
#[path = "../git_config.rs"]
mod git_config;
#[path = "../application/pull.rs"]
mod pull;
#[cfg(test)]
#[path = "../application/test_support.rs"]
mod test_support;

pub fn get_config() -> AppConfig {
    let mut config: AppConfig = match AppConfig::new() {
        Ok(loaded_data) => loaded_data,
        Err(e) => {
            log!(LogLevel::Error, "Couldn't load config: {}", e.to_string());
            std::process::exit(0)
        }
    };
    config.app_name = Stringy::from(env!("CARGO_PKG_NAME"));
    config.database = None;
    config
}

fn git_credentials_path(config: &AppConfig) -> PathType {
    let configured_path = config
        .git
        .as_ref()
        .map(|git_config| git_config.credentials_file.as_str());
    resolve_git_credentials_path(configured_path)
}

async fn get_git_credentials(config: &AppConfig) -> Result<GitCredentials, ErrorArrayItem> {
    let git_file = git_credentials_path(config);

    log!(LogLevel::Debug, "Loading git credentials from {}", git_file);
    GitCredentials::new(Some(&git_file)).await
}

async fn prompt_server_choice() -> GitServer {
    println!("Select the Git server:");
    println!("1. GitHub");
    println!("2. GitLab");
    println!("3. Custom");

    loop {
        let choice: Stringy = get_user_input("Enter your choice (1/2/3): ");

        match choice.to_string().as_str() {
            "1" => return GitServer::GitHub,
            "2" => return GitServer::GitLab,
            "3" => {
                let custom_url: Stringy = get_user_input("Enter the custom server URL: ");
                return GitServer::Custom(custom_url.to_string());
            }
            _ => {
                println!("Invalid choice. Please enter 1, 2, or 3.");
            }
        }
    }
}

fn is_managed_checkout_name(name: &str) -> bool {
    name.len() == 8 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn stale_checkout_candidates(git_credentials: &GitCredentials) -> io::Result<Vec<PathBuf>> {
    let expected_paths: HashSet<PathBuf> = git_credentials
        .auth_items
        .iter()
        .map(|auth| PathBuf::from(generate_git_project_path(auth).to_string()))
        .collect();

    let entries = match fs::read_dir(AIS_REPO_ROOT) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };

    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };

        if is_managed_checkout_name(&name) && !expected_paths.contains(&path) {
            candidates.push(path);
        }
    }

    candidates.sort();
    Ok(candidates)
}

fn remove_stale_checkout(path: &Path) -> io::Result<()> {
    let root = Path::new(AIS_REPO_ROOT);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if path.parent() != Some(root) || !is_managed_checkout_name(name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to remove unmanaged path '{}'", path.display()),
        ));
    }

    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

enum CheckoutAudit {
    Ready,
    Missing,
    Invalid(String),
}

fn git_command(git_project_path: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .env("GIT_CONFIG_GLOBAL", config::app_git_config_path())
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("-c")
        .arg(format!("safe.directory={}", git_project_path.display()))
        .arg("-C")
        .arg(git_project_path);
    command
}

// Same as `git_command`, but also scopes the GitHub auth header to the
// invocation via `-c http.extraheader=...` so commands that touch the
// remote (fetch) don't fall back to an interactive credential prompt.
fn authenticated_git_command(git_project_path: &Path) -> Result<Command, String> {
    let header = auth::github_auth_header()
        .ok_or_else(|| "GitHub token not initialized; run option 6 after credentials are loaded".to_string())?;
    let mut command = git_command(git_project_path);
    command.arg("-c").arg(format!("http.extraheader={}", header));
    Ok(command)
}

fn inspect_checkout(git_project_path: &Path) -> CheckoutAudit {
    let metadata = match fs::symlink_metadata(git_project_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return CheckoutAudit::Missing,
        Err(err) => return CheckoutAudit::Invalid(err.to_string()),
    };

    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return CheckoutAudit::Invalid("checkout path is not a directory".to_string());
    }

    let output = match git_command(git_project_path)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
    {
        Ok(output) => output,
        Err(err) => return CheckoutAudit::Invalid(err.to_string()),
    };

    if !output.status.success() {
        return CheckoutAudit::Invalid(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }

    let reported_root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let expected_root =
        fs::canonicalize(git_project_path).unwrap_or_else(|_| git_project_path.to_path_buf());
    let reported_root = fs::canonicalize(&reported_root).unwrap_or(reported_root);
    if reported_root != expected_root {
        return CheckoutAudit::Invalid(format!(
            "Git root '{}' does not match checkout path",
            reported_root.display()
        ));
    }

    CheckoutAudit::Ready
}

fn run_command_step(mut command: Command, step: &str) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|err| format!("{} failed to start: {}", step, err))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("{} exited with {}", step, status))
    }
}

fn run_git_step(git_project_path: &Path, args: &[&str], step: &str) -> Result<(), String> {
    let mut command = git_command(git_project_path);
    command.args(args);
    run_command_step(command, step)
}

fn force_sync_checkout(auth: &GitAuth, git_project_path: &Path) -> Result<(), String> {
    let branch = auth.branch.to_string();
    let remote_branch = format!("origin/{}", branch);

    let mut fetch = authenticated_git_command(git_project_path)?;
    fetch.arg("fetch").arg("origin");
    run_command_step(fetch, "git fetch")?;
    run_git_step(
        git_project_path,
        &["checkout", "-B", &branch, &remote_branch],
        "git checkout",
    )?;
    run_git_step(
        git_project_path,
        &["reset", "--hard", &remote_branch],
        "git reset",
    )?;
    run_git_step(git_project_path, &["clean", "-ffd"], "git clean")?;
    Ok(())
}

// Best-effort: chown drift shouldn't mask a successful git sync (or block an
// audit-only pass), but it should always be logged so an operator notices.
fn reassert_checkout_ownership(repo_id: &str, git_project_path: &Path) {
    if let Err(err) =
        git::enforce_checkout_ownership(&PathType::from(git_project_path.to_path_buf()))
    {
        log!(
            LogLevel::Warn,
            "{}: failed to re-assert www-data ownership on '{}': {}",
            repo_id,
            git_project_path.display(),
            err
        );
    }
}

async fn audit_configured_checkouts(git_credentials: &GitCredentials, force_sync: bool) -> bool {
    let mut ready = 0_usize;
    let mut cloned = 0_usize;
    let mut missing = 0_usize;
    let mut invalid = 0_usize;
    let mut sync_failed = 0_usize;

    for auth in &git_credentials.auth_items {
        let generated_path = generate_git_project_path(auth);
        let git_project_path = PathBuf::from(generated_path.to_string());
        let repo_id = auth.generate_id();

        match inspect_checkout(&git_project_path) {
            CheckoutAudit::Ready if force_sync => {
                log!(
                    LogLevel::Info,
                    "{}: checkout ready; force syncing {} to origin/{}",
                    repo_id,
                    git_project_path.display(),
                    auth.branch
                );
                match force_sync_checkout(auth, &git_project_path) {
                    Ok(()) => {
                        ready += 1;
                        reassert_checkout_ownership(&repo_id, &git_project_path);
                        log!(LogLevel::Info, "{}: force sync complete", repo_id);
                    }
                    Err(err) => {
                        sync_failed += 1;
                        log!(LogLevel::Error, "{}: force sync failed: {}", repo_id, err);
                    }
                }
            }
            CheckoutAudit::Ready => {
                ready += 1;
                reassert_checkout_ownership(&repo_id, &git_project_path);
                log!(
                    LogLevel::Info,
                    "{}: checkout ready at {}",
                    repo_id,
                    git_project_path.display()
                );
            }
            CheckoutAudit::Missing => {
                log!(
                    LogLevel::Warn,
                    "{}: checkout missing at {}; cloning from git.cf",
                    repo_id,
                    git_project_path.display()
                );
                match git::handle_new_repo(auth, &generated_path).await {
                    Ok(_) => {
                        cloned += 1;
                        log!(
                            LogLevel::Info,
                            "{}: clone complete at {}",
                            repo_id,
                            git_project_path.display()
                        );
                    }
                    Err(err) => {
                        missing += 1;
                        log!(LogLevel::Error, "{}: clone failed: {}", repo_id, err);
                    }
                }
            }
            CheckoutAudit::Invalid(reason) => {
                invalid += 1;
                log!(
                    LogLevel::Error,
                    "{}: checkout invalid at {}: {}",
                    repo_id,
                    git_project_path.display(),
                    reason
                );
            }
        }
    }

    log!(
        LogLevel::Info,
        "Checkout audit complete: {} ready, {} cloned, {} missing after clone attempt, {} invalid, {} sync failures",
        ready,
        cloned,
        missing,
        invalid,
        sync_failed
    );
    missing == 0 && invalid == 0 && sync_failed == 0
}

#[tokio::main]
async fn main() {
    // load the data
    let config = get_config();
    let (mut git_credentials, credentials_loaded) = match get_git_credentials(&config).await {
        Ok(data) => (data, true),
        Err(err) => {
            log!(LogLevel::Error, "{}", err);
            log!(
                LogLevel::Warn,
                "Couldn't load existing credentials bootstrapping"
            );
            (
                GitCredentials::bootstrap_git_credentials().await.unwrap(),
                false,
            )
        }
    };

    if config.debug_mode {
        log!(LogLevel::Info, "{}", config)
    }

    println!("1. View stored git credentials");
    println!("2. Create new git credential file");
    println!("3. Append data to git credential file");
    println!("4. Remove data from git credential file");
    println!("5. Clean all stale repository checkouts");
    println!("6. Audit and clone configured repository checkouts");

    loop {
        let choice: String = get_user_input("Enter number of desired action: ").to_string();

        match choice.as_str() {
            "1" => {
                for git in git_credentials.to_vec() {
                    let id = git.generate_id();
                    log!(LogLevel::Info, "{}\nId: {}", git, id);
                }
                log!(LogLevel::Info, "Done");
                std::process::exit(0)
            }
            "2" => {
                log!(LogLevel::Info, "Creating new git credential file");
                let mut git_creds = GitCredentials::bootstrap_git_credentials().await.unwrap();

                let num_instances: usize =
                    get_user_input("Enter the number of GitAuth instances to create: ")
                        .parse()
                        .expect("Invalid input");

                for i in 0..num_instances {
                    println!("Enter details for GitAuth instance {}", i + 1);

                    let user: Stringy = get_user_input("User");
                    let repo: Stringy = get_user_input("Repo");
                    let branch: Stringy = get_user_input("Branch");
                    let server: GitServer = prompt_server_choice().await; // Prompt for the server

                    let auth = GitAuth {
                        user,
                        repo,
                        branch,
                        token: None,
                        server,
                    };

                    git_creds.add_auth(auth);
                }

                let git_path = git_credentials_path(&config);

                match git_creds.save(&git_path).await {
                    Ok(_) => log!(LogLevel::Info, "Git credentials saved @: {}", git_path),
                    Err(err) => log!(LogLevel::Error, "{}", err),
                }

                std::process::exit(0)
            }
            "3" => {
                log!(LogLevel::Info, "Appending to git credential file");

                let num_instances: usize =
                    get_user_input("Enter the number of GitAuth instances to add: ")
                        .parse()
                        .expect("Invalid input");

                for i in 0..num_instances {
                    println!("Enter details for GitAuth instance {}", i + 1);

                    let user: Stringy = get_user_input("User");
                    let repo: Stringy = get_user_input("Repo");
                    let branch: Stringy = get_user_input("Branch");
                    let server: GitServer = prompt_server_choice().await; // Prompt for the server

                    let auth = GitAuth {
                        user,
                        repo,
                        branch,
                        token: None,
                        server,
                    };

                    git_credentials.add_auth(auth);
                }

                let git_path = git_credentials_path(&config);

                match git_credentials.save(&git_path).await {
                    Ok(_) => log!(LogLevel::Info, "Git credentials saved @: {}", git_path),
                    Err(err) => log!(LogLevel::Error, "{}", err),
                }

                std::process::exit(0)
            }
            "4" => {
                log!(LogLevel::Info, "Deleting entries from git credentials");

                if git_credentials.auth_items.is_empty() {
                    log!(LogLevel::Info, "No git credentials are configured");
                    std::process::exit(0)
                }

                let mut options: Vec<String> = vec![];

                for item in git_credentials.clone().to_vec() {
                    let entry = format!("{}-{}@{}", item.user, item.repo, item.branch);
                    options.push(entry);
                }

                let num = get_user_selection(&options) - 1;
                let selected_auth = git_credentials.auth_items[num].clone();
                let checkout_id = selected_auth.generate_id();
                if !get_yes_no(&format!(
                    "Remove {}-{}@{} ({}) from git.cf",
                    selected_auth.user, selected_auth.repo, selected_auth.branch, checkout_id
                )) {
                    log!(LogLevel::Info, "Credential removal cancelled");
                    std::process::exit(0)
                }

                let new_credentials = git_credentials.delete_item(num).await.unwrap();

                let git_path = git_credentials_path(&config);

                if let Err(err) = new_credentials.save(&git_path).await {
                    log!(LogLevel::Error, "{}", err);
                    std::process::exit(1)
                }
                log!(LogLevel::Info, "Git credentials saved @: {}", git_path);

                let checkout_path =
                    PathBuf::from(generate_git_project_path(&selected_auth).to_string());
                match remove_stale_checkout(&checkout_path) {
                    Ok(()) => log!(
                        LogLevel::Info,
                        "Removed checkout '{}' after git.cf entry removal",
                        checkout_path.display()
                    ),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => log!(
                        LogLevel::Info,
                        "Checkout '{}' was already absent",
                        checkout_path.display()
                    ),
                    Err(err) => {
                        log!(
                            LogLevel::Error,
                            "Credential was removed, but checkout cleanup failed for '{}': {}",
                            checkout_path.display(),
                            err
                        );
                        std::process::exit(1)
                    }
                }
                log!(
                    LogLevel::Warn,
                    "Restart GitMonitor if it is running so the removed repository worker is retired"
                );

                std::process::exit(0)
            }
            "5" => {
                if !credentials_loaded {
                    log!(
                        LogLevel::Error,
                        "Cleanup refused because the configured git credential file was not loaded"
                    );
                    std::process::exit(1)
                }

                let candidates = match stale_checkout_candidates(&git_credentials) {
                    Ok(candidates) => candidates,
                    Err(err) => {
                        log!(
                            LogLevel::Error,
                            "Failed to inspect {}: {}",
                            AIS_REPO_ROOT,
                            err
                        );
                        std::process::exit(1)
                    }
                };

                if candidates.is_empty() {
                    log!(
                        LogLevel::Info,
                        "No stale GitMonitor checkout paths found in {}",
                        AIS_REPO_ROOT
                    );
                    std::process::exit(0)
                }

                let mut removed = 0_usize;
                let mut failed = 0_usize;
                for candidate in candidates {
                    match remove_stale_checkout(&candidate) {
                        Ok(()) => {
                            removed += 1;
                            log!(
                                LogLevel::Info,
                                "Removed stale checkout '{}'",
                                candidate.display()
                            );
                        }
                        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                        Err(err) => {
                            failed += 1;
                            log!(
                                LogLevel::Error,
                                "Failed to remove '{}': {}",
                                candidate.display(),
                                err
                            );
                        }
                    }
                }

                log!(
                    LogLevel::Info,
                    "Stale checkout cleanup complete: {} removed, {} failed",
                    removed,
                    failed
                );
                if failed > 0 {
                    std::process::exit(1)
                }
                std::process::exit(0)
            }
            "6" => {
                if !credentials_loaded {
                    log!(
                        LogLevel::Error,
                        "Audit refused because the configured git credential file was not loaded"
                    );
                    std::process::exit(1)
                }

                if let Err(err) = auth::init_gh_token(config::get_git_token_file().as_deref()) {
                    log!(
                        LogLevel::Error,
                        "Failed to load GitHub token; missing repository clones may fail: {}",
                        err
                    );
                }

                let force_sync =
                    get_yes_no("Force sync valid checkouts to their configured origin branches");
                if audit_configured_checkouts(&git_credentials, force_sync).await {
                    std::process::exit(0)
                } else {
                    log!(
                        LogLevel::Warn,
                        "Unresolved repository checkouts will be retried by the monitor on its next reconciliation cycle"
                    );
                    std::process::exit(2)
                }
            }
            "or" => {
                set_log_level(LogLevel::Debug);
                log!(
                    LogLevel::Debug,
                    "No \" or \" isn't actually an option dumbass"
                );
                set_log_level(config.log_level);
            }
            _ => {
                println!("Invalid choice. Please enter 1, 2, 3, 4, 5 or 6.");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{inspect_checkout, is_managed_checkout_name, remove_stale_checkout, CheckoutAudit};
    use std::{io, path::Path};

    #[test]
    fn recognizes_only_gitmonitor_checkout_ids() {
        assert!(is_managed_checkout_name("a1b2c3d4"));
        assert!(is_managed_checkout_name("ABCDEF12"));
        assert!(!is_managed_checkout_name("a1b2c3d"));
        assert!(!is_managed_checkout_name("a1b2c3d45"));
        assert!(!is_managed_checkout_name("repository"));
        assert!(!is_managed_checkout_name("a1b2c3g4"));
    }

    #[test]
    fn reports_an_absent_checkout_as_missing() {
        let path = Path::new("/tmp/ais_gitmon_checkout_that_does_not_exist");
        assert!(matches!(inspect_checkout(path), CheckoutAudit::Missing));
    }

    #[test]
    fn cleanup_refuses_paths_outside_the_managed_root() {
        let err = remove_stale_checkout(Path::new("/tmp/a1b2c3d4")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
