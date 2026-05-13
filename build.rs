fn main() {
    // Only want to run this on Windows.
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
        let icon_path = "assets/icon.ico";

        // Check if the icon exists before trying to compile it.
        // This prevents the build from failing if the assets folder is empty.
        if std::path::Path::new(icon_path).exists() {
            let mut res = winresource::WindowsResource::new();
            res.set_icon(icon_path);
            let _ = res.compile();
        } else {
            // Log a warning during build instead of crashing.
            println!(
                "cargo:warning=Icon file not found at {}. Skipping icon embedding.",
                icon_path
            );
        }
    }
}
