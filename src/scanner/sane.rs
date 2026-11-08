//! SANE backend using the `scanimage` command line tool.

use super::{Backend, Capabilities, ScannerInfo};
use crate::config::{ColorMode, ScanSettings, Source};
use anyhow::{bail, Context, Result};
use image::RgbImage;
use std::path::PathBuf;
use std::process::Command;

fn scanimage() -> Option<PathBuf> {
    which::which("scanimage")
        .ok()
        .or_else(|| {
            ["/opt/homebrew/bin/scanimage", "/usr/local/bin/scanimage", "/usr/bin/scanimage"]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.exists())
        })
}

pub fn discover() -> Vec<ScannerInfo> {
    let Some(bin) = scanimage() else { return Vec::new() };
    let Ok(out) = Command::new(bin).arg("-L").output() else { return Vec::new() };
    parse_list(&String::from_utf8_lossy(&out.stdout))
}

/// Parses lines like ``device `epson2:libusb:001:003' is a Epson GT-1500 flatbed scanner``.
fn parse_list(text: &str) -> Vec<ScannerInfo> {
    text.lines()
        .filter_map(|line| {
            let start = line.find('`')? + 1;
            let end = start + line[start..].find('\'')?;
            let device = line[start..end].to_string();
            let name = line[end + 1..]
                .trim()
                .trim_start_matches("is a")
                .trim()
                .to_string();
            Some(ScannerInfo {
                id: format!("sane:{device}"),
                name: if name.is_empty() { device.clone() } else { name },
                backend: Backend::Sane { device },
            })
        })
        .collect()
}

#[derive(Default, Debug)]
struct SaneOptions {
    modes: Vec<String>,
    sources: Vec<String>,
    resolutions: Vec<u32>,
}

/// Values of an option line such as `    --source Flatbed|ADF|ADF Duplex [Flatbed]`.
fn option_values(line: &str, opt: &str) -> Option<Vec<String>> {
    let rest = line.trim().strip_prefix(opt)?.trim();
    let values = rest.split(" [").next().unwrap_or(rest).trim();
    Some(values.split('|').map(|s| s.trim().to_string()).collect())
}

fn parse_options(text: &str) -> SaneOptions {
    let mut o = SaneOptions::default();
    for line in text.lines() {
        if let Some(v) = option_values(line, "--mode ") {
            o.modes = v;
        } else if let Some(v) = option_values(line, "--source ") {
            o.sources = v;
        } else if let Some(v) = option_values(line, "--resolution ") {
            let joined = v.join("|").replace("dpi", "");
            if let Some((min, max)) = joined.split_once("..") {
                let min: u32 = min.trim().parse().unwrap_or(75);
                let max: u32 = max
                    .split(|c: char| !c.is_ascii_digit())
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(600);
                o.resolutions = [75, 100, 150, 200, 300, 400, 600, 1200]
                    .into_iter()
                    .filter(|r| *r >= min && *r <= max)
                    .collect();
            } else {
                o.resolutions = joined.split('|').filter_map(|s| s.trim().parse().ok()).collect();
            }
        }
    }
    o
}

fn read_options(device: &str) -> Result<SaneOptions> {
    let bin = scanimage().context("scanimage not found")?;
    let out = Command::new(bin)
        .args(["-d", device, "-A"])
        .output()
        .context("failed to run scanimage")?;
    Ok(parse_options(&String::from_utf8_lossy(&out.stdout)))
}

fn find_source(o: &SaneOptions, source: Source) -> Option<String> {
    let lower = |s: &String| s.to_lowercase();
    o.sources
        .iter()
        .find(|s| {
            let l = lower(s);
            match source {
                Source::Flatbed => l.contains("flatbed") || l.contains("platen") || l == "normal",
                Source::Feeder => (l.contains("adf") || l.contains("feeder")) && !l.contains("duplex"),
                Source::FeederDuplex => l.contains("duplex"),
            }
        })
        .cloned()
}

fn find_mode(o: &SaneOptions, color: ColorMode) -> Option<String> {
    let want = match color {
        ColorMode::Color => "color",
        _ => "gray",
    };
    o.modes.iter().find(|m| m.to_lowercase().contains(want)).cloned()
}

pub fn capabilities(device: &str) -> Result<Capabilities> {
    let o = read_options(device)?;
    let mut caps = Capabilities::default();
    if !o.resolutions.is_empty() {
        caps.resolutions = o.resolutions.clone();
    }
    let mut sources = Vec::new();
    for s in [Source::Flatbed, Source::Feeder, Source::FeederDuplex] {
        if find_source(&o, s).is_some() {
            sources.push(s);
        }
    }
    if !sources.is_empty() {
        caps.sources = sources;
    }
    Ok(caps)
}

pub fn scan(device: &str, settings: &ScanSettings, progress: &dyn Fn(String)) -> Result<Vec<RgbImage>> {
    let bin = scanimage().context("scanimage not found")?;
    let o = read_options(device).unwrap_or_default();
    let dir = tempfile::tempdir()?;
    let mut cmd = Command::new(bin);
    cmd.args(["-d", device, "--format=png"]);
    cmd.arg(format!("--resolution={}", settings.resolution));
    if let Some(mode) = find_mode(&o, settings.color) {
        cmd.arg(format!("--mode={mode}"));
    }
    if let Some(src) = find_source(&o, settings.source) {
        cmd.arg(format!("--source={src}"));
    }
    if let Some((w, h)) = settings.page_size.mm() {
        cmd.args(["-x", &w.to_string(), "-y", &h.to_string()]);
    }
    let single = dir.path().join("page.png");
    if settings.source.is_feeder() {
        cmd.arg(format!("--batch={}", dir.path().join("page%03d.png").display()));
    } else {
        cmd.arg(format!("--output-file={}", single.display()));
    }
    progress("Scanning…".into());
    log::info!("running {cmd:?}");
    let out = cmd.output().context("failed to run scanimage")?;

    let mut files: Vec<PathBuf> = std::fs::read_dir(dir.path())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "png"))
        .collect();
    files.sort();
    if files.is_empty() {
        bail!(
            "scanimage failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    files
        .iter()
        .map(|p| Ok(image::open(p)?.to_rgb8()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list_and_options() {
        let l = parse_list("device `epson2:libusb:001:003' is a Epson GT-1500 flatbed scanner\n");
        assert_eq!(l[0].id, "sane:epson2:libusb:001:003");
        assert_eq!(l[0].name, "Epson GT-1500 flatbed scanner");
        let o = parse_options(
            "    --mode Lineart|Gray|Color [Color]\n    --resolution 50..1200dpi [300]\n    --source Flatbed|ADF|ADF Duplex [Flatbed]\n",
        );
        assert_eq!(o.sources, vec!["Flatbed", "ADF", "ADF Duplex"]);
        assert_eq!(find_source(&o, Source::FeederDuplex).as_deref(), Some("ADF Duplex"));
        assert_eq!(find_source(&o, Source::Feeder).as_deref(), Some("ADF"));
        assert_eq!(o.resolutions, vec![75, 100, 150, 200, 300, 400, 600, 1200]);
        assert_eq!(find_mode(&o, ColorMode::BlackWhite).as_deref(), Some("Gray"));
    }
}
