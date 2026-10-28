use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, TryRecvError},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Result, bail};
use grep::matcher::Matcher;
use grep::regex::{RegexMatcher, RegexMatcherBuilder};
use ignore::{
    WalkBuilder, WalkState,
    overrides::{Override, OverrideBuilder},
};

/// A complete result for a single file.
/// Contains the path and all lines that matched the query.
pub struct SearchResult {
    /// Canonicalized path to the file.
    pub path: Arc<str>,
    /// Byte offsets of search term matches within the path string itself.
    pub path_matches: Arc<[(usize, usize)]>,
    /// Last modified time, used for sorting in the UI.
    pub modified_at: Option<SystemTime>,
}

/// A handle to a search currently running in the background.
pub struct PendingSearch {
    /// Receiver for results found by the worker threads.
    rx: mpsc::Receiver<SearchResult>,
    /// Atomic flag to signal worker threads to stop early.
    quit: Arc<AtomicBool>,
    /// When the search was started, used for timing.
    start_time: Instant,
}

impl PendingSearch {
    pub fn new(rx: mpsc::Receiver<SearchResult>) -> Self {
        Self {
            rx,
            quit: Arc::new(AtomicBool::new(false)),
            start_time: Instant::now(),
        }
    }

    /// Signals all background threads to stop immediately.
    pub fn signal_stop(&self) {
        self.quit.store(true, Ordering::Relaxed);
    }

    /// Returns the duration of the current search.
    pub fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }

    /// Non-blocking check for new results.
    pub fn try_recv(&self) -> std::result::Result<SearchResult, TryRecvError> {
        self.rx.try_recv()
    }
}

/// If the UI handle is dropped (tab closed), stop the search threads.
impl Drop for PendingSearch {
    fn drop(&mut self) {
        self.signal_stop();
    }
}

/// A single search term wrapper.
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub query: String,
}

impl SearchQuery {
    pub fn new() -> Self {
        Self {
            query: String::new(),
        }
    }
}

/// Full configuration for a search operation.
#[derive(Debug, Clone, Default)]
pub struct SearchConfig {
    pub paths: Vec<String>,
    pub patterns: String,
    pub queries: Vec<SearchQuery>,
}

impl SearchConfig {
    /// Creates a new search config with the given paths and patterns.
    pub fn new(paths: Vec<String>, patterns: String) -> Self {
        Self {
            paths,
            patterns,
            queries: vec![SearchQuery::new()],
        }
    }

    /// Returns a list of reference paths for the directory walker.
    pub fn paths(&self) -> Vec<&Path> {
        self.paths.iter().map(Path::new).collect()
    }

    /// Parses the pattern string (e.g., "*.rs *.md") into a glob override object.
    pub fn overrides(&self) -> Override {
        let mut builder = OverrideBuilder::new("/");

        // Add default excludes for Windows system directories to improve performance.
        if cfg!(target_os = "windows") {
            let _ = builder.add("!C:/Windows/**");
            let _ = builder.add("!C:/Program Files/**");
            let _ = builder.add("!C:/Program Files (x86)/**");
        }

        if !self.patterns.is_empty() {
            for glob in self.patterns.split_whitespace() {
                let _ = builder.add(glob);
            }
        }
        builder.build().unwrap_or_else(|_| Override::empty())
    }

    /// Creates a combined Regex matcher from all search terms.
    /// Scan for all words in a single pass.
    fn create_matcher(&self) -> Result<RegexMatcher> {
        let mut builder = RegexMatcherBuilder::new();
        builder.case_smart(true).unicode(true);

        let literals: Vec<String> = self
            .queries
            .iter()
            .map(|q| q.query.trim())
            .filter(|s| !s.is_empty())
            .map(regex::escape)
            .collect();

        if literals.is_empty() {
            bail!("No search terms");
        }

        // build_literals creates a highly efficient Aho-Corasick or similar automata.
        Ok(builder.build_literals(&literals)?)
    }
}

/// Spawns a background search. This is the heart of the engine.
pub fn spawn_search(config: &SearchConfig) -> Result<PendingSearch> {
    // Create variables for the search configuration.
    let matcher = config.create_matcher()?;
    let paths = config.paths();
    if paths.is_empty() {
        bail!("No search paths provided");
    }

    // Create the channel for communication between the search thread and the UI.
    let (tx, rx) = mpsc::channel();
    let pending = PendingSearch::new(rx);
    let quit = pending.quit.clone();

    // Configure the recursive directory walker (skips hidden files/folders by default).
    let mut walk_builder = WalkBuilder::new(paths[0]);
    for path in &paths[1..] {
        walk_builder.add(path);
    }
    walk_builder.overrides(config.overrides()).hidden(true);

    // Build the parallel walker based on available CPU cores.
    let walker = walk_builder
        .threads(
            thread::available_parallelism()
                .map(|v| v.get())
                .unwrap_or(1),
        )
        .build_parallel();
    let matcher = Arc::new(matcher);

    // Spawn a dedicated controller thread so the UI never blocks.
    thread::spawn(move || {
        walker.run(|| {
            let tx = tx.clone();
            let quit = quit.clone();
            let matcher = matcher.clone();

            // This closure runs for every file found.
            Box::new(move |result| {
                // Check if user cancelled the search.
                if quit.load(Ordering::Relaxed) {
                    return WalkState::Quit;
                }

                let entry = match result {
                    Ok(e) if e.file_type().map(|ft| ft.is_file()).unwrap_or(false) => e,
                    Ok(e) if e.file_type().map(|ft| ft.is_dir()).unwrap_or(false) => {
                        return WalkState::Continue;
                    }
                    Err(walk_err) => {
                        log::warn!("Skipping entry: {}", walk_err);
                        return WalkState::Continue;
                    }
                    _ => return WalkState::Continue,
                };

                // Check if the path itself matches the query.
                let path = entry.path();
                let mut path_matches = Vec::new();
                if let Some(path_str) = path.to_str() {
                    let mut at = 0;
                    while let Ok(Some(m)) = matcher.find_at(path_str.as_bytes(), at) {
                        path_matches.push((m.start(), m.end()));
                        at = m.end();
                    }

                    if !path_matches.is_empty() {
                        let path_text: Arc<str> = path.to_string_lossy().into();
                        let modified_at = entry.metadata().ok().and_then(|m| m.modified().ok());
                        if tx
                            .send(SearchResult {
                                path: path_text,
                                path_matches: path_matches.into(),
                                modified_at,
                            })
                            .is_err()
                        {
                            return WalkState::Quit;
                        }
                    }
                }
                WalkState::Continue
            })
        });
    });
    Ok(pending)
}
