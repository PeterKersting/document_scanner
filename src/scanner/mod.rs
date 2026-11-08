//! Scanner discovery and scanning.
//!
//! Two backends are supported:
//! * **eSCL / AirScan** (network scanners announced via Bonjour/mDNS) – pure Rust,
//!   works on macOS, Linux and Windows. This is what macOS Image Capture uses for
//!   most current network scanners and multifunction printers.
//! * **SANE** via the `scanimage` command line tool (USB scanners on Linux, or on
//!   macOS after `brew install sane-backends`).

pub mod escl;
pub mod sane;

use crate::config::{ColorMode, ScanSettings, Source};
use anyhow::Result;
use image::RgbImage;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub enum Backend {
    /// eSCL base URL, e.g. `http://192.168.1.20:8080/eSCL`
    Escl { base: String },
    /// SANE device name as printed by `scanimage -L`
    Sane { device: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScannerInfo {
    /// Stable id used to remember the scanner between runs.
    pub id: String,
    pub name: String,
    pub backend: Backend,
}

impl ScannerInfo {
    pub fn kind(&self) -> &'static str {
        match self.backend {
            Backend::Escl { .. } => "network",
            Backend::Sane { .. } => "SANE",
        }
    }
}

/// What a scanner supports. Sizes are in 1/300 inch.
#[derive(Clone, Debug)]
pub struct Capabilities {
    pub sources: Vec<Source>,
    pub resolutions: Vec<u32>,
    pub colors: Vec<ColorMode>,
    /// Maximum scan area of the flatbed (width, height).
    pub platen_max: (u32, u32),
    /// Maximum scan area of the document feeder (width, height).
    pub adf_max: (u32, u32),
    pub escl_version: Option<String>,
}

impl Capabilities {
    pub fn max_for(&self, source: Source) -> (u32, u32) {
        if source.is_feeder() {
            self.adf_max
        } else {
            self.platen_max
        }
    }
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            sources: vec![Source::Flatbed],
            resolutions: vec![75, 150, 200, 300, 600],
            colors: ColorMode::ALL.to_vec(),
            platen_max: (2550, 3508),
            adf_max: (2550, 4200),
            escl_version: None,
        }
    }
}

/// Finds all scanners reachable from this machine.
pub fn discover(timeout: Duration, manual: &[String]) -> Vec<ScannerInfo> {
    let sane = std::thread::spawn(sane::discover);
    let mut found = escl::discover(timeout);
    for url in manual {
        let base = escl::normalize_base(url);
        if !found.iter().any(|s| s.backend == (Backend::Escl { base: base.clone() })) {
            found.push(ScannerInfo {
                id: format!("escl-url:{base}"),
                name: format!("{base} (manual)"),
                backend: Backend::Escl { base },
            });
        }
    }
    if let Ok(mut s) = sane.join() {
        // AirScan devices also show up in SANE (airscan backend); prefer the native eSCL entry.
        if !found.is_empty() {
            s.retain(|d| match &d.backend {
                Backend::Sane { device } => {
                    !(device.starts_with("airscan:") || device.starts_with("escl:"))
                }
                _ => true,
            });
        }
        found.extend(s);
    }
    found
}

pub fn capabilities(scanner: &ScannerInfo) -> Result<Capabilities> {
    match &scanner.backend {
        Backend::Escl { base } => escl::capabilities(base),
        Backend::Sane { device } => sane::capabilities(device),
    }
}

/// Scans all pages and returns them as RGB images (colour reduction happens later).
pub fn scan(
    scanner: &ScannerInfo,
    settings: &ScanSettings,
    caps: &Capabilities,
    progress: &dyn Fn(String),
) -> Result<Vec<RgbImage>> {
    match &scanner.backend {
        Backend::Escl { base } => escl::scan(base, settings, caps, progress),
        Backend::Sane { device } => sane::scan(device, settings, progress),
    }
}
