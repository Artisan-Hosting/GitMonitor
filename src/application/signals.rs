use signal_hook::{consts::signal::SIGHUP, iterator::Signals};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::thread;
use dusa_collection_utils::log;
use dusa_collection_utils::log::LogLevel;


pub fn sighup_watch(reload: Arc<AtomicBool>) {
    thread::spawn(move || {
        let mut signals = Signals::new(&[SIGHUP]).expect("Failed to register signals");
        for _ in signals.forever() {
            reload.store(true, Ordering::Relaxed);
            log!(LogLevel::Trace, "Received SIGHUP, marked for reload");
        }
    });    
}
