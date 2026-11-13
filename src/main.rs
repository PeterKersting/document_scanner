use scandesk::app;

fn main() -> eframe::Result {
    // Optional file argument: its folder and name are used for saving.
    // (macOS may add its own `-psn_…` argument when launched from Finder.)
    let start_file = std::env::args_os()
        .skip(1)
        .map(std::path::PathBuf::from)
        .find(|a| !a.to_string_lossy().starts_with('-'));
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ScanDesk")
            .with_inner_size([1200.0, 820.0])
            .with_min_inner_size([800.0, 560.0])
            .with_icon(std::sync::Arc::new(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png"))
                    .expect("valid icon"),
            )),
        ..Default::default()
    };
    eframe::run_native("ScanDesk", options, Box::new(|cc| Ok(Box::new(app::ScanApp::new(cc, start_file)))))
}
