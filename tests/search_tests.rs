use std::fs::File;

use tempfile::tempdir;

use lexi::search::{SearchConfig, spawn_search};

#[test]
fn test_search_filename() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("test_file.txt");
    File::create(&file_path).unwrap();

    let mut config = SearchConfig::new(
        vec![dir.path().to_string_lossy().to_string()],
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
fn test_search_ignore_case() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("CASE_INSENSITIVE.txt");
    File::create(&file_path).unwrap();

    let mut config = SearchConfig::new(
        vec![dir.path().to_string_lossy().to_string()],
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
}

#[test]
fn test_search_korean_utf8() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("korean_utf8_안녕하세요.txt");
    File::create(&file_path).unwrap();

    let mut config = SearchConfig::new(
        vec![dir.path().to_string_lossy().to_string()],
        "".to_string(),
    );
    config.queries[0].query = "안녕".to_string();

    let pending = spawn_search(&config).unwrap();

    let mut results = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < 2 {
        match pending.try_recv() {
            Ok(result) => results.push(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
    }

    assert!(!results.is_empty(), "Should find Korean text in UTF-8 file");
}
