//! Cemu per-game Graphic Packs overlay — port of Lacrima's
//! `CemuGraphicPacksOverlay` on the shared patch-overlay chrome: dim
//! scrim, header with the detected Wii U Title ID, a toggle list of
//! matching pack rows with per-category preset combos, Download /
//! Enable All / Disable All, and a status + Save/Close footer.
//! Activation persists to Cemu's `settings.xml`
//! (`content/GraphicPack/Entry`) via `cemu_gpack::save_enabled_states`;
//! packs come from `<user-data>/graphicPacks/**/rules.txt`.

use eframe::egui::{self, CornerRadius, Rect, Stroke, UiBuilder};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::emulation::cemu_gpack::{self, PackEntry};
use crate::emulation::emu_dl::Shared;
use crate::emulation::emu_meta;
use crate::tag_editor::{scale_style, TagEditor as T};

const SUBTEXT: egui::Color32 = egui::Color32::from_rgb(0xD8, 0xD8, 0xD8);
const PANEL_BG: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x1A, 0x1A, 0x1A, 0x1A);
const PANEL_LINE: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x3F, 0x3F, 0x3F, 0x3F);
const CARD_BG: egui::Color32 = egui::Color32::from_black_alpha(0x14);
const CARD_LINE: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x33, 0x33, 0x33, 0x33);
const TOGGLE_ON: egui::Color32 = egui::Color32::from_rgb(0x2F, 0x7D, 0x46);

struct Load {
    title_id: Option<String>,
    result: Result<Vec<PackEntry>, String>,
}

pub struct CemuGraphicPacksOverlay {
    open: bool,
    shown: bool,
    header: String,
    status: String,
    title_id: String,
    entries: Vec<PackEntry>,
    dirty: bool,
    /// Cemu executable dir — user data resolves to its `portable/`
    /// subdir when present (CemuPathsService.TryResolveUserDataDirectory).
    exe_dir: Option<PathBuf>,
    rx: Option<mpsc::Receiver<Load>>,
    dl: Option<Shared>,
}

impl CemuGraphicPacksOverlay {
    pub fn new() -> Self {
        Self {
            open: false,
            shown: false,
            header: String::new(),
            status: String::new(),
            title_id: String::new(),
            entries: Vec::new(),
            dirty: false,
            exe_dir: None,
            rx: None,
            dl: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// `OpenCurrentSectionCemuGraphicPacks` — `exe_dir` is the
    /// resolved Cemu emulator directory (configured launcher's dir or
    /// the managed install).
    pub fn open(&mut self, rom: &Path, title: &str, exe_dir: Option<PathBuf>) {
        *self = Self::new();
        self.open = true;
        self.shown = true;
        self.header = format!("{title} Graphic Packs");
        self.status = "Detecting Wii U Title ID and loading graphic packs…".into();
        self.exe_dir = exe_dir;
        let rom = rom.to_path_buf();
        let exe = self.exe_dir.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let tid = emu_meta::wiiu_title_id_cached(&rom);
            let result = match (&exe, &tid) {
                (None, _) => Err("Cemu emulator directory was not configured.".into()),
                (Some(_), None) => {
                    Err("Unable to detect Wii U Title ID for the selected game.".into())
                }
                (Some(d), Some(t)) => cemu_gpack::packs_for_title(d, t),
            };
            let _ = tx.send(Load {
                title_id: tid,
                result,
            });
        });
        self.rx = Some(rx);
    }

    /// Reload the entry list in place (after a pack download) —
    /// pending toggles survive by entry key, like the RPCS3 reload.
    fn reload(&mut self) {
        let (Some(exe), tid) = (self.exe_dir.clone(), self.title_id.clone()) else {
            return;
        };
        if tid.is_empty() {
            return;
        }
        match cemu_gpack::packs_for_title(&exe, &tid) {
            Ok(entries) => {
                let mut prev = std::collections::HashMap::new();
                for e in &self.entries {
                    prev.insert(e.key.clone(), e.enabled);
                }
                let mut merged = Vec::with_capacity(entries.len());
                for mut e in entries {
                    if let Some(was) = prev.get(&e.key) {
                        e.enabled = *was;
                    }
                    merged.push(e);
                }
                self.entries = merged;
                self.status =
                    format!("Loaded {} graphic pack(s).", self.entries.len());
            }
            Err(e) => {
                self.entries.clear();
                self.status = e;
            }
        }
    }

    fn save(&mut self) -> bool {
        if self.entries.is_empty() {
            self.status = "No graphic packs loaded to save.".into();
            return false;
        }
        let Some(exe) = &self.exe_dir else {
            self.status = "Emulator directory is not configured.".into();
            return false;
        };
        let toggles: Vec<cemu_gpack::Toggle> = self
            .entries
            .iter()
            .map(|e| cemu_gpack::Toggle {
                rel_rules: e.rel_rules.clone(),
                enabled: e.enabled,
                presets: e
                    .groups
                    .iter()
                    .filter_map(|g| {
                        g.names
                            .get(g.selected)
                            .map(|n| (g.category.clone(), n.clone()))
                    })
                    .collect(),
            })
            .collect();
        match cemu_gpack::save_enabled_states(exe, &toggles) {
            Ok(()) => {
                self.dirty = false;
                self.status = "Graphic pack settings saved.".into();
                true
            }
            Err(e) => {
                self.status = format!("Failed to save graphic packs: {e}");
                false
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, content: Rect, s: f32) {
        if let Some(rx) = &self.rx {
            if let Ok(load) = rx.try_recv() {
                self.rx = None;
                self.title_id = load.title_id.unwrap_or_default();
                match load.result {
                    Ok(entries) => {
                        self.entries = entries;
                        self.status = if self.entries.is_empty() {
                            format!(
                                "No graphic packs found for title ID {}. Download packs to get started.",
                                self.title_id
                            )
                        } else {
                            format!(
                                "Loaded {} graphic pack(s) for title ID {}.",
                                self.entries.len(),
                                self.title_id
                            )
                        };
                    }
                    Err(e) => self.status = e,
                }
            } else {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(50));
            }
        }
        // Pack download progress → reload when it lands.
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
                    Err(e) => self.status = format!("Failed to download graphic packs: {e}"),
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
            egui::Id::new("cemu_gpacks"),
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
            egui::Id::new("cemu_gpacks_scrim"),
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
                // Header — "<game> Graphic Packs" + Title ID row.
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
                                ui.label(
                                    egui::RichText::new("Title ID")
                                        .size(20.0 * s)
                                        .color(SUBTEXT),
                                );
                                ui.label(
                                    egui::RichText::new(if self.title_id.is_empty() {
                                        "Unknown"
                                    } else {
                                        self.title_id.as_str()
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
                let panel = Rect::from_min_max(
                    egui::pos2(area.min.x, head.max.y + 12.0 * s),
                    egui::pos2(area.max.x, area.max.y - footer_h - 12.0 * s),
                );
                self.panel(ui, panel, s);
                // Footer — status + Save + Close.
                let foot = Rect::from_min_max(
                    egui::pos2(area.min.x + 6.0 * s, area.max.y - 36.0 * s),
                    area.max,
                );
                let mut close = false;
                let mut do_save = false;
                let busy = self.dl.is_some() || self.rx.is_some();
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
                                && T::button(ui, "Save", egui::Vec2::ZERO, egui::vec2(14.0, 8.0) * s)
                                    .clicked()
                            {
                                do_save = true;
                            }
                        });
                    },
                );
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

    /// The pack list: header row + scrollable toggle cards.
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
                let mut do_dl = false;
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(head)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
                        ui.label(
                            egui::RichText::new("Graphic packs")
                                .size(22.0 * s)
                                .strong()
                                .color(egui::Color32::WHITE),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let busy = self.dl.is_some();
                            if !busy
                                && T::button(ui, "Disable All", egui::Vec2::ZERO, egui::vec2(12.0, 6.0) * s)
                                    .clicked()
                            {
                                all = -1;
                            }
                            if !busy
                                && T::button(ui, "Enable All", egui::Vec2::ZERO, egui::vec2(12.0, 6.0) * s)
                                    .clicked()
                            {
                                all = 1;
                            }
                            if !busy
                                && T::button(ui, "Download Packs", egui::Vec2::ZERO, egui::vec2(12.0, 6.0) * s)
                                    .clicked()
                            {
                                do_dl = true;
                            }
                        });
                    },
                );
                if do_dl {
                    if let Some(exe) = &self.exe_dir {
                        self.dl =
                            Some(cemu_gpack::download(cemu_gpack::user_data_dir(exe)));
                    } else {
                        self.status = "Emulator directory is not configured.".into();
                    }
                }
                if all != 0 {
                    for e in &mut self.entries {
                        e.enabled = all > 0;
                    }
                    self.dirty = true;
                    self.status = format!(
                        "{} {} graphic pack(s).",
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
                            .id_salt("cemu_gpack_entries")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if self.entries.is_empty()
                                    && self.rx.is_none()
                                    && self.dl.is_none()
                                {
                                    ui.label(
                                        egui::RichText::new(
                                            "No graphic packs match this game.",
                                        )
                                        .size(18.0 * s)
                                        .color(SUBTEXT),
                                    );
                                }
                                let w = ui.available_width();
                                for i in 0..self.entries.len() {
                                    self.card(ui, i, w, s);
                                    ui.add_space(8.0 * s);
                                }
                            });
                    },
                );
            },
        );
    }

    /// One pack card: name + subtitle + per-category preset combos +
    /// the toggle switch on the right (Lacrima's entry template).
    fn card(&mut self, ui: &mut egui::Ui, i: usize, w: f32, s: f32) {
        let group_rows = self.entries[i].groups.len();
        let name_h = 24.0 * s;
        let sub_h = 34.0 * s;
        let group_h = 58.0 * s; // label + combo + gap
        let pad = 12.0 * s;
        let h = pad * 2.0 + name_h + 4.0 * s + sub_h + group_rows as f32 * group_h;
        let (card, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
        let cr = card.shrink2(egui::vec2(0.0, 0.0));
        ui.painter().rect(
            cr,
            CornerRadius::same((8.0 * s).round() as u8),
            CARD_BG,
            Stroke::new(1.0, CARD_LINE),
            egui::StrokeKind::Inside,
        );
        let text_w = (cr.width() - 110.0 * s).max(60.0 * s);
        let name_g = ui.painter().layout_no_wrap(
            self.entries[i].name.clone(),
            egui::FontId::proportional(20.0 * s),
            egui::Color32::WHITE,
        );
        let desc_g = ui.painter().layout(
            self.entries[i].subtitle.clone(),
            egui::FontId::proportional(14.0 * s),
            egui::Color32::from_rgb(0xB0, 0xB0, 0xB0),
            text_w,
        );
        let mut ty = cr.min.y + pad;
        ui.painter()
            .galley(egui::pos2(cr.min.x + pad, ty), name_g, egui::Color32::WHITE);
        ty += name_h;
        ui.painter().galley(
            egui::pos2(cr.min.x + pad, ty),
            desc_g,
            egui::Color32::from_rgb(0xB0, 0xB0, 0xB0),
        );
        ty += sub_h;

        // Preset groups — label above a ComboBox of preset names.
        let combo_w = 280.0_f32.max((text_w * 0.55).min(420.0 * s));
        for gi in 0..group_rows {
            let label = self.entries[i].groups[gi].label.clone();
            ui.painter().text(
                egui::pos2(cr.min.x + pad, ty),
                egui::Align2::LEFT_TOP,
                label,
                egui::FontId::proportional(14.0 * s),
                SUBTEXT,
            );
            let combo_rect = Rect::from_min_size(
                egui::pos2(cr.min.x + pad, ty + 20.0 * s),
                egui::vec2(combo_w, 30.0 * s),
            );
            let mut child = ui.new_child(UiBuilder::new().max_rect(combo_rect));
            let names = self.entries[i].groups[gi].names.clone();
            let sel = self.entries[i].groups[gi]
                .selected
                .min(names.len().saturating_sub(1));
            let mut picked = sel;
            egui::ComboBox::from_id_salt(("cemu_gpack", i, gi))
                .width(combo_w - 4.0 * s)
                .selected_text(names.get(sel).cloned().unwrap_or_default())
                .show_ui(&mut child, |ui| {
                    for (ni, n) in names.iter().enumerate() {
                        if ui.selectable_label(ni == sel, n).clicked() {
                            picked = ni;
                        }
                    }
                });
            if picked != sel {
                self.entries[i].groups[gi].selected = picked;
                self.dirty = true;
            }
            ty += group_h;
        }

        // Toggle switch — same chrome as the other overlays'.
        let mut en = self.entries[i].enabled;
        let tr = Rect::from_center_size(
            egui::pos2(cr.max.x - 34.0 * s, cr.min.y + pad + name_h * 0.5),
            egui::vec2(48.0 * s, 24.0 * s),
        );
        let resp = ui.interact(
            tr,
            egui::Id::new("cemu_gpack_toggle").with(i),
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
        ui.painter()
            .circle_filled(egui::pos2(kx, tr.center().y), knob_r, egui::Color32::WHITE);
        if self.entries[i].enabled != en {
            self.entries[i].enabled = en;
            self.dirty = true;
        }
    }
}
