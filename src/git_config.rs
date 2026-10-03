use artisan_middleware::dusa_collection_utils::core::types::pathtype::PathType;

pub const DEFAULT_GIT_CREDENTIALS_PATH: &str = "/opt/artisan/etc/git.cf";

pub fn resolve_git_credentials_path(configured_path: Option<&str>) -> PathType {
    let path = configured_path
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .unwrap_or(DEFAULT_GIT_CREDENTIALS_PATH);

    PathType::Content(path.to_string())
}

#[cfg(test)]
mod tests {
    use super::{resolve_git_credentials_path, DEFAULT_GIT_CREDENTIALS_PATH};

    #[test]
    fn uses_configured_credentials_path_when_present() {
        assert_eq!(
            resolve_git_credentials_path(Some("/srv/config/custom.git.cf")).to_string(),
            "/srv/config/custom.git.cf"
        );
    }

    #[test]
    fn uses_default_for_missing_or_blank_credentials_path() {
        assert_eq!(
            resolve_git_credentials_path(None).to_string(),
            DEFAULT_GIT_CREDENTIALS_PATH
        );
        assert_eq!(
            resolve_git_credentials_path(Some("  ")).to_string(),
            DEFAULT_GIT_CREDENTIALS_PATH
        );
    }
}

#[cfg(test)]
mod credentials_file_roundtrip_tests {
    use crate::test_support::Sandbox;
    use artisan_middleware::git_actions::{GitCredentials, GitServer};
    use serial_test::serial;

    // Confirms the claim this test suite was built on: artisan_middleware
    // already has what's needed to produce a real git.cf fixture
    // (`GitCredentials::save`, the exact reverse of the `GitCredentials::new`
    // load path this app uses in production) -- self-contained encryption,
    // no external key/keyfile required.
    #[tokio::test]
    #[serial]
    async fn saved_credentials_round_trip_through_the_real_load_path() {
        let sandbox = Sandbox::new();
        let auth_items = vec![
            sandbox.git_auth("acme", "widgets", "main"),
            sandbox.git_auth("acme", "gadgets", "develop"),
        ];
        let path = sandbox.write_credentials_file(auth_items.clone()).await;

        let loaded = GitCredentials::new(Some(&path))
            .await
            .expect("load the fixture git.cf back");

        assert_eq!(loaded.auth_items.len(), 2);
        assert_eq!(loaded.auth_items[0].user, auth_items[0].user);
        assert_eq!(loaded.auth_items[0].repo, auth_items[0].repo);
        assert_eq!(loaded.auth_items[0].branch, auth_items[0].branch);
        assert!(matches!(loaded.auth_items[0].server, GitServer::Custom(_)));
        assert_eq!(loaded.auth_items[1].repo, auth_items[1].repo);

        // The on-disk file must actually be encrypted, not plain JSON --
        // otherwise this "round trip" would trivially pass even if
        // encryption were silently broken.
        let raw = std::fs::read_to_string(path.to_string()).expect("read raw git.cf bytes");
        assert!(
            !raw.contains("widgets") && !raw.contains("acme"),
            "git.cf should be encrypted on disk, not contain plaintext repo names"
        );
    }
}
