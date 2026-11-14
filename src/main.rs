use scandesk::{app, cli};
use std::sync::{Arc, Mutex};

fn main() -> eframe::Result {
    let options = match cli::StartOptions::parse(std::env::args_os().skip(1)) {
        Ok(o) if o.help => {
            println!("{}", cli::USAGE);
            return Ok(());
        }
        Ok(o) => o,
        Err(e) => {
            eprintln!("scandesk: {e}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    let called_by_program = options.called_by_program();
    let saved: Arc<Mutex<Vec<std::path::PathBuf>>> = Arc::default();
    let saved_in_app = saved.clone();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let native = eframe::NativeOptions {
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
    eframe::run_native(
        "ScanDesk",
        native,
        Box::new(|cc| Ok(Box::new(app::ScanApp::new(cc, options, saved_in_app)))),
    )?;
    // Tell a calling program whether a document was saved.
    if called_by_program && saved.lock().map(|s| s.is_empty()).unwrap_or(true) {
        std::process::exit(1);
    }
    Ok(())
}
