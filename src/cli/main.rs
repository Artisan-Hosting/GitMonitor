use artisan_middleware::{cli::{get_user_input, get_user_selection}, config::AppConfig, git_actions::{GitAuth, GitCredentials, GitServer}};
use dusa_collection_utils::{errors::{ErrorArrayItem, Errors}, log::LogLevel, stringy::Stringy, types::PathType};
use dusa_collection_utils::log;

fn get_config() -> AppConfig {
    let mut config: AppConfig = match AppConfig::new() {
        Ok(loaded_data) => loaded_data,
        Err(e) => {
            log!(LogLevel::Error, "Couldn't load config: {}", e.to_string());
            std::process::exit(0)
        }
    };
    config.app_name = Stringy::from(env!("CARGO_PKG_NAME"));
    config.version = env!("CARGO_PKG_VERSION").to_string();
    config.database = None;
    config.aggregator = None;
    config
}

async fn get_git_credentials(config: &AppConfig) -> Result<GitCredentials, ErrorArrayItem> {
    match &config.git {
        Some(git_config) => {
            let git_file: PathType = PathType::Str(git_config.credentials_file.clone().into());
            GitCredentials::new(Some(&git_file)).await
        }
        None => Err(ErrorArrayItem::new(
            Errors::ReadingFile,
            "Git configuration not found".to_string(),
        )),
    }
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

#[tokio::main]
async fn main() {
    // load the data 
    let config = get_config();
    let mut git_credentials = match get_git_credentials(&config).await {
        Ok(data) => data,
        Err(err) => {
            log!(LogLevel::Error, "{}", err);
            log!(LogLevel::Warn, "Couldn't load existing credentials bootstrapping");
            GitCredentials::bootstrap_git_credentials().await.unwrap()
        },
    };

    if config.debug_mode {
        log!(LogLevel::Info, "{}", config)
    }


    println!("1. View stored git credentials");
    println!("2. Create new git credential file");
    println!("3. Append data to git credential file");
    println!("4. Remove data from git credential file");

    loop {
        let choice: String = get_user_input("Enter number of desired action: ").to_string();

        match choice.as_str() {
            "1" => {
                for git in git_credentials.to_vec() {
                    log!(LogLevel::Info, "{}", git);
                }
                log!(LogLevel::Info, "Done");
                std::process::exit(0)
            },
            "2" => {
                log!(LogLevel::Info, "Creating new git credential file");
                let mut git_creds = GitCredentials::bootstrap_git_credentials().await.unwrap();

                let num_instances: usize = get_user_input("Enter the number of GitAuth instances to create: ")
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

                let git_path = match config.git {
                    Some(data) => data.credentials_file,
                    None => "/tmp/git_credenaitls".to_owned(),
                };

                match git_creds.save(&PathType::Content(git_path.clone())).await {
                    Ok(_) => log!(LogLevel::Info, "Git credentials saved @: {}", git_path),
                    Err(err) => log!(LogLevel::Error, "{}", err),
                }

                std::process::exit(0)
            },
            "3" => {
                log!(LogLevel::Info, "Appending to git credential file");

                let num_instances: usize = get_user_input("Enter the number of GitAuth instances to add: ")
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

                let git_path = match config.git {
                    Some(data) => data.credentials_file,
                    None => "/tmp/git_credenaitls".to_owned(),
                };

                match git_credentials.save(&PathType::Content(git_path.clone())).await {
                    Ok(_) => log!(LogLevel::Info, "Git credentials saved @: {}", git_path),
                    Err(err) => log!(LogLevel::Error, "{}", err),
                }
            
                std::process::exit(0)
  
            },
            "4" => {
                log!(LogLevel::Info, "Deleting entried from git credentials");

                let mut options: Vec<String> = vec![];

                for item in git_credentials.clone().to_vec() {
                    let entry = format!("{}-{}@{}", item.user, item.repo, item.branch);
                    options.push(entry);
                }
                
                let mut num = get_user_selection(&options);
                num -= 1; // to align with the 0 starting index

                let new_credentials = git_credentials.delete_item(num).await.unwrap();

                let git_path = match config.git {
                    Some(data) => PathType::Content(data.credentials_file),
                    None => PathType::Str("/tmp/git_credenaitls".into()),
                };

                match new_credentials.save(&git_path.clone()).await {
                    Ok(_) => log!(LogLevel::Info, "Git credentials saved @: {}", git_path),
                    Err(err) => log!(LogLevel::Error, "{}", err),
                }
            
                std::process::exit(0)

            },
            _ => {
                println!("Invalid choice. Please enter 1, 2, 3 or 4.");
            }
        }
    }
}
