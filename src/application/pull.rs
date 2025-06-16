use dusa_collection_utils::logger::LogLevel;
use dusa_collection_utils::types::pathtype::PathType;
use dusa_collection_utils::types::stringy::Stringy;
use dusa_collection_utils::{errors::ErrorArrayItem, log};
use git2::build::RepoBuilder;
use git2::{BranchType, Cred, FetchOptions, RemoteCallbacks, Repository};
use std::process::Command;

use crate::auth::get_gh_token;

/// Pulls the latest changes using `git pull`.
pub fn pull_latest_changes(repo_path: &str, branch_name: Stringy) -> std::io::Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("pull")
        .arg("origin")
        .arg(branch_name)
        .arg("--rebase")
        .output()?;

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
pub fn clone_repo(repo_url: &str, dest_path: &PathType) -> Result<(), git2::Error> {
    if !dest_path.exists() {
        log!(LogLevel::Info, "Cloning repository into {}", dest_path);

        let token: String = match get_gh_token() {
            Ok(token) => token,
            Err(err) => {
                let error = ErrorArrayItem::from(err);
                log!(
                    LogLevel::Error,
                    "Error using gh to get token: {}",
                    error.err_mesg
                );
                return Ok(());
            }
        };

        log!(LogLevel::Debug, "Token: {}", token);
        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(move |_url, username_from_url, _allowed_types| {
            Cred::userpass_plaintext(username_from_url.unwrap_or("oauth2"), &token)
        });

        let mut fetch_options = FetchOptions::new();
        fetch_options.remote_callbacks(callbacks);

        let mut builder = RepoBuilder::new();
        builder.fetch_options(fetch_options);
        builder.clone(repo_url, &dest_path)?;
    }
    Ok(())
}

pub fn branch_exists(repo: &Repository, branch_name: Stringy) -> bool {
    repo.find_branch(&branch_name, BranchType::Local).is_ok()
}

/// Creates a local tracking branch if it does not exist.
fn create_tracking_branch(repo: &Repository, branch_name: &str) -> Result<(), git2::Error> {
    let remote_branch_ref = format!("refs/remotes/origin/{}", branch_name);
    let remote_branch = repo.refname_to_id(&remote_branch_ref)?;

    let commit = repo.find_commit(remote_branch)?;
    repo.branch(branch_name, &commit, false)?;

    Ok(())
}

/// Switches to the specified branch (creates it if necessary).
pub fn checkout_branch(repo: &Repository, branch_name: Stringy) -> Result<(), git2::Error> {
    if !branch_exists(repo, branch_name.clone()) {
        log!(
            LogLevel::Debug,
            "Branch '{}' does not exist locally. Creating a tracking branch...",
            branch_name
        );
        create_tracking_branch(repo, &branch_name)?;
    }

    let branch_ref = format!("refs/heads/{}", branch_name);
    let obj = repo.revparse_single(&branch_ref)?;

    let mut checkout_opts = git2::build::CheckoutBuilder::new();
    repo.checkout_tree(&obj, Some(&mut checkout_opts))?;

    repo.set_head(&branch_ref)?;

    log!(LogLevel::Debug, "Switched to branch '{}'", branch_name);

    Ok(())
}
