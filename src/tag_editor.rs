//! Tag editor overlay — a port of AES_Lacrima's `MetadataOverlay.axaml`.
//!
//! Full-content overlay: 90% black scrim, "Metadata Editor" header, a
//! two-column panel (Details fields on the left, embedded-image cards on
//! the right) and a footer with a status line + Save/Close buttons.

use eframe::egui::{self, CornerRadius, Margin, Rect, Stroke, UiBuilder};
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::probe::Probe;
use std::path::{Path, PathBuf};

// Palette lifted from MetadataOverlay.axaml.
const SUBTEXT: egui::Color32 = egui::Color32::from_rgb(0xD8, 0xD8, 0xD8);
const FAINT: egui::Color32 = egui::Color32::from_rgb(0xB0, 0xB0, 0xB0);
const PANEL_BG: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x1A, 0x1A, 0x1A, 0x1A); // #1AFFFFFF
const PANEL_LINE: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x3F, 0x3F, 0x3F, 0x3F); // #3FFFFFFF
const BOX_BG: egui::Color32 = egui::Color32::from_black_alpha(0x14); // #14000000
const BOX_LINE: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x33, 0x33, 0x33, 0x33); // #33FFFFFF
const GREEN: egui::Color32 = egui::Color32::from_rgb(0x8F, 0xE3, 0xA1); // "Active cover"
const TEXT: egui::Color32 = egui::Color32::from_gray(0xF0);
const TEXTBOX_BG: egui::Color32 = egui::Color32::from_gray(0x10);
const TEXTBOX_LINE: egui::Color32 = egui::Color32::from_gray(0x6A);
const TEXTBOX_FOCUS: egui::Color32 = egui::Color32::from_gray(0xC8);
const BUTTON_BG: egui::Color32 = egui::Color32::from_gray(0x3C);
const BUTTON_HOVER: egui::Color32 = egui::Color32::from_gray(0x4A);
const BUTTON_PRESS: egui::Color32 = egui::Color32::from_gray(0x30);

const KINDS: &[(PictureType, &str)] = &[
    (PictureType::CoverFront, "Cover"),
    (PictureType::CoverBack, "Back Cover"),
    (PictureType::Icon, "Icon"),
    (PictureType::LeadArtist, "Artist"),
    (PictureType::Media, "Media"),
    (PictureType::Other, "Other"),
];

fn kind_label(t: PictureType) -> &'static str {
    KINDS.iter().find(|(k, _)| *k == t).map(|(_, l)| *l).unwrap_or("Other")
}

fn sniff_mime(data: &[u8]) -> MimeType {
    if data.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        MimeType::Png
    } else if data.starts_with(&[0xFF, 0xD8]) {
        MimeType::Jpeg
    } else if data.starts_with(b"GIF8") {
        MimeType::Gif
    } else if data.starts_with(b"BM") {
        MimeType::Bmp
    } else {
        MimeType::Jpeg
    }
}

/// Cover-fit UV rect (`UniformToFill`): crops the texture so it fills `rect`
/// without letterboxing.
pub(crate) fn cover_uv(tex: egui::Vec2, rect: egui::Vec2) -> Rect {
    let ta = tex.x / tex.y;
    let ra = rect.x / rect.y;
    let uv = if ta > ra {
        let v = ra / ta; // crop left/right
        Rect::from_min_max(egui::pos2((1.0 - v) * 0.5, 0.0), egui::pos2(1.0 - (1.0 - v) * 0.5, 1.0))
    } else {
        let v = ta / ra; // crop top/bottom
        Rect::from_min_max(egui::pos2(0.0, (1.0 - v) * 0.5), egui::pos2(1.0, 1.0 - (1.0 - v) * 0.5))
    };
    uv
}

struct ImageEntry {
    kind: PictureType,
    data: Vec<u8>,
    desc: String,
    tex: Option<egui::TextureHandle>,
    tex_failed: bool,
}

pub struct TagEditor {
    /// Still occupying the view — stays true while the dismiss
    /// fade-out runs; `ui()` clears it once the scrim is invisible.
    open: bool,
    /// Fade target: open() sets it, every dismiss path clears it.
    shown: bool,
    saved: Option<PathBuf>,
    path: PathBuf,
    /// Online media item — no file exists to tag; fields and the
    /// cover persist into the local meta cache instead (Lacrima's
    /// .meta store semantics).
    stream: bool,
    title: String,
    album: String,
    artist: String,
    track: String,
    year: String,
    genres: String,
    comment: String,
    lyrics: String,
    images: Vec<ImageEntry>,
    add_kind: PictureType,
    add_path: String,
    err: Option<String>,
    dirty: bool,
    /// App's music-note cover, used for entries whose bytes don't decode
    /// (linked art, BMP/TIFF outside our `image` features).
    placeholder: Option<egui::TextureHandle>,
}

impl Default for TagEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl TagEditor {
    pub fn new() -> Self {
        Self {
            open: false,
            shown: false,
            saved: None,
            path: PathBuf::new(),
            stream: false,
            title: String::new(),
            album: String::new(),
            artist: String::new(),
            track: String::new(),
            year: String::new(),
            genres: String::new(),
            comment: String::new(),
            lyrics: String::new(),
            images: Vec::new(),
            add_kind: PictureType::CoverFront,
            add_path: String::new(),
            err: None,
            dirty: false,
            placeholder: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Back button / Esc / Close — dismiss without saving; the overlay
    /// keeps rendering until the fade-out finishes.
    pub fn close(&mut self) {
        self.shown = false;
    }

    /// A save landed this frame — carries the written path so the caller
    /// can invalidate cover caches / re-read track info.
    pub fn take_saved(&mut self) -> Option<PathBuf> {
        self.saved.take()
    }

    pub fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        *self = Self::new();
        self.open = true;
        self.shown = true;
        self.path = path;
        self.stream = crate::library::is_url(&self.path);
        // animate_bool jumps to its target on the first call — seed the
        // id at 0 so the first painted frame actually fades in.
        ctx.animate_bool(egui::Id::new("meta_editor").with(&self.path), false);
        if self.stream {
            self.open_stream();
            return;
        }
        match Probe::open(&self.path).and_then(|p| p.read()) {
            Ok(tagged) => {
                if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
                    self.title = tag.title().unwrap_or_default().to_string();
                    self.album = tag.album().unwrap_or_default().to_string();
                    self.artist = tag.artist().unwrap_or_default().to_string();
                    self.track = tag.track().map(|t| t.to_string()).unwrap_or_default();
                    self.year = tag.year().map(|y| y.to_string()).unwrap_or_default();
                    self.genres = tag.genre().unwrap_or_default().to_string();
                    self.comment = tag.comment().unwrap_or_default().to_string();
                    self.lyrics = tag
                        .get_string(&lofty::tag::ItemKey::Lyrics)
                        .unwrap_or_default()
                        .to_string();
                    self.images = tag
                        .pictures()
                        .iter()
                        .map(|p| ImageEntry {
                            kind: p.pic_type(),
                            data: p.data().to_vec(),
                            desc: p.description().map(|d| d.to_string()).unwrap_or_default(),
                            tex: None,
                            tex_failed: false,
                        })
                        .collect();
                }
            }
            Err(e) => self.err = Some(format!("Could not read tags: {e}")),
        }
    }

    /// Stream item open — fields come from the local meta cache (same
    /// store `read_track_info` reads), the cover card from the edited
    /// local cover or the cached remote thumbnail, so the editor shows
    /// exactly what the item displays.
    fn open_stream(&mut self) {
        let url = self.path.to_string_lossy().into_owned();
        let meta = crate::meta_cache::load(&url);
        if let Some(m) = &meta {
            self.title = m.title.clone();
            self.album = m.album.clone();
            self.artist = m.artist.clone();
            self.track = m.track.clone();
            self.year = m.year.clone();
            self.genres = m.genre.clone();
            self.comment = m.comment.clone();
            self.lyrics = m.lyrics.clone();
        }
        let bytes = crate::meta_cache::load_cover(&url).or_else(|| {
            meta.as_ref()
                .and_then(|m| m.thumb.clone())
                .and_then(|t| crate::meta_cache::load_image(&t))
        });
        if let Some(data) = bytes {
            self.images.push(ImageEntry {
                kind: PictureType::CoverFront,
                data,
                desc: String::new(),
                tex: None,
                tex_failed: false,
            });
        }
    }

    fn add_image_file(&mut self, path: &Path) {
        match std::fs::read(path) {
            Ok(data) => {
                // Lacrima's AddFrontCoverCandidateImage: exactly one
                // active Cover at a time — extra cover picks are stored
                // as alternates that can be promoted via "Use as Cover".
                let kind = if self.add_kind == PictureType::CoverFront
                    && self
                        .images
                        .iter()
                        .any(|i| i.kind == PictureType::CoverFront)
                {
                    PictureType::Other
                } else {
                    self.add_kind
                };
                self.images.push(ImageEntry {
                    kind,
                    data,
                    desc: String::new(),
                    tex: None,
                    tex_failed: false,
                });
                self.dirty = true;
                self.err = None;
            }
            Err(e) => self.err = Some(format!("Could not load image: {e}")),
        }
    }

    fn save(&mut self) -> Result<(), String> {
        if self.stream {
            return self.save_stream();
        }
        let mut tagged = Probe::open(&self.path)
            .and_then(|p| p.read())
            .map_err(|e| format!("Could not open file: {e}"))?;

        if tagged.primary_tag().is_none() && tagged.first_tag().is_none() {
            tagged.insert_tag(lofty::tag::Tag::new(tagged.primary_tag_type()));
        }
        {
            let tag = match tagged.primary_tag_mut() {
                Some(t) => t,
                None => tagged
                    .first_tag_mut()
                    .ok_or_else(|| "Could not create a tag".to_string())?,
            };
            let mut set = |key: lofty::tag::ItemKey, val: &str| {
                let val = val.trim();
                if val.is_empty() {
                    tag.remove_key(&key);
                } else {
                    tag.insert_text(key, val.to_string());
                }
            };
            set(lofty::tag::ItemKey::TrackTitle, &self.title);
            set(lofty::tag::ItemKey::TrackArtist, &self.artist);
            set(lofty::tag::ItemKey::AlbumTitle, &self.album);
            set(lofty::tag::ItemKey::Genre, &self.genres);
            set(lofty::tag::ItemKey::Comment, &self.comment);
            set(lofty::tag::ItemKey::Lyrics, &self.lyrics);
            let t = self.track.trim();
            match t.parse::<u32>() {
                Ok(n) if !t.is_empty() => tag.set_track(n),
                _ => tag.remove_track(),
            }
            let y = self.year.trim();
            match y.parse::<u32>() {
                Ok(n) if !y.is_empty() => tag.set_year(n),
                _ => tag.remove_year(),
            }
            while tag.picture_count() > 0 {
                tag.remove_picture(0);
            }
            for img in &self.images {
                tag.push_picture(Picture::new_unchecked(
                    img.kind,
                    Some(sniff_mime(&img.data)),
                    if img.desc.is_empty() {
                        None
                    } else {
                        Some(img.desc.clone())
                    },
                    img.data.clone(),
                ));
            }
        }
        tagged
            .save_to_path(&self.path, WriteOptions::default())
            .map_err(|e| format!("Write failed: {e}"))
    }

    /// Stream item save — everything lands in the local meta cache;
    /// the remote source is never touched. The fetched thumbnail url
    /// is kept as the remote fallback while the active cover image is
    /// stored as the local cover override (first CoverFront entry,
    /// else the first image). No image entries -> drop the override,
    /// which returns the item to its remote thumbnail.
    fn save_stream(&mut self) -> Result<(), String> {
        let url = self.path.to_string_lossy().into_owned();
        let thumb = crate::meta_cache::load(&url).and_then(|m| m.thumb);
        crate::meta_cache::save(
            &url,
            &crate::meta_cache::CachedMeta {
                title: self.title.trim().to_string(),
                artist: self.artist.trim().to_string(),
                album: self.album.trim().to_string(),
                track: self.track.trim().to_string(),
                year: self.year.trim().to_string(),
                genre: self.genres.trim().to_string(),
                comment: self.comment.trim().to_string(),
                lyrics: self.lyrics.clone(),
                thumb,
            },
        );
        match self
            .images
            .iter()
            .find(|i| i.kind == PictureType::CoverFront)
            .or_else(|| self.images.first())
        {
            Some(img) => crate::meta_cache::save_cover(&url, &img.data),
            None => crate::meta_cache::remove_cover(&url),
        }
        Ok(())
    }

    /// Painted as the last content layer of the music view (above the
    /// carousel, below the window's rain/border effects).
    pub fn ui(&mut self, ui: &mut egui::Ui, content: Rect, s: f32) {
        let ctx = ui.ctx().clone();
        // Lacrima's DoubleTransition: Opacity over 0.32s, eased.
        let fade = ctx.animate_bool_with_time_and_easing(
            egui::Id::new("meta_editor").with(&self.path),
            self.shown,
            0.32,
            egui::emath::easing::quadratic_in_out,
        );
        if fade <= 0.0 {
            // Fade-out finished — let the caller drop the overlay. The
            // reopen seed can also yield 0 on the first painted frame
            // (elapsed=0); that must not kill the overlay.
            self.open = self.shown;
            return;
        }

        // Scrim (#E6000000) + click swallow so nothing under the overlay
        // keeps receiving pointer input.
        ui.painter().rect_filled(
            content,
            0.0,
            egui::Color32::from_black_alpha((230.0 * fade) as u8),
        );
        let _ = ui.interact(content, egui::Id::new("meta_scrim"), egui::Sense::click());

        // Root grid: Margin 24,16,24,20 — rows Auto (header) / * (panel) /
        // Auto (footer). Header and footer share the panel's edges.
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
                // Header StackPanel: Margin 6,0,0,12.
                let head = ui
                    .scope_builder(
                        UiBuilder::new()
                            .max_rect(area.with_min_x(area.min.x + 6.0 * s))
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                        |ui| self.header(ui, s),
                    )
                    .response
                    .rect;
                // Footer Grid: Margin 6,12,0,0.
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

        if self.shown && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.shown = false;
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
        let or_unknown = |v: &str| if v.trim().is_empty() { "Unknown".to_string() } else { v.to_string() };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 16.0 * s;
            ui.label(egui::RichText::new("Title").size(20.0 * s).color(SUBTEXT));
            ui.label(
                egui::RichText::new(or_unknown(&self.title))
                    .size(20.0 * s)
                    .color(egui::Color32::WHITE),
            );
            ui.label(egui::RichText::new("Artist").size(20.0 * s).color(SUBTEXT));
            ui.label(
                egui::RichText::new(or_unknown(&self.artist))
                    .size(20.0 * s)
                    .color(egui::Color32::WHITE),
            );
        });
    }

    /// Fluent-ish TextBox: dark fill, 1px gray border (light when focused).
    /// Shared with `rom_editor` (the EmulationMetadataOverlay port).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn text_box(
        ui: &mut egui::Ui,
        s: f32,
        id: egui::Id,
        text: &mut dyn egui::TextBuffer,
        width: f32,
        min_h: f32,
        font: f32,
        hint: &str,
    ) -> egui::Response {
        let focused = ui.memory(|m| m.has_focus(id));
        let margin = Margin::symmetric((8.0 * s).round() as i8, (5.0 * s).round() as i8);
        let frame = egui::Frame::new()
            .fill(TEXTBOX_BG)
            .stroke(Stroke::new(1.0, if focused { TEXTBOX_FOCUS } else { TEXTBOX_LINE }))
            .corner_radius(CornerRadius::same((4.0 * s).round() as u8))
            .inner_margin(margin);
        let multiline = min_h > 0.0;
        let edit = if multiline {
            egui::TextEdit::multiline(text).desired_rows(1)
        } else {
            egui::TextEdit::singleline(text)
        };
        ui.add(
            edit.id(id)
                .frame(frame)
                .margin(margin)
                .font(egui::FontId::proportional(font * s))
                .text_color(TEXT)
                .hint_text(hint)
                .desired_width(width)
                .min_size(egui::vec2(width, min_h)),
        )
    }

    pub(crate) fn button(
        ui: &mut egui::Ui,
        text: &str,
        min: egui::Vec2,
        pad: egui::Vec2,
    ) -> egui::Response {
        ui.scope(|ui| {
            ui.spacing_mut().button_padding = pad;
            ui.add(egui::Button::new(text).min_size(min))
        })
        .inner
    }

    /// Image-kind ComboBox styled like the text boxes. Returns `true`
    /// when the selection changed.
    fn kind_combo(
        ui: &mut egui::Ui,
        s: f32,
        salt: impl std::hash::Hash + std::fmt::Debug,
        width: f32,
        kind: &mut PictureType,
    ) -> bool {
        ui.scope(|ui| {
            let w = &mut ui.visuals_mut().widgets;
            for v in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
                v.weak_bg_fill = TEXTBOX_BG;
                v.bg_stroke = Stroke::new(1.0, TEXTBOX_LINE);
            }
            w.hovered.bg_stroke.color = TEXTBOX_FOCUS;
            w.open.bg_stroke.color = TEXTBOX_FOCUS;
            let mut changed = false;
            egui::ComboBox::from_id_salt(salt)
                .selected_text(egui::RichText::new(kind_label(*kind)).size(14.0 * s))
                .width(width)
                .show_ui(ui, |ui| {
                    for (k, l) in KINDS {
                        if ui
                            .selectable_label(*kind == *k, egui::RichText::new(*l).size(14.0 * s))
                            .clicked()
                            && *kind != *k
                        {
                            *kind = *k;
                            changed = true;
                        }
                    }
                });
            changed
        })
        .inner
    }

    /// `StackPanel Spacing=4`: label (14) above a TextBox (16); the
    /// enclosing StackPanel's Spacing=10 separates the groups.
    #[allow(clippy::too_many_arguments)]
    fn field(
        ui: &mut egui::Ui,
        s: f32,
        label: &str,
        value: &mut String,
        dirty: &mut bool,
        width: f32,
        min_h: f32,
    ) {
        ui.label(egui::RichText::new(label).size(14.0 * s).color(SUBTEXT));
        let id = egui::Id::new(("meta_field", label));
        if Self::text_box(ui, s, id, value, width, min_h, 16.0, "").changed() {
            *dirty = true;
        }
        ui.add_space(6.0 * s);
    }

    fn panel(&mut self, ui: &mut egui::Ui, rect: Rect, s: f32) {
        // Border: #1AFFFFFF / #3FFFFFFF, CornerRadius 10, Padding 12.
        ui.painter().rect(
            rect,
            CornerRadius::same((10.0 * s).round() as u8),
            PANEL_BG,
            Stroke::new(1.0, PANEL_LINE),
            egui::StrokeKind::Inside,
        );
        let inner = rect.shrink(13.0 * s);
        // ColumnDefinitions="340,*" ColumnSpacing="12".
        let gap = 12.0 * s;
        let left_w = (340.0 * s).min((inner.width() - gap) * 0.5);
        let details = Rect::from_min_size(inner.min, egui::vec2(left_w, inner.height()));
        let images = Rect::from_min_max(egui::pos2(details.max.x + gap, inner.min.y), inner.max);

        for (r, is_details) in [(details, true), (images, false)] {
            ui.scope_builder(
                UiBuilder::new()
                    .max_rect(r)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
                |ui| {
                    ui.set_clip_rect(r.expand(2.0).intersect(ui.clip_rect()));
                    if is_details {
                        self.details(ui, s);
                    } else {
                        self.images_ui(ui, s);
                    }
                },
            );
        }
    }

    fn details(&mut self, ui: &mut egui::Ui, s: f32) {
        // RowDefinitions="Auto,*" RowSpacing="8".
        ui.spacing_mut().item_spacing.y = 8.0 * s;
        ui.label(
            egui::RichText::new("Details")
                .size(22.0 * s)
                .strong()
                .color(egui::Color32::WHITE),
        );
        egui::ScrollArea::vertical()
            .id_salt("meta_details_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0 * s;
                let wide = ui.available_width();
                let short = (120.0 * s).min(wide);
                let d = &mut self.dirty;
                Self::field(ui, s, "Title", &mut self.title, d, wide, 0.0);
                Self::field(ui, s, "Album", &mut self.album, d, wide, 0.0);
                Self::field(ui, s, "Artists", &mut self.artist, d, wide, 0.0);
                Self::field(ui, s, "Track", &mut self.track, d, short, 0.0);
                Self::field(ui, s, "Year", &mut self.year, d, short, 0.0);
                Self::field(ui, s, "Genre", &mut self.genres, d, wide, 0.0);
                Self::field(ui, s, "Comments", &mut self.comment, d, wide, 0.0);
                Self::field(ui, s, "Lyrics", &mut self.lyrics, d, wide, 90.0 * s);

                // File Path (FontSize 14, wrapping) — read-only here.
                ui.label(egui::RichText::new("File Path").size(14.0 * s).color(SUBTEXT));
                let fp = self.path.to_string_lossy().to_string();
                Self::text_box(
                    ui,
                    s,
                    egui::Id::new("meta_file_path"),
                    &mut fp.as_str(),
                    wide,
                    1.0,
                    14.0,
                    "",
                );
            });
    }

    fn pick_image(&mut self) {
        if let Some(f) = rfd::FileDialog::new()
            .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif", "bmp"])
            .pick_file()
        {
            self.add_image_file(&f);
        }
    }

    fn images_ui(&mut self, ui: &mut egui::Ui, s: f32) {
        // RowDefinitions="Auto,Auto,*" RowSpacing="8".
        ui.spacing_mut().item_spacing.y = 8.0 * s;
        let pad = egui::vec2(12.0, 6.0) * s;

        // Header: "Images" | kind ComboBox (MinWidth 140) | Add Image,
        // ColumnSpacing 10.
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
                if Self::button(ui, "Add Image", egui::Vec2::ZERO, pad).clicked() {
                    self.pick_image();
                }
                Self::kind_combo(ui, s, "meta_add_kind", 140.0 * s, &mut self.add_kind);
            },
        );

        // "Cover From …" box (Lacrima's URL/clipboard/search grid,
        // ColumnDefinitions="*,104,104"; the file-path row is the offline
        // equivalent).
        egui::Frame::new()
            .fill(BOX_BG)
            .stroke(Stroke::new(1.0, BOX_LINE))
            .corner_radius(CornerRadius::same((8.0 * s).round() as u8))
            .inner_margin(Margin::same((12.0 * s).round() as i8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0) * s;
                ui.label(
                    egui::RichText::new("Cover From File")
                        .size(16.0 * s)
                        .strong()
                        .color(egui::Color32::WHITE),
                );
                ui.horizontal(|ui| {
                    let bw = 104.0 * s;
                    let tw = (ui.available_width() - 2.0 * (bw + 8.0 * s)).max(60.0 * s);
                    let resp = Self::text_box(
                        ui,
                        s,
                        egui::Id::new("meta_add_path"),
                        &mut self.add_path,
                        tw,
                        0.0,
                        14.0,
                        "Paste image file path...",
                    );
                    let enter = resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        && !self.add_path.trim().is_empty();
                    let bsize = egui::vec2(bw, resp.rect.height());
                    if Self::button(ui, "Add File", bsize, pad).clicked() || enter {
                        let p = PathBuf::from(self.add_path.trim().trim_matches('"'));
                        self.add_image_file(&p);
                        if self.err.is_none() {
                            self.add_path.clear();
                        }
                    }
                    if Self::button(ui, "Browse...", bsize, pad).clicked() {
                        self.pick_image();
                    }
                });
            });

        // Image cards: WrapPanel of 220px cards, Margin 0,0,8,8.
        egui::ScrollArea::vertical()
            .id_salt("meta_images_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.placeholder.is_none() {
                    self.placeholder = Some(ui.ctx().load_texture(
                        "meta_cover_placeholder",
                        crate::library::placeholder_image(),
                        egui::TextureOptions::LINEAR,
                    ));
                }
                let ph = self.placeholder.clone();
                let card_w = 220.0 * s;
                let gap = 8.0 * s;
                // Border 1 + Padding 12 on each side.
                let inner_w = card_w - 26.0 * s;
                let cols = ((ui.available_width() + gap) / (card_w + gap))
                    .floor()
                    .max(1.0) as usize;
                let mut remove: Option<usize> = None;
                let mut front: Option<usize> = None;
                let mut dirty = false;
                // The single active cover (Lacrima's IsActiveFrontCover);
                // every other card offers "Use as Cover".
                let active = self
                    .images
                    .iter()
                    .position(|i| i.kind == PictureType::CoverFront);
                for row_start in (0..self.images.len()).step_by(cols) {
                    let row_end = (row_start + cols).min(self.images.len());
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        for i in row_start..row_end {
                            let img = &mut self.images[i];
                            egui::Frame::new()
                                .fill(BOX_BG)
                                .stroke(Stroke::new(1.0, BOX_LINE))
                                .corner_radius(CornerRadius::same((8.0 * s).round() as u8))
                                .inner_margin(Margin::same((12.0 * s).round() as i8))
                                .show(ui, |ui| {
                                    // The Frame's child inherits the row's
                                    // horizontal layout — stack explicitly.
                                    ui.vertical(|ui| {
                                        ui.set_width(inner_w);
                                        ui.spacing_mut().item_spacing.y = 8.0 * s;
                                        if Self::kind_combo(ui, s, ("meta_kind", i), inner_w, &mut img.kind) {
                                            dirty = true;
                                            if img.kind == PictureType::CoverFront {
                                                front = Some(i);
                                            }
                                        }
                                        Self::preview(ui, img, i, ph.as_ref(), inner_w, 160.0 * s);
                                        if !img.desc.is_empty() {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(&img.desc)
                                                        .size(13.0 * s)
                                                        .color(FAINT),
                                                )
                                                .truncate(),
                                            );
                                        }
                                        if active == Some(i) {
                                            ui.label(
                                                egui::RichText::new("Active cover")
                                                    .size(13.0 * s)
                                                    .strong()
                                                    .color(GREEN),
                                            );
                                        } else if Self::button(
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
                                                if Self::button(ui, "Remove", egui::Vec2::ZERO, pad)
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
                    ui.add_space(gap - ui.spacing().item_spacing.y);
                }
                if let Some(i) = front {
                    for (j, im) in self.images.iter_mut().enumerate() {
                        if j != i && im.kind == PictureType::CoverFront {
                            im.kind = PictureType::Other;
                        }
                    }
                    self.images[i].kind = PictureType::CoverFront;
                    dirty = true;
                }
                if let Some(i) = remove {
                    self.images.remove(i);
                    dirty = true;
                }
                self.dirty |= dirty;
            });
    }

    /// `Image Stretch=UniformToFill Height=160` — decodes the embedded
    /// bytes once; undecodable art falls back to the music-note cover.
    fn preview(
        ui: &mut egui::Ui,
        img: &mut ImageEntry,
        i: usize,
        ph: Option<&egui::TextureHandle>,
        w: f32,
        h: f32,
    ) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
        if img.tex.is_none() && !img.tex_failed {
            match image::load_from_memory(&img.data) {
                Ok(c) => {
                    let rgba = c.to_rgba8();
                    let (tw, th) = rgba.dimensions();
                    img.tex = Some(ui.ctx().load_texture(
                        format!("meta_img_{i}_{}", img.data.len()),
                        egui::ColorImage::from_rgba_unmultiplied([tw as usize, th as usize], &rgba),
                        egui::TextureOptions::LINEAR,
                    ));
                }
                Err(_) => img.tex_failed = true,
            }
        }
        if let Some(tex) = img.tex.as_ref().or(ph) {
            ui.painter().image(
                tex.id(),
                rect,
                cover_uv(tex.size_vec2(), rect.size()),
                if img.tex.is_some() {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::from_white_alpha(110)
                },
            );
        }
    }

    fn footer(&mut self, ui: &mut egui::Ui, rect: Rect, s: f32) {
        // ColumnDefinitions="*,Auto,Auto" ColumnSpacing="10"; buttons
        // Padding 14,8, right edge flush with the panel's.
        let mut clicked = None;
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing.x = 10.0 * s;
                let pad = egui::vec2(14.0, 8.0) * s;
                if Self::button(ui, "Close", egui::Vec2::ZERO, pad).clicked() {
                    clicked = Some("close");
                }
                if Self::button(ui, "Save", egui::Vec2::ZERO, pad).clicked() {
                    clicked = Some("save");
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new("Edit metadata and images for the selected track.")
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
            Some("close") => self.shown = false,
            Some("save") => match self.save() {
                Ok(()) => {
                    self.saved = Some(self.path.clone());
                    self.dirty = false;
                    self.err = None;
                    // Lacrima's SaveMetadataAsync ends with Close().
                    self.shown = false;
                }
                Err(e) => self.err = Some(e),
            },
            _ => {}
        }
    }
}

/// Lacrima's overlay is laid out in the same scaled space as the rest of
/// the UI; egui's stock widgets aren't, so scale their fonts/metrics by
/// `s` and give them the Fluent dark look (borderless gray buttons).
pub(crate) fn scale_style(style: &mut egui::Style, s: f32) {
    use egui::{FontId, TextStyle};
    style.text_styles = [
        (TextStyle::Small, FontId::proportional(12.0 * s)),
        (TextStyle::Body, FontId::proportional(16.0 * s)),
        (TextStyle::Button, FontId::proportional(14.0 * s)),
        (TextStyle::Heading, FontId::proportional(22.0 * s)),
        (TextStyle::Monospace, FontId::monospace(14.0 * s)),
    ]
    .into();
    let sp = &mut style.spacing;
    sp.item_spacing = egui::vec2(8.0, 8.0) * s;
    sp.button_padding = egui::vec2(12.0, 6.0) * s;
    sp.interact_size = egui::vec2(40.0, 20.0) * s;
    sp.icon_width = 14.0 * s;
    sp.icon_width_inner = 8.0 * s;
    sp.icon_spacing = 6.0 * s;
    sp.combo_width = 140.0 * s;
    sp.scroll.bar_width *= s;
    sp.scroll.floating_width *= s;
    sp.scroll.floating_allocated_width *= s;
    sp.scroll.bar_inner_margin *= s;
    sp.scroll.bar_outer_margin *= s;

    let r = CornerRadius::same((4.0 * s).round() as u8);
    let w = &mut style.visuals.widgets;
    for (v, fill) in [
        (&mut w.inactive, BUTTON_BG),
        (&mut w.hovered, BUTTON_HOVER),
        (&mut w.active, BUTTON_PRESS),
        (&mut w.open, BUTTON_HOVER),
    ] {
        v.weak_bg_fill = fill;
        v.bg_fill = fill;
        v.bg_stroke = Stroke::NONE;
        v.corner_radius = r;
        v.expansion = 0.0;
        v.fg_stroke.color = TEXT;
    }
}

