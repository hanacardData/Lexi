// This is the library root that exposes our core modules.
// 'app' handles the UI logic and state management using egui.
// 'search' contains the search engine.
pub mod app;
pub mod search;

use log::error;
use simplelog::{Config, LevelFilter, WriteLogger};

/// Sets up a global panic hook that logs unexpected errors to a file.
/// The log file is only created when a panic actually occurs.
pub fn setup_logging() {
    std::panic::set_hook(Box::new(|panic_info| {
        let mut log_path = std::env::current_exe().unwrap_or_default();
        log_path.set_extension("log");

        // If we can't get the executable path, use a default name.
        if log_path.as_os_str().is_empty() {
            log_path = std::path::PathBuf::from("LEXI.log");
        }

        // Only create the logger and file when a panic occurs.
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            let _ = WriteLogger::init(LevelFilter::Info, Config::default(), file);
        }

        let message = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Unknown panic".to_string()
        };

        let location = panic_info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());

        error!("PANIC occurred at {}: {}", location, message);

        let backtrace = std::backtrace::Backtrace::force_capture();
        error!("Backtrace:\n{}", backtrace);
    }));
}
