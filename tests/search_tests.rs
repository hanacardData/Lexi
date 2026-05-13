use lexi::search::{SearchConfig, spawn_search};
use std::fs::File;
use std::io::Write;
use tempfile::tempdir;

#[test]
fn test_search_filename() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("test_file.txt");
    File::create(&file_path).unwrap();

    let mut config = SearchConfig::with_paths_and_patterns(
        dir.path().to_string_lossy().to_string(),
        "".to_string(),
    );
    config.queries[0].query = "test_file".to_string();

    let pending = spawn_search(&config).unwrap();

    let mut results = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < 5 {
        match pending.try_recv() {
            Ok(result) => results.push(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
    }

    assert!(!results.is_empty(), "Must find at least one result.");
    assert!(
        results
            .iter()
            .any(|r| &*r.path == file_path.to_string_lossy())
    );
    assert!(results.iter().any(|r| !r.path_matches.is_empty()));
}

#[test]
fn test_search_content() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("content.txt");
    let mut file = File::create(&file_path).unwrap();
    writeln!(file, "Hello World").unwrap();
    writeln!(file, "Rust is awesome").unwrap();

    let mut config = SearchConfig::with_paths_and_patterns(
        dir.path().to_string_lossy().to_string(),
        "".to_string(),
    );
    config.queries[0].query = "Rust".to_string();

    let pending = spawn_search(&config).unwrap();

    let mut results = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < 5 {
        match pending.try_recv() {
            Ok(result) => results.push(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
    }

    assert!(!results.is_empty());
    let r = results
        .iter()
        .find(|r| &*r.path == file_path.to_string_lossy())
        .unwrap();
    assert!(!r.entries.is_empty());
    assert_eq!(r.entries[0].line_number, 2);
    assert!(r.entries[0].text.contains("Rust"));
}

#[test]
fn test_search_ignore_case() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("case.txt");
    let mut file = File::create(&file_path).unwrap();
    writeln!(file, "CASE INSENSITIVE").unwrap();

    let mut config = SearchConfig::with_paths_and_patterns(
        dir.path().to_string_lossy().to_string(),
        "".to_string(),
    );
    config.queries[0].query = "case".to_string();

    let pending = spawn_search(&config).unwrap();

    let mut results = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < 5 {
        match pending.try_recv() {
            Ok(result) => results.push(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
    }

    assert!(!results.is_empty());
    assert!(results.iter().any(|r| !r.entries.is_empty()));
}
