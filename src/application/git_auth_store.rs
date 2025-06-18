use artisan_middleware::git_actions::GitAuth;
use dusa_collection_utils::types::rwarc::LockWithTimeout;
use once_cell::sync::OnceCell;

static AUTH_BOX: OnceCell<Box<Vec<LockWithTimeout<GitAuth>>>> = OnceCell::new();

pub fn init_auth_box(items: Vec<GitAuth>) {
    let locked: Vec<LockWithTimeout<GitAuth>> = items.into_iter().map(LockWithTimeout::new).collect();
    let _ = AUTH_BOX.set(Box::new(locked));
}

pub fn auth_items() -> Option<&'static Vec<LockWithTimeout<GitAuth>>> {
    AUTH_BOX.get().map(|v| &**v)
}
