//! Writing scan results to a temporary folder (PDF with OCR, JPEG, PNG) and
//! moving them to their final location.

use crate::config::{OutputFormat, ScanSettings};
use anyhow::{bail, Context, Result};
use image::codecs::jpeg::{JpegEncoder, PixelDensity};
use image::{DynamicImage, ExtendedColorType};
use std::borrow::Cow;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One scanned page, kept in memory until the document is saved.
pub struct ScannedPage {
    /// The whole scan area (colour mode already applied).
    pub full: DynamicImage,
    /// The page cropped and straightened along its detected edges, if found.
    pub cropped: Option<DynamicImage>,
    /// The page looks empty (typically the back of a one-sided sheet).
    pub blank: bool,
}

/// A finished scan waiting in its temporary folder. The output format, OCR and
/// cropping are chosen afterwards, when the document is saved.
pub struct ScannedDocument {
    /// Temporary folder holding the raw scan; deleted when the document is dropped.
    pub temp: tempfile::TempDir,
    pub resolution: u32,
    pub pages: Vec<ScannedPage>,
    /// Pages per document: 2 for duplex feeder scans (front + back), else 1.
    pub pages_per_doc: usize,
}

impl ScannedDocument {
    /// Stores the scanned pages in a new temporary folder.
    pub fn new(pages: Vec<ScannedPage>, resolution: u32, pages_per_doc: usize) -> Result<Self> {
        let temp = tempfile::Builder::new().prefix("scandesk-").tempdir()?;
        for (i, page) in pages.iter().enumerate() {
            let path = temp.path().join(format!("scan-{:03}.jpg", i + 1));
            std::fs::write(&path, encode_jpeg(&page.full, 95, resolution)?)?;
        }
        Ok(Self {
            temp,
            resolution,
            pages,
            pages_per_doc: pages_per_doc.max(1),
        })
    }

    /// Number of documents (sheets) in the scan.
    pub fn doc_count(&self) -> usize {
        self.pages.len().div_ceil(self.pages_per_doc)
    }

    /// Page indices of each document, leaving out pages the user deleted and
    /// optionally blank pages (documents that end up empty are dropped).
    pub fn documents(&self, skip_blank: bool, deleted: &HashSet<usize>) -> Vec<Vec<usize>> {
        (0..self.pages.len())
            .collect::<Vec<_>>()
            .chunks(self.pages_per_doc)
            .map(|c| {
                c.iter()
                    .copied()
                    .filter(|i| !deleted.contains(i) && !(skip_blank && self.pages[*i].blank))
                    .collect::<Vec<_>>()
            })
            .filter(|d| !d.is_empty())
            .collect()
    }

    pub fn blank_count(&self) -> usize {
        self.pages.iter().filter(|p| p.blank).count()
    }

    pub fn page(&self, i: usize, crop: bool) -> &DynamicImage {
        let p = &self.pages[i];
        match (&p.cropped, crop) {
            (Some(c), true) => c,
            _ => &p.full,
        }
    }

    pub fn edges_found(&self) -> usize {
        self.pages.iter().filter(|p| p.cropped.is_some()).count()
    }
}

/// Crops an image to a normalised rectangle `[x0, y0, x1, y1]` (0…1).
pub fn crop_normalized(img: &DynamicImage, c: [f32; 4]) -> DynamicImage {
    let (w, h) = (img.width() as f32, img.height() as f32);
    let x0 = (c[0].clamp(0.0, 1.0) * w).round() as u32;
    let y0 = (c[1].clamp(0.0, 1.0) * h).round() as u32;
    let x1 = ((c[2].clamp(0.0, 1.0) * w).round() as u32).max(x0 + 1).min(img.width());
    let y1 = ((c[3].clamp(0.0, 1.0) * h).round() as u32).max(y0 + 1).min(img.height());
    img.crop_imm(x0, y0, x1 - x0, y1 - y0)
}

/// Rotates an image by `degrees` clockwise around its centre, keeping its size.
/// Uncovered corners become white. Bilevel images stay bilevel.
pub fn rotate_degrees(img: &DynamicImage, degrees: f32, bilevel: bool) -> DynamicImage {
    use imageproc::geometric_transformations::{rotate_about_center, Border, Interpolation};
    if degrees.abs() < 0.005 {
        return img.clone();
    }
    let theta = degrees.to_radians();
    match img {
        DynamicImage::ImageLuma8(g) => {
            let mut out = rotate_about_center(g, theta, Interpolation::Bilinear, Border::Constant(image::Luma([255])));
            if bilevel {
                out.pixels_mut().for_each(|p| p.0[0] = if p.0[0] < 128 { 0 } else { 255 });
            }
            DynamicImage::ImageLuma8(out)
        }
        other => DynamicImage::ImageRgb8(rotate_about_center(
            &other.to_rgb8(),
            theta,
            Interpolation::Bilinear,
            Border::Constant(image::Rgb([255, 255, 255])),
        )),
    }
}

/// Changes the user made to one page in the preview.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PageEdit {
    /// Rotation in degrees, clockwise; applied before the crop.
    pub rotation: f32,
    /// Manual crop as normalised `[x0, y0, x1, y1]` (0…1) of the rotated page.
    pub crop: Option<[f32; 4]>,
}

/// How the scanned pages are split into output files: one entry per file,
/// each holding the page indices that go into it.
pub fn output_groups(doc: &ScannedDocument, settings: &ScanSettings, deleted: &HashSet<usize>) -> Vec<Vec<usize>> {
    let docs = doc.documents(settings.skip_blank, deleted);
    match settings.format {
        OutputFormat::Pdf if settings.combine_pdf => {
            let all = docs.concat();
            if all.is_empty() { Vec::new() } else { vec![all] }
        }
        OutputFormat::Pdf => docs,
        _ => docs.concat().into_iter().map(|i| vec![i]).collect(),
    }
}

/// Files written for saving, plus notes for the user.
pub struct BuiltOutput {
    pub files: Vec<PathBuf>,
    pub notes: Vec<String>,
}

// ---- tesseract ------------------------------------------------------------

pub fn tesseract_bin() -> Option<PathBuf> {
    which::which("tesseract").ok().or_else(|| {
        // Apps started from Finder don't inherit the shell PATH.
        ["/opt/homebrew/bin/tesseract", "/usr/local/bin/tesseract", "/usr/bin/tesseract"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.exists())
    })
}

fn tesseract_cmd(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    // A stale TESSDATA_PREFIX pointing to a missing folder breaks tesseract.
    if let Some(prefix) = std::env::var_os("TESSDATA_PREFIX") {
        if !Path::new(&prefix).is_dir() {
            cmd.env_remove("TESSDATA_PREFIX");
        }
    }
    cmd
}

pub fn tesseract_languages() -> Vec<String> {
    let Some(bin) = tesseract_bin() else { return Vec::new() };
    let Ok(out) = tesseract_cmd(&bin).arg("--list-langs").output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip_while(|l| !l.starts_with("List of available languages"))
        .skip(1)
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Keeps only installed languages from a "deu+eng" style list.
fn usable_languages(requested: &str, installed: &[String]) -> String {
    let langs: Vec<&str> = requested
        .split(['+', ',', ' '])
        .map(str::trim)
        .filter(|l| !l.is_empty() && installed.iter().any(|i| i == l))
        .collect();
    if langs.is_empty() {
        if installed.iter().any(|i| i == "eng") {
            "eng".into()
        } else {
            installed.first().cloned().unwrap_or_default()
        }
    } else {
        langs.join("+")
    }
}

// ---- encoding ---------------------------------------------------------------

fn encode_jpeg(img: &DynamicImage, quality: u8, dpi: u32) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut enc = JpegEncoder::new_with_quality(&mut buf, quality);
    enc.set_pixel_density(PixelDensity::dpi(dpi.min(u16::MAX as u32) as u16));
    match img {
        DynamicImage::ImageLuma8(g) => enc.encode(g.as_raw(), g.width(), g.height(), ExtendedColorType::L8)?,
        other => {
            let rgb = other.to_rgb8();
            enc.encode(rgb.as_raw(), rgb.width(), rgb.height(), ExtendedColorType::Rgb8)?
        }
    }
    Ok(buf)
}

/// Writes a PDF with one full-page JPEG image per page (no text layer).
pub fn write_image_pdf(path: &Path, pages: &[(Vec<u8>, u32, u32, bool)], dpi: u32) -> Result<()> {
    // pages: (jpeg bytes, width px, height px, is_gray)
    let mut out: Vec<u8> = Vec::new();
    let mut offsets: Vec<usize> = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");

    let n = pages.len();
    // Object numbering: 1 catalog, 2 pages, 3 info, then per page: page, contents, image.
    let page_obj = |i: usize| 4 + i * 3;
    let obj = |out: &mut Vec<u8>, offsets: &mut Vec<usize>, body: &[u8]| {
        offsets.push(out.len());
        let num = offsets.len();
        out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    };

    obj(&mut out, &mut offsets, b"<< /Type /Catalog /Pages 2 0 R >>");
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", page_obj(i))).collect();
    obj(
        &mut out,
        &mut offsets,
        format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")).as_bytes(),
    );
    obj(
        &mut out,
        &mut offsets,
        b"<< /Producer (ScanDesk) /Creator (ScanDesk) >>",
    );
    for (i, (jpeg, w, h, gray)) in pages.iter().enumerate() {
        let pw = *w as f32 * 72.0 / dpi as f32;
        let ph = *h as f32 * 72.0 / dpi as f32;
        let p = page_obj(i);
        obj(
            &mut out,
            &mut offsets,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {pw:.2} {ph:.2}] /Contents {} 0 R /Resources << /XObject << /Im0 {} 0 R >> >> >>",
                p + 1,
                p + 2
            )
            .as_bytes(),
        );
        let content = format!("q {pw:.2} 0 0 {ph:.2} 0 0 cm /Im0 Do Q");
        obj(
            &mut out,
            &mut offsets,
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()).as_bytes(),
        );
        let mut img = format!(
            "<< /Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace /{} /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
            if *gray { "DeviceGray" } else { "DeviceRGB" },
            jpeg.len()
        )
        .into_bytes();
        img.extend_from_slice(jpeg);
        img.extend_from_slice(b"\nendstream");
        obj(&mut out, &mut offsets, &img);
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
    for o in &offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R /Info 3 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len() + 1
        )
        .as_bytes(),
    );
    let mut f = std::fs::File::create(path)?;
    f.write_all(&out)?;
    Ok(())
}

/// Runs tesseract on the page images and produces a searchable PDF
/// (page image + invisible OCR text layer).
fn ocr_pdf(bin: &Path, images: &[PathBuf], out_base: &Path, lang: &str, dpi: u32) -> Result<PathBuf> {
    // Must not be named like the "txt" output (document.txt), which tesseract writes.
    let list = out_base.with_extension("lst");
    let mut text = String::new();
    for p in images {
        text.push_str(&p.display().to_string());
        text.push('\n');
    }
    std::fs::write(&list, text)?;
    let out = tesseract_cmd(bin)
        .arg(&list)
        .arg(out_base)
        .args(["-l", lang, "--dpi", &dpi.to_string(), "-c", "textonly_pdf=0", "pdf", "txt"])
        .output()
        .context("failed to run tesseract")?;
    let pdf = out_base.with_extension("pdf");
    if !out.status.success() || !pdf.exists() {
        bail!("tesseract failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(pdf)
}

/// Writes the scan in the requested format into a fresh subfolder of its
/// temporary folder: one file per entry of `groups` (see [`output_groups`]).
/// Runs OCR for PDF if enabled.
///
/// `edits` holds the rotation and manual crop per page (missing = unchanged).
pub fn build(
    doc: &ScannedDocument,
    settings: &ScanSettings,
    edits: &[PageEdit],
    groups: &[Vec<usize>],
    progress: &dyn Fn(String),
) -> Result<BuiltOutput> {
    if groups.is_empty() {
        bail!("There is nothing to save – all pages are deleted or blank.");
    }
    let out_dir = tempfile::Builder::new().prefix("out-").tempdir_in(doc.temp.path())?.keep();
    let dpi = doc.resolution;
    let image = |i: usize| -> Cow<DynamicImage> {
        let img = doc.page(i, settings.auto_crop);
        let edit = edits.get(i).copied().unwrap_or_default();
        let bilevel = settings.color == crate::config::ColorMode::BlackWhite;
        let img = if edit.rotation.abs() >= 0.005 {
            Cow::Owned(rotate_degrees(img, edit.rotation, bilevel))
        } else {
            Cow::Borrowed(img)
        };
        match edit.crop {
            Some(c) => Cow::Owned(crop_normalized(&img, c)),
            None => img,
        }
    };
    let mut notes = Vec::new();
    let mut files = Vec::new();

    match settings.format {
        OutputFormat::Jpeg | OutputFormat::Png => {
            let jpeg = settings.format == OutputFormat::Jpeg;
            for (k, &i) in groups.iter().flatten().enumerate() {
                progress(format!("Encoding page {} of {}…", k + 1, groups.len()));
                let img = image(i);
                let path = out_dir.join(format!("page-{:03}.{}", k + 1, settings.format.extension()));
                if jpeg {
                    std::fs::write(&path, encode_jpeg(&img, settings.jpeg_quality, dpi)?)?;
                } else {
                    img.save(&path)?;
                }
                files.push(path);
            }
        }
        OutputFormat::Pdf => {
            let ocr = if settings.ocr {
                match tesseract_bin() {
                    Some(bin) => Some((bin, usable_languages(&settings.ocr_lang, &tesseract_languages()))),
                    None => {
                        notes.push(
                            "OCR skipped: tesseract not found (install with `brew install tesseract tesseract-lang`)".into(),
                        );
                        None
                    }
                }
            } else {
                None
            };
            let mut chars = 0usize;
            let mut ocr_failed = None;
            for (g, pages) in groups.iter().enumerate() {
                let n = g + 1;
                let of = groups.len();
                // Page images are embedded as JPEG; tesseract passes them through unchanged.
                let mut page_files = Vec::new();
                let mut jpegs = Vec::new();
                for &i in pages {
                    let img = image(i);
                    let jpeg = encode_jpeg(&img, settings.jpeg_quality, dpi)?;
                    let path = out_dir.join(format!("doc{n:03}-page{i:03}.jpg"));
                    std::fs::write(&path, &jpeg)?;
                    page_files.push(path);
                    jpegs.push((jpeg, img.width(), img.height(), matches!(img.as_ref(), DynamicImage::ImageLuma8(_))));
                }
                let base = out_dir.join(format!("document-{n:03}"));
                let target = base.with_extension("pdf");
                let mut done = false;
                if let Some((bin, lang)) = &ocr {
                    progress(if of > 1 {
                        format!("Text recognition ({lang}), document {n} of {of}…")
                    } else {
                        format!("Text recognition ({lang})…")
                    });
                    match ocr_pdf(bin, &page_files, &base, lang, dpi) {
                        Ok(pdf) => {
                            chars += std::fs::read_to_string(base.with_extension("txt"))
                                .map(|t| t.chars().filter(|c| !c.is_whitespace()).count())
                                .unwrap_or(0);
                            if pdf != target {
                                std::fs::rename(&pdf, &target)?;
                            }
                            done = true;
                        }
                        Err(e) => ocr_failed = Some(e),
                    }
                }
                if !done {
                    progress(format!("Writing PDF {n} of {of}…"));
                    write_image_pdf(&target, &jpegs, dpi)?;
                }
                files.push(target);
            }
            if let Some((_, lang)) = &ocr {
                notes.push(format!("OCR ({lang}): {chars} characters recognised"));
            }
            if let Some(e) = ocr_failed {
                notes.push(format!("OCR failed for some pages, saved without text layer: {e}"));
            }
        }
    }
    Ok(BuiltOutput { files, notes })
}

/// Moves a file, falling back to copy + delete across file systems.
pub fn move_file(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        bail!("{} already exists", to.display());
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).with_context(|| format!("cannot write {}", to.display()))?;
    std::fs::remove_file(from).ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_are_filtered() {
        let installed = vec!["deu".to_string(), "eng".to_string(), "osd".to_string()];
        assert_eq!(usable_languages("deu+eng", &installed), "deu+eng");
        assert_eq!(usable_languages("fra+eng", &installed), "eng");
        assert_eq!(usable_languages("xyz", &installed), "eng");
    }

    fn test_doc(blank: &[bool], pages_per_doc: usize) -> ScannedDocument {
        let pages = blank
            .iter()
            .map(|&b| ScannedPage {
                full: DynamicImage::ImageRgb8(image::RgbImage::from_pixel(100, 140, image::Rgb([250, 250, 250]))),
                cropped: None,
                blank: b,
            })
            .collect();
        ScannedDocument::new(pages, 75, pages_per_doc).unwrap()
    }

    #[test]
    fn groups_duplex_documents() {
        // 3 sheets, duplex: the back of sheet 2 is blank.
        let doc = test_doc(&[false, false, false, true, false, false], 2);
        assert_eq!(doc.doc_count(), 3);
        let none = HashSet::new();
        let mut s = ScanSettings { ocr: false, ..Default::default() };

        s.combine_pdf = false;
        assert_eq!(output_groups(&doc, &s, &none), vec![vec![0, 1], vec![2], vec![4, 5]]);
        s.skip_blank = false;
        assert_eq!(output_groups(&doc, &s, &none), vec![vec![0, 1], vec![2, 3], vec![4, 5]]);
        s.combine_pdf = true;
        s.skip_blank = true;
        assert_eq!(output_groups(&doc, &s, &none), vec![vec![0, 1, 2, 4, 5]]);
        s.format = OutputFormat::Jpeg;
        assert_eq!(output_groups(&doc, &s, &none).len(), 5);

        // Separate PDFs are written, one per document.
        s.format = OutputFormat::Pdf;
        s.combine_pdf = false;
        let groups = output_groups(&doc, &s, &none);
        let built = build(&doc, &s, &[], &groups, &|_| {}).unwrap();
        assert_eq!(built.files.len(), 3);
        let pdf = std::fs::read(&built.files[1]).unwrap();
        assert_eq!(String::from_utf8_lossy(&pdf).matches("/Type /Page ").count(), 1);
        let pdf = std::fs::read(&built.files[0]).unwrap();
        assert_eq!(String::from_utf8_lossy(&pdf).matches("/Type /Page ").count(), 2);
    }

    #[test]
    fn deleted_pages_are_left_out() {
        let doc = test_doc(&[false, false, false, false], 2);
        let s = ScanSettings { combine_pdf: false, ..Default::default() };
        let deleted: HashSet<usize> = [1, 2, 3].into_iter().collect();
        assert_eq!(output_groups(&doc, &s, &deleted), vec![vec![0]]);
        let deleted: HashSet<usize> = [0, 1, 2, 3].into_iter().collect();
        assert!(output_groups(&doc, &s, &deleted).is_empty());
    }

    #[test]
    fn all_blank_is_an_error() {
        let doc = test_doc(&[true, true], 2);
        let s = ScanSettings::default();
        let none = HashSet::new();
        let groups = output_groups(&doc, &s, &none);
        assert!(groups.is_empty());
        assert!(build(&doc, &s, &[], &groups, &|_| {}).is_err());
    }

    #[test]
    fn crops_normalized() {
        let img = DynamicImage::ImageRgb8(image::RgbImage::new(200, 100));
        let c = crop_normalized(&img, [0.25, 0.1, 0.75, 0.9]);
        assert_eq!((c.width(), c.height()), (100, 80));
    }

    #[test]
    fn rotation_is_clockwise_and_keeps_size() {
        // A black dot right of the centre moves down when turned clockwise.
        let mut img = image::GrayImage::from_pixel(201, 201, image::Luma([255]));
        for y in 95..106 {
            for x in 170..181 {
                img.put_pixel(x, y, image::Luma([0]));
            }
        }
        let out = rotate_degrees(&DynamicImage::ImageLuma8(img), 5.0, true).into_luma8();
        assert_eq!(out.dimensions(), (201, 201));
        let (s, c) = 5f32.to_radians().sin_cos();
        let (x, y) = (100.0 + 75.0 * c, 100.0 + 75.0 * s);
        assert_eq!(out.get_pixel(x.round() as u32, y.round() as u32).0[0], 0);
        assert!(out.pixels().all(|p| p.0[0] == 0 || p.0[0] == 255), "stays bilevel");
        assert_eq!(out.get_pixel(0, 0).0[0], 255, "corners are white");
    }

    #[test]
    fn writes_plain_pdf() {
        let dir = tempfile::tempdir().unwrap();
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(300, 400, image::Rgb([200, 10, 10])));
        let jpeg = encode_jpeg(&img, 80, 150).unwrap();
        let path = dir.path().join("t.pdf");
        write_image_pdf(&path, &[(jpeg.clone(), 300, 400, false), (jpeg, 300, 400, false)], 150).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"%PDF-1.4"));
        assert!(bytes.ends_with(b"%%EOF\n"));
    }
}
