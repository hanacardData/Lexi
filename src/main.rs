#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use eframe::egui;

use lexi::app::SearchApp;

/// Tries to load Malgun Gothic (Standard Windows Korean font).
/// This ensures that the UI doesn't show broken boxes for Korean text.
fn setup_custom_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    // Path to the default Korean font on Windows.
    let font_path = "C:\\Windows\\Fonts\\malgun.ttf";

    if let Ok(data) = std::fs::read(font_path) {
        // Embed the font data into the context.
        fonts.font_data.insert(
            "korean_font".to_owned(),
            std::sync::Arc::new(egui::FontData::from_owned(data)),
        );

        // Add to both proportional and monospace families so all UI elements can use it.
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "korean_font".to_owned());

        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("korean_font".to_owned());

        ctx.set_fonts(fonts);
    } else {
        // Log a warning if the font is missing, though most Windows installs will have it.
        eprintln!(
            "Warning: No Korean font found on this system. Korean characters may not display correctly."
        );
    }
}

fn main() -> eframe::Result {
    // Load the icon from memory (embedded at compile time)
    let icon_data = include_bytes!("../assets/icon.png");
    let icon = image::load_from_memory(icon_data)
        .expect("Failed to load icon")
        .to_rgba8();
    let (width, height) = icon.dimensions();

    // Configure the main window options.
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1024.0, 768.0])
            .with_title("Lexi")
            .with_icon(egui::IconData {
                rgba: icon.into_raw(),
                width,
                height,
            }),
        ..Default::default()
    };

    // Start the eframe application loop.
    eframe::run_native(
        "lexi",
        options,
        Box::new(|cc| {
            // Need custom fonts to correctly display Korean characters on Windows.
            setup_custom_fonts(&cc.egui_ctx);
            // Initialize the main App state.
            Ok(Box::new(SearchApp::new()))
        }),
    )
}
