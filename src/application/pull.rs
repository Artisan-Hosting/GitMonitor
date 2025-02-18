use dusa_collection_utils::log;
use dusa_collection_utils::logger::LogLevel;
use dusa_collection_utils::types::pathtype::PathType;
use dusa_collection_utils::types::stringy::Stringy;
use git2::{BranchType, Repository};
use std::process::Command;

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
    } else {
        log!(LogLevel::Error, "Failed to pull changes: {:?}", output);
    }

    Ok(())
}

/// Clones the repository if it does not exist.
pub fn clone_repo(repo_url: &str, dest_path: &PathType) -> Result<(), git2::Error> {
    if !dest_path.exists() {
        log!(LogLevel::Info, "Cloning repository into {}", dest_path);
        Repository::clone(repo_url, dest_path)?;
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
