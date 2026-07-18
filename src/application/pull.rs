use crate::auth::github_auth_header;
use crate::config::app_git_config_path;
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
    cmd.env("GIT_CONFIG_GLOBAL", app_git_config_path());
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

#[cfg(test)]
mod integration_tests {
    use super::clone_repo;
    use crate::auth::github_auth_header;
    use crate::test_support::{AuthHeaderProbeServer, Sandbox};
    use serial_test::serial;

    #[tokio::test]
    #[serial]
    async fn clone_repo_sends_the_configured_auth_header() {
        let sandbox = Sandbox::new();
        let bare = sandbox
            .seed_origin("acme", "widgets", "main", &[("a.txt", "a")])
            .await;
        let probe = AuthHeaderProbeServer::start(bare.parent().unwrap());
        let clone_url = format!("{}/widgets.git", probe.base_url());
        let dest = sandbox.checkout_path("widgets-via-probe");

        clone_repo(&clone_url, &dest)
            .await
            .expect("clone should succeed");

        let expected_header = github_auth_header().expect("token should be initialized");
        let expected_value = expected_header
            .strip_prefix("Authorization: ")
            .expect("header should be an Authorization line");
        assert!(
            probe
                .received_auth_headers()
                .iter()
                .any(|h| h.as_deref() == Some(expected_value)),
            "expected an Authorization header matching {:?}, got {:?}",
            expected_value,
            probe.received_auth_headers()
        );
    }

    #[tokio::test]
    #[serial]
    async fn clone_repo_refuses_to_overwrite_an_existing_destination() {
        let sandbox = Sandbox::new();
        sandbox
            .seed_origin("acme", "widgets", "main", &[("a.txt", "a")])
            .await;
        let dest = sandbox.checkout_path("widgets-exists");
        std::fs::create_dir_all(std::path::Path::new(&dest.to_string()))
            .expect("pre-create destination");

        let result = clone_repo("file:///does/not/matter", &dest).await;
        assert!(
            result.is_err(),
            "clone_repo should refuse an existing destination"
        );
    }
}
