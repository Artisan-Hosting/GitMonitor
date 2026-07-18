use crate::auth::github_auth_header;
use crate::config::APP_GIT_CONFIG_PATH;
use artisan_middleware::dusa_collection_utils::{
    core::{
        logger::LogLevel,
        types::{pathtype::PathType, stringy::Stringy},
    },
    log,
};
use tokio::process::Command;

fn git_cmd() -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_CONFIG_GLOBAL", APP_GIT_CONFIG_PATH);
    cmd
}

/// Clones the repository if it does not exist.
pub async fn clone_repo(repo_url: &str, dest_path: &PathType) -> std::io::Result<()> {
    if dest_path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("clone destination already exists: {}", dest_path),
        ));
    }

    log!(LogLevel::Info, "Cloning repository into {}", dest_path);

    let auth_header = match github_auth_header() {
        Some(header) => header,
        None => {
            let message =
                "GitHub token not initialized; clone deferred until credentials are available";
            log!(LogLevel::Error, "{}", message);
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, message));
        }
    };

    let output = git_cmd()
        .arg("-c")
        .arg(format!("http.extraheader={}", auth_header))
        .arg("clone")
        .arg(repo_url)
        .arg(dest_path.to_string())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await?;

    if output.status.success() {
        Ok(())
    } else {
        let msg = format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Err(std::io::Error::new(std::io::ErrorKind::Other, msg))
    }
}

/// Switches to the specified branch.
pub async fn checkout_branch(repo_path: &str, branch_name: Stringy) -> std::io::Result<()> {
    let branch = branch_name.to_string();
    let remote_branch = format!("origin/{}", branch);
    let checkout = git_cmd()
        .arg("-C")
        .arg(repo_path)
        .arg("checkout")
        .arg("-B")
        .arg(&branch)
        .arg(&remote_branch)
        .output()
        .await?;

    if !checkout.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "git checkout failed: {}",
                String::from_utf8_lossy(&checkout.stderr)
            ),
        ));
    }

    let reset = git_cmd()
        .arg("-C")
        .arg(repo_path)
        .arg("reset")
        .arg("--hard")
        .arg(&remote_branch)
        .output()
        .await?;

    if !reset.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "git reset failed: {}",
                String::from_utf8_lossy(&reset.stderr)
            ),
        ));
    }

    let clean = git_cmd()
        .arg("-C")
        .arg(repo_path)
        .arg("clean")
        .arg("-ffd")
        .output()
        .await?;

    if !clean.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "git clean failed: {}",
                String::from_utf8_lossy(&clean.stderr)
            ),
        ));
    }

    log!(
        LogLevel::Debug,
        "Reset checkout to configured branch '{}'",
        branch
    );
    Ok(())
}
