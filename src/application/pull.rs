use artisan_middleware::git_actions::{GitAction, GitAuth};
use dusa_collection_utils::log;
use dusa_collection_utils::log::LogLevel;
use dusa_collection_utils::{
    errors::{ErrorArray, ErrorArrayItem, Errors},
    types::PathType,
};
use std::{process::Output, time::Duration};
use tokio::time::sleep;

use crate::git::{fetch_updates, set_safe_directory};

const MAX_RETRIES: u8 = 3; // Maximum number of retries
const RETRY_DELAY_SECS: u64 = 3; // Delay between retries in seconds

pub async fn pull_updates(auth: &GitAuth, git_project_path: &PathType) -> Result<bool, ErrorArray> {
    log!(LogLevel::Trace, "Starting update for {}", auth.generate_id());
    let error_array = &mut ErrorArray::new_container();
    let mut retries = 0;

    loop {
        let pull_update = GitAction::Pull {
            target_branch: auth.branch.clone(),
            destination: git_project_path.clone(),
        };

        log!(LogLevel::Trace, "Pulling: {}", auth.generate_id());
        match pull_update.execute().await {
            Ok(output) => {
                let hpo = handle_pull_output(output);
                match hpo {
                    Ok(d) => return Ok(d),
                    Err(e) => {
                        error_array.push(e);
                        return Err(error_array.to_owned());
                    }
                }
            }
            Err(e) => {
                error_array.push(e.clone());

                if retries >= MAX_RETRIES {
                    error_array.push(ErrorArrayItem::new(
                        dusa_collection_utils::errors::Errors::Git,
                        format!("Maximum retry attempts reached for: {}", git_project_path),
                    ));
                    return Err(error_array.to_owned());
                }

                if let Some(result) =
                    handle_pull_error(e, error_array, auth, git_project_path).await
                {
                    match result {
                        Ok(b) => return Ok(b),
                        Err(ea) => {
                            return Err(ea);
                        }
                    } // Either a success or non-recoverable error was handled
                }

                retries += 1;
                log!(
                    LogLevel::Error,
                    "Attempt {} failed. Retrying in {} seconds...",
                    retries,
                    RETRY_DELAY_SECS
                );
                sleep(Duration::from_secs(RETRY_DELAY_SECS)).await; // Delay before retrying
            }
        }
    }
}

fn handle_pull_output(output: Option<Output>) -> Result<bool, ErrorArrayItem> {
    if let Some(data) = output {
        let stdout_str = String::from_utf8_lossy(&data.stdout);
        if stdout_str.contains("Already up to date.") {
            log!(LogLevel::Trace, "Already up to date");
            Ok(false) // No new data was pulled
        } else {
            log!(LogLevel::Trace, "Updated");
            Ok(true) // New data was pulled
        }
    } else {
        log!(LogLevel::Trace, "Git cli returned no output");
        Ok(false) // No data was available
    }
}

async fn handle_pull_error(
    e: ErrorArrayItem,
    ea: &mut ErrorArray,
    _auth: &GitAuth,
    git_project_path: &PathType,
) -> Option<Result<bool, ErrorArray>> {
    if e.err_type == Errors::GeneralError {
        log!(LogLevel::Debug, "Non-critical errors occurred");
        return Some(Ok(true)); // Assume new data was pulled in case of non-critical error
    } else if e.to_string().contains("safe directory") {
        // Handle "safe directory" error by setting the safe directory and retrying the pull
        if let Err(e) = set_safe_directory(git_project_path).await {
            ea.push(e);  // Capture any errors that occur while setting the safe directory
        }
        if let Err(e) = fetch_updates(git_project_path).await {
            ea.push(e); // Capture any errors during the fetch
        }
        // Recursively call pull_updates inside a Box to avoid infinite future size
        return None; // Allow the main loop to handle retry after a delay
    }

    Some(Err(ea.to_owned())) // Propagate any other errors
}
