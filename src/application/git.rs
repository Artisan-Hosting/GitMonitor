use artisan_middleware::{
    git_actions::{GitAction, GitAuth},
    users::{get_id, set_file_ownership},
};
use dusa_collection_utils::logger::LogLevel;
use dusa_collection_utils::{
    errors::{ErrorArrayItem, Errors},
    types::pathtype::PathType,
};
use dusa_collection_utils::{functions::truncate, log};
use git2::{Cred, FetchOptions, RemoteCallbacks, Repository};

use crate::{
    auth::get_gh_token,
    pull::{checkout_branch, clone_repo, pull_latest_changes},
};

// Handle an existing repo: fetch, pull if upstream is ahead, set tracking, restart if needed
pub async fn handle_existing_repo(
    auth: &GitAuth,
    repo: Repository,
    git_project_path: &PathType,
) -> Result<(), ErrorArrayItem> {
    log!(
        LogLevel::Trace,
        "Working on existing git repo {}",
        auth.generate_id()
    );

    // set_safe_directory(git_project_path).await?;
    fetch_updates(&repo).await?;

    // Check if upstream is ahead
    let remote_ahead: bool = match is_remote_ahead(auth, &repo).await {
        Ok(b) => Ok(b),
        Err(err) => Err(ErrorArrayItem::new(Errors::Git, err.message())),
    }?;

    if remote_ahead {
        pull_latest_changes(git_project_path.to_str().unwrap(), auth.branch.clone())
            .map_err(ErrorArrayItem::from)?;

        checkout_branch(&repo, auth.branch.clone())
            .map_err(|err| ErrorArrayItem::new(Errors::Git, err.message()))?;

        log!(
            LogLevel::Info,
            "{} Updated, runner should rebuild this shortly.",
            auth.generate_id()
        );
    } else {
        log!(LogLevel::Info, "{}: Up to date !", auth.generate_id());
    }

    Ok(())
}

pub async fn handle_new_repo(
    auth: &GitAuth,
    git_project_path: &PathType,
) -> Result<(), ErrorArrayItem> {
    // Clone the repository
    let repo_url = auth.assemble_remote_url();
    clone_repo(&repo_url, git_project_path)
        .map_err(|err| ErrorArrayItem::new(Errors::Git, err.message()))?;

    // Set ownership to the web user
    let webuser = get_id("www-data")?;
    set_file_ownership(&git_project_path, webuser.0, webuser.1)?;

    // Set safe directory
    set_safe_directory(git_project_path).await?;

    let repo = Repository::open(git_project_path)
        .map_err(|err| ErrorArrayItem::new(Errors::Git, err.message()))?;

    checkout_branch(&repo, auth.branch.clone())
        .map_err(|err| ErrorArrayItem::new(Errors::Git, err.message()))?;

    Ok(())
}

// Set the git project as a safe directory
pub async fn set_safe_directory(git_project_path: &PathType) -> Result<(), ErrorArrayItem> {
    log!(
        LogLevel::Trace,
        "Setting safe dir for {}",
        git_project_path.to_string()
    );
    let set_safe = GitAction::SetSafe {
        directory: git_project_path.clone(),
    };
    set_safe.execute().await?;

    Ok(())
}

// Fetch updates from the remote repository
pub async fn fetch_updates(repo: &Repository) -> Result<(), ErrorArrayItem> {
    log!(
        LogLevel::Debug,
        "Fetching updates for, {}",
        PathType::Path(repo.path().into())
    );

    let token: String = match get_gh_token() {
        Ok(token) => token,
        Err(err) => {
            let mut error = ErrorArrayItem::from(err);
            error.err_mesg = format!("Error using gh to get token: {}", error.err_mesg).into();
            return Err(error);
        }
    };

    log!(LogLevel::Debug, "Token: {}", token);

    // Authentication callback
    let mut auth_cb = RemoteCallbacks::new();
    auth_cb.credentials(move |_url, username_from_url, _allowed_types| {
        Cred::userpass_plaintext(
            username_from_url.unwrap_or("oauth2"), // GitHub accepts "x-access-token" or "oauth2" as user
            &token,
        )
    });

    // TODO allow changing the remote from origin

    match repo.find_remote("origin") {
        Ok(mut remote) => {
            let mut fetch_options = FetchOptions::new();
            fetch_options.remote_callbacks(auth_cb);

            if let Err(err) = remote.fetch(
                &["+refs/heads/*:refs/remotes/origin/*"],
                Some(&mut fetch_options),
                None,
            ) {
                Err(ErrorArrayItem::new(Errors::Git, err.message()))
            } else {
                Ok(())
            }
        }
        Err(err) => Err(ErrorArrayItem::new(Errors::Git, err.message())),
    }
}

// Check if the upstream branch is ahead of the local branch
async fn is_remote_ahead(auth: &GitAuth, repo: &Repository) -> Result<bool, git2::Error> {
    let head = repo.head()?.peel_to_commit()?;
    let remote_ref = repo.refname_to_id(&format!("refs/remotes/origin/{}", auth.branch))?;
    let remote_commit = repo.find_commit(remote_ref)?;

    log!(
        LogLevel::Debug,
        "Latest commit on remote: {}",
        truncate(format!("{}", remote_commit.id()), 8)
    );
    log!(
        LogLevel::Debug,
        "Latest local commit: {}",
        truncate(format!("{}", head.id()), 8)
    );

    Ok(head.id() != remote_commit.id())
}
