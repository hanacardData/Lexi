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
use grep::{
    regex::{RegexMatcher, RegexMatcherBuilder},
    searcher::{self, BinaryDetection, Searcher, SearcherBuilder, SinkMatch},
};
use ignore::{
    WalkBuilder, WalkState,
    overrides::{Override, OverrideBuilder},
};

/// Represents a single line match in a file.
/// Use Arc to share the heavy text data between the search worker and the UI thread.
pub struct SearchEntry {
    /// 1-based line number.
    pub line_number: u64,
    /// The actual text content of the line, potentially truncated.
    pub text: Arc<str>,
    /// Byte offsets of the search term matches within the text.
    pub matches: Arc<[(usize, usize)]>,
}

/// A complete result for a single file.
/// Contains the path and all lines that matched the query.
pub struct SearchResult {
    /// Canonicalized path to the file.
    pub path: Arc<str>,
    /// Byte offsets of search term matches within the path string itself.
    pub path_matches: Arc<[(usize, usize)]>,
    /// List of content matches found inside the file.
    pub entries: Vec<SearchEntry>,
    /// Last modified time, used for sorting in the UI.
    pub modified_at: Option<SystemTime>,
}

#[derive(Debug)]
pub struct SearchError;
impl searcher::SinkError for SearchError {
    fn error_message<T: std::fmt::Display>(message: T) -> Self {
        log::error!("Search Sink Error: {}", message);
        Self
    }
}

/// The Sink is the "callback" object.
/// It gets called whenever a match is found in a file.
struct SearchSink<'a, 'm> {
    /// Accumulates results found during the scan of a single file.
    results: &'a mut Vec<SearchEntry>,
    /// The matcher used to find the exact byte offsets of multiple terms.
    matcher: &'m RegexMatcher,
}

impl searcher::Sink for SearchSink<'_, '_> {
    type Error = SearchError;

    /// Called by the searcher when a line matches the regex.
    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let line_number = mat.line_number().unwrap_or(0);
        let bytes = mat.bytes();

        // The grep crate tells us the line matches, but not where all the terms are.
        // Do a second pass here to find all match offsets (for highlighting in UI).
        let mut all_matches = Vec::new();
        let mut at = 0;
        while let Ok(Some(m)) = self.matcher.find_at(bytes, at) {
            all_matches.push((m.start(), m.end()));
            at = m.end();
        }

        // Logic for handling extremely long lines (like log files or minified JS).
        // Center the view around the first match to keep the UI snappy.
        const MAX_LINE_LENGTH: usize = 1024;
        let (display_text, display_matches) = if bytes.len() > MAX_LINE_LENGTH {
            if let Some(&(m_start, _)) = all_matches.first() {
                // Calculate a window around the first match.
                let mut window_start = m_start.saturating_sub(MAX_LINE_LENGTH / 2);
                let mut window_end = (window_start + MAX_LINE_LENGTH).min(bytes.len());
                window_start = window_end.saturating_sub(MAX_LINE_LENGTH);

                // Ensure the window starts on a character boundary.
                while window_start > 0 && (bytes[window_start] & 0xC0) == 0x80 {
                    window_start -= 1;
                }

                // Ensure the window ends on a character boundary.
                while window_end > 0
                    && window_end < bytes.len()
                    && (bytes[window_end] & 0xC0) == 0x80
                {
                    window_end -= 1;
                }

                // Truncate the window to fit within MAX_LINE_LENGTH, preserving character boundaries.
                let mut truncated = String::new();
                if window_start > 0 {
                    truncated.push_str("...");
                }
                truncated.push_str(&String::from_utf8_lossy(&bytes[window_start..window_end]));
                if window_end < bytes.len() {
                    truncated.push_str("...");
                }

                // Shift the match offsets to match the truncated string.
                let offset = if window_start > 0 { 3 } else { 0 };
                let shifted_matches = all_matches
                    .into_iter()
                    .filter(|&(s, e)| s >= window_start && e <= window_end)
                    .map(|(s, e)| (s - window_start + offset, e - window_start + offset))
                    .collect::<Vec<_>>();
                (truncated.into(), shifted_matches.into())
            } else {
                // Fallback if a match is found but find_at fails.
                let mut end = MAX_LINE_LENGTH.min(bytes.len());
                while end > 0 && end < bytes.len() && (bytes[end] & 0xC0) == 0x80 {
                    end -= 1;
                }
                let mut truncated = String::from_utf8_lossy(&bytes[..end]).into_owned();
                truncated.push_str("...");
                (truncated.into(), Vec::new().into())
            }
        } else {
            // Normal case: line is short enough to display in full.
            (String::from_utf8_lossy(bytes).into(), all_matches.into())
        };

        self.results.push(SearchEntry {
            line_number,
            text: display_text,
            matches: display_matches,
        });

        Ok(true)
    }
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
    pub file_name_only: bool,
    pub search_doc_content: bool,
}

impl SearchConfig {
    /// Creates a new search config with the given paths and patterns.
    pub fn new(paths: Vec<String>, patterns: String) -> Self {
        Self {
            paths,
            patterns,
            queries: vec![SearchQuery::new()],
            file_name_only: false,
            search_doc_content: false,
        }
    }

    /// Returns a list of reference paths for the directory walker.
    pub fn paths(&self) -> Vec<&Path> {
        self.paths.iter().map(Path::new).collect()
    }

    /// Parses the pattern string (e.g., "*.rs *.md") into a glob override object.
    pub fn overrides(&self) -> Override {
        if self.patterns.is_empty() {
            Override::empty()
        } else {
            let mut builder = OverrideBuilder::new(
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            );
            for glob in self.patterns.split_whitespace() {
                let _ = builder.add(glob);
            }
            builder.build().unwrap_or_else(|_| Override::empty())
        }
    }

    /// Creates a combined Regex matcher from all search terms.
    /// Scan for all words in a single pass.
    fn create_matcher(&self) -> Result<RegexMatcher> {
        let mut builder = RegexMatcherBuilder::new();
        builder
            .case_smart(true)
            .case_insensitive(true)
            .multi_line(true)
            .unicode(true);

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
    let file_name_only = config.file_name_only;
    let search_doc_content = config.search_doc_content;
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
            let mut searcher = SearcherBuilder::new()
                .line_number(true)
                // Immediately quit if hit a null byte (binary file).
                .binary_detection(BinaryDetection::quit(b'\x00'))
                .build();

            // This closure runs for every file found.
            Box::new(move |result| {
                // Check if user cancelled the search.
                if quit.load(Ordering::Relaxed) {
                    return WalkState::Quit;
                }

                let entry = match result {
                    Ok(e) if e.file_type().map(|ft| ft.is_file()).unwrap_or(false) => e,
                    _ => return WalkState::Continue,
                };

                let path = entry.path();
                let path_text: Arc<str> = path.to_string_lossy().into();

                // First pass: check if the path itself matches the query.
                let mut path_matches = Vec::new();
                let mut at = 0;
                while let Ok(Some(m)) = matcher.find_at(path_text.as_bytes(), at) {
                    path_matches.push((m.start(), m.end()));
                    at = m.end();
                }

                let mut entries = Vec::new();
                // Second pass: scan file content (unless "File name only" mode is on).
                if !file_name_only {
                    let mut handled = false;
                    let extension = path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .unwrap_or("")
                        .to_lowercase();

                    if search_doc_content {
                        match extension.as_str() {
                            "docx" | "pptx" | "xlsx" | "doc" | "ppt" | "xls" => {
                                if let Ok(text) = office_oxide::extract_text(path) {
                                    let mut sink = SearchSink {
                                        results: &mut entries,
                                        matcher: &matcher,
                                    };
                                    let _ = searcher.search_slice(
                                        &*matcher,
                                        text.as_bytes(),
                                        &mut sink,
                                    );
                                    handled = true;
                                }
                            }
                            "pdf" => {
                                if let Ok(doc) = pdf_oxide::PdfDocument::open(path) {
                                    let mut full_pdf_text = String::new();
                                    let mut page = 0;
                                    while let Ok(page_text) = doc.extract_text(page) {
                                        full_pdf_text.push_str(&page_text);
                                        full_pdf_text.push('\n');
                                        page += 1;
                                    }
                                    if !full_pdf_text.is_empty() {
                                        let mut sink = SearchSink {
                                            results: &mut entries,
                                            matcher: &matcher,
                                        };
                                        let _ = searcher.search_slice(
                                            &*matcher,
                                            full_pdf_text.as_bytes(),
                                            &mut sink,
                                        );
                                        handled = true;
                                    }
                                }
                            }
                            "eml" => {
                                // Korean .eml files often use Quoted-Printable encoding for the body.
                                if let Ok(content) = std::fs::read(path)
                                    && let Ok(decoded) = quoted_printable::decode(
                                        &content,
                                        quoted_printable::ParseMode::Robust,
                                    )
                                {
                                    let mut sink = SearchSink {
                                        results: &mut entries,
                                        matcher: &matcher,
                                    };
                                    let _ = searcher.search_slice(&*matcher, &decoded, &mut sink);
                                    handled = true;
                                }
                            }
                            _ => {}
                        }
                    }

                    if !handled {
                        let sink = SearchSink {
                            results: &mut entries,
                            matcher: &matcher,
                        };
                        // The actual heavy lifting: disk I/O and regex scanning.
                        let _ = searcher.search_path(&*matcher, path, sink);
                    }
                }

                // If anything matched (name or content), send it to the UI.
                if !entries.is_empty() || !path_matches.is_empty() {
                    let modified_at = entry.metadata().ok().and_then(|m| m.modified().ok());
                    let _ = tx.send(SearchResult {
                        path: path_text,
                        path_matches: path_matches.into(),
                        entries,
                        modified_at,
                    });
                }
                WalkState::Continue
            })
        });
    });

    Ok(pending)
}
