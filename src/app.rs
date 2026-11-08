//! The egui user interface. All controls live in the left panel, in workflow order:
//! 1 scanner → 2 scan settings & scan → 3 preview → 4 output format & save.
//! The central area only shows the zoomable / pannable page preview.

use crate::config::{Config, OutputFormat, PageSize, Source};
use crate::naming;
use crate::output::{self, BuiltOutput, ScannedDocument};
use crate::processing;
use crate::scanner::{self, Capabilities, ScannerInfo};
use egui::{Color32, Rect, RichText, Sense, TextureHandle, TextureOptions, Vec2};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::Duration;

enum JobMsg {
    Progress(String),
    Scanned(Result<ScannedDocument, String>),
    Built(Result<BuiltOutput, String>),
}

#[derive(Clone, Copy, PartialEq)]
enum MsgKind {
    Info,
    Error,
}

struct View {
    zoom: f32,
    /// Offset of the image centre from the viewport centre, in screen points.
    pan: Vec2,
    fit: bool,
}

impl Default for View {
    fn default() -> Self {
        Self { zoom: 1.0, pan: Vec2::ZERO, fit: true }
    }
}

struct PageTexture {
    handle: TextureHandle,
    /// Size of the full-resolution page in pixels.
    size: Vec2,
}

/// The collapsible groups of the left panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Scanner = 1,
    Scan = 2,
    Preview = 3,
    Save = 4,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Section::Scanner => "Scanner",
            Section::Scan => "Scan",
            Section::Preview => "Preview",
            Section::Save => "Save",
        }
    }
}

const SECTION_GAP: f32 = 14.0;
const MAX_ROTATION: f32 = 5.0;
const GRID_SPACING: f32 = 40.0;
const WARN: Color32 = Color32::from_rgb(214, 134, 20);

pub struct ScanApp {
    config: Config,
    saved_config: Config,

    scanners: Vec<ScannerInfo>,
    discovery: Option<Receiver<Vec<ScannerInfo>>>,
    caps: Capabilities,
    caps_loaded_for: Option<String>,
    caps_rx: Option<Receiver<(String, Result<Capabilities, String>)>>,
    ocr_available: bool,

    job: Option<Receiver<JobMsg>>,
    status: String,

    doc: Option<Arc<ScannedDocument>>,
    /// Preview textures keyed by (page, cropped).
    textures: HashMap<(usize, bool), PageTexture>,
    page: usize,
    view: View,
    zoom_request: Option<f32>,
    /// Manual crop per (page, uses auto-cropped image), normalised 0…1.
    manual_crops: HashMap<(usize, bool), Rect>,
    crop_mode: bool,
    /// The expanded group in the left panel (at most one).
    open_section: Option<Section>,
    /// Rotation in degrees (clockwise) per (page, uses auto-cropped image).
    rotations: HashMap<(usize, bool), f32>,
    /// Pages the user deleted from the scan, in deletion order (for undo).
    deleted: Vec<usize>,
    name: String,

    message: Option<(String, MsgKind)>,
    last_saved: Option<PathBuf>,
    manual_url: String,
    /// Write config changes to disk (off in tests).
    persist: bool,
}

impl ScanApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::with_config(Config::load());
        app.start_discovery(&cc.egui_ctx);
        app
    }

    fn with_config(config: Config) -> Self {
        let open_section = Some(if config.scanner_id.is_some() { Section::Scan } else { Section::Scanner });
        Self {
            saved_config: config.clone(),
            config,
            scanners: Vec::new(),
            discovery: None,
            caps: Capabilities::default(),
            caps_loaded_for: None,
            caps_rx: None,
            ocr_available: output::tesseract_bin().is_some(),
            job: None,
            status: String::new(),
            doc: None,
            textures: HashMap::new(),
            page: 0,
            view: View::default(),
            zoom_request: None,
            manual_crops: HashMap::new(),
            rotations: HashMap::new(),
            open_section,
            deleted: Vec::new(),
            crop_mode: false,
            name: String::new(),
            message: None,
            last_saved: None,
            manual_url: String::new(),
            persist: true,
        }
    }

    // ---- background work ------------------------------------------------------

    fn start_discovery(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        let manual = self.config.manual_scanners.clone();
        std::thread::spawn(move || {
            let found = scanner::discover(Duration::from_secs(3), &manual);
            let _ = tx.send(found);
            ctx.request_repaint();
        });
        self.discovery = Some(rx);
    }

    fn selected_scanner(&self) -> Option<&ScannerInfo> {
        let id = self.config.scanner_id.as_ref()?;
        self.scanners.iter().find(|s| &s.id == id)
    }

    fn load_capabilities(&mut self, ctx: &egui::Context) {
        let Some(scanner) = self.selected_scanner().cloned() else { return };
        if self.caps_loaded_for.as_ref() == Some(&scanner.id) {
            return;
        }
        self.caps_loaded_for = Some(scanner.id.clone());
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let caps = scanner::capabilities(&scanner).map_err(|e| format!("{e:#}"));
            let _ = tx.send((scanner.id, caps));
            ctx.request_repaint();
        });
        self.caps_rx = Some(rx);
    }

    /// Makes the stored settings valid for the current scanner.
    fn fit_settings_to_caps(&mut self) {
        let s = &mut self.config.settings;
        if !self.caps.sources.contains(&s.source) {
            s.source = self.caps.sources[0];
        }
        if !self.caps.resolutions.contains(&s.resolution) {
            let want = s.resolution as i64;
            if let Some(r) = self.caps.resolutions.iter().min_by_key(|r| (**r as i64 - want).abs()) {
                s.resolution = *r;
            }
        }
        if !self.caps.colors.contains(&s.color) {
            s.color = self.caps.colors[0];
        }
    }

    fn poll_background(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.discovery {
            if let Ok(found) = rx.try_recv() {
                self.discovery = None;
                self.scanners = found;
                if self.selected_scanner().is_none() {
                    self.config.scanner_id = self.scanners.first().map(|s| s.id.clone());
                }
                self.caps_loaded_for = None;
                self.load_capabilities(ctx);
            }
        }
        if let Some(rx) = &self.caps_rx {
            if let Ok((id, caps)) = rx.try_recv() {
                self.caps_rx = None;
                if Some(&id) == self.config.scanner_id.as_ref() {
                    match caps {
                        Ok(c) => {
                            self.caps = c;
                            self.fit_settings_to_caps();
                        }
                        Err(e) => {
                            self.caps = Capabilities::default();
                            self.message = Some((format!("Could not read scanner options: {e}"), MsgKind::Error));
                        }
                    }
                }
            }
        }
        let mut finished = None;
        if let Some(rx) = &self.job {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    JobMsg::Progress(p) => self.status = p,
                    other => finished = Some(other),
                }
            }
        }
        match finished {
            Some(JobMsg::Scanned(r)) => {
                self.job = None;
                self.status.clear();
                match r {
                    Ok(doc) => self.show_document(doc),
                    Err(e) => self.message = Some((e, MsgKind::Error)),
                }
            }
            Some(JobMsg::Built(r)) => {
                self.job = None;
                self.status.clear();
                match r {
                    Ok(built) => self.finish_save(built),
                    Err(e) => self.message = Some((format!("Saving failed: {e}"), MsgKind::Error)),
                }
            }
            _ => {}
        }
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        let Some(scanner) = self.selected_scanner().cloned() else {
            self.message = Some(("No scanner selected.".into(), MsgKind::Error));
            return;
        };
        self.discard_document();
        self.message = None;
        let settings = self.config.settings.clone();
        let caps = self.caps.clone();
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        self.status = "Starting scan…".into();
        std::thread::spawn(move || {
            let progress = |s: String| {
                let _ = tx.send(JobMsg::Progress(s));
                ctx.request_repaint();
            };
            let run = || -> anyhow::Result<ScannedDocument> {
                let raw = scanner::scan(&scanner, &settings, &caps, &progress)?;
                let total = raw.len();
                let mut pages = Vec::with_capacity(total);
                for (i, img) in raw.into_iter().enumerate() {
                    progress(format!("Detecting page edges ({} of {total})…", i + 1));
                    pages.push(processing::process_page(img, settings.color));
                }
                let pages_per_doc = if settings.source == Source::FeederDuplex { 2 } else { 1 };
                ScannedDocument::new(pages, settings.resolution, pages_per_doc)
            };
            let result = run().map_err(|e| format!("{e:#}"));
            let _ = tx.send(JobMsg::Scanned(result));
            ctx.request_repaint();
        });
        self.job = Some(rx);
    }

    fn show_document(&mut self, doc: ScannedDocument) {
        self.textures.clear();
        self.manual_crops.clear();
        self.rotations.clear();
        self.deleted.clear();
        self.crop_mode = false;
        self.open_section = Some(Section::Preview);
        self.page = 0;
        self.view = View::default();
        self.name = if self.config.last_name.trim().is_empty() {
            "scan".to_string()
        } else {
            naming::next_name(&self.config.last_name)
        };
        let n = doc.pages.len();
        let docs = doc.doc_count();
        let edges = doc.edges_found();
        let blank = doc.blank_count();
        let plural = |k: usize, word: &str| format!("{k} {word}{}", if k == 1 { "" } else { "s" });
        let mut text = if docs > 1 {
            format!("Scanned {} ({}).", plural(docs, "document"), plural(n, "page"))
        } else {
            format!("Scanned {}.", plural(n, "page"))
        };
        if blank > 0 {
            text.push_str(&format!(" {} look blank.", plural(blank, "page")));
        }
        text.push_str(&format!(" Page edges detected on {edges} of {n}."));
        self.message = Some((text, MsgKind::Info));
        self.doc = Some(Arc::new(doc));
    }

    fn discard_document(&mut self) {
        self.doc = None; // drops the temporary folder
        if matches!(self.open_section, Some(Section::Preview | Section::Save)) {
            self.open_section = Some(Section::Scan);
        }
        self.textures.clear();
        self.manual_crops.clear();
        self.rotations.clear();
        self.deleted.clear();
        self.crop_mode = false;
    }

    /// Key of the image shown for page `i`: (page, uses the auto-cropped variant).
    fn page_key(&self, i: usize) -> (usize, bool) {
        let auto = self.config.settings.auto_crop
            && self.doc.as_ref().is_some_and(|d| d.pages[i].cropped.is_some());
        (i, auto)
    }

    fn manual_crop(&self, i: usize) -> Rect {
        self.manual_crops
            .get(&self.page_key(i))
            .copied()
            .unwrap_or(Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)))
    }

    fn rotation(&self, i: usize) -> f32 {
        self.rotations.get(&self.page_key(i)).copied().unwrap_or(0.0)
    }

    fn set_rotation(&mut self, degrees: f32) {
        let key = self.page_key(self.page);
        let degrees = ((degrees.clamp(-MAX_ROTATION, MAX_ROTATION)) * 100.0).round() / 100.0;
        if degrees == 0.0 {
            self.rotations.remove(&key);
        } else {
            self.rotations.insert(key, degrees);
        }
    }

    fn deleted_set(&self) -> HashSet<usize> {
        self.deleted.iter().copied().collect()
    }

    /// Documents (sheets) with the pages that are still there.
    fn visible_docs(&self) -> Vec<Vec<usize>> {
        match &self.doc {
            Some(doc) => doc.documents(false, &self.deleted_set()),
            None => Vec::new(),
        }
    }

    /// Position of the current page: (document index, index within it).
    fn locate(&self, docs: &[Vec<usize>]) -> Option<(usize, usize)> {
        docs.iter()
            .enumerate()
            .find_map(|(d, pages)| pages.iter().position(|&p| p == self.page).map(|i| (d, i)))
    }

    /// Shows another page and fits it into the window.
    fn go_to_page(&mut self, page: usize) {
        if page != self.page {
            self.page = page;
            self.view.fit = true;
        }
    }

    /// Flips to the previous / next document, keeping the side (front / back).
    fn flip_document(&mut self, delta: i64) {
        let docs = self.visible_docs();
        let Some((cur, _)) = self.locate(&docs) else { return };
        let ppd = self.doc.as_ref().map_or(1, |d| d.pages_per_doc);
        let side = self.page % ppd;
        let target = (cur as i64 + delta).clamp(0, docs.len() as i64 - 1) as usize;
        let pages = &docs[target];
        let page = pages.iter().copied().find(|p| p % ppd == side).unwrap_or(pages[0]);
        self.go_to_page(page);
    }

    /// Shows the front (0) or back (1) of the current sheet, if it is still there.
    fn set_side(&mut self, side: usize) {
        let docs = self.visible_docs();
        let Some((cur, _)) = self.locate(&docs) else { return };
        let ppd = self.doc.as_ref().map_or(1, |d| d.pages_per_doc);
        if let Some(p) = docs[cur].iter().copied().find(|p| p % ppd == side) {
            self.go_to_page(p);
        }
    }

    /// Deletes the page currently shown and moves on to a neighbouring page.
    fn delete_current_page(&mut self) {
        let before = self.visible_docs();
        if self.locate(&before).is_none() {
            return;
        }
        let order: Vec<usize> = before.concat();
        let pos = order.iter().position(|&p| p == self.page).unwrap_or(0);
        self.deleted.push(self.page);
        self.crop_mode = false;
        // Prefer the next page, otherwise the previous one.
        if let Some(&next) = order.get(pos + 1).or_else(|| pos.checked_sub(1).and_then(|i| order.get(i))) {
            self.go_to_page(next);
        }
    }

    fn undo_delete(&mut self) {
        if let Some(page) = self.deleted.pop() {
            self.go_to_page(page);
        }
    }

    /// Final file paths for `files` output files, following the naming rules:
    /// separate PDFs continue the numbering scheme (doc, doc2, doc3 …),
    /// several images of one scan get page suffixes (doc-p1, doc-p2 …).
    /// Also returns the name to remember for the next proposal.
    fn plan_targets(&self, dir: &Path, files: usize) -> (Vec<PathBuf>, String) {
        let name = naming::sanitize(&self.name);
        let ext = self.config.settings.format.extension();
        if self.config.settings.format == OutputFormat::Pdf && files > 1 {
            let names = naming::sequence_names(dir, &name, ext, files);
            let last = names.last().cloned().unwrap_or(name);
            (names.iter().map(|n| dir.join(format!("{n}.{ext}"))).collect(), last)
        } else {
            let unique = naming::unique_name(dir, &name, ext, files);
            (naming::target_paths(dir, &unique, ext, files), unique)
        }
    }

    fn choose_folder(&mut self) -> bool {
        let mut dialog = rfd::FileDialog::new().set_title("Choose target folder");
        if let Some(dir) = &self.config.target_dir {
            dialog = dialog.set_directory(dir);
        }
        match dialog.pick_folder() {
            Some(dir) => {
                self.config.target_dir = Some(dir);
                true
            }
            None => false,
        }
    }

    /// Starts writing the document in the chosen format (OCR may take a while).
    fn start_save(&mut self, ctx: &egui::Context) {
        if self.config.target_dir.as_ref().is_none_or(|d| !d.is_dir()) && !self.choose_folder() {
            return;
        }
        if naming::sanitize(&self.name).is_empty() {
            self.message = Some(("Please enter a document name.".into(), MsgKind::Error));
            return;
        }
        let Some(doc) = self.doc.clone() else { return };
        self.crop_mode = false;
        let settings = self.config.settings.clone();
        let edits: Vec<output::PageEdit> = (0..doc.pages.len())
            .map(|i| output::PageEdit {
                rotation: self.rotation(i),
                crop: self.manual_crops.get(&self.page_key(i)).map(|r| [r.min.x, r.min.y, r.max.x, r.max.y]),
            })
            .collect();
        let groups = output::output_groups(&doc, &settings, &self.deleted_set());
        if groups.is_empty() {
            self.message = Some(("Nothing to save – all pages are deleted or blank.".into(), MsgKind::Error));
            return;
        }
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        self.status = "Preparing document…".into();
        self.message = None;
        std::thread::spawn(move || {
            let progress = |s: String| {
                let _ = tx.send(JobMsg::Progress(s));
                ctx.request_repaint();
            };
            let result = output::build(&doc, &settings, &edits, &groups, &progress).map_err(|e| format!("{e:#}"));
            let _ = tx.send(JobMsg::Built(result));
            ctx.request_repaint();
        });
        self.job = Some(rx);
    }

    /// Moves the written files from the temporary folder to the target folder.
    fn finish_save(&mut self, built: BuiltOutput) {
        let Some(dir) = self.config.target_dir.clone() else { return };
        let (targets, final_name) = self.plan_targets(&dir, built.files.len());
        for (from, to) in built.files.iter().zip(&targets) {
            if let Err(e) = output::move_file(from, to) {
                self.message = Some((format!("Saving failed: {e:#}"), MsgKind::Error));
                return;
            }
        }
        self.config.last_name = final_name;
        self.last_saved = targets.first().cloned();
        let shown = if targets.len() == 1 {
            targets[0].display().to_string()
        } else {
            format!("{} files in {}", targets.len(), dir.display())
        };
        let mut text = format!("Saved {shown}");
        for n in &built.notes {
            text.push_str("\n");
            text.push_str(n);
        }
        self.message = Some((text, MsgKind::Info));
        self.discard_document();
    }

    // ---- left panel -----------------------------------------------------------------

    /// One-line summary shown next to a section title.
    fn section_summary(&self, section: Section) -> String {
        match section {
            Section::Scanner => self
                .selected_scanner()
                .map(|s| s.name.clone())
                .unwrap_or_else(|| "none selected".into()),
            Section::Scan => {
                let s = &self.config.settings;
                format!("{} · {} dpi · {}", s.source.label(), s.resolution, s.color.label())
            }
            Section::Preview => match &self.doc {
                Some(doc) => {
                    let n = doc.pages.len() - self.deleted.len();
                    format!("{n} page{}", if n == 1 { "" } else { "s" })
                }
                None => "after scanning".into(),
            },
            Section::Save => match &self.doc {
                Some(_) => {
                    let name = naming::sanitize(&self.name);
                    format!("{} · {name}", self.config.settings.format.extension().to_uppercase())
                }
                None => "after scanning".into(),
            },
        }
    }

    /// A collapsible section; only `self.open_section` is expanded.
    fn section(&mut self, ui: &mut egui::Ui, section: Section, add_body: impl FnOnce(&mut Self, &mut egui::Ui)) {
        let open = self.open_section == Some(section);
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::click());
        if resp.clicked() {
            self.open_section = if open { None } else { Some(section) };
        }
        let visuals = ui.style().interact_selectable(&resp, open);
        let bg = if resp.hovered() || open { visuals.weak_bg_fill } else { Color32::TRANSPARENT };
        ui.painter().rect_filled(rect, 6.0, bg);
        let openness = ui.ctx().animate_bool(resp.id, open);
        let arrow_center = egui::pos2(rect.left() + 14.0, rect.center().y);
        egui::collapsing_header::paint_default_icon(
            ui,
            openness,
            &resp.clone().with_new_rect(Rect::from_center_size(arrow_center, Vec2::splat(10.0))),
        );
        let title = egui::WidgetText::from(RichText::new(format!("{}  {}", section as u8, section.title())).heading().strong())
            .into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Heading);
        let title_pos = egui::pos2(rect.left() + 28.0, rect.center().y - title.size().y / 2.0);
        let title_right = title_pos.x + title.size().x;
        ui.painter().galley(title_pos, title, ui.visuals().strong_text_color());
        if !open {
            let max = (rect.right() - 8.0 - (title_right + 14.0)).max(0.0);
            let summary = egui::WidgetText::from(RichText::new(self.section_summary(section)).weak())
                .into_galley(ui, Some(egui::TextWrapMode::Truncate), max, egui::TextStyle::Body);
            let pos = egui::pos2(rect.right() - 8.0 - summary.size().x, rect.center().y - summary.size().y / 2.0);
            ui.painter().galley(pos, summary, ui.visuals().weak_text_color());
        }
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), resp.id.with("body"), open);
        state.set_open(open);
        state.show_body_unindented(ui, |ui| {
            ui.add_space(4.0);
            egui::Frame::new().inner_margin(egui::Margin { left: 8, right: 0, top: 0, bottom: 6 }).show(ui, |ui| {
                add_body(self, ui);
            });
        });
        ui.add_space(2.0);
    }

    fn scanner_section(&mut self, ui: &mut egui::Ui, busy: bool) {
        let selected = self.selected_scanner().map(|s| s.name.clone()).unwrap_or_else(|| {
            if self.discovery.is_some() { "Searching…".into() } else { "No scanner found".into() }
        });
        let mut new_id = self.config.scanner_id.clone();
        let mut refresh = false;
        // Right-to-left: the refresh button is placed first, the combo box gets
        // exactly the remaining width (never more than the panel has).
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.discovery.is_some() {
                    ui.spinner();
                } else if ui
                    .add_enabled(!busy, egui::Button::new("⟳"))
                    .on_hover_text("Search for scanners again")
                    .clicked()
                {
                    refresh = true;
                }
                ui.add_enabled_ui(!busy, |ui| {
                    egui::ComboBox::from_id_salt("scanner")
                        .width(ui.available_width())
                        .truncate()
                        .selected_text(selected)
                        .show_ui(ui, |ui| {
                            for s in &self.scanners {
                                ui.selectable_value(&mut new_id, Some(s.id.clone()), format!("{} ({})", s.name, s.kind()));
                            }
                        });
                });
            });
        });
        if new_id != self.config.scanner_id {
            self.config.scanner_id = new_id;
            self.load_capabilities(ui.ctx());
        }
        if refresh {
            self.start_discovery(ui.ctx());
        }
        if self.caps_rx.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Reading scanner options…");
            });
        }
        egui::CollapsingHeader::new("Add network scanner by address")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.manual_url).hint_text("192.168.1.20:8080").desired_width(170.0));
                    if ui.button("Add").clicked() && !self.manual_url.trim().is_empty() {
                        let url = scanner::escl::normalize_base(&self.manual_url);
                        if !self.config.manual_scanners.contains(&url) {
                            self.config.manual_scanners.push(url);
                        }
                        self.manual_url.clear();
                        self.start_discovery(ui.ctx());
                    }
                });
                ui.small("For network (eSCL / AirScan) scanners that are not found automatically.");
            });
    }

    fn scan_section(&mut self, ui: &mut egui::Ui, busy: bool) {
        ui.add_enabled_ui(!busy, |ui| {
            let s = &mut self.config.settings;
            egui::Grid::new("scan_settings").num_columns(2).spacing([12.0, 12.0]).show(ui, |ui| {
                ui.label("Source");
                egui::ComboBox::from_id_salt("source").selected_text(s.source.label()).show_ui(ui, |ui| {
                    for src in &self.caps.sources {
                        ui.selectable_value(&mut s.source, *src, src.label());
                    }
                });
                ui.end_row();

                ui.label("Resolution");
                egui::ComboBox::from_id_salt("resolution")
                    .selected_text(format!("{} dpi", s.resolution))
                    .show_ui(ui, |ui| {
                        for r in &self.caps.resolutions {
                            ui.selectable_value(&mut s.resolution, *r, format!("{r} dpi"));
                        }
                    });
                ui.end_row();

                ui.label("Color");
                egui::ComboBox::from_id_salt("color").selected_text(s.color.label()).show_ui(ui, |ui| {
                    for c in &self.caps.colors {
                        ui.selectable_value(&mut s.color, *c, c.label());
                    }
                });
                ui.end_row();

                ui.label("Page size");
                egui::ComboBox::from_id_salt("page").selected_text(s.page_size.label()).show_ui(ui, |ui| {
                    for p in PageSize::ALL {
                        ui.selectable_value(&mut s.page_size, p, p.label());
                    }
                });
                ui.end_row();
            });
        });
        if self.config.settings.source.is_feeder() {
            ui.small("All pages in the feeder are scanned into one document.");
        }
        ui.add_space(8.0);
        let can_scan = !busy && self.selected_scanner().is_some() && self.caps_rx.is_none();
        let label = if self.doc.is_some() { "Scan again" } else { "Scan" };
        let button = egui::Button::new(RichText::new(format!("📄  {label}")).size(17.0))
            .min_size(Vec2::new(ui.available_width(), 38.0));
        if ui.add_enabled(can_scan, button).clicked() {
            self.start_scan(ui.ctx());
        }
    }

    fn preview_section(&mut self, ui: &mut egui::Ui, busy: bool) {
        let Some(doc) = self.doc.clone() else {
            ui.weak("Available after scanning.");
            return;
        };
        let ppd = doc.pages_per_doc;
        let visible = self.visible_docs();
        let docs = visible.len();
        let Some((cur, _)) = self.locate(&visible) else {
            ui.colored_label(WARN, "All pages are deleted.");
            if ui.add_enabled(!busy, egui::Button::new("↶  Undo delete")).clicked() {
                self.undo_delete();
            }
            return;
        };
        let side = self.page % ppd;
        if docs > 1 {
            ui.horizontal(|ui| {
                if ui.add_enabled(cur > 0, egui::Button::new("‹ Prev")).on_hover_text("Previous document (←)").clicked() {
                    self.flip_document(-1);
                }
                ui.label(RichText::new(format!("Document {} of {docs}", cur + 1)).strong());
                if ui.add_enabled(cur + 1 < docs, egui::Button::new("Next ›")).on_hover_text("Next document (→)").clicked() {
                    self.flip_document(1);
                }
            });
        }
        if ppd == 2 {
            let has = |s: usize| visible[cur].iter().any(|p| p % 2 == s);
            ui.horizontal(|ui| {
                ui.label("Side");
                if ui.add_enabled(has(0), egui::Button::selectable(side == 0, "Front")).on_hover_text("↑").clicked() {
                    self.set_side(0);
                }
                if ui.add_enabled(has(1), egui::Button::selectable(side == 1, "Back")).on_hover_text("↓").clicked() {
                    self.set_side(1);
                }
            });
        }
        if doc.pages[self.page].blank {
            let note = if self.config.settings.skip_blank {
                "This page looks blank – it will be left out when saving."
            } else {
                "This page looks blank."
            };
            ui.colored_label(WARN, note);
        }
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                if ui.button("🗑  Delete page").on_hover_text("Remove this page from the scan (Delete key)").clicked() {
                    self.delete_current_page();
                }
                if ui
                    .add_enabled(!self.deleted.is_empty(), egui::Button::new("↶  Undo delete"))
                    .on_hover_text("Bring back the last deleted page")
                    .clicked()
                {
                    self.undo_delete();
                }
            });
        });
        if !self.deleted.is_empty() {
            let n = self.deleted.len();
            ui.small(format!("{n} page{} deleted – not saved.", if n == 1 { "" } else { "s" }));
        }
        ui.horizontal(|ui| {
            if ui.button(" − ").on_hover_text("Zoom out (−)").clicked() {
                self.zoom_request = Some(1.0 / 1.25);
            }
            ui.label(format!("{:.0} %", self.view.zoom * 100.0));
            if ui.button(" + ").on_hover_text("Zoom in (+)").clicked() {
                self.zoom_request = Some(1.25);
            }
            if ui.button("Fit").on_hover_text("Fit page in window (0)").clicked() {
                self.view.fit = true;
            }
            if ui.button("100 %").on_hover_text("Actual pixels (1)").clicked() {
                self.zoom_request = Some(1.0 / self.view.zoom);
            }
        });
        ui.small("Drag to pan · pinch or ⌘/Ctrl + scroll to zoom · double-click toggles fit");

        ui.add_space(4.0);
        let found = doc.edges_found();
        let pages = doc.pages.len();
        ui.add_enabled_ui(!busy && found > 0, |ui| {
            let changed = ui
                .checkbox(&mut self.config.settings.auto_crop, "Crop to detected page edges")
                .on_hover_text("Straightens the page and removes the scanner background.")
                .changed();
            if changed {
                self.view.fit = true;
            }
        });
        if found == 0 {
            ui.small("No page edges detected – the full scan area is used.");
        } else if found < pages {
            ui.small(format!("Edges detected on {found} of {pages} pages."));
        }

        ui.add_space(4.0);
        let key = self.page_key(self.page);
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                if self.crop_mode {
                    if ui.button("✔  Done cropping").clicked() {
                        self.crop_mode = false;
                        self.view.fit = true;
                    }
                } else if ui.button("✂  Crop…").on_hover_text("Cut away parts of this page").clicked() {
                    self.crop_mode = true;
                    self.view.fit = true;
                }
                let has_crop = self.manual_crops.contains_key(&key);
                if ui.add_enabled(has_crop, egui::Button::new("Reset crop")).clicked() {
                    self.manual_crops.remove(&key);
                    self.view.fit = true;
                }
            });
        });

        ui.add_space(4.0);
        ui.add_enabled_ui(!busy, |ui| {
            let mut rot = self.rotation(self.page);
            ui.horizontal(|ui| {
                ui.label("Rotate");
                ui.spacing_mut().slider_width = 150.0;
                let slider = egui::Slider::new(&mut rot, -MAX_ROTATION..=MAX_ROTATION)
                    .step_by(0.05)
                    .fixed_decimals(2)
                    .suffix("°")
                    .clamping(egui::SliderClamping::Always);
                if ui.add(slider).on_hover_text("Straighten the page; [ and ] turn by 0.1°").changed() {
                    self.set_rotation(rot);
                }
            });
            ui.horizontal(|ui| {
                if ui.button("⟲ 0.1°").on_hover_text("Turn counter-clockwise ([)").clicked() {
                    self.set_rotation(rot - 0.1);
                }
                if ui.button("⟳ 0.1°").on_hover_text("Turn clockwise (])").clicked() {
                    self.set_rotation(rot + 0.1);
                }
                if ui.add_enabled(rot != 0.0, egui::Button::new("Reset")).clicked() {
                    self.set_rotation(0.0);
                }
            });
            if ui
                .checkbox(&mut self.config.show_grid, "Show grid")
                .on_hover_text("Overlay a grid to check that lines of text run straight (G)")
                .changed()
            {
                ui.ctx().request_repaint();
            }
        });

        if self.crop_mode {
            ui.small("Drag the corners or edges of the frame to set the crop area. Drag inside the frame to move it.");
        }
        ui.add_space(4.0);
        if ui.add_enabled(!busy, egui::Button::new("Continue to Save  ›")).clicked() {
            self.crop_mode = false;
            self.open_section = Some(Section::Save);
        }
    }

    fn save_section(&mut self, ui: &mut egui::Ui, busy: bool) {
        let Some(doc) = self.doc.clone() else {
            ui.weak("Choose file format, folder and name after scanning.");
            return;
        };
        let mut do_save = false;
        let mut do_discard = false;
        ui.add_enabled_ui(!busy, |ui| {
            let s = &mut self.config.settings;
            egui::Grid::new("output_settings").num_columns(2).spacing([12.0, 12.0]).show(ui, |ui| {
                ui.label("Format");
                ui.horizontal(|ui| {
                    for f in OutputFormat::ALL {
                        ui.selectable_value(&mut s.format, f, f.label());
                    }
                });
                ui.end_row();

                if s.format != OutputFormat::Png {
                    ui.label("Quality");
                    ui.add(egui::Slider::new(&mut s.jpeg_quality, 40..=100).text("JPEG"));
                    ui.end_row();
                }
            });
            let docs = doc.doc_count();
            if s.format == OutputFormat::Pdf && docs > 1 {
                ui.add_space(4.0);
                ui.checkbox(&mut s.combine_pdf, format!("Combine all {docs} documents into one PDF"))
                    .on_hover_text("Off: every document (sheet) is saved as its own PDF.");
            }
            let blank = doc.blank_count();
            if blank > 0 {
                ui.checkbox(
                    &mut s.skip_blank,
                    format!("Leave out {blank} blank page{}", if blank == 1 { "" } else { "s" }),
                );
            }
            if s.format == OutputFormat::Pdf {
                ui.add_space(4.0);
                ui.checkbox(&mut s.ocr, "Text recognition (searchable PDF)");
                if s.ocr {
                    ui.horizontal(|ui| {
                        ui.label("Languages");
                        ui.add(egui::TextEdit::singleline(&mut s.ocr_lang).desired_width(110.0))
                            .on_hover_text("Tesseract language codes, e.g. deu+eng");
                    });
                    if !self.ocr_available {
                        ui.colored_label(WARN, "tesseract not found – the PDF will have no text layer");
                    }
                }
            }

            ui.add_space(10.0);
            ui.label("Folder");
            ui.horizontal(|ui| {
                let folder = self
                    .config
                    .target_dir
                    .as_ref()
                    .map(|d| d.display().to_string())
                    .unwrap_or_else(|| "(not chosen)".into());
                if ui.button("Choose…").clicked() {
                    self.choose_folder();
                }
                ui.add(egui::Label::new(RichText::new(folder).monospace()).truncate());
            });

            ui.add_space(4.0);
            ui.label("Name");
            let ext = self.config.settings.format.extension();
            let count = output::output_groups(&doc, &self.config.settings, &self.deleted_set()).len();
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(format!(".{ext}"));
                    let edit = ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(f32::INFINITY));
                    if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        do_save = true;
                    }
                });
            });
            let clean = naming::sanitize(&self.name);
            if count == 0 {
                ui.colored_label(WARN, "All pages are deleted or blank – nothing to save.");
            } else if let (Some(dir), false) = (self.config.target_dir.clone(), clean.is_empty()) {
                let (targets, _) = self.plan_targets(&dir, count);
                let file = |p: &PathBuf| p.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                let first_stem = targets[0].file_stem().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                let summary = if count == 1 {
                    format!("Saves {}", file(&targets[0]))
                } else {
                    format!("Saves {count} files: {} … {}", file(&targets[0]), file(&targets[count - 1]))
                };
                // Without a collision the first file is "name" (or "name-p1" for page images).
                let expected = if count > 1 && self.config.settings.format != OutputFormat::Pdf {
                    format!("{clean}-p1")
                } else {
                    clean.clone()
                };
                let taken = first_stem != expected;
                if taken {
                    ui.colored_label(WARN, format!("“{clean}” exists – {}", summary.to_lowercase()));
                } else {
                    ui.small(summary);
                }
            }

            ui.add_space(8.0);
            let save = egui::Button::new(RichText::new("💾  Save").size(17.0)).min_size(Vec2::new(ui.available_width(), 38.0));
            if ui.add(save).clicked() {
                do_save = true;
            }
            if ui.add(egui::Button::new("Discard scan").min_size(Vec2::new(ui.available_width(), 28.0))).clicked() {
                do_discard = true;
            }
        });
        if do_save {
            self.start_save(ui.ctx());
        }
        if do_discard {
            self.discard_document();
            self.message = Some(("Scan discarded.".into(), MsgKind::Info));
        }
    }

    fn status_section(&mut self, ui: &mut egui::Ui) {
        if self.job.is_some() {
            ui.add_space(SECTION_GAP);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(&self.status);
            });
        }
        let Some((text, kind)) = self.message.clone() else { return };
        ui.add_space(SECTION_GAP);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            let color = match kind {
                MsgKind::Info => ui.visuals().text_color(),
                MsgKind::Error => Color32::from_rgb(220, 60, 60),
            };
            ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
            ui.horizontal(|ui| {
                if kind == MsgKind::Info && self.doc.is_none() {
                    if let Some(path) = self.last_saved.clone() {
                        if ui.small_button("Show in folder").clicked() {
                            reveal(&path);
                        }
                    }
                }
                if ui.small_button("Dismiss").clicked() {
                    self.message = None;
                }
            });
        });
    }

    fn left_panel(&mut self, ui: &mut egui::Ui) {
        let busy = self.job.is_some();
        {
            let sp = ui.spacing_mut();
            sp.item_spacing = Vec2::new(10.0, 10.0);
            sp.button_padding = Vec2::new(10.0, 5.0);
            sp.interact_size.y = 26.0;
            sp.combo_width = 190.0;
        }
        ui.add_space(6.0);
        ui.spacing_mut().item_spacing.y = 4.0;
        self.section(ui, Section::Scanner, |app, ui| {
            ui.spacing_mut().item_spacing.y = 10.0;
            app.scanner_section(ui, busy)
        });
        self.section(ui, Section::Scan, |app, ui| {
            ui.spacing_mut().item_spacing.y = 10.0;
            app.scan_section(ui, busy)
        });
        self.section(ui, Section::Preview, |app, ui| {
            ui.spacing_mut().item_spacing.y = 10.0;
            app.preview_section(ui, busy)
        });
        self.section(ui, Section::Save, |app, ui| {
            ui.spacing_mut().item_spacing.y = 10.0;
            app.save_section(ui, busy)
        });
        ui.spacing_mut().item_spacing.y = 10.0;
        self.status_section(ui);
        ui.add_space(10.0);
    }

    // ---- preview ----------------------------------------------------------------

    fn texture(&mut self, ctx: &egui::Context, i: usize, crop: bool) -> Option<(egui::TextureId, Vec2)> {
        let doc = self.doc.clone()?;
        let key = (i, crop && doc.pages[i].cropped.is_some());
        if !self.textures.contains_key(&key) {
            let img = doc.page(i, key.1);
            let size = Vec2::new(img.width() as f32, img.height() as f32);
            let max_side = ctx.input(|inp| inp.max_texture_side) as u32;
            let rgba = if img.width().max(img.height()) > max_side {
                img.resize(max_side, max_side, image::imageops::FilterType::Triangle).to_rgba8()
            } else {
                img.to_rgba8()
            };
            let color = egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
            let options = TextureOptions::LINEAR.with_mipmap_mode(Some(egui::TextureFilter::Linear));
            let handle = ctx.load_texture(format!("page-{i}-{}", key.1), color, options);
            self.textures.insert(key, PageTexture { handle, size });
        }
        self.textures.get(&key).map(|t| (t.handle.id(), t.size))
    }

    fn zoom_at(&mut self, viewport: Rect, anchor: egui::Pos2, factor: f32) {
        let new_zoom = (self.view.zoom * factor).clamp(0.02, 16.0);
        let factor = new_zoom / self.view.zoom;
        let center = viewport.center() + self.view.pan;
        let new_center = anchor + (center - anchor) * factor;
        self.view.pan = new_center - viewport.center();
        self.view.zoom = new_zoom;
        self.view.fit = false;
    }

    fn preview(&mut self, ui: &mut egui::Ui) {
        if self.deleted.contains(&self.page) {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("All pages are deleted. Use “Undo delete” to bring them back.").size(16.0).weak());
            });
            return;
        }
        let ctx = ui.ctx().clone();
        let crop = self.config.settings.auto_crop;
        let Some((tex, full_size)) = self.texture(&ctx, self.page, crop) else { return };
        let key = self.page_key(self.page);
        let mut crop_rect = self.manual_crop(self.page);
        // Outside crop mode only the cropped part is shown (via texture coordinates).
        let (uv, img_size) = if self.crop_mode {
            (Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), full_size)
        } else {
            (crop_rect, full_size * crop_rect.size())
        };
        let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        ui.painter().rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);

        if self.view.fit {
            self.view.zoom = ((rect.width() - 32.0) / img_size.x).min((rect.height() - 32.0) / img_size.y).max(0.01);
            self.view.pan = Vec2::ZERO;
        }

        if resp.hovered() {
            let (zoom_delta, scroll, pointer) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta, i.pointer.hover_pos()));
            let anchor = pointer.unwrap_or(rect.center());
            if (zoom_delta - 1.0).abs() > 1e-4 {
                self.zoom_at(rect, anchor, zoom_delta);
            } else if scroll != Vec2::ZERO {
                self.view.pan += scroll;
                self.view.fit = false;
            }
            let (plus, minus, zero, one) = ui.input(|i| {
                (
                    i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals),
                    i.key_pressed(egui::Key::Minus),
                    i.key_pressed(egui::Key::Num0),
                    i.key_pressed(egui::Key::Num1),
                )
            });
            if plus {
                self.zoom_request = Some(1.25);
            }
            if minus {
                self.zoom_request = Some(1.0 / 1.25);
            }
            if zero {
                self.view.fit = true;
            }
            if one {
                self.zoom_request = Some(1.0 / self.view.zoom);
            }
            let typing = ctx.memory(|m| m.focused().is_some());
            if !typing {
                let (left, right, up, down) = ui.input(|i| {
                    (
                        i.key_pressed(egui::Key::ArrowLeft),
                        i.key_pressed(egui::Key::ArrowRight),
                        i.key_pressed(egui::Key::ArrowUp),
                        i.key_pressed(egui::Key::ArrowDown),
                    )
                });
                if left {
                    self.flip_document(-1);
                }
                if right {
                    self.flip_document(1);
                }
                if up {
                    self.set_side(0);
                }
                if down {
                    self.set_side(1);
                }
                if ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)) {
                    self.delete_current_page();
                }
                let (ccw, cw, grid) = ui.input(|i| {
                    (
                        i.key_pressed(egui::Key::OpenBracket),
                        i.key_pressed(egui::Key::CloseBracket),
                        i.key_pressed(egui::Key::G),
                    )
                });
                if ccw || cw {
                    let rot = self.rotation(self.page);
                    self.set_rotation(rot + if cw { 0.1 } else { -0.1 });
                }
                if grid {
                    self.config.show_grid = !self.config.show_grid;
                }
            }
        }
        if resp.dragged() {
            self.view.pan += resp.drag_delta();
            self.view.fit = false;
        }
        if resp.double_clicked() {
            if self.view.fit {
                let anchor = resp.interact_pointer_pos().unwrap_or(rect.center());
                self.zoom_at(rect, anchor, 1.0 / self.view.zoom);
            } else {
                self.view.fit = true;
            }
        }
        if let Some(f) = self.zoom_request.take() {
            self.zoom_at(rect, rect.center(), f);
        }
        if resp.dragged() {
            ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if resp.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::Grab);
        }

        let img_rect = Rect::from_center_size(rect.center() + self.view.pan, img_size * self.view.zoom);
        let painter = ui.painter_at(rect);
        painter.rect_filled(img_rect.translate(Vec2::new(3.0, 3.0)), 0.0, Color32::from_black_alpha(60));
        let rotation = self.rotation(self.page);
        if rotation == 0.0 {
            painter.image(tex, img_rect, uv, Color32::WHITE);
        } else {
            // Draw the whole page turned around its centre, clipped to the shown
            // area; uncovered corners are white, as in the saved file.
            let full = full_size * self.view.zoom;
            let full_min = img_rect.min - Vec2::new(uv.min.x * full.x, uv.min.y * full.y);
            let full_rect = Rect::from_min_size(full_min, full);
            let clipped = painter.with_clip_rect(img_rect.intersect(rect));
            clipped.rect_filled(img_rect, 0.0, Color32::WHITE);
            clipped.add(rotated_mesh(tex, full_rect, rotation));
        }
        if self.config.show_grid {
            draw_grid(&painter, img_rect.intersect(rect), img_rect.center());
        }

        if self.crop_mode {
            if crop_frame(ui, rect, img_rect, &mut crop_rect) {
                if crop_rect.width() > 0.999 && crop_rect.height() > 0.999 {
                    self.manual_crops.remove(&key);
                } else {
                    self.manual_crops.insert(key, crop_rect);
                }
            }
        }
    }
}

/// The page texture drawn into `rect`, turned clockwise by `degrees` around its centre.
fn rotated_mesh(tex: egui::TextureId, rect: Rect, degrees: f32) -> egui::Shape {
    let (s, c) = degrees.to_radians().sin_cos();
    let center = rect.center();
    let turn = |p: egui::Pos2| {
        let d = p - center;
        center + Vec2::new(d.x * c - d.y * s, d.x * s + d.y * c)
    };
    let mut mesh = egui::Mesh::with_texture(tex);
    let corners = [rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom()];
    let uvs = [egui::pos2(0.0, 0.0), egui::pos2(1.0, 0.0), egui::pos2(1.0, 1.0), egui::pos2(0.0, 1.0)];
    for (p, uv) in corners.into_iter().zip(uvs) {
        mesh.vertices.push(egui::epaint::Vertex { pos: turn(p), uv, color: Color32::WHITE });
    }
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    egui::Shape::mesh(mesh)
}

/// Overlays a square grid on `area`, aligned to `origin`, with every fifth
/// line drawn stronger.
fn draw_grid(painter: &egui::Painter, area: Rect, origin: egui::Pos2) {
    if !area.is_positive() {
        return;
    }
    let thin = egui::Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 140, 255, 70));
    let thick = egui::Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 140, 255, 150));
    let first = |min: f32, o: f32| ((min - o) / GRID_SPACING).ceil() as i64;
    let mut k = first(area.min.x, origin.x);
    loop {
        let x = origin.x + k as f32 * GRID_SPACING;
        if x > area.max.x {
            break;
        }
        let x = x.round() + 0.5;
        painter.line_segment([egui::pos2(x, area.min.y), egui::pos2(x, area.max.y)], if k % 5 == 0 { thick } else { thin });
        k += 1;
    }
    let mut k = first(area.min.y, origin.y);
    loop {
        let y = origin.y + k as f32 * GRID_SPACING;
        if y > area.max.y {
            break;
        }
        let y = y.round() + 0.5;
        painter.line_segment([egui::pos2(area.min.x, y), egui::pos2(area.max.x, y)], if k % 5 == 0 { thick } else { thin });
        k += 1;
    }
}

/// Draws the interactive crop frame over the page shown in `img_rect`.
/// `crop` is normalised (0…1) to the page; returns true if it was changed.
fn crop_frame(ui: &mut egui::Ui, clip: Rect, img_rect: Rect, crop: &mut Rect) -> bool {
    const MIN: f32 = 0.03;
    const HANDLE: f32 = 18.0;
    let to_screen = |p: egui::Pos2| img_rect.min + p.to_vec2() * img_rect.size();
    let frame = Rect::from_min_max(to_screen(crop.min), to_screen(crop.max));
    let id = ui.id().with("crop_frame");
    let mut changed = false;
    let mut c = *crop;

    // (position, affects left, top, right, bottom, cursor)
    use egui::CursorIcon as C;
    let handles = [
        (frame.left_top(), true, true, false, false, C::ResizeNwSe),
        (frame.right_top(), false, true, true, false, C::ResizeNeSw),
        (frame.right_bottom(), false, false, true, true, C::ResizeNwSe),
        (frame.left_bottom(), true, false, false, true, C::ResizeNeSw),
        (frame.center_top(), false, true, false, false, C::ResizeVertical),
        (frame.center_bottom(), false, false, false, true, C::ResizeVertical),
        (frame.left_center(), true, false, false, false, C::ResizeHorizontal),
        (frame.right_center(), false, false, true, false, C::ResizeHorizontal),
    ];

    // Moving the whole frame (registered first so the handles stay on top).
    let body = ui.interact(frame.shrink(HANDLE / 2.0), id.with("body"), Sense::drag());
    if body.hovered() || body.dragged() {
        ui.ctx().set_cursor_icon(C::Move);
    }
    if body.dragged() {
        let d = body.drag_delta() / img_rect.size();
        let size = c.size();
        let min = (c.min + d).clamp(egui::pos2(0.0, 0.0), egui::pos2(1.0 - size.x, 1.0 - size.y));
        c = Rect::from_min_size(min, size);
        changed = true;
    }

    for (i, (pos, l, t, r, b, cursor)) in handles.iter().enumerate() {
        let resp = ui.interact(Rect::from_center_size(*pos, Vec2::splat(HANDLE)), id.with(i), Sense::drag());
        if resp.hovered() || resp.dragged() {
            ui.ctx().set_cursor_icon(*cursor);
        }
        if resp.dragged() {
            let d = resp.drag_delta() / img_rect.size();
            if *l {
                c.min.x = (c.min.x + d.x).clamp(0.0, c.max.x - MIN);
            }
            if *t {
                c.min.y = (c.min.y + d.y).clamp(0.0, c.max.y - MIN);
            }
            if *r {
                c.max.x = (c.max.x + d.x).clamp(c.min.x + MIN, 1.0);
            }
            if *b {
                c.max.y = (c.max.y + d.y).clamp(c.min.y + MIN, 1.0);
            }
            changed = true;
        }
    }
    if changed {
        *crop = c;
    }

    // Draw: darken everything outside the frame, then the frame and its handles.
    let frame = Rect::from_min_max(to_screen(c.min), to_screen(c.max));
    let painter = ui.painter_at(clip);
    let shade = Color32::from_black_alpha(150);
    painter.rect_filled(Rect::from_min_max(img_rect.min, egui::pos2(img_rect.max.x, frame.min.y)), 0.0, shade);
    painter.rect_filled(Rect::from_min_max(egui::pos2(img_rect.min.x, frame.max.y), img_rect.max), 0.0, shade);
    painter.rect_filled(Rect::from_min_max(egui::pos2(img_rect.min.x, frame.min.y), egui::pos2(frame.min.x, frame.max.y)), 0.0, shade);
    painter.rect_filled(Rect::from_min_max(egui::pos2(frame.max.x, frame.min.y), egui::pos2(img_rect.max.x, frame.max.y)), 0.0, shade);
    painter.rect_stroke(frame, 0.0, egui::Stroke::new(2.0, Color32::WHITE), egui::StrokeKind::Middle);
    // rule-of-thirds guides
    let guide = egui::Stroke::new(1.0, Color32::from_white_alpha(70));
    for k in 1..3 {
        let x = frame.min.x + frame.width() * k as f32 / 3.0;
        let y = frame.min.y + frame.height() * k as f32 / 3.0;
        painter.line_segment([egui::pos2(x, frame.min.y), egui::pos2(x, frame.max.y)], guide);
        painter.line_segment([egui::pos2(frame.min.x, y), egui::pos2(frame.max.x, y)], guide);
    }
    for pos in [
        frame.left_top(),
        frame.right_top(),
        frame.right_bottom(),
        frame.left_bottom(),
        frame.center_top(),
        frame.center_bottom(),
        frame.left_center(),
        frame.right_center(),
    ] {
        let r = Rect::from_center_size(pos, Vec2::splat(10.0));
        painter.rect_filled(r, 2.0, Color32::WHITE);
        painter.rect_stroke(r, 2.0, egui::Stroke::new(1.0, Color32::from_gray(60)), egui::StrokeKind::Outside);
    }
    changed
}

fn reveal(path: &Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(dir) = path.parent() {
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}

impl eframe::App for ScanApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

impl ScanApp {
    /// Draws one frame; returns the width of the control panel.
    fn draw(&mut self, ui: &mut egui::Ui) -> f32 {
        let ctx = ui.ctx().clone();
        self.poll_background(&ctx);

        let panel = egui::Panel::left("controls")
            .resizable(true)
            .default_size(340.0)
            .size_range(300.0..=560.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 0)).show(ui, |ui| self.left_panel(ui));
                });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            if self.doc.is_some() {
                self.preview(ui);
            } else {
                ui.centered_and_justified(|ui| {
                    if self.job.is_some() {
                        ui.label(RichText::new(format!("⏳  {}", self.status)).size(18.0));
                    } else {
                        ui.label(RichText::new("Choose a scanner and settings on the left, then click Scan.").size(16.0).weak());
                    }
                });
            }
        });

        if self.config != self.saved_config {
            if self.persist {
                self.config.save();
            }
            self.saved_config = self.config.clone();
        }
        panel.response.rect.width()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::ScannedPage;

    fn run_frames(app: &mut ScanApp, ctx: &egui::Context, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|_| {
                let input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1200.0, 820.0))),
                    ..Default::default()
                };
                let mut width = 0.0;
                let _ = ctx.run_ui(input, |ui| width = app.draw(ui));
                width
            })
            .collect()
    }

    #[test]
    fn delete_and_undo_pages() {
        let mut app = ScanApp::with_config(Config::default());
        app.persist = false;
        let page = || ScannedPage {
            full: image::DynamicImage::ImageRgb8(image::RgbImage::new(20, 30)),
            cropped: None,
            blank: false,
        };
        // 2 sheets, duplex: pages 0/1 and 2/3.
        app.show_document(ScannedDocument::new((0..4).map(|_| page()).collect(), 75, 2).unwrap());
        assert_eq!(app.visible_docs(), vec![vec![0, 1], vec![2, 3]]);

        app.delete_current_page(); // deletes the front of sheet 1
        assert_eq!(app.page, 1);
        assert_eq!(app.visible_docs(), vec![vec![1], vec![2, 3]]);
        app.flip_document(1);
        assert_eq!(app.page, 3, "keeps the back side");
        app.set_side(0);
        assert_eq!(app.page, 2);

        app.delete_current_page();
        app.delete_current_page();
        assert_eq!(app.page, 1, "falls back to the previous page");
        app.delete_current_page();
        assert!(app.visible_docs().is_empty());

        app.undo_delete();
        assert_eq!(app.page, 1);
        assert_eq!(app.visible_docs(), vec![vec![1]]);
    }

    #[test]
    fn panel_width_is_stable() {
        let mut app = ScanApp::with_config(Config::default());
        app.persist = false;
        app.scanners = vec![ScannerInfo {
            id: "x".into(),
            name: "A scanner with a really very extremely long model name for testing".into(),
            backend: scanner::Backend::Escl { base: "http://localhost/eSCL".into() },
        }];
        app.config.scanner_id = Some("x".into());
        app.config.target_dir = Some(std::env::temp_dir().join("a/very/long/folder/path/that/does/not/fit/into/the/panel"));
        let ctx = egui::Context::default();

        // Before scanning.
        let w = run_frames(&mut app, &ctx, 30);
        assert!(w.windows(2).all(|p| (p[0] - p[1]).abs() < 0.5), "width changed: {w:?}");

        // After scanning, with a long message and a multi-page document.
        let page = || ScannedPage {
            full: image::DynamicImage::ImageRgb8(image::RgbImage::new(200, 300)),
            cropped: None,
            blank: false,
        };
        let doc = ScannedDocument::new(vec![page(), page()], 150, 1).unwrap();
        app.show_document(doc);
        app.config.settings.format = OutputFormat::Jpeg;
        app.name = "a_long_document_name_that_goes_on_and_on_and_on".into();
        app.message = Some(("x".repeat(400), MsgKind::Error));
        let w2 = run_frames(&mut app, &ctx, 60);
        assert!(w2.windows(2).all(|p| (p[0] - p[1]).abs() < 0.5), "width changed: {w2:?}");
        assert!(w2[59] <= 560.0, "{}", w2[59]);

        // With a rotated page and the grid shown.
        app.set_rotation(-3.3);
        app.config.show_grid = true;
        let w3 = run_frames(&mut app, &ctx, 30);
        assert!(w3.iter().all(|w| (w - w2[59]).abs() < 0.5), "width changed: {w3:?}");
    }

    #[test]
    fn rotation_per_page_is_clamped() {
        let mut app = ScanApp::with_config(Config::default());
        app.persist = false;
        let page = || ScannedPage {
            full: image::DynamicImage::ImageRgb8(image::RgbImage::new(20, 30)),
            cropped: None,
            blank: false,
        };
        app.show_document(ScannedDocument::new(vec![page(), page()], 75, 1).unwrap());
        app.set_rotation(1.234);
        assert_eq!(app.rotation(0), 1.23);
        app.set_rotation(9.0);
        assert_eq!(app.rotation(0), MAX_ROTATION);
        app.set_rotation(-9.0);
        assert_eq!(app.rotation(0), -MAX_ROTATION);
        assert_eq!(app.rotation(1), 0.0, "other pages are not affected");
        app.set_rotation(0.0);
        assert!(app.rotations.is_empty());
    }

    #[test]
    fn one_section_open_at_a_time() {
        let mut app = ScanApp::with_config(Config::default());
        app.persist = false;
        assert_eq!(app.open_section, Some(Section::Scanner), "no scanner chosen yet");
        let page = || ScannedPage {
            full: image::DynamicImage::ImageRgb8(image::RgbImage::new(20, 30)),
            cropped: None,
            blank: false,
        };
        app.show_document(ScannedDocument::new(vec![page()], 75, 1).unwrap());
        assert_eq!(app.open_section, Some(Section::Preview), "preview opens after scanning");
        app.open_section = Some(Section::Save);
        app.discard_document();
        assert_eq!(app.open_section, Some(Section::Scan), "back to scanning afterwards");

        // Clicking a header opens it and closes the others.
        let ctx = egui::Context::default();
        run_frames(&mut app, &ctx, 2);
        let click = |pos: egui::Pos2| {
            let mut input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1200.0, 820.0))),
                ..Default::default()
            };
            input.events = vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() },
                egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() },
            ];
            input
        };
        // The first header ("1 Scanner") sits at the top of the panel.
        let _ = ctx.run_ui(click(egui::pos2(60.0, 26.0)), |ui| {
            app.draw(ui);
        });
        assert_eq!(app.open_section, Some(Section::Scanner));
        let _ = ctx.run_ui(click(egui::pos2(60.0, 26.0)), |ui| {
            app.draw(ui);
        });
        assert_eq!(app.open_section, None, "clicking the open header closes it");
    }
}
