use lexi::setup_logging;
use std::fs;

#[test]
fn test_deferred_panic_logging() {
    let mut log_path = std::env::current_exe().unwrap_or_default();
    log_path.set_extension("log");

    if log_path.as_os_str().is_empty() {
        log_path = std::path::PathBuf::from("LEXI.log");
    }

    if log_path.exists() {
        let _ = fs::remove_file(&log_path);
    }

    // Setup logging
    setup_logging();

    // Normal log messages should NOT create the file
    log::info!("This should not create a file");
    assert!(!log_path.exists(), "Log file should NOT exist before panic");

    // Trigger a panic
    let handle = std::thread::spawn(|| {
        panic!("Triggering deferred log creation");
    });
    let _ = handle.join();

    // Now the file should exist
    assert!(log_path.exists(), "Log file SHOULD exist after panic");
    let content = fs::read_to_string(&log_path).unwrap();
    assert!(content.contains("PANIC occurred at"));
    assert!(content.contains("Triggering deferred log creation"));

    // Clean up
    let _ = fs::remove_file(&log_path);
}
