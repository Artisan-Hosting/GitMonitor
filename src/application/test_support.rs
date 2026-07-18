#![cfg(test)]
//! Shared fixtures for the integration/soak test suite. A `Sandbox` is a
//! hermetic temp-dir environment: it points this app's shared git config at
//! a temp path (via `AIS_GITMON_CONFIG_DIR`) and provides helpers to create
//! local bare "origin" repos, seed and update them, and build matching
//! `GitAuth`/`GitCredentials` fixtures -- so tests never touch the real
//! `/etc/ais_gitmon` or `/var/www/ais`.
//!
//! Every test that constructs a `Sandbox` (or otherwise shells out to `git`
//! through this app's `git_cmd()` helpers) must be marked `#[serial]`:
//! `AIS_GITMON_CONFIG_DIR` is a process-wide env var, and `cargo test` runs
//! tests in parallel threads within one process by default.

use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    thread::{self, JoinHandle},
};

use artisan_middleware::{
    dusa_collection_utils::core::types::{pathtype::PathType, stringy::Stringy},
    git_actions::{GitAuth, GitCredentials, GitServer},
};
use tempfile::TempDir;
use tokio::process::Command;

/// Fixed dummy GitHub token used across the whole test binary.
/// `auth::init_gh_token` stores its value in a process-wide `OnceCell`, so
/// every test that needs a token must agree on the same value regardless of
/// which test happens to initialize it first (later calls are no-ops).
pub const TEST_TOKEN: &str = "gitmon-test-token";

pub struct Sandbox {
    dir: TempDir,
}

impl Sandbox {
    /// Creates a hermetic sandbox and points `AIS_GITMON_CONFIG_DIR` at it.
    pub fn new() -> Self {
        let dir = TempDir::new().expect("create sandbox temp dir");
        std::env::set_var("AIS_GITMON_CONFIG_DIR", dir.path().join("gitconfig-root"));

        let token_file = dir.path().join("token.toml");
        fs::write(&token_file, format!("token = \"{}\"\n", TEST_TOKEN))
            .expect("write dummy token file");
        // Ignored if a token is already set by an earlier test in this
        // process -- every caller uses the same TEST_TOKEN value.
        let _ = crate::auth::init_gh_token(Some(token_file.to_str().unwrap()));

        Self { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Directory containing every bare repo created via `seed_origin`
    /// (`<remotes_root>/<user>/<repo>.git`). Exposed so tests can point an
    /// `AuthHeaderProbeServer` at it directly.
    pub fn remotes_root(&self) -> PathBuf {
        self.dir.path().join("remotes")
    }

    /// Base usable as a `GitServer::Custom(..)` value: `file://<sandbox>/remotes`.
    /// `expected_remote_url` appends `/<user>/<repo>.git` to whatever base is
    /// configured, so repos created via `seed_origin` live at exactly the
    /// path this resolves to.
    pub fn remotes_base_url(&self) -> String {
        format!("file://{}", self.remotes_root().display())
    }

    fn bare_repo_path(&self, user: &str, repo: &str) -> PathBuf {
        self.remotes_root().join(user).join(format!("{}.git", repo))
    }

    /// Creates a bare repo at `<remotes_root>/<user>/<repo>.git`, seeds it
    /// with one commit on `branch`, and returns its on-disk path (also
    /// usable directly as a git remote, or as the root for
    /// `AuthHeaderProbeServer`).
    pub async fn seed_origin(
        &self,
        user: &str,
        repo: &str,
        branch: &str,
        files: &[(&str, &str)],
    ) -> PathBuf {
        let bare = self.bare_repo_path(user, repo);
        fs::create_dir_all(&bare).expect("create bare repo dir");
        run_git(&[], &bare, &["init", "--bare", "-q"]).await;

        let scratch = TempDir::new_in(self.dir.path()).expect("create seed scratch dir");
        run_git(&[], scratch.path(), &["init", "-q", "-b", branch]).await;
        write_files(scratch.path(), files);
        run_git(&[], scratch.path(), &["add", "-A"]).await;
        run_git(
            &identity_env(),
            scratch.path(),
            &["commit", "-q", "-m", "seed"],
        )
        .await;
        run_git(
            &[],
            scratch.path(),
            &["remote", "add", "origin", bare.to_str().unwrap()],
        )
        .await;
        run_git(&[], scratch.path(), &["push", "-q", "origin", branch]).await;

        // `git init --bare` (no -b) leaves HEAD pointing at whatever
        // init.defaultBranch happens to be on this machine (often "master"),
        // and pushing a new branch never retargets it -- a plain `git clone`
        // of this bare repo with no --branch override would try to resolve
        // that stale HEAD and fail ("remote HEAD refers to nonexistent
        // ref"). file:// clones in this test suite always pass an explicit
        // --branch so they never hit this, but dumb-http clients (a
        // submodule fetched via AuthHeaderProbeServer, or any real client
        // that doesn't pin a branch) rely on HEAD being correct.
        run_git(
            &[],
            &bare,
            &["symbolic-ref", "HEAD", &format!("refs/heads/{}", branch)],
        )
        .await;

        // Keep dumb-http metadata current so repos created here are usable
        // directly by AuthHeaderProbeServer too, not just file:// clones.
        run_git(&[], &bare, &["update-server-info"]).await;

        bare
    }

    /// Clones `bare` fresh, writes `files`, commits, and pushes a new commit
    /// to `branch` -- simulates upstream activity for the tracking tests.
    /// Returns the new commit SHA.
    pub async fn commit_to_origin(
        &self,
        bare: &Path,
        branch: &str,
        files: &[(&str, &str)],
        message: &str,
    ) -> String {
        let scratch = TempDir::new_in(self.dir.path()).expect("create push scratch dir");
        // Clone into the (already-created, empty) scratch dir.
        run_git(
            &[],
            self.dir.path(),
            &[
                "clone",
                "-q",
                "--branch",
                branch,
                bare.to_str().unwrap(),
                scratch.path().to_str().unwrap(),
            ],
        )
        .await;
        write_files(scratch.path(), files);
        run_git(&[], scratch.path(), &["add", "-A"]).await;
        run_git(
            &identity_env(),
            scratch.path(),
            &["commit", "-q", "-m", message],
        )
        .await;
        run_git(&[], scratch.path(), &["push", "-q", "origin", branch]).await;
        run_git(&[], bare, &["update-server-info"]).await;

        run_git_output(scratch.path(), &["rev-parse", "HEAD"])
            .await
            .trim()
            .to_string()
    }

    /// Builds a `GitAuth` fixture pointing at this sandbox's local `file://`
    /// remotes base (no builder exists upstream -- plain struct literal).
    pub fn git_auth(&self, user: &str, repo: &str, branch: &str) -> GitAuth {
        GitAuth {
            user: Stringy::from(user),
            repo: Stringy::from(repo),
            branch: Stringy::from(branch),
            server: GitServer::Custom(self.remotes_base_url()),
            token: None,
        }
    }

    /// Same as [`Sandbox::git_auth`] but pointed at an arbitrary custom
    /// server base (e.g. an `AuthHeaderProbeServer`'s `base_url()`).
    pub fn git_auth_with_server(
        &self,
        user: &str,
        repo: &str,
        branch: &str,
        base: &str,
    ) -> GitAuth {
        GitAuth {
            user: Stringy::from(user),
            repo: Stringy::from(repo),
            branch: Stringy::from(branch),
            server: GitServer::Custom(base.to_string()),
            token: None,
        }
    }

    /// Writes a real, valid encrypted git.cf fixture via the upstream
    /// `GitCredentials::save` (the exact reverse of the `GitCredentials::new`
    /// load path this app already uses).
    pub async fn write_credentials_file(&self, auth_items: Vec<GitAuth>) -> PathType {
        let file = self.dir.path().join("git.cf");
        let path = PathType::from(file);
        let creds = GitCredentials { auth_items };
        creds.save(&path).await.expect("save git.cf fixture");
        path
    }

    /// A fresh temp path (not yet existing) to use as a managed checkout
    /// destination, standing in for the hardcoded `/var/www/ais/<hash>` path
    /// production uses (see `generate_git_project_path`, not overridable).
    pub fn checkout_path(&self, name: &str) -> PathType {
        PathType::from(self.dir.path().join("checkouts").join(name))
    }

    /// Plain `git clone` of `bare` straight to `dest`, bypassing
    /// `handle_new_repo` entirely (no `www-data` chown, no safe.directory
    /// setup beyond what's needed to run git commands against it). Lets
    /// tests reach an "already exists" checkout state to exercise
    /// `handle_existing_repo`/`inspect_repo_checkout`/submodule sync without
    /// needing the ownership-changing privileges `handle_new_repo` requires.
    pub async fn clone_checkout(&self, bare: &Path, branch: &str, dest: &PathType) {
        if let Some(parent) = Path::new(&dest.to_string()).parent() {
            fs::create_dir_all(parent).expect("create checkout parent dir");
        }
        run_git(
            &[],
            self.dir.path(),
            &[
                "clone",
                "-q",
                "--branch",
                branch,
                bare.to_str().unwrap(),
                &dest.to_string(),
            ],
        )
        .await;
    }

    /// Adds a submodule (at `submodule_url`, checked out under
    /// `submodule_path`) as a new commit on `bare`/`branch`. `submodule_url`
    /// should be reachable over http(s) -- modern git blocks `file://`
    /// submodule transport by default (CVE-2022-39253), and production only
    /// ever deals with http(s) submodule URLs anyway, so an
    /// `AuthHeaderProbeServer` URL is the right stand-in here, not a second
    /// `file://` bare repo.
    pub async fn add_submodule(
        &self,
        bare: &Path,
        branch: &str,
        submodule_url: &str,
        submodule_path: &str,
    ) {
        let scratch = TempDir::new_in(self.dir.path()).expect("create submodule scratch dir");
        run_git(
            &[],
            self.dir.path(),
            &[
                "clone",
                "-q",
                "--branch",
                branch,
                bare.to_str().unwrap(),
                scratch.path().to_str().unwrap(),
            ],
        )
        .await;
        run_git(
            &[],
            scratch.path(),
            &["submodule", "add", "-q", submodule_url, submodule_path],
        )
        .await;
        run_git(
            &identity_env(),
            scratch.path(),
            &["commit", "-q", "-m", "add submodule"],
        )
        .await;
        run_git(&[], scratch.path(), &["push", "-q", "origin", branch]).await;
        run_git(&[], bare, &["update-server-info"]).await;
    }
}

/// Probes whether this process can actually `chown` a file to the `www-data`
/// user (requires both that the user exists on this machine, and that the
/// process has root/CAP_CHOWN -- neither of which holds on an arbitrary dev
/// box). `handle_new_repo`/`recreate_repo` unconditionally chown a fresh
/// clone to `www-data`, so tests that exercise those need this to be true;
/// gate them with `#[ignore]` rather than letting them fail confusingly on
/// machines that aren't set up like the real deployment target.
pub fn can_chown_to_www_data() -> bool {
    let Ok((uid, gid)) = artisan_middleware::users::get_id("www-data") else {
        return false;
    };
    let probe = std::env::temp_dir().join(format!("gitmon-chown-probe-{}", std::process::id()));
    if fs::write(&probe, b"x").is_err() {
        return false;
    }
    let ok =
        artisan_middleware::users::set_file_ownership(&PathType::from(probe.clone()), uid, gid)
            .is_ok();
    let _ = fs::remove_file(&probe);
    ok
}

fn identity_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("GIT_AUTHOR_NAME", "GitMon Test"),
        ("GIT_AUTHOR_EMAIL", "gitmon-test@example.invalid"),
        ("GIT_COMMITTER_NAME", "GitMon Test"),
        ("GIT_COMMITTER_EMAIL", "gitmon-test@example.invalid"),
    ]
}

fn write_files(dir: &Path, files: &[(&str, &str)]) {
    for (name, contents) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture file parent dir");
        }
        fs::write(path, contents).expect("write fixture file");
    }
}

pub(crate) async fn run_git(envs: &[(&str, &str)], cwd: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd).args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let output = cmd.output().await.expect("spawn git");
    assert!(
        output.status.success(),
        "git {:?} in {:?} failed: {}",
        args,
        cwd,
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(crate) async fn run_git_output(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .await
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {:?} in {:?} failed: {}",
        args,
        cwd,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// A tiny local HTTP server exposing a directory of bare repos over git's
/// "dumb http" protocol (plain static file serving; the served repos must
/// already have up-to-date `info/refs`/`objects/info/packs`, which
/// `Sandbox::seed_origin`/`commit_to_origin` maintain via
/// `git update-server-info`). Records every request's `Authorization`
/// header so tests can assert on the app's auth-header wiring against a
/// real HTTP round trip instead of a `file://` remote (which never touches
/// HTTP headers at all).
pub struct AuthHeaderProbeServer {
    port: u16,
    received_auth_headers: Arc<StdMutex<Vec<Option<String>>>>,
    _handle: JoinHandle<()>,
}

impl AuthHeaderProbeServer {
    /// Serves everything under `root` as static files at `http://127.0.0.1:<port>/...`.
    pub fn start(root: &Path) -> Self {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind probe http server");
        let port = match server.server_addr() {
            tiny_http::ListenAddr::IP(addr) => addr.port(),
            _ => unreachable!("bound with an IP address"),
        };
        // Sanity-check the port is actually loopback-bound before handing it out.
        let _: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

        let received = Arc::new(StdMutex::new(Vec::new()));
        let received_for_thread = received.clone();
        let root = root.to_path_buf();

        let handle = thread::spawn(move || {
            for request in server.incoming_requests() {
                let auth_header = request
                    .headers()
                    .iter()
                    .find(|h| {
                        h.field
                            .as_str()
                            .as_str()
                            .eq_ignore_ascii_case("authorization")
                    })
                    .map(|h| h.value.as_str().to_string());
                received_for_thread.lock().unwrap().push(auth_header);

                // Strip the query string (matching a plain static file
                // server's usual path translation): git's smart-http probe
                // (`GET info/refs?service=git-upload-pack`) then resolves to
                // the same literal `info/refs` file dumb clients fetch
                // separately. Serving that file's content is correct for
                // both -- what actually matters is including a Content-Type
                // below (see the request.respond call), without which git's
                // dumb-http client fetches objects fine but silently fails
                // to resolve which ref HEAD points to.
                let url_path = request.url().split('?').next().unwrap_or("/");
                let rel = url_path.trim_start_matches('/');
                let file_path = root.join(rel);

                let response_result = match fs::read(&file_path) {
                    Ok(bytes) => {
                        // git's dumb-http client requires *some* Content-Type
                        // to be present to treat this as a successful static
                        // fetch; tiny_http sends none by default, which git
                        // interpreted ambiguously (objects fetched fine, but
                        // "remote HEAD refers to nonexistent ref" -- checkout
                        // silently failed). Match a plain static file server.
                        let content_type: tiny_http::Header =
                            "Content-Type: application/octet-stream".parse().unwrap();
                        let response =
                            tiny_http::Response::from_data(bytes).with_header(content_type);
                        request.respond(response)
                    }
                    Err(_) => request.respond(
                        tiny_http::Response::from_string("not found").with_status_code(404),
                    ),
                };
                let _ = response_result;
            }
        });

        Self {
            port,
            received_auth_headers: received,
            _handle: handle,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn received_auth_headers(&self) -> Vec<Option<String>> {
        self.received_auth_headers.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.received_auth_headers.lock().unwrap().len()
    }
}
