use artisan_middleware::{
    git_actions::{GitAction, GitAuth, GitServer}, log, logger::LogLevel, users::{get_id, set_file_ownership}
};
use dusa_collection_utils::{
    errors::{ErrorArrayItem, Errors},
    types::{ClonePath, PathType},
};

use crate::pull::pull_updates;

// Handle an existing repo: fetch, pull if upstream is ahead, set tracking, restart if needed
pub async fn handle_existing_repo(
    auth: &GitAuth,
    git_project_path: &PathType,
) -> Result<(), ErrorArrayItem> {
    log!(LogLevel::Trace, "Working on existing git repo {}", auth.generate_id());

    if is_upstream_ahead(auth, git_project_path).await? {
        let new_data_downloaded = match pull_updates(auth, git_project_path).await {
            Ok(d) => d,
            Err(ea) => {
                ea.display(false);
                return Err(ErrorArrayItem::new(Errors::Git, format!("Errors occurred while updating, {}", auth.generate_id())))
            },
        };

        if new_data_downloaded {
            // finalize_git_actions(auth, git_project_path).await?;
            log!(LogLevel::Info, "{} has been updated", auth.generate_id());
        } else {
            log!(LogLevel::Trace, "No new data pulled for. {}", auth.generate_id());
        }
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
pub async fn set_safe_directory(git_project_path: &PathType) -> Result<(), ErrorArrayItem> {
    log!(LogLevel::Trace, "Setting safe dir for {}", git_project_path.to_string());
    let set_safe = GitAction::SetSafe {
        directory: git_project_path.clone(),
    };
    set_safe.execute().await?;

    Ok(())
}

// Fetch updates from the remote repository
pub async fn fetch_updates(git_project_path: &PathType) -> Result<(), ErrorArrayItem> {
    log!(LogLevel::Trace, "Fetching updates for, {}", git_project_path.to_string());
    let fetch_update = GitAction::Fetch {
        destination: git_project_path.clone(),
    };
    fetch_update.execute().await?;

    Ok(())
}

// Check if the upstream branch is ahead of the local branch
async fn is_upstream_ahead(
    auth: &GitAuth,
    git_project_path: &PathType,
) -> Result<bool, ErrorArrayItem> {
    // Assemble the remote URL
    let remote_url = auth.assemble_remote_url();

    // The base for comparison should be the remote branch (e.g., "origin/main")
    let base_branch = format!("{}/{}", remote_url, auth.branch);

    // Create the GitAction::RevList to compare the local and remote branches
    let rev_list = GitAction::RevList {
        base: base_branch,         // The remote branch
        target: auth.branch.to_string(), // The local branch
        destination: git_project_path.clone_path(),
    };

    // Execute the RevList action to determine if the remote branch is ahead
    match rev_list.execute().await {
        Ok(Some(output)) => {
            let stdout_str = String::from_utf8_lossy(&output.stdout);
            let ahead_count: usize = stdout_str.trim().parse().unwrap_or(0);
            Ok(ahead_count > 0) // If count > 0, upstream is ahead
        }
        _ => Ok(false),
    }
}