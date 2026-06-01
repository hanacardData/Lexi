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
struct SearchSink<'a> {
    /// Accumulates results found during the scan of a single file.
    results: &'a mut Vec<SearchEntry>,
    /// The matcher used to find the exact byte offsets of multiple terms.
    matcher: &'a RegexMatcher,
    /// Atomic flag to signal worker threads to stop early.
    quit: Arc<AtomicBool>,
}

impl searcher::Sink for SearchSink<'_> {
    type Error = SearchError;

    /// Called by the searcher when a line matches the regex.
    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        // Check if user cancelled the search even during file scan.
        if self.quit.load(Ordering::Relaxed) {
            return Ok(false);
        }

        let line_number = mat.line_number().unwrap_or(0);
        let bytes = mat.bytes();

        // The grep crate tells us the line matches, but not where all the terms are.
        // Do a second pass here to find all match offsets (for highlighting in UI).
        // Limit to 5 matches per line to prevent performance degradation.
        let mut all_matches = Vec::new();
        let mut at = 0;
        while let Ok(Some(m)) = self.matcher.find_at(bytes, at) {
            all_matches.push((m.start(), m.end()));
            at = m.end();
            if all_matches.len() >= 5 {
                break;
            }
        }

        // Logic for handling extremely long lines (like log files or minified JS).
        // Center the view around the first match to keep the UI snappy.
        const MAX_LINE_LENGTH: usize = 128;
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

                let has_leading = window_start > 0;
                let has_trailing = window_end < bytes.len();
                let estimated_cap = (window_end - window_start)
                    + 3 * has_leading as usize
                    + 3 * has_trailing as usize;

                // Truncate the window to fit within MAX_LINE_LENGTH, preserving character boundaries.
                let mut truncated = String::with_capacity(estimated_cap);
                if has_leading {
                    truncated.push_str("...");
                }
                truncated.push_str(&String::from_utf8_lossy(&bytes[window_start..window_end]));
                if has_trailing {
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
                let mut truncated = String::with_capacity(end + 3);
                truncated.push_str(&String::from_utf8_lossy(&bytes[..end]));
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchMode {
    #[default]
    PathAndContent,
    FileNameOnly,
    IncludeDocContent,
}

impl SearchMode {
    pub fn label(&self) -> &'static str {
        match self {
            SearchMode::FileNameOnly => "파일명만",
            SearchMode::PathAndContent => "파일명 + 텍스트",
            SearchMode::IncludeDocContent => "파일명 + 텍스트 + 문서내용",
        }
    }
}

/// Full configuration for a search operation.
#[derive(Debug, Clone, Default)]
pub struct SearchConfig {
    pub paths: Vec<String>,
    pub patterns: String,
    pub queries: Vec<SearchQuery>,
    pub mode: SearchMode,
}

impl SearchConfig {
    /// Creates a new search config with the given paths and patterns.
    pub fn new(paths: Vec<String>, patterns: String) -> Self {
        Self {
            paths,
            patterns,
            queries: vec![SearchQuery::new()],
            mode: SearchMode::PathAndContent,
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
    let mode = config.mode;
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
                    Ok(e) if e.file_type().map(|ft| ft.is_dir()).unwrap_or(false) => {
                        return WalkState::Continue;
                    }
                    Err(walk_err) => {
                        log::warn!("Skipping entry: {}", walk_err);
                        return WalkState::Continue;
                    }
                    _ => return WalkState::Continue,
                };

                let path = entry.path();

                // First pass: check if the path itself matches the query.
                let mut path_matches = Vec::new();
                if let Some(path_str) = path.to_str() {
                    let mut at = 0;
                    while let Ok(Some(m)) = matcher.find_at(path_str.as_bytes(), at) {
                        path_matches.push((m.start(), m.end()));
                        at = m.end();
                    }
                }

                // Second pass: scan file content (unless "File name only" mode is on).
                let mut entries = Vec::new();
                if mode == SearchMode::FileNameOnly {
                    if !path_matches.is_empty() {
                        let path_text: Arc<str> = path.to_string_lossy().into();
                        if tx
                            .send(SearchResult {
                                path: path_text,
                                path_matches: path_matches.into(),
                                entries,
                                modified_at: None,
                            })
                            .is_err()
                        {
                            return WalkState::Quit;
                        }
                    }
                    return WalkState::Continue;
                }

                let mut handled = false;
                if mode == SearchMode::IncludeDocContent
                    && let Some(ext) = path.extension().and_then(|e| e.to_str())
                {
                    let ext_lower = ext.to_ascii_lowercase();
                    match ext_lower.as_str() {
                        "docx" | "xlsx" | "pptx" | "doc" | "ppt" | "xls" => {
                            match office_oxide::extract_text(path) {
                                Ok(text) => {
                                    let mut sink = SearchSink {
                                        results: &mut entries,
                                        matcher: &matcher,
                                        quit: quit.clone(),
                                    };
                                    let _ = searcher.search_slice(
                                        &*matcher,
                                        text.as_bytes(),
                                        &mut sink,
                                    );
                                    handled = true;
                                }
                                Err(err) => {
                                    log::warn!(
                                        "Failed to load DOCX/XLSX/PPTX file: {}, error: {}",
                                        path.display(),
                                        err
                                    );
                                    handled = true;
                                }
                            }
                        }
                        "pdf" => match pdf_oxide::PdfDocument::open(path) {
                            Ok(doc) => {
                                if let Ok(total_pages) = doc.page_count() {
                                    let path_text: Arc<str> = path.to_string_lossy().into();
                                    let modified_at =
                                        entry.metadata().ok().and_then(|m| m.modified().ok());

                                    let path_matches_arc: Arc<[(usize, usize)]> =
                                        Arc::from(path_matches.as_slice());
                                    let mut reported_any = false;

                                    for page in 0..total_pages {
                                        if quit.load(Ordering::Relaxed) {
                                            return WalkState::Quit;
                                        }
                                        if let Ok(page_text) = doc.extract_text(page) {
                                            let mut page_entries = Vec::new();
                                            let mut sink = SearchSink {
                                                results: &mut page_entries,
                                                matcher: &matcher,
                                                quit: quit.clone(),
                                            };
                                            let _ = searcher.search_slice(
                                                &*matcher,
                                                page_text.as_bytes(),
                                                &mut sink,
                                            );

                                            if !page_entries.is_empty() {
                                                if tx
                                                    .send(SearchResult {
                                                        path: Arc::clone(&path_text),
                                                        path_matches: Arc::clone(&path_matches_arc),
                                                        entries: page_entries,
                                                        modified_at,
                                                    })
                                                    .is_err()
                                                {
                                                    return WalkState::Quit;
                                                }
                                                reported_any = true;
                                            }
                                        }
                                    }

                                    // If the path matched but no content was found, report the path match now.
                                    if !reported_any && !path_matches.is_empty() {
                                        if tx
                                            .send(SearchResult {
                                                path: path_text,
                                                path_matches: path_matches_arc,
                                                entries: Vec::new(),
                                                modified_at,
                                            })
                                            .is_err()
                                        {
                                            return WalkState::Quit;
                                        }
                                    }
                                }
                                return WalkState::Continue;
                            }
                            Err(err) => {
                                log::warn!(
                                    "Failed to load PDF file: {}, error: {}",
                                    path.display(),
                                    err
                                );
                                return WalkState::Continue;
                            }
                        },
                        "eml" => match std::fs::read(path) {
                            Ok(content) => {
                                match quoted_printable::decode(
                                    &content,
                                    quoted_printable::ParseMode::Robust,
                                ) {
                                    Ok(decoded) => {
                                        let mut sink = SearchSink {
                                            results: &mut entries,
                                            matcher: &matcher,
                                            quit: quit.clone(),
                                        };
                                        let _ =
                                            searcher.search_slice(&*matcher, &decoded, &mut sink);
                                        handled = true;
                                    }
                                    Err(err) => {
                                        log::warn!(
                                            "Failed to decode EML file: {}, error: {}",
                                            path.display(),
                                            err
                                        );
                                        handled = true;
                                    }
                                }
                            }
                            Err(err) => {
                                log::warn!(
                                    "Failed to load EML file: {}, error: {}",
                                    path.display(),
                                    err
                                );
                                handled = true;
                            }
                        },
                        _ => {}
                    }
                }

                if !handled {
                    let sink = SearchSink {
                        results: &mut entries,
                        matcher: &matcher,
                        quit: quit.clone(),
                    };
                    // The actual heavy lifting: disk I/O and regex scanning.
                    if let Err(search_err) = searcher.search_path(&*matcher, path, sink) {
                        log::warn!(
                            "Failed to search path: {}, error: {:?}",
                            path.display(),
                            search_err
                        );
                    }
                }

                // If anything matched (name or content), send it to the UI.
                if !entries.is_empty() || !path_matches.is_empty() {
                    let path_text: Arc<str> = path.to_string_lossy().into();
                    let modified_at = entry.metadata().ok().and_then(|m| m.modified().ok());
                    if tx
                        .send(SearchResult {
                            path: path_text,
                            path_matches: path_matches.into(),
                            entries,
                            modified_at,
                        })
                        .is_err()
                    {
                        return WalkState::Quit;
                    }
                }
                WalkState::Continue
            })
        });
    });

    Ok(pending)
}
