use std::io::{self, Write};

use artisan_middleware::git_actions::{GitAuth, GitCredentials, GitServer};
use dusa_collection_utils::{stringy::Stringy, types::PathType};
use simple_pretty::{halt, pass};

fn prompt_input(prompt: &str) -> Stringy {
    print!("{}", prompt);
    io::stdout().flush().unwrap();

    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    input.trim().into()
}

fn prompt_server_choice() -> GitServer {
    println!("Select the Git server:");
    println!("1. GitHub");
    println!("2. GitLab");
    println!("3. Custom");

    loop {
        let choice: String = prompt_input("Enter your choice (1/2/3): ").to_string();

        match choice.as_str() {
            "1" => return GitServer::GitHub,
            "2" => return GitServer::GitLab,
            "3" => {
                let custom_url: Stringy = prompt_input("Enter the custom server URL: ");
                return GitServer::Custom(custom_url.to_string());
            }
            _ => {
                println!("Invalid choice. Please enter 1, 2, or 3.");
            }
        }
    }
}

fn main() {
    let mut git_creds = GitCredentials::bootstrap_git_credentials().unwrap();

    let num_instances: usize = prompt_input("Enter the number of GitAuth instances to create: ")
        .parse()
        .expect("Invalid input");

    for i in 0..num_instances {
        println!("Enter details for GitAuth instance {}", i + 1);

        let user: Stringy = prompt_input("User: ");
        let repo: Stringy = prompt_input("Repo: ");
        let branch: Stringy = prompt_input("Branch: ");
        let server: GitServer = prompt_server_choice(); // Prompt for the server

        let auth = GitAuth {
            user,
            repo,
            branch,
            token: None,
            server,
        };

        git_creds.add_auth(auth);
    }

    match git_creds.save(&PathType::Str("./Credentials.cf".into())) {
        Ok(_) => pass("New multiplexed file created"),
        Err(e) => halt(&format!(
            "Error while creating manifest: {}",
            &e.to_string()
        )),
    }
}
