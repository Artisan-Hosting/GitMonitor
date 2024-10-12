use artisan_middleware::{log, logger::LogLevel};
use signal_hook::{consts::signal::SIGHUP, iterator::Signals};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::thread;


pub fn sighup_watch(reload: Arc<AtomicBool>) {
    thread::spawn(move || {
        let mut signals = Signals::new(&[SIGHUP]).expect("Failed to register signals");
        for _ in signals.forever() {
            reload.store(true, Ordering::Relaxed);
            log!(LogLevel::Info, "Received SIGHUP, marked for reload");
        }
    });    
}
