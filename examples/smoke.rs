//! End-to-end check without the GUI: discover → capabilities → scan → process → output.
//! Usage: cargo run --example smoke -- [resolution] [pdf|jpeg|png] [image file instead of scanning]
use scandesk::config::{OutputFormat, ScanSettings};
use scandesk::{output, processing, scanner};
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let res: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(75);
    let format = match args.get(2).map(String::as_str) {
        Some("jpeg") => OutputFormat::Jpeg,
        Some("png") => OutputFormat::Png,
        _ => OutputFormat::Pdf,
    };
    let settings = ScanSettings { resolution: res, format, ..Default::default() };
    let pages = if let Some(file) = args.get(3) {
        vec![image::open(file)?.to_rgb8()]
    } else {
        scan_pages(&settings)?
    };
    process(pages, &settings)
}

fn scan_pages(settings: &ScanSettings) -> anyhow::Result<Vec<image::RgbImage>> {
    let found = scanner::discover(Duration::from_secs(3), &[]);
    for s in &found {
        println!("found: {} [{}] {:?}", s.name, s.id, s.backend);
    }
    let s = found.first().ok_or_else(|| anyhow::anyhow!("no scanner"))?;
    let caps = scanner::capabilities(s)?;
    println!("caps: {caps:?}");
    let pages = scanner::scan(s, settings, &caps, &|m| println!("  {m}"))?;
    println!("pages: {}", pages.len());
    Ok(pages)
}

fn process(pages: Vec<image::RgbImage>, settings: &ScanSettings) -> anyhow::Result<()> {
    let processed: Vec<_> = pages
        .into_iter()
        .map(|p| processing::process_page(p, settings.color))
        .collect();
    let doc = output::ScannedDocument::new(processed, settings.resolution, 1)?;
    println!("edges found on {} page(s)", doc.edges_found());
    let result = output::build(&doc, settings, &[], &output::output_groups(&doc, settings, &Default::default()), &|m| println!("  {m}"))?;
    println!("notes: {:?}", result.notes);
    let keep = std::env::temp_dir().join("scandesk-smoke");
    std::fs::create_dir_all(&keep)?;
    for f in &result.files {
        let to = keep.join(f.file_name().unwrap());
        let _ = std::fs::remove_file(&to);
        output::move_file(f, &to)?;
        println!("wrote {}", to.display());
    }
    Ok(())
}
