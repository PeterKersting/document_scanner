//! Persistent application settings (scanner, scan options, target folder, last name).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Flatbed,
    Feeder,
    FeederDuplex,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Flatbed => "Flatbed",
            Source::Feeder => "Document feeder (ADF)",
            Source::FeederDuplex => "Document feeder, duplex",
        }
    }
    pub fn is_feeder(self) -> bool {
        !matches!(self, Source::Flatbed)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    Color,
    Gray,
    BlackWhite,
}

impl ColorMode {
    pub const ALL: [ColorMode; 3] = [ColorMode::Color, ColorMode::Gray, ColorMode::BlackWhite];
    pub fn label(self) -> &'static str {
        match self {
            ColorMode::Color => "Color",
            ColorMode::Gray => "Grayscale",
            ColorMode::BlackWhite => "Black & white",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageSize {
    A4,
    A5,
    Letter,
    Legal,
    Max,
}

impl PageSize {
    pub const ALL: [PageSize; 5] = [
        PageSize::A4,
        PageSize::A5,
        PageSize::Letter,
        PageSize::Legal,
        PageSize::Max,
    ];
    pub fn label(self) -> &'static str {
        match self {
            PageSize::A4 => "A4 (210 × 297 mm)",
            PageSize::A5 => "A5 (148 × 210 mm)",
            PageSize::Letter => "US Letter",
            PageSize::Legal => "US Legal",
            PageSize::Max => "Maximum scan area",
        }
    }
    /// Size in millimetres, `None` for the maximum scan area.
    pub fn mm(self) -> Option<(f32, f32)> {
        match self {
            PageSize::A4 => Some((210.0, 297.0)),
            PageSize::A5 => Some((148.0, 210.0)),
            PageSize::Letter => Some((215.9, 279.4)),
            PageSize::Legal => Some((215.9, 355.6)),
            PageSize::Max => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Pdf,
    Jpeg,
    Png,
}

impl OutputFormat {
    pub const ALL: [OutputFormat; 3] = [OutputFormat::Pdf, OutputFormat::Jpeg, OutputFormat::Png];
    pub fn label(self) -> &'static str {
        match self {
            OutputFormat::Pdf => "PDF",
            OutputFormat::Jpeg => "JPEG",
            OutputFormat::Png => "PNG",
        }
    }
    pub fn extension(self) -> &'static str {
        match self {
            OutputFormat::Pdf => "pdf",
            OutputFormat::Jpeg => "jpg",
            OutputFormat::Png => "png",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ScanSettings {
    pub source: Source,
    pub resolution: u32,
    pub color: ColorMode,
    pub page_size: PageSize,
    pub format: OutputFormat,
    /// Run OCR and embed a text layer (PDF only).
    pub ocr: bool,
    /// Tesseract language codes, e.g. "deu+eng".
    pub ocr_lang: String,
    /// Detect the document edges and crop / straighten the page.
    pub auto_crop: bool,
    pub jpeg_quality: u8,
    /// Leave out empty pages (e.g. blank backs in duplex scans).
    pub skip_blank: bool,
    /// Put all scanned documents into one PDF instead of one PDF each.
    pub combine_pdf: bool,
}

impl Default for ScanSettings {
    fn default() -> Self {
        Self {
            source: Source::Flatbed,
            resolution: 300,
            color: ColorMode::Color,
            page_size: PageSize::A4,
            format: OutputFormat::Pdf,
            ocr: true,
            ocr_lang: "deu+eng".into(),
            auto_crop: true,
            jpeg_quality: 85,
            skip_blank: true,
            combine_pdf: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Config {
    /// Stable id of the last selected scanner.
    pub scanner_id: Option<String>,
    pub settings: ScanSettings,
    pub target_dir: Option<PathBuf>,
    /// Name of the last saved document (without extension).
    pub last_name: String,
    /// Scanners added by hand (eSCL base URL, e.g. `http://192.168.1.20:8080/eSCL`).
    pub manual_scanners: Vec<String>,
    /// Show the orientation grid over the preview.
    pub show_grid: bool,
}

fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("com", "ScanDesk", "ScanDesk")
        .map(|d| d.config_dir().join("config.json"))
}

impl Config {
    pub fn load() -> Config {
        let Some(path) = config_path() else {
            return Config::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log::warn!("Ignoring unreadable config {}: {e}", path.display());
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    log::warn!("Could not save config {}: {e}", path.display());
                }
            }
            Err(e) => log::warn!("Could not serialize config: {e}"),
        }
    }
}
