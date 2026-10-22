//! RPCS3 per-game patches overlay — port of Lacrima's
//! `Rpcs3PatchesOverlay` on the `xenia_overlay` chrome: dim scrim,
//! header with detected Title ID / app version, a toggle list of
//! applicable patch rows, Enable/Disable All, "Download Latest
//! Patches" and a status + Save/Close footer. Activation persists to
//! RPCS3's `config/patch_config.yml` via `rpcs3_patch::save_toggles`.

use eframe::egui::{self, CornerRadius, Rect, Stroke, UiBuilder};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::emulation::emu_dl::Shared;
use crate::emulation::emu_meta;
use crate::emulation::rpcs3_patch::{self, PatchDef};
use crate::tag_editor::{scale_style, TagEditor as T};

const SUBTEXT: egui::Color32 = egui::Color32::from_rgb(0xD8, 0xD8, 0xD8);
const PANEL_BG: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x1A, 0x1A, 0x1A, 0x1A);
const PANEL_LINE: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x3F, 0x3F, 0x3F, 0x3F);
const CARD_BG: egui::Color32 = egui::Color32::from_black_alpha(0x14);
const CARD_LINE: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x33, 0x33, 0x33, 0x33);
const TOGGLE_ON: egui::Color32 = egui::Color32::from_rgb(0x2F, 0x7D, 0x46);

struct Load {
    title_id: Option<String>,
    app_version: Option<String>,
    result: Result<Vec<PatchDef>, String>,
}

pub struct Rpcs3PatchesOverlay {
    open: bool,
    shown: bool,
    header: String,
    status: String,
    title_id: String,
    app_version: String,
    entries: Vec<PatchDef>,
    dirty: bool,
    /// Resolved RPCS3 emulator directory (`patches/`, `config/` under it).
    root: Option<PathBuf>,
    rx: Option<mpsc::Receiver<Load>>,
    dl: Option<Shared>,
}

impl Rpcs3PatchesOverlay {
    pub fn new() -> Self {
        Self {
            open: false,
            shown: false,
            header: String::new(),
            status: String::new(),
            title_id: String::new(),
            app_version: String::new(),
            entries: Vec::new(),
            dirty: false,
            root: None,
            rx: None,
            dl: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// `root` = resolved RPCS3 emulator dir — Lacrima's
    /// `ResolveEmulatorDirectory` result.
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
            let (tid, ver) = emu_meta::ps3_ids(&rom);
            let result = match (&root2, &tid) {
                (None, _) => Err("RPCS3 emulator directory could not be resolved.".into()),
                (Some(_), None) => {
                    Err("Unable to detect a PS3 Title ID for the selected game.".into())
                }
                (Some(r), Some(t)) => {
                    rpcs3_patch::load_for_game(r, t, ver.as_deref().unwrap_or(""))
                }
            };
            let _ = tx.send(Load {
                title_id: tid,
                app_version: ver,
                result,
            });
        });
        self.rx = Some(rx);
    }

    /// Reload the entry list in place (after a patch-db download).
    fn reload(&mut self) {
        let Some(root) = &self.root else { return };
        if self.title_id.is_empty() {
            return;
        }
        match rpcs3_patch::load_for_game(root, &self.title_id, &self.app_version) {
            Ok(entries) => {
                // Keep pending toggles across the reload — match by
                // entry key; fresh rows adopt the persisted state.
                let mut merged = Vec::with_capacity(entries.len());
                let prev: std::collections::HashMap<String, bool> = self
                    .entries
                    .iter()
                    .map(|e| (rpcs3_patch::entry_key(e), e.enabled))
                    .collect();
                for mut e in entries {
                    if let Some(was) = prev.get(&rpcs3_patch::entry_key(&e)) {
                        e.enabled = *was;
                    }
                    merged.push(e);
                }
                self.entries = merged;
                self.status = format!("Loaded {} patch row(s).", self.entries.len());
            }
            Err(e) => {
                self.entries.clear();
                self.status = e;
            }
        }
    }

    fn save(&mut self) -> bool {
        let Some(root) = &self.root else {
            self.status = "No RPCS3 emulator directory.".into();
            return false;
        };
        match rpcs3_patch::save_toggles(root, &self.entries) {
            Ok(()) => {
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
                    Ok(entries) => {
                        self.entries = entries;
                        self.status = format!(
                            "Loaded {} patch row(s) for {}.",
                            self.entries.len(),
                            self.title_id
                        );
                    }
                    Err(e) => self.status = e,
                }
            } else {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(50));
            }
        }
        // Patch download progress → reload the list when it lands.
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
                        self.reload();
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
            egui::Id::unique("rpcs3_patches"),
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
            egui::Id::unique("rpcs3_patches_scrim"),
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
                // Header — title + Title ID / App version row.
                let head = ui
                    .scope_builder(
                        UiBuilder::new()
                            .max_rect(area.with_min_x(area.min.x + 6.0 * s))
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                        |ui| {
                            ui.label(
                                egui::RichText::new(self.header.clone())
                                    .size(42.0 * s)
                                    .strong()
                                    .color(egui::Color32::WHITE),
                            );
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 16.0 * s;
                                for (label, value) in [
                                    ("Title ID", self.title_id.as_str()),
                                    ("App version", self.app_version.as_str()),
                                ] {
                                    ui.label(
                                        egui::RichText::new(label)
                                            .size(20.0 * s)
                                            .color(SUBTEXT),
                                    );
                                    ui.label(
                                        egui::RichText::new(if value.is_empty() {
                                            "Unknown"
                                        } else {
                                            value
                                        })
                                        .size(20.0 * s)
                                        .color(egui::Color32::WHITE),
                                    );
                                }
                            });
                        },
                    )
                    .response
                    .rect;
                let footer_h = 36.0 * s;
                let panel = Rect::from_min_max(
                    egui::pos2(area.min.x, head.max.y + 12.0 * s),
                    egui::pos2(area.max.x, area.max.y - footer_h - 12.0 * s),
                );
                self.panel(ui, panel, s);
                // Footer — status + Download + Save + Close.
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
                        });
                    },
                );
                if do_dl {
                    if let Some(root) = self.root.clone() {
                        self.dl = Some(rpcs3_patch::download(root));
                    } else {
                        self.status = "No RPCS3 emulator directory.".into();
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
        if self.shown && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.shown = false;
        }
    }

    /// The patch list: header row + scrollable toggle cards.
    fn panel(&mut self, ui: &mut egui::Ui, rect: Rect, s: f32) {
        ui.painter().rect(
            rect,
            CornerRadius::same((10.0 * s).round() as u8),
            PANEL_BG,
            Stroke::new(1.0, PANEL_LINE),
            egui::StrokeKind::Inside,
        );
        let inner = rect.shrink(12.0 * s);
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(inner)
                .layout(egui::Layout::top_down(egui::Align::Min)),
            |ui| {
                ui.set_clip_rect(inner.intersect(ui.clip_rect()));
                let head = Rect::from_min_max(
                    inner.min,
                    egui::pos2(inner.max.x, inner.min.y + 30.0 * s),
                );
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
                        "{} {} patch row(s).",
                        if all > 0 { "Enabled" } else { "Disabled" },
                        self.entries.len()
                    );
                }
                ui.advance_cursor_after_rect(head);
                ui.add_space(8.0 * s);
                let scroll = Rect::from_min_max(
                    egui::pos2(inner.min.x, head.max.y + 8.0 * s),
                    inner.max,
                );
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(scroll)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                    |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("rpcs3_patch_entries")
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
                                    // Sub line: serial · app ver · group ·
                                    // patch version · author — then notes.
                                    let mut sub = format!(
                                        "{} · v{} · patch {}",
                                        e.serial, e.app_version, e.patch_version
                                    );
                                    if !e.group.is_empty() {
                                        sub = format!("{} · {}", e.group, sub);
                                    }
                                    if !e.author.is_empty() {
                                        sub = format!("{sub} — {}", e.author);
                                    }
                                    let desc = if e.notes.is_empty() {
                                        sub
                                    } else {
                                        format!("{sub}\n{}", e.notes)
                                    };
                                    let name = e.name.clone();
                                    let h = 66.0 * s;
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
                                    let text_w = (cr.width() - 100.0 * s).max(60.0 * s);
                                    let name_g = ui.painter().layout_no_wrap(
                                        name,
                                        egui::FontId::proportional(20.0 * s),
                                        egui::Color32::WHITE,
                                    );
                                    let desc_g = ui.painter().layout(
                                        desc,
                                        egui::FontId::proportional(14.0 * s),
                                        egui::Color32::from_rgb(0xB0, 0xB0, 0xB0),
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
                                    ui.painter().galley(
                                        egui::pos2(cr.min.x + 12.0 * s, ty),
                                        desc_g,
                                        egui::Color32::from_rgb(0xB0, 0xB0, 0xB0),
                                    );
                                    // Toggle switch — same chrome as the
                                    // Xenia overlay's.
                                    let mut en = self.entries[i].enabled;
                                    let tr = Rect::from_center_size(
                                        egui::pos2(cr.max.x - 34.0 * s, cr.center().y),
                                        egui::vec2(48.0 * s, 24.0 * s),
                                    );
                                    let resp = ui.interact(
                                        tr,
                                        egui::Id::unique("rpcs3_patch_toggle").with(i),
                                        egui::Sense::click(),
                                    );
                                    if resp.clicked() {
                                        en = !en;
                                    }
                                    ui.painter().rect(
                                        tr,
                                        CornerRadius::same((12.0 * s).round() as u8),
                                        if en {
                                            TOGGLE_ON
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
