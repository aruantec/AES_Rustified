//! ROM metadata editor — a compact port of Lacrima's
//! `EmulationMetadataOverlay` + `MetadataImageSearchOverlay`.
//!
//! Full-content overlay: "Metadata Editor" header, a two-column panel
//! (Details left, Images right) and a Save/Close footer — same chrome
//! as `tag_editor` but with the ROM field set (Developer, Platform,
//! Game Info, Gameplay URL, read-only console ids) and the
//! clipboard/URL/web-search cover sources. Saves land in the per-ROM
//! `.meta` + `.img` + `.title` sidecars (`emu_meta`).

use eframe::egui::{self, CornerRadius, Margin, Rect, Stroke, UiBuilder};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;

use crate::emu_meta;
use crate::tag_editor::{cover_uv, scale_style, TagEditor as T};

// Palette shared with tag_editor (MetadataOverlay.axaml colors).
const SUBTEXT: egui::Color32 = egui::Color32::from_rgb(0xD8, 0xD8, 0xD8);
const PANEL_BG: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x1A, 0x1A, 0x1A, 0x1A);
const PANEL_LINE: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x3F, 0x3F, 0x3F, 0x3F);
const BOX_BG: egui::Color32 = egui::Color32::from_black_alpha(0x14);
const BOX_LINE: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x33, 0x33, 0x33, 0x33);
const GREEN: egui::Color32 = egui::Color32::from_rgb(0x8F, 0xE3, 0xA1);

// ------------------------------------------------------------------
// Image kinds — Lacrima's EmulationImageKinds.
// ------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ImgKind {
    Cover,
    BoxArt,
    Gameplay,
    BackCover,
    Wallpaper,
    LiveWallpaper,
    Artist,
    Other,
}

impl ImgKind {
    const ALL: &'static [ImgKind] = &[
        Self::Cover,
        Self::BoxArt,
        Self::Gameplay,
        Self::BackCover,
        Self::Wallpaper,
        Self::LiveWallpaper,
        Self::Artist,
        Self::Other,
    ];
    fn label(self) -> &'static str {
        match self {
            Self::Cover => "Cover",
            Self::BoxArt => "Box Art",
            Self::Gameplay => "Gameplay",
            Self::BackCover => "Back Cover",
            Self::Wallpaper => "Wallpaper",
            Self::LiveWallpaper => "Live Wallpaper",
            Self::Artist => "Artist",
            Self::Other => "Other",
        }
    }
    fn from_str(s: &str) -> Self {
        Self::ALL
            .iter()
            .find(|k| k.label().eq_ignore_ascii_case(s) || format!("{k:?}") == s)
            .copied()
            .unwrap_or(Self::Other)
    }
}

struct Img {
    kind: ImgKind,
    data: Vec<u8>,
    tex: Option<egui::TextureHandle>,
    tex_failed: bool,
}

// ------------------------------------------------------------------
// Web search results (MetadataImageSearchOverlay)
// ------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq)]
enum SearchMode {
    Cover,
    Wallpaper,
    Gameplay,
}

/// A result card — image modes use `thumb`/`url`; gameplay uses the
/// YouTube thumb + watch url.
struct Hit {
    title: String,
    url: String,
    thumb: String,
}

struct SearchOverlay {
    open: bool,
    mode: SearchMode,
    status: String,
    hits: Vec<Hit>,
    /// In-flight search / picked-image download.
    search_rx: Option<mpsc::Receiver<Vec<Hit>>>,
    pick_rx: Option<mpsc::Receiver<Option<Vec<u8>>>>,
    /// Thumbnail fetch pool: the sender fans out to worker threads.
    thumb_tx: mpsc::Sender<(usize, Vec<u8>)>,
    thumb_rx: mpsc::Receiver<(usize, Vec<u8>)>,
    thumb_pending: HashSet<usize>,
    thumbs: HashMap<usize, egui::TextureHandle>,
}

impl SearchOverlay {
    fn new() -> Self {
        let (thumb_tx, thumb_rx) = mpsc::channel();
        Self {
            open: false,
            mode: SearchMode::Cover,
            status: String::new(),
            hits: Vec::new(),
            search_rx: None,
            pick_rx: None,
            thumb_tx,
            thumb_rx,
            thumb_pending: HashSet::new(),
            thumbs: HashMap::new(),
        }
    }

    /// Kick a search: image modes query DDG→Bing; gameplay queries
    /// YouTube. Results stream back through `search_rx`.
    fn start(&mut self, ctx: &egui::Context, mode: SearchMode, query: String) {
        *self = Self::new();
        self.open = true;
        self.mode = mode;
        self.status = format!("Searching \"{query}\"…");
        let (tx, rx) = mpsc::channel();
        self.search_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let hits: Vec<Hit> = match mode {
                SearchMode::Gameplay => emu_meta::youtube_search(&query)
                    .into_iter()
                    .map(|h| Hit {
                        title: h.title,
                        url: h.url,
                        thumb: h.thumb,
                    })
                    .collect(),
                _ => emu_meta::gather_image_candidates(&query)
                    .into_iter()
                    .map(|u| Hit {
                        title: url_label(&u),
                        thumb: u.clone(),
                        url: u,
                    })
                    .collect(),
            };
            let _ = tx.send(hits);
            ctx.request_repaint();
        });
    }

    /// Fetch a result's thumbnail once.
    fn want_thumb(&mut self, ctx: &egui::Context, i: usize) {
        if self.thumbs.contains_key(&i) || self.thumb_pending.contains(&i) {
            return;
        }
        let Some(url) = self.hits.get(i).map(|h| h.thumb.clone()) else {
            return;
        };
        let tx = self.thumb_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send((i, emu_meta::fetch_image(&url).unwrap_or_default()));
            ctx.request_repaint();
        });
        self.thumb_pending.insert(i);
    }
}

/// A result card's label — the image's filename when it parses.
fn url_label(u: &str) -> String {
    u.rsplit('/')
        .next()
        .and_then(|f| f.split(['?', '#']).next())
        .and_then(|f| f.rsplit_once('.'))
        .map(|(s, _)| s.replace(['_', '-', '.'], " "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| u.to_string())
}

// ------------------------------------------------------------------
// The editor
// ------------------------------------------------------------------
pub struct RomEditor {
    open: bool,
    shown: bool,
    saved: Option<PathBuf>,
    path: PathBuf,
    platform: String,
    title: String,
    developer: String,
    year: String,
    genre: String,
    comment: String,
    video_url: String,
    // X360 read-only ids (empty for other sections).
    tid: String,
    mid: String,
    images: Vec<Img>,
    add_kind: ImgKind,
    add_url: String,
    search_q: String,
    search: SearchOverlay,
    /// Add-URL / clipboard fetches — the picked bytes arrive async.
    url_rx: Option<mpsc::Receiver<Vec<u8>>>,
    err: Option<String>,
    dirty: bool,
    placeholder: Option<egui::TextureHandle>,
}

impl Default for RomEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl RomEditor {
    pub fn new() -> Self {
        Self {
            open: false,
            shown: false,
            saved: None,
            path: PathBuf::new(),
            platform: String::new(),
            title: String::new(),
            developer: String::new(),
            year: String::new(),
            genre: String::new(),
            comment: String::new(),
            video_url: String::new(),
            tid: String::new(),
            mid: String::new(),
            images: Vec::new(),
            add_kind: ImgKind::Cover,
            add_url: String::new(),
            search_q: String::new(),
            search: SearchOverlay::new(),
            url_rx: None,
            err: None,
            dirty: false,
            placeholder: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn take_saved(&mut self) -> Option<PathBuf> {
        self.saved.take()
    }

    /// `platform` is the section's display title — the Platform
    /// field's initial value and the web-search qualifier.
    pub fn open(&mut self, ctx: &egui::Context, path: PathBuf, platform: String) {
        *self = Self::new();
        self.open = true;
        self.shown = true;
        self.path = path;
        self.platform = platform;
        ctx.animate_bool(egui::Id::new("rom_editor").with(&self.path), false);

        let doc = emu_meta::load_meta(&self.path);
        self.title = doc
            .as_ref()
            .map(|d| d.title.clone())
            .filter(|t| !t.trim().is_empty())
            .or_else(|| emu_meta::cached_title(&self.path))
            .or_else(|| {
                self.path
                    .file_stem()
                    .map(|s| emu_meta::display_title(&s.to_string_lossy()))
            })
            .unwrap_or_default();
        if let Some(d) = &doc {
            self.developer = d.developer.clone();
            self.platform = if d.platform.trim().is_empty() {
                self.platform.clone()
            } else {
                d.platform.clone()
            };
            self.year = (d.year > 0).then(|| d.year.to_string()).unwrap_or_default();
            self.genre = d.genre.clone();
            self.comment = d.comment.clone();
            self.video_url = d.video_url.clone();
            self.tid = d.x360_title_id.clone();
            self.mid = d.x360_media_id.clone();
        }
        if self.tid.is_empty() {
            if let Some((t, m)) = emu_meta::x360_ids(&self.path) {
                (self.tid, self.mid) = (t, m);
            }
        }
        self.search_q = format!("{} {} cover", self.title, self.platform)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        // Cover sidecar → the active Cover entry; meta images follow.
        if let Ok(data) = std::fs::read(emu_meta::cover_path(&self.path)) {
            self.images.push(Img {
                kind: ImgKind::Cover,
                data,
                tex: None,
                tex_failed: false,
            });
        }
        if let Some(d) = &doc {
            for (i, r) in d.images.iter().enumerate() {
                if let Ok(data) = std::fs::read(emu_meta::meta_image_path(&self.path, i)) {
                    self.images.push(Img {
                        kind: ImgKind::from_str(&r.kind),
                        data,
                        tex: None,
                        tex_failed: false,
                    });
                }
            }
        }
    }

    fn push_image(&mut self, data: Vec<u8>, kind: ImgKind) {
        // Lacrima's AddFrontCoverCandidateImage: a second Cover pick
        // lands as a BoxArt alternate (Use as Cover promotes it).
        let kind = if kind == ImgKind::Cover
            && self.images.iter().any(|i| i.kind == ImgKind::Cover)
        {
            ImgKind::BoxArt
        } else {
            kind
        };
        self.images.push(Img {
            kind,
            data,
            tex: None,
            tex_failed: false,
        });
        self.dirty = true;
    }

    /// Fetch an image URL off-thread; the bytes land next frame.
    fn fetch_url(&mut self, ctx: &egui::Context, url: String) {
        let (tx, rx) = mpsc::channel();
        self.url_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(emu_meta::fetch_image(&url).unwrap_or_default());
            ctx.request_repaint();
        });
    }

    fn save(&mut self) -> Result<(), String> {
        // Fields → .meta + .title; active cover → .img sidecar.
        emu_meta::write_title(&self.path, &self.title);
        let cover = self.images.iter().find(|i| i.kind == ImgKind::Cover);
        match cover {
            Some(img) => {
                emu_meta::write_cover(&self.path, &img.data);
            }
            None => emu_meta::delete_cover(&self.path),
        }
        // Extra images → <key>.k<n> payloads + doc refs.
        let mut refs = Vec::new();
        let mut keep = HashSet::new();
        for (n, img) in self
            .images
            .iter()
            .filter(|i| i.kind != ImgKind::Cover)
            .enumerate()
        {
            let p = emu_meta::meta_image_path(&self.path, n);
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            if std::fs::write(&p, &img.data).is_ok() {
                keep.insert(p.file_name().unwrap().to_string_lossy().into_owned());
                refs.push(emu_meta::MetaImageRef {
                    kind: img.kind.label().to_string(),
                    file: p.file_name().unwrap().to_string_lossy().into_owned(),
                });
            }
        }
        emu_meta::prune_meta_images(&self.path, &keep);
        let doc = emu_meta::RomMetaDoc {
            title: self.title.trim().to_string(),
            developer: self.developer.trim().to_string(),
            platform: self.platform.trim().to_string(),
            year: self.year.trim().parse().unwrap_or(0),
            genre: self.genre.trim().to_string(),
            comment: self.comment.trim().to_string(),
            video_url: self.video_url.trim().to_string(),
            x360_title_id: self.tid.clone(),
            x360_media_id: self.mid.clone(),
            user_edited: true,
            images: refs,
        };
        emu_meta::save_meta(&self.path, &doc).map_err(|e| format!("Save failed: {e}"))
    }

    // --------------------------------------------------------------
    // Painting — EmulationMetadataOverlay.axaml: scrim, header,
    // Details|Images panel, status + Save/Close footer.
    // --------------------------------------------------------------
    pub fn ui(&mut self, ui: &mut egui::Ui, content: Rect, s: f32) {
        let ctx = ui.ctx().clone();
        // Async results: Add URL bytes, search hits, thumbs, picks.
        if let Some(rx) = &self.url_rx {
            if let Ok(data) = rx.try_recv() {
                self.url_rx = None;
                if data.is_empty() {
                    self.err = Some("Could not download that image.".to_string());
                } else {
                    self.push_image(data, self.add_kind);
                    self.add_url.clear();
                }
            }
        }
        self.search_drain(&ctx);

        let fade = ctx.animate_bool_with_time_and_easing(
            egui::Id::new("rom_editor").with(&self.path),
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
        let _ = ui.interact(content, egui::Id::new("rom_scrim"), egui::Sense::click());

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
                        |ui| self.header(ui, s),
                    )
                    .response
                    .rect;
                let footer = Rect::from_min_max(
                    egui::pos2(area.min.x + 6.0 * s, area.max.y - 36.0 * s),
                    area.max,
                );
                let panel = Rect::from_min_max(
                    egui::pos2(area.min.x, head.max.y + 12.0 * s),
                    egui::pos2(area.max.x, footer.min.y - 12.0 * s),
                );
                self.panel(ui, panel, s);
                self.footer(ui, footer, s);
            },
        );

        // The results overlay paints above the editor itself.
        if self.search.open {
            self.search_ui(ui, content, s);
        }
        if self.shown && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.search.open {
                self.search.open = false;
            } else {
                self.shown = false;
            }
        }
    }

    fn header(&self, ui: &mut egui::Ui, s: f32) {
        ui.spacing_mut().item_spacing.y = 4.0 * s;
        ui.label(
            egui::RichText::new("Metadata Editor")
                .size(42.0 * s)
                .strong()
                .color(egui::Color32::WHITE),
        );
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 16.0 * s;
            ui.label(egui::RichText::new("Title").size(20.0 * s).color(SUBTEXT));
            ui.label(
                egui::RichText::new(&self.title)
                    .size(20.0 * s)
                    .color(egui::Color32::WHITE),
            );
            ui.label(egui::RichText::new("Platform").size(20.0 * s).color(SUBTEXT));
            ui.label(
                egui::RichText::new(&self.platform)
                    .size(20.0 * s)
                    .color(egui::Color32::WHITE),
            );
        });
    }

    fn field(&mut self, ui: &mut egui::Ui, s: f32, label: &str, wide: f32, min_h: f32, ro: bool) {
        ui.label(egui::RichText::new(label).size(14.0 * s).color(SUBTEXT));
        let v = match label {
            "Title" => &mut self.title,
            "Developer" => &mut self.developer,
            "Platform" => &mut self.platform,
            "Year" => &mut self.year,
            "Genre" => &mut self.genre,
            "Game Info" => &mut self.comment,
            "Gameplay URL" => &mut self.video_url,
            "Title ID" => &mut self.tid,
            "Media ID" => &mut self.mid,
            _ => return,
        };
        let changed = if ro {
            T::text_box(ui, s, egui::Id::new(("romf", label)), &mut v.as_str(), wide, min_h, 14.0, "")
                .changed()
        } else {
            T::text_box(ui, s, egui::Id::new(("romf", label)), v, wide, min_h, 16.0, "")
                .changed()
        };
        self.dirty |= changed && !ro;
        ui.add_space(6.0 * s);
    }

    fn details(&mut self, ui: &mut egui::Ui, s: f32) {
        ui.spacing_mut().item_spacing.y = 8.0 * s;
        ui.label(
            egui::RichText::new("Details")
                .size(22.0 * s)
                .strong()
                .color(egui::Color32::WHITE),
        );
        egui::ScrollArea::vertical()
            .id_salt("rom_details_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0 * s;
                let wide = ui.available_width();
                self.field(ui, s, "Title", wide, 0.0, false);
                self.field(ui, s, "Developer", wide, 0.0, false);
                self.field(ui, s, "Platform", wide, 0.0, false);
                self.field(ui, s, "Year", (120.0 * s).min(wide), 0.0, false);
                self.field(ui, s, "Genre", wide, 0.0, false);
                self.field(ui, s, "Game Info", wide, 90.0 * s, false);
                // File Path — read-only, wrapping.
                ui.label(egui::RichText::new("File Path").size(14.0 * s).color(SUBTEXT));
                let fp = self.path.to_string_lossy().to_string();
                T::text_box(
                    ui,
                    s,
                    egui::Id::new("rom_file_path"),
                    &mut fp.as_str(),
                    wide,
                    0.0,
                    14.0,
                    "",
                );
                ui.add_space(6.0 * s);
                // Gameplay URL + Add/Remove Gameplay buttons.
                self.field(ui, s, "Gameplay URL", wide, 0.0, false);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0 * s;
                    let pad = egui::vec2(12.0, 8.0) * s;
                    let bw = (wide - 8.0 * s) / 2.0;
                    if T::button(ui, "Add Gameplay", egui::vec2(bw, 0.0), pad).clicked() {
                        let q = format!("{} gameplay", self.title);
                        self.search
                            .start(ui.ctx(), SearchMode::Gameplay, q.trim().to_string());
                    }
                    if T::button(ui, "Remove Gameplay", egui::vec2(bw, 0.0), pad).clicked() {
                        self.video_url.clear();
                        self.dirty = true;
                    }
                });
                ui.add_space(6.0 * s);
                if !self.tid.is_empty() {
                    self.field(ui, s, "Title ID", wide, 0.0, true);
                }
                if !self.mid.is_empty() {
                    self.field(ui, s, "Media ID", wide, 0.0, true);
                }
            });
    }

    /// Kind combobox (EmulationImageKinds).
    fn kind_combo(
        ui: &mut egui::Ui,
        s: f32,
        salt: impl std::hash::Hash + std::fmt::Debug,
        w: f32,
        kind: &mut ImgKind,
    ) -> bool {
        let mut changed = false;
        egui::ComboBox::from_id_salt(salt)
            .selected_text(egui::RichText::new(kind.label()).size(14.0 * s))
            .width(w)
            .show_ui(ui, |ui| {
                for k in ImgKind::ALL {
                    if ui
                        .selectable_label(*kind == *k, egui::RichText::new(k.label()).size(14.0 * s))
                        .clicked()
                        && *kind != *k
                    {
                        *kind = *k;
                        changed = true;
                    }
                }
            });
        changed
    }

    fn panel(&mut self, ui: &mut egui::Ui, rect: Rect, s: f32) {
        ui.painter().rect(
            rect,
            CornerRadius::same((10.0 * s).round() as u8),
            PANEL_BG,
            Stroke::new(1.0, PANEL_LINE),
            egui::StrokeKind::Inside,
        );
        let inner = rect.shrink(13.0 * s);
        let gap = 12.0 * s;
        let left_w = (340.0 * s).min((inner.width() - gap) * 0.5);
        let details = Rect::from_min_size(inner.min, egui::vec2(left_w, inner.height()));
        let images = Rect::from_min_max(egui::pos2(details.max.x + gap, inner.min.y), inner.max);
        for (r, left) in [(details, true), (images, false)] {
            ui.scope_builder(
                UiBuilder::new()
                    .max_rect(r)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
                |ui| {
                    ui.set_clip_rect(r.expand(2.0).intersect(ui.clip_rect()));
                    if left {
                        self.details(ui, s);
                    } else {
                        self.images_ui(ui, s);
                    }
                },
            );
        }
    }

    fn images_ui(&mut self, ui: &mut egui::Ui, s: f32) {
        ui.spacing_mut().item_spacing.y = 8.0 * s;
        let pad = egui::vec2(12.0, 6.0) * s;
        // Header: Images | kind combo | Add Image.
        let (row, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 30.0 * s),
            egui::Sense::hover(),
        );
        ui.painter().text(
            row.left_center(),
            egui::Align2::LEFT_CENTER,
            "Images",
            egui::FontId::proportional(22.0 * s),
            egui::Color32::WHITE,
        );
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(row)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing.x = 10.0 * s;
                if T::button(ui, "Add Image", egui::Vec2::ZERO, pad).clicked() {
                    if let Some(f) = rfd::FileDialog::new()
                        .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif", "bmp"])
                        .pick_file()
                    {
                        if let Ok(data) = std::fs::read(&f) {
                            let k = self.add_kind;
                            self.push_image(data, k);
                        }
                    }
                }
                Self::kind_combo(ui, s, "rom_add_kind", 140.0 * s, &mut self.add_kind);
            },
        );

        // "Cover From Clipboard / URL / Web Search" box.
        egui::Frame::new()
            .fill(BOX_BG)
            .stroke(Stroke::new(1.0, BOX_LINE))
            .corner_radius(CornerRadius::same((8.0 * s).round() as u8))
            .inner_margin(Margin::same((12.0 * s).round() as i8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0) * s;
                ui.label(
                    egui::RichText::new("Cover From Clipboard / URL / Web Search")
                        .size(16.0 * s)
                        .strong()
                        .color(egui::Color32::WHITE),
                );
                let bw = 104.0 * s;
                let tw = (ui.available_width() - 2.0 * (bw + 8.0 * s)).max(60.0 * s);
                // URL row: textbox | Add URL | Paste.
                ui.horizontal(|ui| {
                    let resp = T::text_box(
                        ui,
                        s,
                        egui::Id::new("rom_add_url"),
                        &mut self.add_url,
                        tw,
                        0.0,
                        14.0,
                        "Paste image URL...",
                    );
                    let enter = resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        && !self.add_url.trim().is_empty();
                    if T::button(ui, "Add URL", egui::vec2(bw, 0.0), pad).clicked() || enter {
                        let u = self.add_url.trim().to_string();
                        if !u.is_empty() {
                            self.fetch_url(ui.ctx(), u);
                        }
                    }
                    if T::button(ui, "Paste", egui::vec2(bw, 0.0), pad).clicked() {
                        match clipboard_image() {
                            Some(d) => {
                                let k = self.add_kind;
                                self.push_image(d, k);
                            }
                            None => self.err = Some("No image on the clipboard.".into()),
                        }
                    }
                });
                // Search row: query | Search | Use Title.
                ui.horizontal(|ui| {
                    let resp = T::text_box(
                        ui,
                        s,
                        egui::Id::new("rom_search_q"),
                        &mut self.search_q,
                        tw,
                        0.0,
                        14.0,
                        "Search cover images...",
                    );
                    let enter = resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        && !self.search_q.trim().is_empty();
                    let go = T::button(ui, "Search", egui::vec2(bw, 0.0), pad).clicked() || enter;
                    let by_title = T::button(ui, "Use Title", egui::vec2(bw, 0.0), pad).clicked();
                    if go || by_title {
                        let q = if by_title {
                            format!("{} {} cover", self.title, self.platform)
                        } else {
                            self.search_q.clone()
                        };
                        let q = q.split_whitespace().collect::<Vec<_>>().join(" ");
                        if !q.is_empty() {
                            self.search.start(ui.ctx(), SearchMode::Cover, q);
                        }
                    }
                });
                // Search Wallpaper & Bezels — full-width row.
                if T::button(ui, "Search Wallpaper & Bezels", egui::vec2(ui.available_width(), 0.0), pad)
                    .clicked()
                {
                    let q = format!("{} {} wallpaper", self.title, self.platform)
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    self.search.start(ui.ctx(), SearchMode::Wallpaper, q);
                }
            });

        // Image cards — same wrap grid as the audio editor.
        egui::ScrollArea::vertical()
            .id_salt("rom_images_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.placeholder.is_none() {
                    self.placeholder = Some(ui.ctx().load_texture(
                        "rom_cover_ph",
                        crate::library::placeholder_image(),
                        egui::TextureOptions::LINEAR,
                    ));
                }
                let ph = self.placeholder.clone();
                let card_w = 220.0 * s;
                let gap = 8.0 * s;
                let inner_w = card_w - 26.0 * s;
                let cols = ((ui.available_width() + gap) / (card_w + gap))
                    .floor()
                    .max(1.0) as usize;
                let mut front = None;
                let mut remove = None;
                let active = self.images.iter().position(|i| i.kind == ImgKind::Cover);
                for start in (0..self.images.len()).step_by(cols) {
                    let end = (start + cols).min(self.images.len());
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        for i in start..end {
                            let img = &mut self.images[i];
                            egui::Frame::new()
                                .fill(BOX_BG)
                                .stroke(Stroke::new(1.0, BOX_LINE))
                                .corner_radius(CornerRadius::same((8.0 * s).round() as u8))
                                .inner_margin(Margin::same((12.0 * s).round() as i8))
                                .show(ui, |ui| {
                                    ui.vertical(|ui| {
                                        ui.set_width(inner_w);
                                        ui.spacing_mut().item_spacing.y = 8.0 * s;
                                        if Self::kind_combo(ui, s, ("rom_kind", i), inner_w, &mut img.kind)
                                            && img.kind == ImgKind::Cover
                                        {
                                            front = Some(i);
                                        }
                                        let (r, _) = ui.allocate_exact_size(
                                            egui::vec2(inner_w, 160.0 * s),
                                            egui::Sense::hover(),
                                        );
                                        if img.tex.is_none() && !img.tex_failed {
                                            match image::load_from_memory(&img.data) {
                                                Ok(c) => {
                                                    let rgba = c.to_rgba8();
                                                    let (tw, th) = rgba.dimensions();
                                                    img.tex = Some(ui.ctx().load_texture(
                                                        format!("rom_img_{i}"),
                                                        egui::ColorImage::from_rgba_unmultiplied(
                                                            [tw as usize, th as usize],
                                                            &rgba,
                                                        ),
                                                        egui::TextureOptions::LINEAR,
                                                    ));
                                                }
                                                Err(_) => img.tex_failed = true,
                                            }
                                        }
                                        if let Some(t) = img.tex.as_ref().or(ph.as_ref()) {
                                            ui.painter().image(
                                                t.id(),
                                                r,
                                                cover_uv(t.size_vec2(), r.size()),
                                                egui::Color32::WHITE,
                                            );
                                        }
                                        if active == Some(i) {
                                            ui.label(
                                                egui::RichText::new("Active cover")
                                                    .size(13.0 * s)
                                                    .strong()
                                                    .color(GREEN),
                                            );
                                        } else if T::button(
                                            ui,
                                            "Use as Cover",
                                            egui::vec2(inner_w, 0.0),
                                            pad,
                                        )
                                        .clicked()
                                        {
                                            front = Some(i);
                                        }
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Min),
                                            |ui| {
                                                if T::button(ui, "Remove", egui::Vec2::ZERO, pad)
                                                    .clicked()
                                                {
                                                    remove = Some(i);
                                                }
                                            },
                                        );
                                    });
                                });
                        }
                    });
                    ui.add_space(gap);
                }
                if let Some(i) = front {
                    for (j, im) in self.images.iter_mut().enumerate() {
                        if j != i && im.kind == ImgKind::Cover {
                            im.kind = ImgKind::BoxArt;
                        }
                    }
                    self.images[i].kind = ImgKind::Cover;
                    self.dirty = true;
                }
                if let Some(i) = remove {
                    self.images.remove(i);
                    self.dirty = true;
                }
            });
    }

    fn footer(&mut self, ui: &mut egui::Ui, rect: Rect, s: f32) {
        let mut clicked = None;
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing.x = 10.0 * s;
                let pad = egui::vec2(14.0, 8.0) * s;
                if T::button(ui, "Close", egui::Vec2::ZERO, pad).clicked() {
                    clicked = Some(0);
                }
                if T::button(ui, "Save", egui::Vec2::ZERO, pad).clicked() {
                    clicked = Some(1);
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new("Edit metadata and images for the selected game.")
                                .size(18.0 * s)
                                .color(SUBTEXT),
                        )
                        .truncate(),
                    );
                    if let Some(e) = &self.err {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(e.clone())
                                    .size(14.0 * s)
                                    .color(egui::Color32::LIGHT_RED),
                            )
                            .truncate(),
                        );
                    }
                });
            },
        );
        match clicked {
            Some(0) => self.shown = false,
            Some(1) => match self.save() {
                Ok(()) => {
                    self.saved = Some(self.path.clone());
                    self.dirty = false;
                    self.err = None;
                    self.shown = false;
                }
                Err(e) => self.err = Some(e),
            },
            _ => {}
        }
    }

    // --------------------------------------------------------------
    // The image-search overlay (MetadataImageSearchOverlay): header
    // with query + count, result-card wrap grid, status + Close.
    // --------------------------------------------------------------
    fn search_drain(&mut self, ctx: &egui::Context) {
        let mut picked = None;
        let mut failed = false;
        {
            let s = &mut self.search;
            if let Some(rx) = &s.search_rx {
                if let Ok(hits) = rx.try_recv() {
                    s.search_rx = None;
                    s.status = if hits.is_empty() {
                        "No results found.".to_string()
                    } else {
                        format!("Found {} results.", hits.len())
                    };
                    s.hits = hits;
                }
            }
            while let Ok((i, bytes)) = s.thumb_rx.try_recv() {
                s.thumb_pending.remove(&i);
                if !bytes.is_empty() {
                    if let Ok(c) = image::load_from_memory(&bytes) {
                        let rgba = c.to_rgba8();
                        let (w, h) = rgba.dimensions();
                        s.thumbs.insert(
                            i,
                            ctx.load_texture(
                                format!("rom_search_{i}"),
                                egui::ColorImage::from_rgba_unmultiplied(
                                    [w as usize, h as usize],
                                    &rgba,
                                ),
                                egui::TextureOptions::LINEAR,
                            ),
                        );
                    }
                }
            }
            if let Some(rx) = &s.pick_rx {
                if let Ok(data) = rx.try_recv() {
                    s.pick_rx = None;
                    match data {
                        Some(d) => {
                            let kind = match s.mode {
                                SearchMode::Wallpaper => ImgKind::Wallpaper,
                                _ => ImgKind::Cover,
                            };
                            picked = Some((d, kind));
                            s.open = false;
                        }
                        None => failed = true,
                    }
                }
            }
        }
        if let Some((d, kind)) = picked {
            self.push_image(d, kind);
        }
        if failed {
            self.err = Some("Could not download that image.".into());
        }
        if self.search.search_rx.is_some()
            || self.search.pick_rx.is_some()
            || !self.search.thumb_pending.is_empty()
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn search_ui(&mut self, ui: &mut egui::Ui, content: Rect, s: f32) {
        // Lacrima's MetadataImageSearchOverlay is its own dialog — an
        // opaque backdrop hides the editor beneath and swallows clicks.
        ui.painter()
            .rect_filled(content, 0.0, egui::Color32::from_rgb(0x10, 0x10, 0x10));
        let _ = ui.interact(
            content,
            egui::Id::new("rom_search_scrim"),
            egui::Sense::click(),
        );
        let area = Rect::from_min_max(
            content.min + egui::vec2(48.0, 40.0) * s,
            content.max - egui::vec2(48.0, 40.0) * s,
        );
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(area)
                .layout(egui::Layout::top_down(egui::Align::Min)),
            |ui| {
                let head = match self.search.mode {
                    SearchMode::Cover => "Select Cover Image",
                    SearchMode::Wallpaper => "Select Wallpaper or Bezel",
                    SearchMode::Gameplay => "Select Gameplay Video",
                };
                ui.label(
                    egui::RichText::new(head)
                        .size(42.0 * s)
                        .strong()
                        .color(egui::Color32::WHITE),
                );
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 16.0 * s;
                    ui.label(egui::RichText::new("Results").size(20.0 * s).color(SUBTEXT));
                    ui.label(
                        egui::RichText::new(format!("{}", self.search.hits.len()))
                            .size(20.0 * s)
                            .color(egui::Color32::WHITE),
                    );
                });
                let body = Rect::from_min_max(
                    egui::pos2(area.min.x, ui.cursor().min.y + 8.0 * s),
                    egui::pos2(area.max.x, area.max.y - 44.0 * s),
                );
                ui.painter().rect(
                    body,
                    CornerRadius::same((10.0 * s).round() as u8),
                    PANEL_BG,
                    Stroke::new(1.0, PANEL_LINE),
                    egui::StrokeKind::Inside,
                );
                let inner = body.shrink(12.0 * s);
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(inner)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                    |ui| {
                        ui.set_clip_rect(inner.intersect(ui.clip_rect()));
                        self.search_results(ui, inner, s);
                    },
                );
                // Footer: status + Close.
                let foot = Rect::from_min_max(
                    egui::pos2(area.min.x, area.max.y - 36.0 * s),
                    area.max,
                );
                let mut close = false;
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(foot)
                        .layout(egui::Layout::right_to_left(egui::Align::Center)),
                    |ui| {
                        let pad = egui::vec2(14.0, 8.0) * s;
                        if T::button(ui, "Close", egui::Vec2::ZERO, pad).clicked() {
                            close = true;
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(self.search.status.clone())
                                        .size(18.0 * s)
                                        .color(SUBTEXT),
                                )
                                .truncate(),
                            );
                        });
                    },
                );
                if close {
                    self.search.open = false;
                }
            },
        );
    }

    fn search_results(&mut self, ui: &mut egui::Ui, rect: Rect, s: f32) {
        let ctx = ui.ctx().clone();
        egui::ScrollArea::vertical()
            .id_salt("rom_search_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let card_w = 200.0 * s;
                let gap = 8.0 * s;
                let cols = ((rect.width() + gap) / (card_w + gap)).floor().max(1.0) as usize;
                let n = self.search.hits.len();
                let mut pick = None;
                for start in (0..n).step_by(cols) {
                    let end = (start + cols).min(n);
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        for i in start..end {
                            self.search.want_thumb(&ctx, i);
                            let h = &self.search.hits[i];
                            let tex = self.search.thumbs.get(&i).cloned();
                            egui::Frame::new()
                                .fill(BOX_BG)
                                .stroke(Stroke::new(1.0, BOX_LINE))
                                .corner_radius(CornerRadius::same((8.0 * s).round() as u8))
                                .inner_margin(Margin::same((8.0 * s).round() as i8))
                                .show(ui, |ui| {
                                    ui.vertical(|ui| {
                                        ui.set_width(card_w - 18.0 * s);
                                        ui.spacing_mut().item_spacing.y = 6.0 * s;
                                        let (r, resp) = ui.allocate_exact_size(
                                            egui::vec2(card_w - 18.0 * s, 150.0 * s),
                                            egui::Sense::click(),
                                        );
                                        if let Some(t) = &tex {
                                            ui.painter().image(
                                                t.id(),
                                                r,
                                                cover_uv(t.size_vec2(), r.size()),
                                                egui::Color32::WHITE,
                                            );
                                        } else {
                                            ui.painter().rect_filled(
                                                r,
                                                CornerRadius::same((6.0 * s).round() as u8),
                                                egui::Color32::from_white_alpha(8),
                                            );
                                        }
                                        if resp.hovered() {
                                            ui.painter().rect_stroke(
                                                r,
                                                CornerRadius::same((6.0 * s).round() as u8),
                                                Stroke::new(1.5, egui::Color32::WHITE),
                                                egui::StrokeKind::Inside,
                                            );
                                        }
                                        if resp.clicked() {
                                            pick = Some(i);
                                        }
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(&h.title)
                                                    .size(15.0 * s)
                                                    .strong()
                                                    .color(egui::Color32::WHITE),
                                            )
                                            .truncate(),
                                        );
                                    });
                                });
                        }
                    });
                    ui.add_space(gap);
                }
                if let Some(i) = pick {
                    let h = self.search.hits[i].url.clone();
                    match self.search.mode {
                        SearchMode::Gameplay => {
                            self.video_url = h;
                            self.dirty = true;
                            self.search.open = false;
                        }
                        _ => {
                            let (tx, rx) = mpsc::channel();
                            self.search.pick_rx = Some(rx);
                            let ctx = ctx.clone();
                            std::thread::spawn(move || {
                                let _ = tx.send(emu_meta::fetch_image(&h));
                                ctx.request_repaint();
                            });
                            self.search.status = "Downloading image…".to_string();
                        }
                    }
                }
            });
    }
}

// ------------------------------------------------------------------
// Clipboard image (PasteImageFromClipboard) — CF_DIB → BMP bytes the
// `image` crate decodes.
// ------------------------------------------------------------------
#[cfg(windows)]
fn clipboard_image() -> Option<Vec<u8>> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }
    unsafe {
        OpenClipboard(None).ok()?;
        let _guard = Guard;
        let h = GetClipboardData(8).ok()?; // CF_DIB
        let hg = HGLOBAL(h.0 as *mut _);
        let size = GlobalSize(hg);
        let p = GlobalLock(hg);
        if p.is_null() || size < 40 {
            return None;
        }
        let dib = std::slice::from_raw_parts(p as *const u8, size).to_vec();
        let _ = GlobalUnlock(hg);
        dib_to_bmp(&dib)
    }
}

/// Wrap a CF_DIB payload in a BITMAPFILEHEADER so the `image` crate's
/// BMP decoder accepts it.
#[cfg(windows)]
fn dib_to_bmp(dib: &[u8]) -> Option<Vec<u8>> {
    let bi_size = u32::from_le_bytes(dib[0..4].try_into().ok()?) as usize;
    let bpp = u16::from_le_bytes(dib[14..16].try_into().ok()?) as usize;
    let colors = if bpp <= 8 {
        let used = u32::from_le_bytes(dib.get(32..36)?.try_into().ok()?) as usize;
        (if used == 0 { 1usize << bpp } else { used }) * 4
    } else {
        0
    };
    let mut out = Vec::with_capacity(14 + dib.len());
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(14 + dib.len() as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&((14 + bi_size + colors) as u32).to_le_bytes());
    out.extend_from_slice(dib);
    image::load_from_memory(&out).ok().map(|_| out)
}

#[cfg(not(windows))]
fn clipboard_image() -> Option<Vec<u8>> {
    None
}
