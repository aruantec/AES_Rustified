//! Xenia per-game overlays — ports of Lacrima's `XeniaPatchesOverlay`
//! and `XeniaCustomConfigOverlay`. Same chrome as `rom_editor`: dim
//! scrim, header, two-column panel, status + buttons footer.
//!
//! Patches: left = `*.patch.toml` files for the game's title id,
//! right = the `[[patch]]` toggles of the selected file. Save rewrites
//! `is_enabled` lines in place.
//!
//! Custom Config: left = schema sections, right = the section's
//! fields (bool toggle / choice combo / numeric / GPU adapter combo /
//! text) plus the GPU resolution-scale slider. Save diffs the current
//! values against the template into `custom_configs/<TITLEID>.json`;
//! Apply now additionally merges them into the active config TOML.

use eframe::egui::{self, CornerRadius, Rect, Stroke, UiBuilder};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::tag_editor::{scale_style, TagEditor as T};
use crate::xenia_cfg as xc;
use crate::emu_dl;

const SUBTEXT: egui::Color32 = egui::Color32::from_rgb(0xD8, 0xD8, 0xD8);
const PANEL_BG: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x1A, 0x1A, 0x1A, 0x1A);
const PANEL_LINE: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x3F, 0x3F, 0x3F, 0x3F);
const CARD_BG: egui::Color32 = egui::Color32::from_black_alpha(0x14);
const CARD_LINE: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x33, 0x33, 0x33, 0x33);
const SEL_BG: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x1F, 0x1F, 0x1F, 0x1F);
const PROMPT_BG: egui::Color32 = egui::Color32::from_rgba_premultiplied(0xE6, 0x1F, 0x1F, 0x1F);

fn heading(ui: &mut egui::Ui, text: &str, s: f32) {
    ui.label(
        egui::RichText::new(text)
            .size(42.0 * s)
            .strong()
            .color(egui::Color32::WHITE),
    );
}

// ======================================================================
// Patches overlay
// ======================================================================
struct PatchLoad {
    title_id: Option<String>,
    media_id: Option<String>,
    files: Vec<PathBuf>,
}

pub struct PatchesOverlay {
    open: bool,
    shown: bool,
    header: String,
    status: String,
    title_id: String,
    media_id: String,
    files: Vec<PathBuf>,
    sel: usize,
    doc_path: PathBuf,
    doc_text: String,
    entries: Vec<xc::PatchEntry>,
    dirty: bool,
    pending_switch: Option<usize>,
    rx: Option<mpsc::Receiver<PatchLoad>>,
    /// The resolved Xenia storage root (patches install target).
    root: Option<PathBuf>,
    /// In-flight "Download Patches" run (emu_dl).
    dl: Option<emu_dl::Shared>,
}

impl PatchesOverlay {
    pub fn new() -> Self {
        Self {
            open: false,
            shown: false,
            header: String::new(),
            status: String::new(),
            title_id: String::new(),
            media_id: String::new(),
            files: Vec::new(),
            sel: 0,
            doc_path: PathBuf::new(),
            doc_text: String::new(),
            entries: Vec::new(),
            dirty: false,
            pending_switch: None,
            rx: None,
            root: None,
            dl: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// `root` = resolved Xenia storage root (patches live under
    /// `<root>/patches`).
    pub fn open(&mut self, rom: &Path, title: &str, root: Option<PathBuf>) {
        *self = Self::new();
        self.open = true;
        self.shown = true;
        self.header = format!("{title} — Patches");
        self.status = "Detecting title ID and loading patches...".into();
        self.root = root.clone();
        let (tx, rx) = mpsc::channel();
        let rom = rom.to_path_buf();
        std::thread::spawn(move || {
            let (tid, mid) = xc::ids(&rom).map(|(a, b)| (Some(a), Some(b))).unwrap_or_default();
            let files = match (&root, &tid) {
                (Some(r), Some(t)) => xc::find_patch_files(r, t),
                _ => Vec::new(),
            };
            let _ = tx.send(PatchLoad {
                title_id: tid,
                media_id: mid,
                files,
            });
        });
        self.rx = Some(rx);
    }

    fn load_file(&mut self, i: usize) {
        self.entries.clear();
        self.doc_text.clear();
        self.dirty = false;
        let Some(p) = self.files.get(i) else { return };
        self.doc_path = p.clone();
        match std::fs::read_to_string(p) {
            Ok(text) => {
                self.entries = xc::parse_patch_entries(&text);
                self.doc_text = text;
                self.status = if self.entries.is_empty() {
                    "Selected file has no patch blocks.".into()
                } else {
                    format!("Loaded {} patch option(s).", self.entries.len())
                };
            }
            Err(e) => self.status = format!("Failed to load patch file: {e}"),
        }
        self.sel = i;
    }

    fn save(&mut self) -> bool {
        if self.doc_path.as_os_str().is_empty() {
            self.status = "Select a patch file before saving.".into();
            return false;
        }
        let updated = xc::rewrite_patch_doc(&self.doc_text, &self.entries);
        match std::fs::write(&self.doc_path, &updated) {
            Ok(()) => {
                self.doc_text = updated;
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
        // Async load result.
        if let Some(rx) = &self.rx {
            if let Ok(load) = rx.try_recv() {
                self.rx = None;
                self.title_id = load.title_id.clone().unwrap_or_default();
                self.media_id = load.media_id.clone().unwrap_or_default();
                self.files = load.files;
                if self.title_id.is_empty() {
                    self.status =
                        "Unable to detect Xbox 360 Title ID for the selected game.".into();
                } else {
                    self.status = if self.files.is_empty() {
                        format!("No patch files found for title ID {}.", self.title_id)
                    } else {
                        format!(
                            "Loaded {} patch file(s) for title ID {}.",
                            self.files.len(),
                            self.title_id
                        )
                    };
                    self.load_file(0);
                }
            } else {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
            }
        }

        // Patch-download progress — while running, the status line
        // reports the stage; on completion the file list reloads.
        if let Some(dl) = &self.dl {
            let (stage, frac, done) = {
                let p = dl.lock().unwrap_or_else(|e| e.into_inner());
                (p.stage.clone(), p.frac, p.done.clone())
            };
            if let Some(res) = done {
                self.dl = None;
                match res {
                    Ok(msg) => {
                        self.status = msg;
                        if let (Some(root), tid) = (&self.root, &self.title_id) {
                            if !tid.is_empty() {
                                self.files = xc::find_patch_files(root, tid);
                                if !self.files.is_empty() {
                                    self.load_file(0);
                                }
                            }
                        }
                    }
                    Err(e) => self.status = format!("Patch download failed: {e}"),
                }
            } else {
                self.status = if frac > 0.0 {
                    format!("{stage} ({:.0}%)", frac * 100.0)
                } else {
                    stage
                };
                ui.ctx().request_repaint();
            }
        }

        let ctx = ui.ctx().clone();
        let fade = ctx.animate_bool_with_time_and_easing(
            egui::Id::new("xenia_patches"),
            self.shown,
            0.32,
            egui::emath::easing::quadratic_in_out,
        );
        if fade <= 0.0 {
            self.open = self.shown;
            return;
        }
        ui.painter().rect_filled(
            content,
            0.0,
            egui::Color32::from_black_alpha((230.0 * fade) as u8),
        );
        let _ = ui.interact(
            content,
            egui::Id::new("xenia_patches_scrim"),
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
                // Header — title + Title ID / Media ID row.
                let head = ui
                    .scope_builder(
                        UiBuilder::new()
                            .max_rect(area.with_min_x(area.min.x + 6.0 * s))
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                        |ui| {
                            heading(ui, &self.header.clone(), s);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 16.0 * s;
                                ui.label(
                                    egui::RichText::new("Title ID")
                                        .size(20.0 * s)
                                        .color(SUBTEXT),
                                );
                                ui.label(
                                    egui::RichText::new(if self.title_id.is_empty() {
                                        "Unknown"
                                    } else {
                                        &self.title_id
                                    })
                                    .size(20.0 * s)
                                    .color(egui::Color32::WHITE),
                                );
                                ui.label(
                                    egui::RichText::new("Media ID")
                                        .size(20.0 * s)
                                        .color(SUBTEXT),
                                );
                                ui.label(
                                    egui::RichText::new(if self.media_id.is_empty() {
                                        "Unknown"
                                    } else {
                                        &self.media_id
                                    })
                                    .size(20.0 * s)
                                    .color(egui::Color32::WHITE),
                                );
                            });
                        },
                    )
                    .response
                    .rect;
                let footer_h = 36.0 * s;
                let prompt_h = if self.pending_switch.is_some() {
                    44.0 * s
                } else {
                    0.0
                };
                let panel = Rect::from_min_max(
                    egui::pos2(area.min.x, head.max.y + 12.0 * s),
                    egui::pos2(area.max.x, area.max.y - footer_h - prompt_h - 24.0 * s),
                );
                self.panel(ui, panel, s);
                // Unsaved-changes strip (SaveAndSwitch / SkipAndSwitch).
                if let Some(pend) = self.pending_switch {
                    let strip = Rect::from_min_max(
                        egui::pos2(area.min.x + 6.0 * s, panel.max.y + 6.0 * s),
                        egui::pos2(area.max.x, panel.max.y + 6.0 * s + 38.0 * s),
                    );
                    ui.painter().rect(
                        strip,
                        CornerRadius::same((8.0 * s).round() as u8),
                        PROMPT_BG,
                        Stroke::new(1.0, CARD_LINE),
                        egui::StrokeKind::Inside,
                    );
                    let mut act = 0;
                    ui.scope_builder(
                        UiBuilder::new()
                            .max_rect(strip.shrink(8.0 * s))
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        |ui| {
                            ui.label(
                                egui::RichText::new(
                                    "You have unsaved changes in the current patch file. \
                                     Save or Skip before switching.",
                                )
                                .size(16.0 * s)
                                .color(SUBTEXT),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if T::button(ui, "Skip", egui::Vec2::ZERO, egui::vec2(14.0, 8.0) * s)
                                        .clicked()
                                    {
                                        act = 2;
                                    }
                                    if T::button(ui, "Save", egui::Vec2::ZERO, egui::vec2(14.0, 8.0) * s)
                                        .clicked()
                                    {
                                        act = 1;
                                    }
                                },
                            );
                        },
                    );
                    match act {
                        1 if self.save() => {
                            self.pending_switch = None;
                            self.load_file(pend);
                        }
                        2 => {
                            self.pending_switch = None;
                            self.load_file(pend);
                        }
                        _ => {}
                    }
                }
                // Footer — status + Save + Close.
                let foot = Rect::from_min_max(
                    egui::pos2(area.min.x + 6.0 * s, area.max.y - 36.0 * s),
                    area.max,
                );
                let mut close = false;
                let mut do_save = false;
                let mut do_dl = false;
                let busy = self.dl.is_some();
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
                            // The overlay's own patch fetch — same archive the
                            // emulator Download installs (game-patches.zip).
                            if self.root.is_some()
                                && !busy
                                && T::button(ui, "Download Patches", egui::Vec2::ZERO, egui::vec2(14.0, 8.0) * s)
                                    .clicked()
                            {
                                do_dl = true;
                            }
                        });
                    },
                );
                if do_dl {
                    self.dl = Some(emu_dl::download_patches(self.root.clone().unwrap_or_default()));
                }
                if do_save && self.save() {
                    close = true;
                }
                if close {
                    self.shown = false;
                }
            },
        );
        if self.shown && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.shown = false;
        }
    }

    /// The two-column panel: patch files | patch toggles.
    fn panel(&mut self, ui: &mut egui::Ui, rect: Rect, s: f32) {
        ui.painter().rect(
            rect,
            CornerRadius::same((10.0 * s).round() as u8),
            PANEL_BG,
            Stroke::new(1.0, PANEL_LINE),
            egui::StrokeKind::Inside,
        );
        let inner = rect.shrink(12.0 * s);
        let left_w = 280.0 * s;
        let left = Rect::from_min_max(inner.min, egui::pos2(inner.min.x + left_w, inner.max.y));
        let right = Rect::from_min_max(
            egui::pos2(left.max.x + 12.0 * s, inner.min.y),
            inner.max,
        );

        // Left — the patch-file list.
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(left)
                .layout(egui::Layout::top_down(egui::Align::Min)),
            |ui| {
                ui.set_clip_rect(left.intersect(ui.clip_rect()));
                ui.label(
                    egui::RichText::new("Patch Files")
                        .size(22.0 * s)
                        .strong()
                        .color(egui::Color32::WHITE),
                );
                ui.add_space(8.0 * s);
                egui::ScrollArea::vertical()
                    .id_salt("xenia_patch_files")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let mut click = None;
                        for (i, f) in self.files.iter().enumerate() {
                            let name = f
                                .file_stem()
                                .and_then(|n| n.to_str())
                                .unwrap_or("patch")
                                .trim_end_matches(".patch")
                                .to_string();
                            let w = ui.available_width();
                            let (r, resp) = ui.allocate_exact_size(
                                egui::vec2(w, 30.0 * s),
                                egui::Sense::click(),
                            );
                            if resp.hovered() || i == self.sel {
                                ui.painter().rect_filled(
                                    r,
                                    CornerRadius::same((6.0 * s).round() as u8),
                                    SEL_BG,
                                );
                            }
                            ui.painter().text(
                                r.left_center() + egui::vec2(8.0 * s, 0.0),
                                egui::Align2::LEFT_CENTER,
                                name,
                                egui::FontId::proportional(18.0 * s),
                                egui::Color32::WHITE,
                            );
                            if resp.clicked() {
                                click = Some(i);
                            }
                        }
                        if let Some(i) = click {
                            if i != self.sel {
                                if self.dirty {
                                    self.pending_switch = Some(i);
                                } else {
                                    self.load_file(i);
                                }
                            }
                        }
                    });
            },
        );

        // Right — patch toggles + Enable/Disable All.
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(right)
                .layout(egui::Layout::top_down(egui::Align::Min)),
            |ui| {
                ui.set_clip_rect(right.intersect(ui.clip_rect()));
                let head = Rect::from_min_max(right.min, egui::pos2(right.max.x, right.min.y + 30.0 * s));
                let mut all: i8 = 0;
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(head)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
                        ui.label(
                            egui::RichText::new("Patches")
                                .size(22.0 * s)
                                .strong()
                                .color(egui::Color32::WHITE),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if T::button(ui, "Disable All", egui::Vec2::ZERO, egui::vec2(12.0, 6.0) * s)
                                .clicked()
                            {
                                all = -1;
                            }
                            if T::button(ui, "Enable All", egui::Vec2::ZERO, egui::vec2(12.0, 6.0) * s)
                                .clicked()
                            {
                                all = 1;
                            }
                        });
                    },
                );
                if all != 0 {
                    for e in &mut self.entries {
                        e.enabled = all > 0;
                    }
                    self.dirty = true;
                    self.status = format!(
                        "{} {} patch option(s).",
                        if all > 0 { "Enabled" } else { "Disabled" },
                        self.entries.len()
                    );
                }
                ui.advance_cursor_after_rect(head);
                ui.add_space(8.0 * s);
                let scroll = Rect::from_min_max(
                    egui::pos2(right.min.x, head.max.y + 8.0 * s),
                    right.max,
                );
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(scroll)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                    |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("xenia_patch_entries")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let w = ui.available_width();
                                for i in 0..self.entries.len() {
                                    let (name, desc) = {
                                        let e = &self.entries[i];
                                        (e.name.clone(), e.desc.clone())
                                    };
                                    let h = (if desc.is_empty() { 46.0 } else { 66.0 }) * s;
                                    let (card, _) = ui.allocate_exact_size(
                                        egui::vec2(w, h),
                                        egui::Sense::hover(),
                                    );
                                    let cr = card.shrink2(egui::vec2(0.0, 4.0 * s));
                                    ui.painter().rect(
                                        cr,
                                        CornerRadius::same((8.0 * s).round() as u8),
                                        CARD_BG,
                                        Stroke::new(1.0, CARD_LINE),
                                        egui::StrokeKind::Inside,
                                    );
                                    // Name + desc as one vertically centered block.
                                    let text_w = (cr.width() - 100.0 * s).max(60.0 * s);
                                    let name_g = ui.painter().layout_no_wrap(
                                        name,
                                        egui::FontId::proportional(20.0 * s),
                                        egui::Color32::WHITE,
                                    );
                                    let desc_g = if desc.is_empty() {
                                        None
                                    } else {
                                        Some(ui.painter().layout(
                                            desc,
                                            egui::FontId::proportional(15.0 * s),
                                            egui::Color32::from_rgb(0xB0, 0xB0, 0xB0),
                                            text_w,
                                        ))
                                    };
                                    let name_h = name_g.size().y;
                                    let block_h = name_h
                                        + desc_g
                                            .as_ref()
                                            .map_or(0.0, |g| 4.0 * s + g.size().y);
                                    let mut ty = cr.center().y - block_h / 2.0;
                                    ui.painter().galley(
                                        egui::pos2(cr.min.x + 12.0 * s, ty),
                                        name_g,
                                        egui::Color32::WHITE,
                                    );
                                    if let Some(g) = desc_g {
                                        ty += name_h + 4.0 * s;
                                        ui.painter().galley(
                                            egui::pos2(cr.min.x + 12.0 * s, ty),
                                            g,
                                            egui::Color32::from_rgb(0xB0, 0xB0, 0xB0),
                                        );
                                    }
                                    // Toggle (checkbox painted as a small switch).
                                    let mut en = self.entries[i].enabled;
                                    let tr = Rect::from_center_size(
                                        egui::pos2(cr.max.x - 34.0 * s, cr.center().y),
                                        egui::vec2(48.0 * s, 24.0 * s),
                                    );
                                    let resp = ui.interact(
                                        tr,
                                        egui::Id::new("xenia_patch_toggle").with(i),
                                        egui::Sense::click(),
                                    );
                                    if resp.clicked() {
                                        en = !en;
                                    }
                                    ui.painter().rect(
                                        tr,
                                        CornerRadius::same((12.0 * s).round() as u8),
                                        if en {
                                            egui::Color32::from_rgb(0x2F, 0x7D, 0x46)
                                        } else {
                                            egui::Color32::from_white_alpha(0x28)
                                        },
                                        Stroke::new(1.0, CARD_LINE),
                                        egui::StrokeKind::Inside,
                                    );
                                    let knob_r = 9.0 * s;
                                    let kx = if en {
                                        tr.max.x - knob_r - 4.0 * s
                                    } else {
                                        tr.min.x + knob_r + 4.0 * s
                                    };
                                    ui.painter().circle_filled(
                                        egui::pos2(kx, tr.center().y),
                                        knob_r,
                                        egui::Color32::WHITE,
                                    );
                                    if self.entries[i].enabled != en {
                                        self.entries[i].enabled = en;
                                        self.dirty = true;
                                    }
                                    ui.add_space(8.0 * s);
                                }
                            });
                    },
                );
            },
        );
    }
}

// ======================================================================
// Custom-config overlay
// ======================================================================
struct CfgLoad {
    title_id: Option<String>,
    json_path: Option<PathBuf>,
    json_exists: bool,
    template: BTreeMap<String, String>,
    merged: BTreeMap<String, String>,
    gpus: Vec<(i64, String)>,
}

struct FieldState {
    def: &'static xc::Field,
    val: String,
}

pub struct ConfigOverlay {
    open: bool,
    shown: bool,
    header: String,
    status: String,
    title_id: String,
    root: Option<PathBuf>,
    json_path: PathBuf,
    template: BTreeMap<String, String>,
    fields: Vec<FieldState>,
    draw_scale: i64,
    gpus: Vec<(i64, String)>,
    sel_section: usize,
    dirty: bool,
    rx: Option<mpsc::Receiver<CfgLoad>>,
}

impl ConfigOverlay {
    pub fn new() -> Self {
        Self {
            open: false,
            shown: false,
            header: String::new(),
            status: String::new(),
            title_id: String::new(),
            root: None,
            json_path: PathBuf::new(),
            template: BTreeMap::new(),
            fields: Vec::new(),
            draw_scale: 1,
            gpus: vec![(-1, "Auto Select".into())],
            sel_section: 0,
            dirty: false,
            rx: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, rom: &Path, title: &str, root: Option<PathBuf>) {
        *self = Self::new();
        self.open = true;
        self.shown = true;
        self.header = if title.is_empty() {
            "Xenia Custom Config".into()
        } else {
            format!("{title} — Xenia Config")
        };
        self.status = "Detecting Xbox 360 Title ID and loading config...".into();
        self.root = root;
        let (tx, rx) = mpsc::channel();
        let rom = rom.to_path_buf();
        let root = self.root.clone();
        std::thread::spawn(move || {
            let Some(root) = root else {
                let _ = tx.send(CfgLoad {
                    title_id: None,
                    json_path: None,
                    json_exists: false,
                    template: BTreeMap::new(),
                    merged: BTreeMap::new(),
                    gpus: xc::gpu_adapters(),
                });
                return;
            };
            xc::ensure_template(&root);
            let tid = xc::title_id(&rom);
            let (json_path, json_exists, merged) = match &tid {
                Some(t) => {
                    let p = xc::json_config_path(&root, t);
                    let doc = xc::load_overrides(&root, t);
                    (Some(p.clone()), p.is_file(), xc::merged_values(&root, &doc))
                }
                None => (None, false, BTreeMap::new()),
            };
            let template = xc::merged_values(&root, &xc::OverridesDoc::default());
            let _ = tx.send(CfgLoad {
                title_id: tid,
                json_path,
                json_exists,
                template,
                merged,
                gpus: xc::gpu_adapters(),
            });
        });
        self.rx = Some(rx);
    }

    /// Current field values as a `Section.key` map (draw scale folded
    /// into both keys — ApplyDrawResolutionScaleToValues).
    fn current_values(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for fs in &self.fields {
            out.insert(format!("{}.{}", fs.def.section, fs.def.key), fs.val.clone());
        }
        let v = self.draw_scale.to_string();
        out.insert(format!("GPU.{}", xc::SCALE_X), v.clone());
        out.insert(format!("GPU.{}", xc::SCALE_Y), v);
        out
    }

    fn save_core(&mut self) -> bool {
        let (Some(root), tid) = (self.root.clone(), self.title_id.clone()) else {
            return false;
        };
        if tid.is_empty() {
            return false;
        }
        let doc = xc::build_overrides(&self.current_values(), &self.template);
        match xc::save_overrides(&root, &tid, &doc) {
            Ok(()) => {
                let n: usize = doc.map.values().map(|s| s.len()).sum();
                self.status = if n == 0 {
                    format!("Saved empty override file for {tid} (matches defaults).")
                } else {
                    format!("Saved {n} override(s) for {tid}.")
                };
                self.dirty = false;
                true
            }
            Err(e) => {
                self.status = format!("Failed to save config: {e}");
                false
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, content: Rect, s: f32) {
        if let Some(rx) = &self.rx {
            if let Ok(load) = rx.try_recv() {
                self.rx = None;
                self.gpus = load.gpus;
                self.template = load.template;
                match (load.title_id, load.json_path) {
                    (Some(tid), Some(jp)) => {
                        self.title_id = tid.clone();
                        self.json_path = jp;
                        self.fields = xc::FIELDS
                            .iter()
                            .map(|def| FieldState {
                                def,
                                val: load
                                    .merged
                                    .get(&format!("{}.{}", def.section, def.key))
                                    .cloned()
                                    .unwrap_or_default(),
                            })
                            .collect();
                        self.draw_scale = load
                            .merged
                            .get(&format!("GPU.{}", xc::SCALE_X))
                            .or_else(|| load.merged.get(&format!("GPU.{}", xc::SCALE_Y)))
                            .and_then(|v| v.parse::<i64>().ok())
                            .unwrap_or(1)
                            .clamp(1, 7);
                        self.status = if load.json_exists {
                            format!("Loaded per-game overrides for {tid}.")
                        } else {
                            format!("No custom config for {tid}. Template defaults are shown.")
                        };
                    }
                    _ => {
                        self.status = if self.root.is_none() {
                            "No Xenia install found — set a path or use Download.".into()
                        } else {
                            "Unable to detect Xbox 360 Title ID for the selected game.".into()
                        };
                    }
                }
            } else {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
            }
        }

        let ctx = ui.ctx().clone();
        let fade = ctx.animate_bool_with_time_and_easing(
            egui::Id::new("xenia_cfg"),
            self.shown,
            0.32,
            egui::emath::easing::quadratic_in_out,
        );
        if fade <= 0.0 {
            self.open = self.shown;
            return;
        }
        ui.painter().rect_filled(
            content,
            0.0,
            egui::Color32::from_black_alpha((230.0 * fade) as u8),
        );
        let _ = ui.interact(
            content,
            egui::Id::new("xenia_cfg_scrim"),
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
                let head = ui
                    .scope_builder(
                        UiBuilder::new()
                            .max_rect(area.with_min_x(area.min.x + 6.0 * s))
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                        |ui| {
                            heading(ui, &self.header.clone(), s);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 16.0 * s;
                                ui.label(
                                    egui::RichText::new("Title ID")
                                        .size(20.0 * s)
                                        .color(SUBTEXT),
                                );
                                ui.label(
                                    egui::RichText::new(if self.title_id.is_empty() {
                                        "Unknown"
                                    } else {
                                        &self.title_id
                                    })
                                    .size(20.0 * s)
                                    .color(egui::Color32::WHITE),
                                );
                                ui.label(
                                    egui::RichText::new(
                                        self.json_path.to_string_lossy().to_string(),
                                    )
                                    .size(14.0 * s)
                                    .color(egui::Color32::from_rgb(0xA8, 0xA8, 0xA8)),
                                );
                            });
                        },
                    )
                    .response
                    .rect;
                let panel = Rect::from_min_max(
                    egui::pos2(area.min.x, head.max.y + 12.0 * s),
                    egui::pos2(area.max.x, area.max.y - 48.0 * s),
                );
                self.panel(ui, panel, s);
                // Footer — status | Restore defaults | Apply now | Save | Close.
                let foot = Rect::from_min_max(
                    egui::pos2(area.min.x + 6.0 * s, area.max.y - 36.0 * s),
                    area.max,
                );
                let mut act = 0u8;
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
                            let pad = egui::vec2(14.0, 8.0) * s;
                            if T::button(ui, "Close", egui::Vec2::ZERO, pad).clicked() {
                                act = 1;
                            }
                            if T::button(ui, "Save", egui::Vec2::ZERO, pad).clicked() {
                                act = 2;
                            }
                            if T::button(ui, "Apply now", egui::Vec2::ZERO, pad).clicked() {
                                act = 3;
                            }
                            if T::button(ui, "Restore defaults", egui::Vec2::ZERO, pad).clicked() {
                                act = 4;
                            }
                        });
                    },
                );
                match act {
                    1 => self.shown = false,
                    2 if self.save_core() => self.shown = false,
                    3 => {
                        if self.save_core() {
                            if let (Some(root), tid) = (&self.root, self.title_id.clone()) {
                                xc::prepare_for_launch(root, Some(&tid));
                                self.status =
                                    format!("Applied config to {}.", xc::ACTIVE_CONFIG);
                            }
                        }
                    }
                    4 => {
                        for fs in &mut self.fields {
                            let comp = format!("{}.{}", fs.def.section, fs.def.key);
                            fs.val = self.template.get(&comp).cloned().unwrap_or_default();
                        }
                        self.draw_scale = self
                            .template
                            .get(&format!("GPU.{}", xc::SCALE_X))
                            .and_then(|v| v.parse::<i64>().ok())
                            .unwrap_or(1)
                            .clamp(1, 7);
                        self.dirty = true;
                        self.status = "Restored template defaults in the editor.".into();
                    }
                    _ => {}
                }
            },
        );
        if self.shown && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.shown = false;
        }
    }

    fn panel(&mut self, ui: &mut egui::Ui, rect: Rect, s: f32) {
        ui.painter().rect(
            rect,
            CornerRadius::same((10.0 * s).round() as u8),
            PANEL_BG,
            Stroke::new(1.0, PANEL_LINE),
            egui::StrokeKind::Inside,
        );
        let inner = rect.shrink(12.0 * s);
        let left_w = 280.0 * s;
        let left = Rect::from_min_max(inner.min, egui::pos2(inner.min.x + left_w, inner.max.y));
        let right = Rect::from_min_max(
            egui::pos2(left.max.x + 12.0 * s, inner.min.y),
            inner.max,
        );
        let sections = xc::section_headers();
        self.sel_section = self.sel_section.min(sections.len().saturating_sub(1));
        let sel_name = sections.get(self.sel_section).copied().unwrap_or("");

        // Left — section list.
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(left)
                .layout(egui::Layout::top_down(egui::Align::Min)),
            |ui| {
                ui.set_clip_rect(left.intersect(ui.clip_rect()));
                ui.label(
                    egui::RichText::new("Sections")
                        .size(22.0 * s)
                        .strong()
                        .color(egui::Color32::WHITE),
                );
                ui.add_space(8.0 * s);
                egui::ScrollArea::vertical()
                    .id_salt("xenia_cfg_sections")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (i, name) in sections.iter().enumerate() {
                            let w = ui.available_width();
                            let (r, resp) = ui.allocate_exact_size(
                                egui::vec2(w, 30.0 * s),
                                egui::Sense::click(),
                            );
                            if resp.hovered() || i == self.sel_section {
                                ui.painter().rect_filled(
                                    r,
                                    CornerRadius::same((6.0 * s).round() as u8),
                                    SEL_BG,
                                );
                            }
                            ui.painter().text(
                                r.left_center() + egui::vec2(8.0 * s, 0.0),
                                egui::Align2::LEFT_CENTER,
                                *name,
                                egui::FontId::proportional(18.0 * s),
                                egui::Color32::WHITE,
                            );
                            if resp.clicked() {
                                self.sel_section = i;
                            }
                        }
                    });
            },
        );

        // Right — the selected section's fields.
        let gpus = self.gpus.clone();
        let mut dirty = false;
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(right)
                .layout(egui::Layout::top_down(egui::Align::Min)),
            |ui| {
                ui.set_clip_rect(right.intersect(ui.clip_rect()));
                ui.label(
                    egui::RichText::new(if sel_name.is_empty() { "Options" } else { sel_name })
                        .size(22.0 * s)
                        .strong()
                        .color(egui::Color32::WHITE),
                );
                ui.add_space(8.0 * s);
                egui::ScrollArea::vertical()
                    .id_salt("xenia_cfg_fields")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let w = (ui.available_width() - 8.0 * s).max(200.0 * s).min(980.0 * s);
                        // GPU section gets the resolution-scale row.
                        if sel_name.eq_ignore_ascii_case("GPU") {
                            self.resolution_scale_row(ui, w, s);
                        }
                        for i in 0..self.fields.len() {
                            if !self.fields[i].def.section.eq_ignore_ascii_case(sel_name) {
                                continue;
                            }
                            if self.field_row(ui, i, w, &gpus, s) {
                                dirty = true;
                            }
                        }
                    });
            },
        );
        if dirty {
            self.dirty = true;
        }
    }

    /// Lacrima's "Resolution scale" row — PillProgressSlider 1–7x.
    fn resolution_scale_row(&mut self, ui: &mut egui::Ui, w: f32, s: f32) {
        let label_w = 280.0 * s;
        let (row, resp) =
            ui.allocate_exact_size(egui::vec2(w, 40.0 * s), egui::Sense::hover());
        resp.on_hover_text("Internal render resolution multiplier (applies to X and Y).");
        ui.painter().text(
            egui::pos2(row.min.x + label_w, row.center().y),
            egui::Align2::RIGHT_CENTER,
            "Resolution scale",
            egui::FontId::proportional(18.0 * s),
            egui::Color32::WHITE,
        );
        let ctrl = Rect::from_min_max(
            egui::pos2(row.min.x + label_w + 12.0 * s, row.min.y + 6.0 * s),
            egui::pos2(row.max.x, row.max.y - 6.0 * s),
        );
        let mut v = self.draw_scale;
        let slider = egui::Slider::new(&mut v, 1..=7).suffix("x");
        let mut child = ui.new_child(UiBuilder::new().max_rect(ctrl));
        child.add_sized(ctrl.size(), slider);
        if v != self.draw_scale {
            self.draw_scale = v;
            self.dirty = true;
        }
    }

    /// One field row — label col (280) + control. Returns true on edit.
    fn field_row(
        &mut self,
        ui: &mut egui::Ui,
        i: usize,
        w: f32,
        gpus: &[(i64, String)],
        s: f32,
    ) -> bool {
        let label_w = 280.0 * s;
        let (row, resp) =
            ui.allocate_exact_size(egui::vec2(w, 40.0 * s), egui::Sense::hover());
        if !self.fields[i].def.desc.is_empty() {
            resp.clone().on_hover_text(self.fields[i].def.desc);
        }
        ui.painter().text(
            egui::pos2(row.min.x + label_w, row.center().y),
            egui::Align2::RIGHT_CENTER,
            self.fields[i].def.label,
            egui::FontId::proportional(18.0 * s),
            egui::Color32::WHITE,
        );
        let ctrl = Rect::from_min_max(
            egui::pos2(row.min.x + label_w + 12.0 * s, row.min.y + 4.0 * s),
            egui::pos2(row.min.x + label_w + 12.0 * s + (w - label_w - 12.0 * s).min(420.0 * s), row.max.y - 4.0 * s),
        );
        let fs = &mut self.fields[i];
        let mut changed = false;
        // `new_child`, not `scope_builder` — a scope claims its
        // min_rect on the parent cursor and painter-only controls
        // (the toggle) would rewind it, collapsing the next row.
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(ctrl)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        {
            let ui = &mut child;
            match &fs.def.kind {
                xc::Kind::Bool => {
                    let mut v = fs.val.eq_ignore_ascii_case("true");
                    let tr = Rect::from_center_size(
                        egui::pos2(ctrl.min.x + 26.0 * s, ctrl.center().y),
                        egui::vec2(48.0 * s, 24.0 * s),
                    );
                    let resp = ui.interact(
                        tr,
                        egui::Id::new("xenia_cfg_bool").with(i),
                        egui::Sense::click(),
                    );
                    if resp.clicked() {
                        v = !v;
                    }
                    ui.painter().rect(
                        tr,
                        CornerRadius::same((12.0 * s).round() as u8),
                        if v {
                            egui::Color32::from_rgb(0x2F, 0x7D, 0x46)
                        } else {
                            egui::Color32::from_white_alpha(0x28)
                        },
                        Stroke::new(1.0, CARD_LINE),
                        egui::StrokeKind::Inside,
                    );
                    let knob_r = 9.0 * s;
                    let kx = if v {
                        tr.max.x - knob_r - 4.0 * s
                    } else {
                        tr.min.x + knob_r + 4.0 * s
                    };
                    ui.painter().circle_filled(
                        egui::pos2(kx, tr.center().y),
                        knob_r,
                        egui::Color32::WHITE,
                    );
                    if v != fs.val.eq_ignore_ascii_case("true") {
                        fs.val = v.to_string();
                        changed = true;
                    }
                }
                xc::Kind::Int(lo, hi) => {
                    let mut v = fs.val.trim().parse::<i64>().unwrap_or(0);
                    let r = ui.add_sized(
                        egui::vec2(ctrl.width(), ctrl.height()),
                        egui::DragValue::new(&mut v).range(*lo..=*hi),
                    );
                    if r.changed() {
                        fs.val = v.to_string();
                        changed = true;
                    }
                }
                xc::Kind::Float(lo, hi) => {
                    let mut v = fs.val.trim().parse::<f64>().unwrap_or(0.0);
                    let r = ui.add_sized(
                        egui::vec2(ctrl.width(), ctrl.height()),
                        egui::DragValue::new(&mut v).range(*lo..=*hi).speed(0.01),
                    );
                    if r.changed() {
                        fs.val = format!("{v}");
                        changed = true;
                    }
                }
                xc::Kind::Choice(labels, values) => {
                    let cur = values
                        .iter()
                        .position(|v| v.eq_ignore_ascii_case(fs.val.trim()))
                        .unwrap_or(0);
                    let mut sel = cur;
                    egui::ComboBox::from_id_salt(egui::Id::new("xenia_cfg_choice").with(i))
                        .selected_text(labels.get(sel).copied().unwrap_or(""))
                        .width(ctrl.width())
                        .show_ui(ui, |ui| {
                            for (j, l) in labels.iter().enumerate() {
                                ui.selectable_value(&mut sel, j, *l);
                            }
                        });
                    if sel != cur {
                        fs.val = values.get(sel).copied().unwrap_or("").to_string();
                        changed = true;
                    }
                }
                xc::Kind::Gpu => {
                    let cur_id = fs.val.trim().parse::<i64>().unwrap_or(-1);
                    let cur = gpus.iter().position(|(id, _)| *id == cur_id).unwrap_or(0);
                    let mut sel = cur;
                    let cur_label = gpus.get(cur).map(|(_, l)| l.clone()).unwrap_or_default();
                    egui::ComboBox::from_id_salt(egui::Id::new("xenia_cfg_gpu").with(i))
                        .selected_text(cur_label)
                        .width(ctrl.width())
                        .show_ui(ui, |ui| {
                            for (j, (_, l)) in gpus.iter().enumerate() {
                                ui.selectable_value(&mut sel, j, l);
                            }
                        });
                    if sel != cur {
                        fs.val = gpus.get(sel).map(|(id, _)| id.to_string()).unwrap_or_default();
                        changed = true;
                    }
                }
            }
        }
        changed
    }
}
