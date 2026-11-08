//! eSCL ("AirScan") network scanner backend.

use super::{Backend, Capabilities, ScannerInfo};
use crate::config::{ColorMode, ScanSettings, Source};
use anyhow::{anyhow, bail, Context, Result};
use image::RgbImage;
use std::net::IpAddr;
use std::time::{Duration, Instant};

const SERVICE: &str = "_uscan._tcp.local.";
const MAX_IMAGE_BYTES: u64 = 512 * 1024 * 1024;

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into()
}

/// Accepts `host`, `host:port`, `http://host:port` or a full base URL and returns
/// an eSCL base URL without trailing slash.
pub fn normalize_base(input: &str) -> String {
    let mut s = input.trim().trim_end_matches('/').to_string();
    if !s.starts_with("http://") && !s.starts_with("https://") {
        s = format!("http://{s}");
    }
    let after_scheme = s.splitn(2, "://").nth(1).unwrap_or("");
    if !after_scheme.contains('/') {
        s.push_str("/eSCL");
    }
    s
}

/// Browses mDNS for eSCL scanners for `timeout`.
pub fn discover(timeout: Duration) -> Vec<ScannerInfo> {
    let mut found: Vec<ScannerInfo> = Vec::new();
    let daemon = match mdns_sd::ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            log::warn!("mDNS unavailable: {e}");
            return found;
        }
    };
    let rx = match daemon.browse(SERVICE) {
        Ok(rx) => rx,
        Err(e) => {
            log::warn!("mDNS browse failed: {e}");
            let _ = daemon.shutdown();
            return found;
        }
    };
    let deadline = Instant::now() + timeout;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match rx.recv_timeout(deadline - now) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(svc)) => {
                let txt = &svc.txt_properties;
                let rs = txt
                    .get_property_val_str("rs")
                    .unwrap_or("eSCL")
                    .trim_matches('/')
                    .to_string();
                // Prefer an IPv4 address; fall back to the host name.
                let mut addrs: Vec<IpAddr> = svc.addresses.iter().map(|a| a.to_ip_addr()).collect();
                addrs.sort_by_key(|a| !a.is_ipv4());
                let host = match addrs.first() {
                    Some(IpAddr::V4(a)) => a.to_string(),
                    _ => svc.host.trim_end_matches('.').to_string(),
                };
                let base = if rs.is_empty() {
                    format!("http://{host}:{}", svc.port)
                } else {
                    format!("http://{host}:{}/{rs}", svc.port)
                };
                let instance = svc
                    .fullname
                    .strip_suffix(&format!(".{SERVICE}"))
                    .unwrap_or(&svc.fullname)
                    .to_string();
                let name = txt
                    .get_property_val_str("ty")
                    .map(str::to_string)
                    .unwrap_or(instance);
                let id = match txt.get_property_val_str("UUID").or(txt.get_property_val_str("uuid")) {
                    Some(u) => format!("escl:{}", u.to_lowercase()),
                    None => format!("escl:{}", svc.fullname),
                };
                if let Some(existing) = found.iter_mut().find(|s| s.id == id) {
                    // Same device seen on another interface; keep an IPv4 entry.
                    if matches!(addrs.first(), Some(IpAddr::V4(_))) {
                        existing.backend = Backend::Escl { base };
                    }
                } else {
                    found.push(ScannerInfo {
                        id,
                        name,
                        backend: Backend::Escl { base },
                    });
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.stop_browse(SERVICE);
    let _ = daemon.shutdown();
    found
}

// ---- XML helpers ---------------------------------------------------------

fn children<'a>(node: roxmltree::Node<'a, 'a>, name: &'a str) -> impl Iterator<Item = roxmltree::Node<'a, 'a>> {
    node.children().filter(move |n| n.tag_name().name() == name)
}

fn descendants<'a>(node: roxmltree::Node<'a, 'a>, name: &'a str) -> impl Iterator<Item = roxmltree::Node<'a, 'a>> {
    node.descendants().filter(move |n| n.tag_name().name() == name)
}

fn text_of<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    node.descendants()
        .find(|n| n.tag_name().name() == name)
        .and_then(|n| n.text())
        .map(str::trim)
}

fn max_size(caps: roxmltree::Node) -> Option<(u32, u32)> {
    let w = text_of(caps, "MaxWidth")?.parse().ok()?;
    let h = text_of(caps, "MaxHeight")?.parse().ok()?;
    Some((w, h))
}

pub fn parse_capabilities(xml: &str) -> Result<Capabilities> {
    let doc = roxmltree::Document::parse(xml).context("invalid capabilities XML")?;
    let root = doc.root_element();
    let mut caps = Capabilities {
        sources: Vec::new(),
        resolutions: Vec::new(),
        colors: Vec::new(),
        platen_max: (2550, 3508),
        adf_max: (2550, 4200),
        escl_version: text_of(root, "Version").map(str::to_string),
    };

    let mut input_caps = Vec::new();
    if let Some(platen) = descendants(root, "PlatenInputCaps").next() {
        caps.sources.push(Source::Flatbed);
        if let Some(m) = max_size(platen) {
            caps.platen_max = m;
        }
        input_caps.push(platen);
    }
    if let Some(adf) = descendants(root, "AdfSimplexInputCaps").next() {
        caps.sources.push(Source::Feeder);
        if let Some(m) = max_size(adf) {
            caps.adf_max = m;
        }
        input_caps.push(adf);
        let duplex = descendants(root, "AdfDuplexInputCaps").next().is_some()
            || descendants(root, "AdfOption").any(|n| n.text() == Some("Duplex"));
        if duplex {
            caps.sources.push(Source::FeederDuplex);
        }
    }

    for ic in &input_caps {
        for r in descendants(*ic, "DiscreteResolution") {
            if let Some(x) = children(r, "XResolution").next().and_then(|n| n.text()) {
                if let Ok(x) = x.trim().parse::<u32>() {
                    caps.resolutions.push(x);
                }
            }
        }
        if let Some(range) = descendants(*ic, "XResolutionRange").next() {
            let min: u32 = text_of(range, "Min").and_then(|s| s.parse().ok()).unwrap_or(75);
            let max: u32 = text_of(range, "Max").and_then(|s| s.parse().ok()).unwrap_or(600);
            for r in [75, 100, 150, 200, 300, 400, 600, 1200] {
                if r >= min && r <= max {
                    caps.resolutions.push(r);
                }
            }
        }
        for m in descendants(*ic, "ColorMode") {
            match m.text().map(str::trim) {
                Some("RGB24") | Some("RGB48") => caps.colors.push(ColorMode::Color),
                Some("Grayscale8") | Some("Grayscale16") => caps.colors.push(ColorMode::Gray),
                _ => {}
            }
        }
    }
    caps.resolutions.sort_unstable();
    caps.resolutions.dedup();
    if caps.resolutions.is_empty() {
        caps.resolutions = vec![75, 150, 300, 600];
    }
    if caps.colors.is_empty() {
        caps.colors.push(ColorMode::Color);
    }
    // Black & white is produced locally from a grayscale scan.
    caps.colors.push(ColorMode::BlackWhite);
    let mut seen = Vec::new();
    caps.colors.retain(|c| {
        let new = !seen.contains(c);
        seen.push(*c);
        new
    });
    caps.colors.sort_by_key(|c| ColorMode::ALL.iter().position(|x| x == c));
    if caps.sources.is_empty() {
        caps.sources.push(Source::Flatbed);
    }
    Ok(caps)
}

pub fn capabilities(base: &str) -> Result<Capabilities> {
    let url = format!("{base}/ScannerCapabilities");
    let mut resp = agent(Duration::from_secs(10))
        .get(&url)
        .call()
        .with_context(|| format!("cannot reach scanner at {url}"))?;
    if !resp.status().is_success() {
        bail!("scanner answered {} for {url}", resp.status());
    }
    let xml = resp.body_mut().read_to_string()?;
    parse_capabilities(&xml)
}

fn scanner_state(base: &str) -> Option<(String, String)> {
    let mut resp = agent(Duration::from_secs(10))
        .get(&format!("{base}/ScannerStatus"))
        .call()
        .ok()?;
    let xml = resp.body_mut().read_to_string().ok()?;
    let doc = roxmltree::Document::parse(&xml).ok()?;
    let root = doc.root_element();
    let state = text_of(root, "State").unwrap_or("").to_string();
    let adf = text_of(root, "AdfState").unwrap_or("").to_string();
    Some((state, adf))
}

/// Builds the ScanSettings request body.
pub fn scan_settings_xml(settings: &ScanSettings, caps: &Capabilities) -> String {
    let (max_w, max_h) = caps.max_for(settings.source);
    let (w, h) = match settings.page_size.mm() {
        Some((wmm, hmm)) => (
            ((wmm / 25.4 * 300.0).round() as u32).min(max_w),
            ((hmm / 25.4 * 300.0).round() as u32).min(max_h),
        ),
        None => (max_w, max_h),
    };
    let color = match settings.color {
        ColorMode::Color if caps.colors.contains(&ColorMode::Color) => "RGB24",
        ColorMode::Color => "Grayscale8",
        _ if caps.colors.contains(&ColorMode::Gray) => "Grayscale8",
        _ => "RGB24",
    };
    let source = if settings.source.is_feeder() { "Feeder" } else { "Platen" };
    let duplex = if settings.source.is_feeder() {
        format!(
            "\n  <scan:Duplex>{}</scan:Duplex>",
            settings.source == Source::FeederDuplex
        )
    } else {
        String::new()
    };
    let version = caps.escl_version.as_deref().unwrap_or("2.0");
    let res = settings.resolution;
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<scan:ScanSettings xmlns:scan="http://schemas.hp.com/imaging/escl/2011/05/03" xmlns:pwg="http://www.pwg.org/schemas/2010/12/sm">
  <pwg:Version>{version}</pwg:Version>
  <scan:Intent>Document</scan:Intent>
  <pwg:ScanRegions>
    <pwg:ScanRegion>
      <pwg:Height>{h}</pwg:Height>
      <pwg:ContentRegionUnits>escl:ThreeHundredthsOfInches</pwg:ContentRegionUnits>
      <pwg:Width>{w}</pwg:Width>
      <pwg:XOffset>0</pwg:XOffset>
      <pwg:YOffset>0</pwg:YOffset>
    </pwg:ScanRegion>
  </pwg:ScanRegions>
  <pwg:InputSource>{source}</pwg:InputSource>{duplex}
  <scan:ColorMode>{color}</scan:ColorMode>
  <scan:XResolution>{res}</scan:XResolution>
  <scan:YResolution>{res}</scan:YResolution>
  <pwg:DocumentFormat>image/jpeg</pwg:DocumentFormat>
  <scan:DocumentFormatExt>image/jpeg</scan:DocumentFormatExt>
</scan:ScanSettings>"#
    )
}

fn absolute_job_url(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.trim_end_matches('/').to_string();
    }
    // base = scheme://host:port/path → keep scheme://host:port
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let host_end = base[scheme_end..]
        .find('/')
        .map(|i| i + scheme_end)
        .unwrap_or(base.len());
    let origin = &base[..host_end];
    if location.starts_with('/') {
        format!("{origin}{}", location.trim_end_matches('/'))
    } else {
        format!("{base}/{}", location.trim_end_matches('/'))
    }
}

pub fn scan(
    base: &str,
    settings: &ScanSettings,
    caps: &Capabilities,
    progress: &dyn Fn(String),
) -> Result<Vec<RgbImage>> {
    if let Some((state, adf)) = scanner_state(base) {
        if settings.source.is_feeder() && adf.contains("Empty") {
            bail!("The document feeder is empty. Insert the pages and try again.");
        }
        if state == "Processing" || state == "Testing" {
            progress("Scanner is busy, waiting…".into());
        }
    }

    let http = agent(Duration::from_secs(300));
    let body = scan_settings_xml(settings, caps);
    log::debug!("ScanSettings:\n{body}");

    // Create the job (retry while the scanner is busy).
    let mut job_url = None;
    for attempt in 0..30 {
        let resp = http
            .post(&format!("{base}/ScanJobs"))
            .header("Content-Type", "text/xml")
            .send(body.as_bytes())
            .context("cannot start scan job")?;
        match resp.status().as_u16() {
            200 | 201 => {
                let loc = resp
                    .headers()
                    .get("Location")
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| anyhow!("scanner did not return a job location"))?;
                job_url = Some(absolute_job_url(base, loc));
                break;
            }
            503 => {
                progress(format!("Scanner busy, retrying ({})…", attempt + 1));
                std::thread::sleep(Duration::from_secs(2));
            }
            409 if settings.source.is_feeder() => {
                bail!("The scanner refused the job (409). Is the document feeder empty?")
            }
            code => bail!("The scanner rejected the scan settings (HTTP {code})."),
        }
    }
    let job_url = job_url.ok_or_else(|| anyhow!("scanner stayed busy"))?;
    log::info!("scan job {job_url}");

    let mut pages = Vec::new();
    let mut busy_retries = 0;
    loop {
        progress(format!("Scanning page {}…", pages.len() + 1));
        let mut resp = http
            .get(&format!("{job_url}/NextDocument"))
            .call()
            .context("lost connection to scanner")?;
        match resp.status().as_u16() {
            200 => {
                busy_retries = 0;
                let bytes = resp
                    .body_mut()
                    .with_config()
                    .limit(MAX_IMAGE_BYTES)
                    .read_to_vec()
                    .context("failed to download scanned page")?;
                progress(format!("Received page {} ({} KB)", pages.len() + 1, bytes.len() / 1024));
                let img = image::load_from_memory(&bytes)
                    .context("scanner sent an unreadable image")?
                    .to_rgb8();
                pages.push(img);
                if !settings.source.is_feeder() {
                    break;
                }
            }
            404 | 410 => break, // no more pages
            503 if busy_retries < 60 => {
                busy_retries += 1;
                std::thread::sleep(Duration::from_millis(1000));
            }
            code => {
                let _ = http.delete(&job_url).call();
                if pages.is_empty() {
                    bail!("Scanning failed (HTTP {code}).");
                }
                log::warn!("NextDocument returned {code}; keeping {} page(s)", pages.len());
                break;
            }
        }
    }
    if pages.is_empty() {
        bail!("The scanner returned no pages.");
    }
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls() {
        assert_eq!(normalize_base("192.168.1.5:8080"), "http://192.168.1.5:8080/eSCL");
        assert_eq!(normalize_base("http://h:80/eSCL/"), "http://h:80/eSCL");
        assert_eq!(
            absolute_job_url("http://h:8080/eSCL", "/eSCL/ScanJobs/1"),
            "http://h:8080/eSCL/ScanJobs/1"
        );
    }

    #[test]
    fn parses_capabilities() {
        let xml = r#"<scan:ScannerCapabilities xmlns:scan="s" xmlns:pwg="p">
          <pwg:Version>2.63</pwg:Version>
          <scan:Platen><scan:PlatenInputCaps><scan:MaxWidth>2550</scan:MaxWidth><scan:MaxHeight>3508</scan:MaxHeight>
          <scan:SettingProfiles><scan:SettingProfile><scan:ColorModes><scan:ColorMode>Grayscale8</scan:ColorMode><scan:ColorMode>RGB24</scan:ColorMode></scan:ColorModes>
          <scan:SupportedResolutions><scan:DiscreteResolutions>
            <scan:DiscreteResolution><scan:XResolution>300</scan:XResolution><scan:YResolution>300</scan:YResolution></scan:DiscreteResolution>
            <scan:DiscreteResolution><scan:XResolution>150</scan:XResolution><scan:YResolution>150</scan:YResolution></scan:DiscreteResolution>
          </scan:DiscreteResolutions></scan:SupportedResolutions></scan:SettingProfile></scan:SettingProfiles></scan:PlatenInputCaps></scan:Platen>
          <scan:Adf><scan:AdfSimplexInputCaps><scan:MaxWidth>2550</scan:MaxWidth><scan:MaxHeight>4200</scan:MaxHeight></scan:AdfSimplexInputCaps>
          <scan:AdfOptions><scan:AdfOption>Duplex</scan:AdfOption></scan:AdfOptions></scan:Adf>
        </scan:ScannerCapabilities>"#;
        let c = parse_capabilities(xml).unwrap();
        assert_eq!(c.sources, vec![Source::Flatbed, Source::Feeder, Source::FeederDuplex]);
        assert_eq!(c.resolutions, vec![150, 300]);
        assert_eq!(c.colors, vec![ColorMode::Color, ColorMode::Gray, ColorMode::BlackWhite]);
        assert_eq!(c.adf_max, (2550, 4200));
        assert_eq!(c.escl_version.as_deref(), Some("2.63"));
    }
}
