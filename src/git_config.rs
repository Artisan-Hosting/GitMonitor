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
