use std::pin::Pin;

use artisan_middleware::{git_actions::{GitAction, GitAuth, GitServer}, users::{get_id, set_file_ownership}};
use dusa_collection_utils::{
    errors::{ErrorArrayItem, Errors},
    functions::{create_hash, truncate},
    types::{ClonePath, PathType},
};

// Handle an existing repo: fetch, pull, set tracking, restart if needed
pub async fn handle_existing_repo(
    auth: &GitAuth,
    git_project_path: &PathType,
) -> Result<(), ErrorArrayItem> {
    set_safe_directory(git_project_path).await?;
    fetch_updates(git_project_path).await?;

    let new_data_downloaded = pull_updates(auth, git_project_path).await?;

    if new_data_downloaded {
        // finalize_git_actions(auth, git_project_path).await?;
    } else {
        simple_pretty::notice(&format!(
            "No new data pulled for {}.",
            truncate(
                &create_hash(format!("{}-{}-{}", auth.branch, auth.repo, auth.user)),
                8
            )
        ));
    }

    Ok(())
}

pub async fn handle_new_repo(
    auth: &GitAuth,
    server: &GitServer,
    git_project_path: &PathType,
) -> Result<(), ErrorArrayItem> {
    // Clone the repository
    let git_clone = GitAction::Clone {
        repo_name: auth.clone().repo,
        repo_owner: auth.clone().user,
        destination: git_project_path.clone_path(),
        repo_branch: auth.clone().branch,
        server: server.clone(),
    };
    git_clone.execute().await?;

    // Set ownership to the web user
    let webuser = get_id("www-data")?;
    set_file_ownership(&git_project_path, webuser.0, webuser.1)?;

    // Set safe directory
    set_safe_directory(git_project_path).await?;

    // Force switch to the correct branch after cloning
    fetch_updates(git_project_path).await?;

    Ok(())
}

// Set the git project as a safe directory
async fn set_safe_directory(git_project_path: &PathType) -> Result<(), ErrorArrayItem> {
    let set_safe = GitAction::SetSafe {
        directory: git_project_path.clone(),
    };
    set_safe.execute().await?;

    Ok(())
}

// Fetch updates from the remote repository
async fn fetch_updates(git_project_path: &PathType) -> Result<(), ErrorArrayItem> {
    let fetch_update = GitAction::Fetch {
        destination: git_project_path.clone(),
    };
    fetch_update.execute().await?;

    Ok(())
}

// Pull updates and return whether new data was pulled
async fn pull_updates(auth: &GitAuth, git_project_path: &PathType) -> Result<bool, ErrorArrayItem> {
    let pull_update = GitAction::Pull {
        target_branch: auth.branch.clone(),
        destination: git_project_path.clone_path(),
    };

    match pull_update.execute().await {
        Ok(output) => {
            if let Some(data) = output {
                let stdout_str = String::from_utf8_lossy(&data.stdout);

                if stdout_str.contains("Already up to date.") {
                    Ok(false) // No new data was pulled
                } else {
                    Ok(true) // New data was pulled
                }
            } else {
                Ok(false)
            }
        }
        Err(e) => {
            if e.err_type == Errors::GeneralError {
                simple_pretty::warn("non-critical errors occurred");
                Ok(true) // Assume new data was pulled in case of non-critical error
            } else if e.to_string().contains("safe directory") {
                // Handle "safe directory" error by boxing recursive calls
                set_safe_directory(git_project_path).await?;
                fetch_updates(git_project_path).await?;

                // Recursively call pull_updates inside a Box to avoid infinite future size
                Pin::from(Box::new(pull_updates(auth, git_project_path))).await
            } else {
                Err(e)
            }
        }
    }
}