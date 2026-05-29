use std::{
    sync::Arc,
    sync::mpsc::TryRecvError,
    time::{Duration, Instant, SystemTime},
};

use eframe::egui;
use egui::UiKind;
use egui_extras::{Column, TableBuilder};
use rfd::FileDialog;

use crate::search::{PendingSearch, SearchConfig, SearchMode, SearchResult};

/// Main state for the entire application.
pub struct SearchApp {
    /// List of open search tabs.
    tabs: Vec<SearchTab>,
    /// Index of the currently visible tab.
    selected_tab_index: usize,
}

/// Shared data for a specific file path.
/// Matches in the same file share this data instead of duplicating it.
pub struct UiPathData {
    /// The canonical path string.
    pub path: Arc<str>,
    /// Pre-calculated UI layout for the path (includes highlighting).
    pub path_layout: egui::text::LayoutJob,
    /// Pre-formatted modification date string.
    pub formatted_date: String,
    /// Raw modification time for sorting logic.
    pub modified_at: Option<SystemTime>,
}

/// A single row in the search results table.
pub struct UiSearchEntry {
    /// Reference to the shared path information.
    pub path_data: Arc<UiPathData>,
    /// The line number where the match occurred.
    pub line_number: u64,
    /// The UI layout for the matched line (or path if it's a filename-only match).
    pub layout: egui::text::LayoutJob,
}

impl UiSearchEntry {
    /// Creates a rich-text layout job for egui.
    /// This handles the red highlighting for search term matches.
    fn create_layout(
        ui: &egui::Ui,
        text: &str,
        matches: &[(usize, usize)],
    ) -> egui::text::LayoutJob {
        let mut job = egui::text::LayoutJob::default();
        let default_color = ui.style().visuals.text_color();
        let match_color = egui::Color32::RED;
        let font_id = egui::FontId::default();

        if matches.is_empty() {
            job.append(text, 0.0, egui::TextFormat::simple(font_id, default_color));
        } else {
            // Highlight matched regions in red.
            let mut printed_idx = 0;
            for (start, end) in matches.iter().copied() {
                // Ensure indices are within bounds and on character boundaries.
                let start = start.min(text.len());
                let end = end.min(text.len());

                if printed_idx < start {
                    let part = Self::get_safe_slice(text, printed_idx, start);
                    if !part.is_empty() {
                        job.append(
                            part,
                            0.0,
                            egui::TextFormat::simple(font_id.clone(), default_color),
                        );
                    }
                }

                let matched_part = Self::get_safe_slice(text, start, end);
                if !matched_part.is_empty() {
                    job.append(
                        matched_part,
                        0.0,
                        egui::TextFormat::simple(font_id.clone(), match_color),
                    );
                }
                printed_idx = end;
            }
            // Print any remaining text after the last match.
            if printed_idx < text.len() {
                let part = Self::get_safe_slice(text, printed_idx, text.len());
                if !part.is_empty() {
                    job.append(part, 0.0, egui::TextFormat::simple(font_id, default_color));
                }
            }
        }
        job
    }

    /// Helper to safely slice a string at byte offsets, ensuring character boundaries.
    fn get_safe_slice(text: &str, start: usize, end: usize) -> &str {
        if start >= end || start >= text.len() {
            return "";
        }
        let end = end.min(text.len());

        // Find the nearest character boundaries.
        let mut actual_start = start;
        while actual_start > 0 && !text.is_char_boundary(actual_start) {
            actual_start -= 1;
        }

        let mut actual_end = end;
        while actual_end < text.len() && !text.is_char_boundary(actual_end) {
            actual_end += 1;
        }

        // Final safety check.
        if actual_start < actual_end && actual_end <= text.len() {
            &text[actual_start..actual_end]
        } else {
            ""
        }
    }
}

/// Represents a single search tab's state.
pub struct SearchTab {
    /// The specific configuration for this tab (paths, queries, filters).
    config: SearchConfig,
    /// The accumulated results for this tab.
    results: Vec<UiSearchEntry>,
    /// Handle to the active background search worker.
    pending_search: Option<PendingSearch>,
    /// Counters for UI statistics.
    file_searched: usize,
    line_searched: usize,
    /// How long the search took (or has been taking).
    search_duration: Duration,
    /// Any critical error to show to the user.
    error_message: Option<String>,
    /// Used for debouncing (prevents searching on every single keystroke).
    last_input_time: Option<Instant>,
    /// Sorting state.
    sort_by_modified_asc: bool,
}

impl Default for SearchTab {
    fn default() -> Self {
        Self {
            config: SearchConfig::default(),
            results: Vec::new(),
            pending_search: None,
            file_searched: 0,
            line_searched: 0,
            search_duration: Duration::from_secs(0),
            error_message: None,
            last_input_time: None,
            sort_by_modified_asc: false,
        }
    }
}

impl SearchTab {
    /// Creates a new search tab with the given context and patterns.
    pub fn from_context(context: Vec<String>, patterns: String) -> Self {
        Self {
            config: SearchConfig::new(context, patterns),
            ..Self::default()
        }
    }

    /// Stops any running search and optionally clears the results.
    fn cancel_search(&mut self, clear_results: bool) {
        if let Some(pending) = self.pending_search.as_mut() {
            pending.signal_stop();
            self.search_duration = pending.elapsed();
        }

        // Clear the pending search state.
        self.pending_search = None;

        // Clear the results if requested.
        if clear_results {
            self.results.clear();
            self.file_searched = 0;
            self.line_searched = 0;
            self.search_duration = Duration::from_secs(0);
            self.error_message = None;
        }
    }

    /// Hot-loop for processing results from the background threads.
    /// Limit the processing time to 10ms per frame to keep the UI responsive.
    fn update_pending_search(&mut self, ui: &egui::Ui) {
        let mut is_done = false;
        let mut new_results = Vec::new();

        // Process any pending search results.
        if let Some(pending) = self.pending_search.as_mut() {
            self.search_duration = pending.elapsed();

            // Process results until we run out of time or reach the safety limit.
            let start_processing = Instant::now();
            loop {
                // Stay within the time budget for this frame.
                if start_processing.elapsed() > Duration::from_millis(10) {
                    break;
                }

                // Process a single result, if available.
                match pending.try_recv() {
                    Ok(result) => {
                        // Safety limit: stop at 10k results to prevent the app from eating all RAM.
                        if self.results.len() < 10_000 {
                            if !result.entries.is_empty() || !result.path_matches.is_empty() {
                                self.file_searched += 1;
                                self.line_searched += result.entries.len();
                                new_results.push(result);
                            }
                        } else {
                            pending.signal_stop();
                            is_done = true;
                            break;
                        }
                    }
                    // No more results to process; exit the loop.
                    Err(TryRecvError::Empty) => break,

                    // The worker thread has disconnected; exit the loop.
                    Err(TryRecvError::Disconnected) => {
                        is_done = true;
                        self.search_duration = pending.elapsed();
                        break;
                    }
                }
            }
        }

        // Convert the raw SearchResult into UI-ready entries.
        for result in new_results {
            self.save_results(ui, result);
        }

        // Sort whenever get new data.
        if !is_done || !self.results.is_empty() {
            self.sort_results();
        }

        // Remove the pending search if we're done.
        if is_done {
            self.pending_search = None;
        }
    }

    /// Transforms a background result into formatted UI rows.
    fn save_results(&mut self, ui: &egui::Ui, result: SearchResult) {
        let path: Arc<str> = Arc::clone(&result.path);
        // Pre-calculate path layout once for the file.
        let path_layout = UiSearchEntry::create_layout(ui, &path, &result.path_matches);

        // Pre-format the date so don't do it every frame in the table.
        let formatted_date = if let Some(modified) = result.modified_at {
            let datetime: chrono::DateTime<chrono::Local> = modified.into();
            datetime.format("%Y-%m-%d %H:%M:%S").to_string()
        } else {
            "-".to_string()
        };

        // Shared data for all hits in this file.
        let path_data = Arc::new(UiPathData {
            path,
            path_layout,
            formatted_date,
            modified_at: result.modified_at,
        });

        // Case: File name matches, but no content matches (or "File name only" mode).
        if !result.path_matches.is_empty() && result.entries.is_empty() {
            self.results.push(UiSearchEntry {
                path_data: Arc::clone(&path_data),
                line_number: 0,
                layout: path_data.path_layout.clone(),
            });
        }

        // Case: Multiple lines matched inside the file.
        for entry in result.entries.into_iter() {
            let layout = UiSearchEntry::create_layout(ui, &entry.text, &entry.matches);
            self.results.push(UiSearchEntry {
                path_data: Arc::clone(&path_data),
                line_number: entry.line_number,
                layout,
            });
        }
    }

    /// Returns whether the search tab is currently searching for results.
    fn is_searching(&self) -> bool {
        self.pending_search.is_some()
    }

    /// Returns the duration of the current search, or the total search duration if not searching.
    fn search_duration(&self) -> Duration {
        if let Some(pending) = &self.pending_search {
            pending.elapsed()
        } else {
            self.search_duration
        }
    }

    /// Sorts the results based on modification time.
    fn sort_results(&mut self) {
        self.results.sort_by(|a, b| {
            let a_time = a.path_data.modified_at.unwrap_or(SystemTime::UNIX_EPOCH);
            let b_time = b.path_data.modified_at.unwrap_or(SystemTime::UNIX_EPOCH);
            if self.sort_by_modified_asc {
                a_time.cmp(&b_time)
            } else {
                b_time.cmp(&a_time)
            }
        });
    }
}

impl Default for SearchApp {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchApp {
    pub fn new() -> Self {
        Self {
            tabs: vec![SearchTab::from_context(vec![Self::cwd()], String::new())],
            selected_tab_index: 0,
        }
    }

    /// Helper to get current working directory safely.
    fn cwd() -> String {
        std::env::current_dir()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|_| String::from("./"))
    }

    /// Triggers the background search logic.
    fn search_parallel(tab: &mut SearchTab) {
        tab.cancel_search(true);
        if let Ok(pending) = crate::search::spawn_search(&tab.config) {
            tab.pending_search = Some(pending);
        } else {
            tab.error_message = Some("검색어를 입력해주세요.".to_string());
        }
    }

    /// Opens the Windows folder picker.
    fn pick_paths(config: &mut SearchConfig) -> bool {
        let mut file_dialog = FileDialog::new();
        // Start dialog in the last selected folder for convenience.
        if let Some(path) = config.paths().last() {
            file_dialog = file_dialog.set_directory(path);
        } else {
            file_dialog = file_dialog.set_directory(Self::cwd());
        }

        if let Some(folders) = file_dialog.pick_folders() {
            for folder in folders {
                let path_str = folder.to_string_lossy().to_string();
                if !config.paths.contains(&path_str) {
                    config.paths.push(path_str);
                }
            }
            return true;
        }
        false
    }

    /// Renders the top tab bar.
    fn draw_tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.horizontal(|ui| {
                    let mut to_remove = None;
                    for (i, tab) in self.tabs.iter().enumerate() {
                        let label = {
                            let valid_queries: Vec<String> = tab
                                .config
                                .queries
                                .iter()
                                .map(|q| q.query.trim())
                                .filter(|q| !q.is_empty())
                                .map(|q| q.to_string())
                                .collect();

                            if valid_queries.is_empty() {
                                format!("탭 {}", i + 1)
                            } else {
                                valid_queries.join(", ")
                            }
                        };
                        let response = ui.selectable_label(self.selected_tab_index == i, label);
                        if response.clicked() {
                            self.selected_tab_index = i;
                        }
                        // Only show 'x' if there's more than one tab.
                        if self.tabs.len() > 1 && ui.button("x").clicked() {
                            to_remove = Some(i);
                        }
                    }
                    if let Some(i) = to_remove {
                        self.tabs.remove(i);
                        if self.selected_tab_index >= self.tabs.len() {
                            self.selected_tab_index = self.tabs.len().saturating_sub(1);
                        }
                    }
                });
            });
            if ui.button("+ 새 탭").on_hover_text("새 탭").clicked() {
                self.tabs
                    .push(SearchTab::from_context(vec![Self::cwd()], String::new()));
                self.selected_tab_index = self.tabs.len() - 1;
            }
        });
    }

    /// Renders the inputs for paths, patterns, and search terms.
    fn draw_search_controls(&mut self, ui: &mut egui::Ui, tab_index: usize) -> bool {
        let mut input_changed = false;
        let tab = &mut self.tabs[tab_index];

        ui.vertical(|ui| {
            // Path chips management.
            ui.horizontal_top(|ui| {
                ui.label("폴더경로:");

                ui.vertical(|ui| {
                    let mut path_to_remove = None;
                    let mut open_picker = false;

                    // Render path chips.
                    // Each chip is a horizontal row with a path label and an optional remove button.
                    let paths_count = tab.config.paths.len();
                    for (i, path) in tab.config.paths.iter().enumerate() {
                        let is_last = i == paths_count - 1;

                        ui.scope(|ui| {
                            ui.style_mut().visuals.widgets.inactive.bg_fill = ui
                                .style()
                                .visuals
                                .widgets
                                .active
                                .bg_fill
                                .linear_multiply(0.1);

                            ui.horizontal(|ui| {
                                ui.monospace(path).on_hover_text(path);
                                if paths_count > 1 && ui.small_button("x").clicked() {
                                    path_to_remove = Some(i);
                                }

                                // Add path picker button for the last chip.
                                if is_last && ui.button("+ 경로 추가").clicked() {
                                    open_picker = true;
                                }
                            });
                        });
                    }

                    // Remove path chip.
                    if let Some(i) = path_to_remove {
                        tab.config.paths.remove(i);
                        input_changed = true;
                    }

                    // Add path picker.
                    if open_picker && Self::pick_paths(&mut tab.config) {
                        input_changed = true;
                    }
                });
            });

            ui.add_space(5.0);

            // File pattern input (glob filtering).
            ui.horizontal(|ui| {
                ui.label("파일패턴:");
                let remaining_width = ui.available_width() - 300.0;
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut tab.config.patterns)
                            .desired_width(remaining_width)
                            .hint_text("예시: *.pdf (PDF 파일만 검색) *.{pdf,csv} (PDF, CSV 파일만 검색) !dir/ (dir 폴더 제외)"),
                    )
                    .changed()
                {
                    input_changed = true;
                }

                let combo_id = ui.id().with("search_mode_combo").with(tab_index);
                let combo_res = egui::ComboBox::from_id_salt(combo_id)
                    .selected_text(tab.config.mode.label())
                    .width(180.0)
                    .show_ui(ui, |ui| {
                        let mut sub_changed = false;

                        sub_changed |= ui.selectable_value(
                            &mut tab.config.mode,
                            SearchMode::FileNameOnly,
                            SearchMode::FileNameOnly.label()
                        ).changed();

                        sub_changed |= ui.selectable_value(
                            &mut tab.config.mode,
                            SearchMode::PathAndContent,
                            SearchMode::PathAndContent.label()
                        ).changed();

                        sub_changed |= ui.selectable_value(
                            &mut tab.config.mode,
                            SearchMode::IncludeDocContent,
                            SearchMode::IncludeDocContent.label()
                        ).changed();

                        sub_changed
                    });
                if let Some(true) = combo_res.inner {
                    input_changed = true;
                }
            });

            ui.add_space(5.0);

            // Main search term inputs.
            let mut should_cancel = false;
            for query in &mut tab.config.queries {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("검색어: ").strong());
                    let remaining_width = ui.available_width() - 300.0;
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut query.query)
                                .desired_width(remaining_width)
                                .hint_text("예시: text"),
                        )
                        .changed()
                    {
                        input_changed = true;
                    }
                    if ui
                        .button("검색중지")
                        .on_hover_text("검색을 중지합니다. (Esc)")
                        .clicked()
                    {
                        should_cancel = true;
                    }
                });
            }
            // Allow cancelling with Esc key.
            if should_cancel || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                tab.cancel_search(false);
            }
        });

        input_changed
    }

    /// Renders the bottom status bar with search stats and timing.
    fn draw_status_bar(&self, ui: &mut egui::Ui, tab_index: usize) {
        let tab = &self.tabs[tab_index];
        ui.horizontal(|ui| {
            if let Some(err) = &tab.error_message {
                ui.colored_label(egui::Color32::RED, err);
            } else if tab.is_searching() {
                ui.spinner();
                ui.label("검색중: ");
            } else if tab.last_input_time.is_some() {
                ui.label("대기중: ");
            } else if tab.file_searched > 0 {
                ui.colored_label(egui::Color32::DARK_GREEN, "✅ 완료: ");
            }

            let duration = tab.search_duration();
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.label("검색어 일치:");
                ui.label(format!(
                    "{} 파일 / {} 라인",
                    tab.file_searched, tab.line_searched
                ));
                ui.add_space(10.0);
                ui.label("소요시간:");
                ui.label(format!("{:.3}초", duration.as_secs_f32()));
            });
        });
    }

    /// Renders the main results table.
    fn draw_results_table(&mut self, ui: &mut egui::Ui, tab_index: usize) {
        let tab = &mut self.tabs[tab_index];
        let text_height = egui::TextStyle::Body.resolve(ui.style()).size + 7.0;

        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::auto().at_least(300.0).clip(true))
            .column(Column::auto().at_least(25.0).clip(true))
            .column(Column::auto().at_least(125.0).clip(true))
            .column(Column::remainder().clip(true))
            .header(20.0, |mut header| {
                header.col(|ui| {
                    ui.label("파일명");
                });
                header.col(|ui| {
                    ui.label("라인");
                });
                header.col(|ui| {
                    let label = match tab.sort_by_modified_asc {
                        true => "최근 수정 일시 ▲",
                        false => "최근 수정 일시 ▼",
                    };
                    if ui.button(label).clicked() {
                        tab.sort_by_modified_asc = !tab.sort_by_modified_asc;
                        tab.sort_results();
                    }
                });
                header.col(|ui| {
                    ui.label("파일 내용");
                });
            })
            .body(|body| {
                body.rows(text_height, tab.results.len(), |mut row| {
                    let row_index = row.index();
                    let entry = &tab.results[row_index];

                    // Column: File Path
                    row.col(|ui| {
                        let response = ui.label(entry.path_data.path_layout.clone());
                        let response = ui.interact(
                            response.rect,
                            ui.id().with(row_index),
                            egui::Sense::click(),
                        );

                        // Context menu for common file operations.
                        response.context_menu(|ui| {
                            if ui.button("파일 열기").clicked() {
                                let _ = open::that(entry.path_data.path.as_ref());
                                ui.close_kind(UiKind::Menu);
                            }
                            if ui.button("폴더 열기").clicked() {
                                if let Some(parent) =
                                    std::path::Path::new(entry.path_data.path.as_ref()).parent()
                                {
                                    let _ = open::that(parent);
                                }
                                ui.close_kind(UiKind::Menu);
                            }
                            ui.separator();
                            if ui.button("경로 복사하기").clicked() {
                                ui.ctx().copy_text(entry.path_data.path.to_string());
                                ui.close_kind(UiKind::Menu);
                            }
                        });

                        if response.double_clicked() {
                            let _ = open::that(entry.path_data.path.as_ref());
                        }

                        response.on_hover_text(entry.path_data.path.as_ref());
                    });

                    // Column: Line Number
                    row.col(|ui| {
                        if entry.line_number > 0 {
                            ui.label(entry.line_number.to_string());
                        } else {
                            ui.label("-");
                        }
                    });

                    // Column: Modification Date
                    row.col(|ui| {
                        ui.label(&entry.path_data.formatted_date);
                    });

                    // Column: Snippet / File Content
                    row.col(|ui| {
                        ui.label(entry.layout.clone());
                    });
                });
            });
    }
}

impl eframe::App for SearchApp {
    /// The heart of the egui loop. Runs ~60 times per second.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.draw_tab_bar(ui);
            ui.separator();

            if let Some(tab_index) = self
                .tabs
                .get(self.selected_tab_index)
                .map(|_| self.selected_tab_index)
            {
                // Process background search data.
                if let Some(tab) = self.tabs.get_mut(tab_index) {
                    tab.update_pending_search(ui);
                }

                // Draw search controls and detect input changes.
                let input_changed = self.draw_search_controls(ui, tab_index);

                // Handle debouncing logic.
                if let Some(tab) = self.tabs.get_mut(tab_index) {
                    if input_changed {
                        // Reset timer if user typed something new.
                        tab.last_input_time = Some(Instant::now());
                    }

                    // Only search if 250ms have passed since the last keypress.
                    if let Some(last_time) = tab.last_input_time
                        && last_time.elapsed() > Duration::from_millis(250)
                    {
                        Self::search_parallel(tab);
                        tab.last_input_time = None;
                    }
                }

                // Draw the rest of the UI.
                self.draw_status_bar(ui, tab_index);
                ui.separator();
                self.draw_results_table(ui, tab_index);
            }
        });

        // If searching or waiting for a debounce timer, keep the UI repainting as fast as possible for smooth spinners/response.
        if self
            .tabs
            .iter()
            .any(|t| t.is_searching() || t.last_input_time.is_some())
        {
            ui.request_repaint();
        }
    }
}
