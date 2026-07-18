use artisan_middleware::{
    cli::{get_user_input, get_user_selection},
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
};

const AIS_REPO_ROOT: &str = "/var/www/ais";

#[path = "../git_config.rs"]
mod git_config;

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
    println!("5. Clean stale repository checkout");

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

                let mut options: Vec<String> = vec![];

                for item in git_credentials.clone().to_vec() {
                    let entry = format!("{}-{}@{}", item.user, item.repo, item.branch);
                    options.push(entry);
                }

                let mut num = get_user_selection(&options);
                num -= 1; // to align with the 0 starting index

                let new_credentials = git_credentials.delete_item(num).await.unwrap();

                let git_path = git_credentials_path(&config);

                match new_credentials.save(&git_path.clone()).await {
                    Ok(_) => log!(LogLevel::Info, "Git credentials saved @: {}", git_path),
                    Err(err) => log!(LogLevel::Error, "{}", err),
                }

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

                println!(
                    "GitMonitor does not stop workers when git.cf changes. Restart or stop ais_gitmon before cleanup so an old worker cannot recreate a removed checkout."
                );
                println!("Select one stale checkout to remove:");
                let options: Vec<String> = candidates
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect();
                let selection = get_user_selection(&options);
                if selection == 0 || selection > candidates.len() {
                    log!(LogLevel::Error, "Invalid cleanup selection");
                    std::process::exit(1)
                }

                let selected = &candidates[selection - 1];
                let checkout_id = selected
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                let confirmation: String = get_user_input(&format!(
                    "Type '{}' to remove {}: ",
                    checkout_id,
                    selected.display()
                ))
                .to_string();

                if confirmation.trim() != checkout_id {
                    log!(LogLevel::Info, "Cleanup cancelled");
                    std::process::exit(0)
                }

                match remove_stale_checkout(selected) {
                    Ok(()) => log!(
                        LogLevel::Info,
                        "Removed stale checkout '{}' (manual cleanup is not recoverable)",
                        selected.display()
                    ),
                    Err(err) => {
                        log!(
                            LogLevel::Error,
                            "Failed to remove '{}': {}",
                            selected.display(),
                            err
                        );
                        std::process::exit(1)
                    }
                }

                std::process::exit(0)
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
                println!("Invalid choice. Please enter 1, 2, 3, 4 or 5.");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_managed_checkout_name;

    #[test]
    fn recognizes_only_gitmonitor_checkout_ids() {
        assert!(is_managed_checkout_name("a1b2c3d4"));
        assert!(is_managed_checkout_name("ABCDEF12"));
        assert!(!is_managed_checkout_name("a1b2c3d"));
        assert!(!is_managed_checkout_name("a1b2c3d45"));
        assert!(!is_managed_checkout_name("repository"));
        assert!(!is_managed_checkout_name("a1b2c3g4"));
    }
}
