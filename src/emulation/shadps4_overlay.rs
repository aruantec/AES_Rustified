//! shadPS4 per-game overlays — ports of Lacrima's
//! `ShadPs4PatchesOverlay` + `ShadPs4CheatsOverlay` on the
//! `xenia_overlay`/`rpcs3_overlay` chrome.
//!
//! Patches: `user/patches/<repo>/*.xml` — `<Metadata isEnabled>`
//! toggles, rewritten in place on Save. Cheats:
//! `user/cheats/<TID>_<ver>*.json` mod lists whose state persists to
//! `user/cheats_enabled/` the moment a toggle flips (Lacrima saves
//! per-toggle too). Both offer repo-scoped downloads (shadPS4 /
//! GoldHEN) into the emulator's `user/` dir.

use eframe::egui::{self, CornerRadius, Rect, Stroke, UiBuilder};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::emulation::emu_dl::{DlProgress, Shared};
use crate::emulation::emu_meta;
use crate::emulation::shadps4::{self, Repo};
use crate::tag_editor::{scale_style, TagEditor as T};

const SUBTEXT: egui::Color32 = egui::Color32::from_rgb(0xD8, 0xD8, 0xD8);
const DIM: egui::Color32 = egui::Color32::from_rgb(0xB0, 0xB0, 0xB0);
const PANEL_BG: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x1A, 0x1A, 0x1A, 0x1A);
const PANEL_LINE: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x3F, 0x3F, 0x3F, 0x3F);
const CARD_BG: egui::Color32 = egui::Color32::from_black_alpha(0x14);
const CARD_LINE: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x33, 0x33, 0x33, 0x33);
const TOGGLE_ON: egui::Color32 = egui::Color32::from_rgb(0x2F, 0x7D, 0x46);
const SEL_BG: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x2B, 0x3B, 0x4B, 0x2B);

/// Shared scrim + fade + Esc handling around an overlay body. The
/// shown/open flags go in by value and come back out — the body
/// closure mutably borrows the overlay, so references can't cross.
fn overlay_frame(
    ui: &mut egui::Ui,
    content: Rect,
    s: f32,
    id: &str,
    shown: bool,
    mut open: bool,
    body: impl FnOnce(&mut egui::Ui, Rect),
) -> (bool, bool) {
    let ctx = ui.ctx().clone();
    let mut shown = shown;
    let fade = ctx.animate_bool_with_time_and_easing(
        egui::Id::unique(id),
        shown,
        0.32,
        egui::emath::easing::quadratic_in_out,
    );
    if fade <= 0.0 {
        open = shown;
        return (shown, open);
    }
    ui.painter().rect_filled(
        content,
        0.0,
        egui::Color32::from_black_alpha((230.0 * fade) as u8),
    );
    let _ = ui.interact(
        content,
        egui::Id::unique(format!("{id}_scrim")),
        egui::Sense::click(),
    );
    let area = Rect::from_min_max(
        content.min + egui::vec2(24.0, 16.0) * s,
        content.max - egui::vec2(24.0, 20.0) * s,
    );
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(area)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| {
            ui.set_opacity(fade);
            scale_style(ui.style_mut(), s);
            body(ui, area);
        },
    );
    if shown && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        shown = false;
    }
    (shown, open)
}

/// Header block — `Title — Patches` + the detected Title ID /
/// App version pair; returns the rect it consumed.
fn header(ui: &mut egui::Ui, area: Rect, s: f32, title: &str, tid: &str, ver: &str) -> Rect {
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(area.with_min_x(area.min.x + 6.0 * s))
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| {
            ui.label(
                egui::RichText::new(title)
                    .size(42.0 * s)
                    .strong()
                    .color(egui::Color32::WHITE),
            );
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 16.0 * s;
                for (label, value) in [("Title ID", tid), ("App version", ver)] {
                    ui.label(egui::RichText::new(label).size(20.0 * s).color(SUBTEXT));
                    ui.label(
                        egui::RichText::new(if value.is_empty() { "Unknown" } else { value })
                            .size(20.0 * s)
                            .color(egui::Color32::WHITE),
                    );
                }
            });
        },
    )
    .response
    .rect
}

/// The painted toggle switch used by the other overlays.
fn toggle(ui: &mut egui::Ui, id: egui::Id, s: f32, at: Rect, on: &mut bool) {
    let tr = Rect::from_center_size(
        egui::pos2(at.max.x - 34.0 * s, at.center().y),
        egui::vec2(48.0 * s, 24.0 * s),
    );
    let resp = ui.interact(tr, id, egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
    }
    ui.painter().rect(
        tr,
        CornerRadius::same((12.0 * s).round() as u8),
        if *on {
            TOGGLE_ON
        } else {
            egui::Color32::from_white_alpha(0x28)
        },
        Stroke::new(1.0, CARD_LINE),
        egui::StrokeKind::Inside,
    );
    let knob_r = 9.0 * s;
    let kx = if *on {
        tr.max.x - knob_r - 4.0 * s
    } else {
        tr.min.x + knob_r + 4.0 * s
    };
    ui.painter()
        .circle_filled(egui::pos2(kx, tr.center().y), knob_r, egui::Color32::WHITE);
}

/// Paint a card row: name + wrapped sub-line, toggle on the right.
/// Returns the (possibly flipped) enabled flag.
fn entry_card(
    ui: &mut egui::Ui,
    w: f32,
    h: f32,
    s: f32,
    name: &str,
    sub: &str,
    mut en: bool,
    id: &str,
    i: usize,
) -> bool {
    let (card, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
    let cr = card.shrink2(egui::vec2(0.0, 4.0 * s));
    ui.painter().rect(
        cr,
        CornerRadius::same((8.0 * s).round() as u8),
        CARD_BG,
        Stroke::new(1.0, CARD_LINE),
        egui::StrokeKind::Inside,
    );
    let text_w = (cr.width() - 100.0 * s).max(60.0 * s);
    let name_g = ui.painter().layout_no_wrap(
        name.to_string(),
        egui::FontId::proportional(20.0 * s),
        egui::Color32::WHITE,
    );
    let desc_g = ui.painter().layout(
        sub.to_string(),
        egui::FontId::proportional(14.0 * s),
        DIM,
        text_w,
    );
    let name_h = name_g.size().y;
    let desc_h = desc_g.size().y.min(30.0 * s);
    let block_h = name_h + 4.0 * s + desc_h;
    let mut ty = cr.center().y - block_h / 2.0;
    ui.painter().galley(
        egui::pos2(cr.min.x + 12.0 * s, ty),
        name_g,
        egui::Color32::WHITE,
    );
    ty += name_h + 4.0 * s;
    ui.painter()
        .galley(egui::pos2(cr.min.x + 12.0 * s, ty), desc_g, DIM);
    toggle(ui, egui::Id::unique(id).with(i), s, cr, &mut en);
    ui.add_space(8.0 * s);
    en
}

/// Drain a `Shared` download: returns (in-flight status text, Some if
/// it just finished). Callers reload on `Some`.
fn drain_dl(dl: &Shared, ui: &mut egui::Ui) -> (String, Option<Result<String, String>>) {
    let (stage, frac, done) = {
        let p: std::sync::MutexGuard<'_, DlProgress> = dl.lock().unwrap_or_else(|e| e.into_inner());
        (p.stage.clone(), p.frac, p.done.clone())
    };
    if let Some(res) = done {
        return (String::new(), Some(res));
    }
    ui.ctx().request_repaint();
    (
        if frac > 0.0 {
            format!("{stage} ({:.0}%)", frac * 100.0)
        } else {
            stage
        },
        None,
    )
}

/// Repo selector button — cycles shadPS4 → GoldHEN.
fn repo_button(ui: &mut egui::Ui, s: f32, repo: &mut Repo) {
    let label = format!("Repo: {}", repo.label());
    if T::button(ui, &label, egui::Vec2::ZERO, egui::vec2(14.0, 8.0) * s).clicked() {
        *repo = repo.next();
    }
}

// ======================================================================
// Patches overlay
// ======================================================================

struct PatchLoad {
    title_id: Option<String>,
    app_version: Option<String>,
    /// (patch file, original doc text, entries)
    result: Result<(PathBuf, String, Vec<shadps4::PatchEntry>), String>,
}

pub struct ShadPs4PatchesOverlay {
    open: bool,
    shown: bool,
    header: String,
    status: String,
    title_id: String,
    app_version: String,
    /// Resolved shadPS4 dir (`user/` under it).
    root: Option<PathBuf>,
    file: Option<PathBuf>,
    doc: String,
    entries: Vec<shadps4::PatchEntry>,
    dirty: bool,
    repo: Repo,
    rx: Option<mpsc::Receiver<PatchLoad>>,
    dl: Option<Shared>,
}

impl ShadPs4PatchesOverlay {
    pub fn new() -> Self {
        Self {
            open: false,
            shown: false,
            header: String::new(),
            status: String::new(),
            title_id: String::new(),
            app_version: String::new(),
            root: None,
            file: None,
            doc: String::new(),
            entries: Vec::new(),
            dirty: false,
            repo: Repo::ShadPs4,
            rx: None,
            dl: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, rom: &Path, title: &str, root: Option<PathBuf>) {
        *self = Self::new();
        self.open = true;
        self.shown = true;
        self.header = format!("{title} — Patches");
        self.status = "Detecting title ID and loading patches…".into();
        self.root = root;
        let rom = rom.to_path_buf();
        let (tx, rx) = mpsc::channel();
        let root2 = self.root.clone();
        std::thread::spawn(move || {
            let (tid, ver) = emu_meta::ps4_ids(&rom);
            let result = match (&root2, &tid) {
                (None, _) => Err("shadPS4 emulator directory could not be resolved.".into()),
                (Some(_), None) => {
                    Err("Unable to detect a PS4 Title ID for the selected game.".into())
                }
                (Some(r), Some(t)) => match shadps4::find_patch_file(r, t) {
                    Some(p) => match std::fs::read_to_string(&p) {
                        Ok(text) => {
                            Ok((p.clone(), text.clone(), shadps4::parse_patch_entries(&text)))
                        }
                        Err(e) => Err(format!("Failed to read {}: {e}", p.display())),
                    },
                    None => Err("No patch file found for this game — try Download Patches.".into()),
                },
            };
            let _ = tx.send(PatchLoad {
                title_id: tid,
                app_version: ver,
                result,
            });
        });
        self.rx = Some(rx);
    }

    /// Re-resolve the patch file and reload it — pending toggles
    /// carry over by name+app_ver key.
    fn reload(&mut self) {
        let Some(root) = &self.root else { return };
        if self.title_id.is_empty() {
            return;
        }
        match shadps4::find_patch_file(root, &self.title_id) {
            Some(p) => match std::fs::read_to_string(&p) {
                Ok(text) => {
                    let prev: HashMap<String, bool> = self
                        .entries
                        .iter()
                        .map(|e| (format!("{}\u{1F}{}", e.name, e.app_ver), e.enabled))
                        .collect();
                    self.file = Some(p);
                    self.doc = text;
                    self.entries = shadps4::parse_patch_entries(&self.doc);
                    for e in &mut self.entries {
                        if let Some(was) = prev.get(&format!("{}\u{1F}{}", e.name, e.app_ver)) {
                            e.enabled = *was;
                        }
                    }
                    self.status = format!("Loaded {} patch row(s).", self.entries.len());
                }
                Err(e) => self.status = format!("Failed to reload patches: {e}"),
            },
            None => {
                self.file = None;
                self.entries.clear();
                self.status = "No patch file found for this game.".into();
            }
        }
    }

    fn save(&mut self) -> bool {
        let Some(file) = &self.file else {
            self.status = "No patch file loaded.".into();
            return false;
        };
        let out = shadps4::write_patch_doc(&self.doc, &self.entries);
        match std::fs::write(file, &out) {
            Ok(()) => {
                self.doc = out;
                self.dirty = false;
                self.status = "Patch settings saved.".into();
                true
            }
            Err(e) => {
                self.status = format!("Failed to save patches: {e}");
                false
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, content: Rect, s: f32) {
        if let Some(rx) = &self.rx {
            if let Ok(load) = rx.try_recv() {
                self.rx = None;
                self.title_id = load.title_id.unwrap_or_default();
                self.app_version = load.app_version.unwrap_or_default();
                match load.result {
                    Ok((file, doc, entries)) => {
                        self.status = format!(
                            "Loaded {} patch row(s) for {}.",
                            entries.len(),
                            self.title_id
                        );
                        self.file = Some(file);
                        self.doc = doc;
                        self.entries = entries;
                    }
                    Err(e) => self.status = e,
                }
            } else {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(50));
            }
        }
        if let Some(dl) = &self.dl {
            let (stage, done) = drain_dl(dl, ui);
            if let Some(res) = done {
                self.dl = None;
                match res {
                    Ok(msg) => {
                        self.status = msg;
                        // A download may have introduced the file we
                        // lacked — re-resolve + reload.
                        self.reload();
                    }
                    Err(e) => self.status = format!("Patch download failed: {e}"),
                }
            } else {
                self.status = stage;
            }
        }

        let (shown, open) = overlay_frame(
            ui,
            content,
            s,
            "shadps4_patches",
            self.shown,
            self.open,
            |ui, area| {
                let head = header(ui, area, s, &self.header, &self.title_id, &self.app_version);
                let footer_h = 36.0 * s;
                let panel = Rect::from_min_max(
                    egui::pos2(area.min.x, head.max.y + 12.0 * s),
                    egui::pos2(area.max.x, area.max.y - footer_h - 12.0 * s),
                );
                // Panel — patch entries.
                ui.painter().rect(
                    panel,
                    CornerRadius::same((10.0 * s).round() as u8),
                    PANEL_BG,
                    Stroke::new(1.0, PANEL_LINE),
                    egui::StrokeKind::Inside,
                );
                let inner = panel.shrink(12.0 * s);
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(inner)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                    |ui| {
                        ui.set_clip_rect(inner.intersect(ui.clip_rect()));
                        let head2 = Rect::from_min_max(
                            inner.min,
                            egui::pos2(inner.max.x, inner.min.y + 30.0 * s),
                        );
                        let mut all: i8 = 0;
                        ui.scope_builder(
                            UiBuilder::new()
                                .max_rect(head2)
                                .layout(egui::Layout::left_to_right(egui::Align::Center)),
                            |ui| {
                                ui.label(
                                    egui::RichText::new("Patches")
                                        .size(22.0 * s)
                                        .strong()
                                        .color(egui::Color32::WHITE),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if T::button(
                                            ui,
                                            "Disable All",
                                            egui::Vec2::ZERO,
                                            egui::vec2(12.0, 6.0) * s,
                                        )
                                        .clicked()
                                        {
                                            all = -1;
                                        }
                                        if T::button(
                                            ui,
                                            "Enable All",
                                            egui::Vec2::ZERO,
                                            egui::vec2(12.0, 6.0) * s,
                                        )
                                        .clicked()
                                        {
                                            all = 1;
                                        }
                                    },
                                );
                            },
                        );
                        if all != 0 {
                            for e in &mut self.entries {
                                e.enabled = all > 0;
                            }
                            self.dirty = true;
                            self.status = format!(
                                "{} {} patch row(s).",
                                if all > 0 { "Enabled" } else { "Disabled" },
                                self.entries.len()
                            );
                        }
                        ui.advance_cursor_after_rect(head2);
                        ui.add_space(8.0 * s);
                        let scroll = Rect::from_min_max(
                            egui::pos2(inner.min.x, head2.max.y + 8.0 * s),
                            inner.max,
                        );
                        ui.scope_builder(
                            UiBuilder::new()
                                .max_rect(scroll)
                                .layout(egui::Layout::top_down(egui::Align::Min)),
                            |ui| {
                                egui::ScrollArea::vertical()
                                    .id_salt("shadps4_patch_entries")
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        if self.entries.is_empty() && self.rx.is_none() {
                                            ui.label(
                                                egui::RichText::new("No patches match this game.")
                                                    .size(18.0 * s)
                                                    .color(SUBTEXT),
                                            );
                                        }
                                        let w = ui.available_width();
                                        for i in 0..self.entries.len() {
                                            let e = &self.entries[i];
                                            let mut sub = format!("App {}", e.app_ver);
                                            if !e.note.is_empty() {
                                                sub = format!("{sub} — {}", e.note);
                                            }
                                            let name = e.name.clone();
                                            let en = entry_card(
                                                ui,
                                                w,
                                                66.0 * s,
                                                s,
                                                &name,
                                                &sub,
                                                e.enabled,
                                                "shadps4_patch_toggle",
                                                i,
                                            );
                                            if en != self.entries[i].enabled {
                                                self.entries[i].enabled = en;
                                                self.dirty = true;
                                            }
                                        }
                                    });
                            },
                        );
                    },
                );
                // Footer — status + repo + Download + Save + Close.
                let foot = Rect::from_min_max(
                    egui::pos2(area.min.x + 6.0 * s, area.max.y - 36.0 * s),
                    area.max,
                );
                let mut close = false;
                let mut do_save = false;
                let mut do_dl = false;
                let busy = self.dl.is_some();
                let mut repo = self.repo;
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(foot)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
                        ui.label(
                            egui::RichText::new(self.status.clone())
                                .size(18.0 * s)
                                .color(SUBTEXT),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if T::button(ui, "Close", egui::Vec2::ZERO, egui::vec2(14.0, 8.0) * s)
                                .clicked()
                            {
                                close = true;
                            }
                            if T::button(ui, "Save", egui::Vec2::ZERO, egui::vec2(14.0, 8.0) * s)
                                .clicked()
                            {
                                do_save = true;
                            }
                            if !busy
                                && T::button(
                                    ui,
                                    "Download Patches",
                                    egui::Vec2::ZERO,
                                    egui::vec2(14.0, 8.0) * s,
                                )
                                .clicked()
                            {
                                do_dl = true;
                            }
                            repo_button(ui, s, &mut repo);
                        });
                    },
                );
                self.repo = repo;
                if do_dl {
                    if let Some(root) = self.root.clone() {
                        self.dl = Some(shadps4::download_patches(root, self.repo));
                    } else {
                        self.status = "No shadPS4 emulator directory.".into();
                    }
                }
                if do_save && self.save() {
                    close = true;
                }
                if close {
                    self.shown = false;
                }
            },
        );
        self.shown &= shown;
        self.open = open;
    }
}

// ======================================================================
// Cheats overlay — file list left, mod toggles right. Toggling saves
// the enabled-state file immediately (Lacrima's SaveEnabledState on
// PropertyChanged).
// ======================================================================

struct CheatLoad {
    title_id: Option<String>,
    app_version: Option<String>,
    result: Result<Vec<PathBuf>, String>,
}

struct CheatUiMod {
    name: String,
    /// `hint` = the mod's apply targets/description (raw text).
    hint: String,
    /// Only `checkbox` mods toggle; others are informational
    /// (checkbox mods are what cheats_enabled applies at boot).
    checkbox: bool,
    enabled: bool,
}

pub struct ShadPs4CheatsOverlay {
    open: bool,
    shown: bool,
    header: String,
    status: String,
    title_id: String,
    app_version: String,
    root: Option<PathBuf>,
    files: Vec<PathBuf>,
    selected: Option<usize>,
    /// Loaded doc for the selected file.
    doc_name: String,
    doc_version: String,
    credits: String,
    mods: Vec<CheatUiMod>,
    repo: Repo,
    rx: Option<mpsc::Receiver<CheatLoad>>,
    dl: Option<Shared>,
}

impl ShadPs4CheatsOverlay {
    pub fn new() -> Self {
        Self {
            open: false,
            shown: false,
            header: String::new(),
            status: String::new(),
            title_id: String::new(),
            app_version: String::new(),
            root: None,
            files: Vec::new(),
            selected: None,
            doc_name: String::new(),
            doc_version: String::new(),
            credits: String::new(),
            mods: Vec::new(),
            repo: Repo::ShadPs4,
            rx: None,
            dl: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, rom: &Path, title: &str, root: Option<PathBuf>) {
        *self = Self::new();
        self.open = true;
        self.shown = true;
        self.header = format!("{title} — Cheats");
        self.status = "Detecting title ID and scanning cheats…".into();
        self.root = root;
        let rom = rom.to_path_buf();
        let (tx, rx) = mpsc::channel();
        let root2 = self.root.clone();
        std::thread::spawn(move || {
            let (tid, ver) = emu_meta::ps4_ids(&rom);
            let result = match (&root2, &tid) {
                (None, _) => Err("shadPS4 emulator directory could not be resolved.".into()),
                (Some(_), None) => {
                    Err("Unable to detect a PS4 Title ID for the selected game.".into())
                }
                (Some(r), Some(t)) => Ok(shadps4::find_cheat_files(
                    r,
                    t,
                    ver.as_deref().unwrap_or(""),
                )),
            };
            let _ = tx.send(CheatLoad {
                title_id: tid,
                app_version: ver,
                result,
            });
        });
        self.rx = Some(rx);
    }

    fn select(&mut self, idx: usize) {
        self.selected = Some(idx);
        self.doc_name.clear();
        self.doc_version.clear();
        self.credits.clear();
        self.mods.clear();
        let (Some(root), Some(file)) = (&self.root, self.files.get(idx)) else {
            return;
        };
        match shadps4::load_cheat_doc(file) {
            Some(doc) => {
                self.doc_name = doc.name;
                self.doc_version = doc.version;
                self.credits = doc.credits.join(", ");
                let state = shadps4::load_cheat_state(root, file);
                self.mods = doc
                    .mods
                    .into_iter()
                    .map(|m| {
                        let checkbox = m.r#type.eq_ignore_ascii_case("checkbox");
                        CheatUiMod {
                            name: m.name,
                            hint: m.hint.unwrap_or_default(),
                            checkbox,
                            enabled: false,
                        }
                    })
                    .collect();
                for m in &mut self.mods {
                    m.enabled = state.get(&m.name).copied().unwrap_or(false);
                }
                self.status = format!("{} mods loaded.", self.mods.len());
            }
            None => {
                self.status = format!(
                    "Failed to parse {}.",
                    file.file_name().unwrap_or_default().to_string_lossy()
                );
            }
        }
    }

    /// Persist the selected file's enabled state (SaveEnabledState).
    fn save_state(&mut self) {
        let (Some(root), Some(idx)) = (&self.root, self.selected) else {
            return;
        };
        let Some(file) = self.files.get(idx) else {
            return;
        };
        let state: HashMap<String, bool> = self
            .mods
            .iter()
            .filter(|m| m.checkbox)
            .map(|m| (m.name.clone(), m.enabled))
            .collect();
        match shadps4::save_cheat_state(root, file, &state) {
            Ok(()) => {}
            Err(e) => self.status = format!("Failed to save cheats: {e}"),
        }
    }

    fn rescan(&mut self) {
        let Some(root) = &self.root else { return };
        if self.title_id.is_empty() {
            return;
        }
        self.files = shadps4::find_cheat_files(root, &self.title_id, &self.app_version);
        if let Some(idx) = self.selected.filter(|i| *i < self.files.len()) {
            self.select(idx);
        } else if !self.files.is_empty() {
            self.select(0);
        }
        self.status = format!("{} cheat file(s) for {}.", self.files.len(), self.title_id);
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, content: Rect, s: f32) {
        if let Some(rx) = &self.rx {
            if let Ok(load) = rx.try_recv() {
                self.rx = None;
                self.title_id = load.title_id.unwrap_or_default();
                self.app_version = load.app_version.unwrap_or_default();
                match load.result {
                    Ok(files) => {
                        self.files = files;
                        self.status =
                            format!("{} cheat file(s) for {}.", self.files.len(), self.title_id);
                        if !self.files.is_empty() {
                            self.select(0);
                        }
                    }
                    Err(e) => self.status = e,
                }
            } else {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(50));
            }
        }
        if let Some(dl) = &self.dl {
            let (stage, done) = drain_dl(dl, ui);
            if let Some(res) = done {
                self.dl = None;
                match res {
                    Ok(msg) => {
                        self.status = msg;
                        self.rescan();
                    }
                    Err(e) => self.status = format!("Cheat download failed: {e}"),
                }
            } else {
                self.status = stage;
            }
        }

        let (shown, open) = overlay_frame(
            ui,
            content,
            s,
            "shadps4_cheats",
            self.shown,
            self.open,
            |ui, area| {
                let head = header(ui, area, s, &self.header, &self.title_id, &self.app_version);
                let footer_h = 36.0 * s;
                let panel = Rect::from_min_max(
                    egui::pos2(area.min.x, head.max.y + 12.0 * s),
                    egui::pos2(area.max.x, area.max.y - footer_h - 12.0 * s),
                );
                ui.painter().rect(
                    panel,
                    CornerRadius::same((10.0 * s).round() as u8),
                    PANEL_BG,
                    Stroke::new(1.0, PANEL_LINE),
                    egui::StrokeKind::Inside,
                );
                let inner = panel.shrink(12.0 * s);
                let col_w = (inner.width() * 0.30).clamp(200.0 * s, 340.0 * s);
                let left =
                    Rect::from_min_max(inner.min, egui::pos2(inner.min.x + col_w, inner.max.y));
                let right =
                    Rect::from_min_max(egui::pos2(left.max.x + 14.0 * s, inner.min.y), inner.max);

                // Left column — cheat file list.
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(left)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                    |ui| {
                        ui.set_clip_rect(left.intersect(ui.clip_rect()));
                        ui.label(
                            egui::RichText::new("Cheat files")
                                .size(22.0 * s)
                                .strong()
                                .color(egui::Color32::WHITE),
                        );
                        ui.add_space(8.0 * s);
                        let w = ui.available_width();
                        egui::ScrollArea::vertical()
                            .id_salt("shadps4_cheat_files")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if self.files.is_empty() && self.rx.is_none() {
                                    ui.label(
                                        egui::RichText::new("No cheat files for this game.")
                                            .size(18.0 * s)
                                            .color(SUBTEXT),
                                    );
                                }
                                for i in 0..self.files.len() {
                                    let name = self.files[i]
                                        .file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .into_owned();
                                    let (r, resp) = ui.allocate_exact_size(
                                        egui::vec2(w, 34.0 * s),
                                        egui::Sense::click(),
                                    );
                                    if self.selected == Some(i) {
                                        ui.painter().rect_filled(
                                            r,
                                            CornerRadius::same((6.0 * s).round() as u8),
                                            SEL_BG,
                                        );
                                    }
                                    let g = ui.painter().layout_no_wrap(
                                        name,
                                        egui::FontId::proportional(16.0 * s),
                                        egui::Color32::WHITE,
                                    );
                                    let tw = (w - 16.0 * s).max(40.0);
                                    let clipped = if g.size().x > tw {
                                        ui.painter().layout(
                                            self.files[i]
                                                .file_name()
                                                .unwrap_or_default()
                                                .to_string_lossy()
                                                .into_owned(),
                                            egui::FontId::proportional(16.0 * s),
                                            egui::Color32::WHITE,
                                            tw,
                                        )
                                    } else {
                                        g
                                    };
                                    ui.painter().galley(
                                        egui::pos2(
                                            r.min.x + 8.0 * s,
                                            r.center().y - clipped.size().y / 2.0,
                                        ),
                                        clipped,
                                        egui::Color32::WHITE,
                                    );
                                    if resp.clicked() {
                                        self.select(i);
                                    }
                                }
                            });
                    },
                );

                // Right column — selected file's mods.
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(right)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                    |ui| {
                        ui.set_clip_rect(right.intersect(ui.clip_rect()));
                        let head2 = Rect::from_min_max(
                            right.min,
                            egui::pos2(right.max.x, right.min.y + 30.0 * s),
                        );
                        let mut all: i8 = 0;
                        ui.scope_builder(
                            UiBuilder::new()
                                .max_rect(head2)
                                .layout(egui::Layout::left_to_right(egui::Align::Center)),
                            |ui| {
                                let mut title = if self.doc_name.is_empty() {
                                    "Mods".to_string()
                                } else {
                                    self.doc_name.clone()
                                };
                                if !self.doc_version.is_empty() {
                                    title = format!("{title} v{}", self.doc_version);
                                }
                                ui.label(
                                    egui::RichText::new(title)
                                        .size(22.0 * s)
                                        .strong()
                                        .color(egui::Color32::WHITE),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if T::button(
                                            ui,
                                            "Disable All",
                                            egui::Vec2::ZERO,
                                            egui::vec2(12.0, 6.0) * s,
                                        )
                                        .clicked()
                                        {
                                            all = -1;
                                        }
                                        if T::button(
                                            ui,
                                            "Enable All",
                                            egui::Vec2::ZERO,
                                            egui::vec2(12.0, 6.0) * s,
                                        )
                                        .clicked()
                                        {
                                            all = 1;
                                        }
                                    },
                                );
                            },
                        );
                        if all != 0 {
                            for m in &mut self.mods {
                                if m.checkbox {
                                    m.enabled = all > 0;
                                }
                            }
                            self.save_state();
                            self.status = format!(
                                "{} {} mod(s).",
                                if all > 0 { "Enabled" } else { "Disabled" },
                                self.mods.len()
                            );
                        }
                        ui.advance_cursor_after_rect(head2);
                        if !self.credits.is_empty() {
                            ui.label(
                                egui::RichText::new(format!("Credits: {}", self.credits))
                                    .size(14.0 * s)
                                    .color(SUBTEXT),
                            );
                        }
                        ui.add_space(6.0 * s);
                        let scroll = Rect::from_min_max(
                            egui::pos2(right.min.x, head2.max.y + 20.0 * s),
                            right.max,
                        );
                        ui.scope_builder(
                            UiBuilder::new()
                                .max_rect(scroll)
                                .layout(egui::Layout::top_down(egui::Align::Min)),
                            |ui| {
                                egui::ScrollArea::vertical()
                                    .id_salt("shadps4_cheat_mods")
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        if self.mods.is_empty() {
                                            ui.label(
                                                egui::RichText::new(
                                                    "Select a cheat file on the left.",
                                                )
                                                .size(18.0 * s)
                                                .color(SUBTEXT),
                                            );
                                        }
                                        let w = ui.available_width();
                                        let mut changed = false;
                                        for i in 0..self.mods.len() {
                                            let m = &self.mods[i];
                                            let sub = if m.checkbox {
                                                m.hint.clone()
                                            } else {
                                                format!(
                                                    "{} (applies at boot — not a toggle)",
                                                    m.hint
                                                )
                                            };
                                            let name = m.name.clone();
                                            let en = entry_card(
                                                ui,
                                                w,
                                                66.0 * s,
                                                s,
                                                &name,
                                                &sub,
                                                m.enabled,
                                                "shadps4_cheat_toggle",
                                                i,
                                            );
                                            if self.mods[i].checkbox && en != self.mods[i].enabled {
                                                self.mods[i].enabled = en;
                                                changed = true;
                                            }
                                        }
                                        if changed {
                                            self.save_state();
                                        }
                                    });
                            },
                        );
                    },
                );

                // Footer — status + repo + Download Cheats + Close.
                let foot = Rect::from_min_max(
                    egui::pos2(area.min.x + 6.0 * s, area.max.y - 36.0 * s),
                    area.max,
                );
                let mut close = false;
                let mut do_dl = false;
                let busy = self.dl.is_some();
                let mut repo = self.repo;
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(foot)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
                        ui.label(
                            egui::RichText::new(self.status.clone())
                                .size(18.0 * s)
                                .color(SUBTEXT),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if T::button(ui, "Close", egui::Vec2::ZERO, egui::vec2(14.0, 8.0) * s)
                                .clicked()
                            {
                                close = true;
                            }
                            if !busy
                                && T::button(
                                    ui,
                                    "Download Cheats",
                                    egui::Vec2::ZERO,
                                    egui::vec2(14.0, 8.0) * s,
                                )
                                .clicked()
                            {
                                do_dl = true;
                            }
                            repo_button(ui, s, &mut repo);
                        });
                    },
                );
                self.repo = repo;
                if do_dl {
                    match (self.root.clone(), self.title_id.is_empty()) {
                        (Some(root), false) => {
                            self.dl = Some(shadps4::download_cheats(
                                root,
                                self.title_id.clone(),
                                self.app_version.clone(),
                                self.repo,
                            ));
                        }
                        (None, _) => {
                            self.status = "No shadPS4 emulator directory.".into();
                        }
                        (_, true) => {
                            self.status = "Title ID is required to download cheats.".into();
                        }
                    }
                }
                if close {
                    self.shown = false;
                }
            },
        );
        self.shown &= shown;
        self.open = open;
    }
}
