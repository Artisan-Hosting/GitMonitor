use artisan_middleware::dusa_collection_utils::{core::logger::LogLevel, log};
use signal_hook::{
    consts::{signal::SIGHUP, SIGUSR1},
    iterator::Signals,
};
use std::{sync::Arc, thread};
use tokio::sync::Notify;

pub fn sighup_watch(reload_watcher: Arc<Notify>) {
    thread::spawn(move || {
        let mut signals = Signals::new(&[SIGHUP]).expect("Failed to register signals");
        for _ in signals.forever() {
            reload_watcher.notify_one();
            log!(LogLevel::Info, "Received SIGHUP, marked for reload");
        }
    });
}

pub fn sigusr_watch(exit_watcher: Arc<Notify>) {
    thread::spawn(move || {
        let mut signals = Signals::new(&[SIGUSR1]).expect("Failed to register signals");
        for _ in signals.forever() {
            exit_watcher.notify_one();
            log!(LogLevel::Info, "Received SIGHUP, exiting");
        }
    });
}
