//! The Emulation view prototype — Lacrima's EmulationView chrome:
//! `Grid RowDefinitions="*,Auto,Auto"` = the item view on top, the
//! 140px control band (section-list toggle left, cover-layout toggle
//! right), and the collapsible console-section strip at the bottom.
//! The item area is a placeholder until ROM scanning lands; the
//! settings tab renders the Emulation sections with the console cards
//! expandable but no handler controls wired yet.

use std::collections::HashSet;
use std::path::PathBuf;

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Layout, Pos2, Rect, Sense,
    Stroke, StrokeKind, UiBuilder, Vec2,
};

use crate::{album_row, library, paint, search_pill, settings, transport};

/// Console sections — (key, display title, art asset stem). Lacrima
/// *discovers* sections by enumerating Assets/Consoles (a console with
/// no art file gets no card — Nintendo DS has none), orders them by
/// filename (that's why GameCube's `GC.png` precedes `Genesis.png`),
/// and derives titles via the catalog's display-name lookup.
const CONSOLES: &[(&str, &str, &str)] = &[
    ("3DS", "Nintendo 3DS", "3DS"),
    ("ARCADE", "Arcade", "Arcade"),
    ("DC", "Dreamcast", "Dreamcast"),
    ("FBN", "Final Burn Neo", "FBN"),
    ("GBA", "Game Boy Advance", "GBA"),
    ("GCN", "Nintendo GameCube", "GC"),
    ("GENESIS", "Sega Genesis", "Genesis"),
    ("N64", "Nintendo 64", "N64"),
    ("NES", "Nintendo Entertainment System", "NES"),
    ("PS2", "PlayStation 2", "PS2"),
    ("PS3", "PlayStation 3", "PS3"),
    ("PS4", "PlayStation 4", "PS4"),
    ("PSP", "PlayStation Portable", "PSP"),
    ("PSX", "PlayStation", "PSX"),
    ("SNES", "Super Nintendo", "SNES"),
    ("SATURN", "Sega Saturn", "Saturn"),
    ("STEAM", "Steam", "Steam"),
    ("SWITCH", "Nintendo Switch", "Switch"),
    ("WII", "Nintendo Wii", "Wii"),
    ("WIIU", "Nintendo Wii U", "WiiU"),
    ("X360", "Xbox 360", "X360"),
    ("XBOX", "Xbox", "Xbox"),
];

/// 384px thumbnails of Lacrima's Assets/Consoles art, embedded so the
/// exe stays self-contained. Extracted to the data dir at startup and
/// referenced as `CoverSrc::File` — the row's decode/cache path then
/// handles them like any folder cover.
const ART: &[(&str, &[u8])] = &[
    ("3DS", include_bytes!("../assets/consoles/3DS.png")),
    ("Arcade", include_bytes!("../assets/consoles/Arcade.png")),
    ("Dreamcast", include_bytes!("../assets/consoles/Dreamcast.png")),
    ("FBN", include_bytes!("../assets/consoles/FBN.png")),
    ("GBA", include_bytes!("../assets/consoles/GBA.png")),
    ("GC", include_bytes!("../assets/consoles/GC.png")),
    ("Genesis", include_bytes!("../assets/consoles/Genesis.png")),
    ("N64", include_bytes!("../assets/consoles/N64.png")),
    ("NES", include_bytes!("../assets/consoles/NES.png")),
    ("PS2", include_bytes!("../assets/consoles/PS2.png")),
    ("PS3", include_bytes!("../assets/consoles/PS3.png")),
    ("PS4", include_bytes!("../assets/consoles/PS4.png")),
    ("PSP", include_bytes!("../assets/consoles/PSP.png")),
    ("PSX", include_bytes!("../assets/consoles/PSX.png")),
    ("SNES", include_bytes!("../assets/consoles/SNES.png")),
    ("Saturn", include_bytes!("../assets/consoles/Saturn.png")),
    ("Steam", include_bytes!("../assets/consoles/Steam.png")),
    ("Switch", include_bytes!("../assets/consoles/Switch.png")),
    ("Wii", include_bytes!("../assets/consoles/Wii.png")),
    ("WiiU", include_bytes!("../assets/consoles/WiiU.png")),
    ("X360", include_bytes!("../assets/consoles/X360.png")),
    ("Xbox", include_bytes!("../assets/consoles/Xbox.png")),
];

/// Extract the embedded console art into `<data>/Assets/Consoles/`
/// (only missing/stale files are rewritten) — returns the dir.
fn extract_art() -> Option<PathBuf> {
    let dir = settings::data_root()?.join("Assets").join("Consoles");
    std::fs::create_dir_all(&dir).ok()?;
    for (name, bytes) in ART {
        let path = dir.join(format!("{name}.png"));
        let stale = std::fs::metadata(&path)
            .map(|m| m.len() != bytes.len() as u64)
            .unwrap_or(true);
        if stale {
            let _ = std::fs::write(&path, bytes);
        }
    }
    Some(dir)
}

/// Per-view state for Emulation.
pub struct EmuView {
    /// The console-section strip — Lacrima's EmulationListView shares
    /// the album-row control, so does ours.
    pub row: album_row::AlbumRow,
    /// Section tiles: one synthesized `Album` per console; `dir` is a
    /// synthetic `emu:` key and `covers` the extracted console art.
    pub sections: Vec<library::Album>,
    /// Section-strip visibility — Lacrima's IsAlbumListCollapsed
    /// (session-only, not persisted).
    pub list_open: bool,
    /// The item search pill (RomSearch) — filtering lands with items.
    pub search: search_pill::SearchPill,
    /// The selected section index — drives the item-area header.
    pub active: Option<usize>,
    /// Settings tab: which console cards are expanded (IsExpanded —
    /// session-only like Lacrima's view-model state).
    pub expanded: HashSet<String>,
}

impl EmuView {
    pub fn new() -> Self {
        let art_dir = extract_art();
        let sections = CONSOLES
            .iter()
            .map(|(key, title, art)| library::Album {
                title: (*title).to_string(),
                dir: PathBuf::from(format!("emu:{key}")),
                tracks: Vec::new(),
                covers: if art.is_empty() {
                    Vec::new()
                } else {
                    art_dir
                        .as_ref()
                        .map(|d| vec![library::CoverSrc::File(d.join(format!("{art}.png")))])
                        .unwrap_or_default()
                },
                url_count: 0,
            })
            .collect();
        Self {
            row: album_row::AlbumRow::new(),
            sections,
            list_open: true,
            search: search_pill::SearchPill::default(),
            active: None,
            expanded: HashSet::new(),
        }
    }
}

/// The Emulation view: item area, control band, section strip.
#[allow(clippy::too_many_arguments)]
pub fn view(
    ui: &mut egui::Ui,
    content: Rect,
    alpha: f32,
    interactive: bool,
    emu: &mut EmuView,
    cover_layout: &mut u8,
    radius: u8,
    s: f32,
) {
    let row_rect = crate::album_row_rect(content, s);
    // Section-strip collapse — Lacrima's MaxHeightToggleBehavior
    // (0.55s CubicEaseInOut), the same animation the music strip uses.
    let list_t = crate::ease_in_out_cubic(ui.ctx().animate_bool_with_time(
        ui.id().with("emu-sections-open"),
        emu.list_open,
        0.55,
    ));
    let menu_top = content.max.y - (content.max.y - row_rect.min.y) * list_t;
    let band_top = (menu_top - crate::TRANSPORT_BAND_H * s).max(content.min.y);
    let band = Rect::from_min_max(
        Pos2::new(content.min.x, band_top),
        Pos2::new(content.max.x, menu_top),
    );
    let menu_band = Rect::from_min_max(Pos2::new(content.min.x, menu_top), content.max);
    let upper = Rect::from_min_max(content.min, Pos2::new(content.max.x, band.min.y));

    let painter = ui.painter().clone();
    // Row 0's Border Background=#FF000000 — a pure black item area.
    painter.rect_filled(upper, 0.0, Color32::BLACK.gamma_multiply(alpha));

    // Placeholder item view — the ROM cover view lands here once
    // scanning exists; until then the selected section's name stands
    // in (Lacrima's "No album loaded" when nothing is selected).
    let title = emu.active.and_then(|i| emu.sections.get(i));
    match title {
        Some(sec) => {
            let t = sec.title.clone();
            painter.text(
                upper.center() - Vec2::new(0.0, 20.0 * s),
                Align2::CENTER_CENTER,
                t,
                FontId::proportional(40.0 * s),
                Color32::WHITE.gamma_multiply(alpha),
            );
            painter.text(
                upper.center() + Vec2::new(0.0, 22.0 * s),
                Align2::CENTER_CENTER,
                "No items",
                FontId::proportional(22.0 * s),
                Color32::from_gray(120).gamma_multiply(alpha),
            );
        }
        None => {
            painter.text(
                upper.center(),
                Align2::CENTER_CENTER,
                "No album loaded",
                FontId::proportional(30.0 * s),
                Color32::from_gray(110).gamma_multiply(alpha),
            );
        }
    }

    // Row 1 — the 140px control band (Background #E6000000 plus the
    // top Separator that every Lacrima chrome band draws).
    painter.rect_filled(band, 0.0, Color32::from_black_alpha((230.0 * alpha) as u8));
    let line = Stroke::new(
        s.max(1.0),
        Color32::from_white_alpha((51.0 * alpha) as u8),
    );
    painter.hline(content.min.x..=content.max.x, band.min.y, line);
    if list_t > 0.001 {
        painter.hline(content.min.x..=content.max.x, menu_top, line);
        painter.rect_filled(
            menu_band,
            CornerRadius {
                nw: 0,
                ne: 0,
                sw: radius,
                se: radius,
            },
            Color32::BLACK.gamma_multiply(alpha * list_t),
        );
    }

    // Left: the section-list toggle (StackPanel Margin 50,0,0,0,
    // 60px button — the bar-over-square glyph, lit while open).
    let cy = band.center().y;
    let btn = Rect::from_center_size(
        Pos2::new(band.min.x + 50.0 * s + 30.0 * s, cy),
        Vec2::splat(60.0 * s),
    );
    let r = if interactive {
        ui.interact(btn, ui.id().with("emu-list-toggle"), Sense::click())
    } else {
        ui.interact(btn, ui.id().with("emu-list-toggle"), Sense::hover())
    };
    if r.clicked() {
        emu.list_open = !emu.list_open;
    }
    if r.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    transport::icon_album_list(&painter, btn, emu.list_open, alpha);

    // Right: the cover-layout cycle (Margin 0,0,50,0) — Lacrima binds
    // the same global CoverLayoutMode the music band uses.
    let btn = Rect::from_center_size(
        Pos2::new(band.max.x - 50.0 * s - 30.0 * s, cy),
        Vec2::splat(60.0 * s),
    );
    let r = if interactive {
        ui.interact(btn, ui.id().with("emu-layout"), Sense::click())
    } else {
        ui.interact(btn, ui.id().with("emu-layout"), Sense::hover())
    };
    if r.clicked() {
        *cover_layout = (*cover_layout + 1) % 3;
    }
    if r.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    transport::icon_layout(&painter, btn, *cover_layout, alpha);

    // Row 2 — the section strip slides down out of view while the
    // band holds (same trick as the music album row).
    if list_t > 0.001 {
        let slide = row_rect.translate(Vec2::new(
            0.0,
            (1.0 - list_t) * (content.max.y - row_rect.min.y),
        ));
        match emu.row.ui(
            ui,
            slide,
            &mut emu.sections,
            alpha * list_t,
            interactive && list_t > 0.999,
            s,
            false,
            false,
        ) {
            album_row::Action::Selected(i) | album_row::Action::Opened(i) => {
                emu.active = Some(i);
            }
            _ => {}
        }
    }

    // Item search pill docked top-right (RomSearch — same slot as the
    // music view's item search).
    emu.search.ui(
        ui,
        content.max.x - 30.0 * s,
        content.min.y + 26.0 * s,
        "Search",
        true,
        false,
        alpha,
        interactive,
        s,
    );
}

/// The Settings-overlay Emulation tab (Lacrima's SettingsOverlay
/// "Emulation" TabItem): Miscellaneous toggles, the Gameplay
/// Recording output folder, and the expandable Consoles cards.
/// Handler controls render as placeholders — wiring lands with the
/// emulator-session work.
pub fn settings_tab(
    ui: &mut egui::Ui,
    panel: Rect,
    t: f32,
    emu: &mut EmuView,
    cfg: &mut settings::Emulation,
    s: f32,
) {
    // Same scrollable-body pattern as the Audio tab: paint into the
    // panel, clip to the band between the tab strip and the footer,
    // track the offset in persisted data.
    // Clip ends just above the footer separator (-70s) — content must
    // never underlap the "Open AES Directory" row.
    let body = Rect::from_min_max(
        Pos2::new(panel.min.x, panel.min.y + 78.0 * s),
        Pos2::new(panel.max.x, panel.max.y - 74.0 * s),
    );
    let scroll_id = ui.id().with("emu-scroll");
    let mut scroll = ui
        .ctx()
        .data_mut(|d| d.get_persisted::<f32>(scroll_id).unwrap_or(0.0));
    if ui
        .ctx()
        .input(|i| i.pointer.hover_pos().is_some_and(|p| body.contains(p)))
    {
        let dy = ui.ctx().input(|i| i.smooth_scroll_delta.y);
        if dy != 0.0 {
            scroll -= dy;
        }
    }
    scroll = scroll.max(0.0);

    let mut cui = ui.new_child(UiBuilder::new().max_rect(panel));
    cui.set_clip_rect(body);
    let painter = cui.painter().clone();
    let x = panel.min.x + 30.0 * s;
    let right = panel.max.x - 30.0 * s;
    let mut y = panel.min.y + 95.0 * s - scroll;
    let whitesmoke = Color32::from_rgb(0xF5, 0xF5, 0xF5);
    let darkgray = Color32::from_gray(0xA9);

    let header = |painter: &egui::Painter, y: &mut f32, name: &str| {
        painter.text(
            Pos2::new(x, *y),
            Align2::LEFT_TOP,
            name,
            FontId::proportional(30.0 * s),
            Color32::WHITE.gamma_multiply(t),
        );
        *y += 46.0 * s;
        painter.line_segment(
            [Pos2::new(x, *y), Pos2::new(right, *y)],
            Stroke::new(1.0, Color32::from_gray(55).gamma_multiply(t)),
        );
        *y += 25.0 * s;
    };
    let label = FontId::proportional(20.0 * s);
    let toggle_row = |ui: &mut egui::Ui, y: f32, name: &str, on: &mut bool, salt: &str| {
        painter.text(
            Pos2::new(x, y),
            Align2::LEFT_TOP,
            name,
            label.clone(),
            whitesmoke.gamma_multiply(t),
        );
        let w = painter
            .layout_no_wrap(name.to_owned(), label.clone(), Color32::WHITE)
            .rect
            .width();
        let sw = Rect::from_min_size(
            Pos2::new(x + w + 8.0 * s, y - 2.0 * s),
            Vec2::new(44.0 * s, 22.0 * s),
        );
        crate::toggle_switch(ui, sw, salt, on, t);
    };

    // --- Miscellaneous ------------------------------------------------
    header(&painter, &mut y, "Miscellaneous");
    toggle_row(
        &mut cui,
        y,
        "Use first ROM item cover for album tiles",
        &mut cfg.use_first_item_cover,
        "emu-first-cover",
    );
    y += 40.0 * s;
    toggle_row(
        &mut cui,
        y,
        "Gameplay Autoplay",
        &mut cfg.gameplay_autoplay,
        "emu-autoplay",
    );
    y += 40.0 * s;
    toggle_row(
        &mut cui,
        y,
        "Fill letterbox bars with back cover art",
        &mut cfg.back_cover_letterbox_fill,
        "emu-letterbox",
    );
    y += 55.0 * s;

    // --- Gameplay Recording -------------------------------------------
    header(&painter, &mut y, "Gameplay Recording");
    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_TOP,
        "Output folder",
        label.clone(),
        whitesmoke.gamma_multiply(t),
    );
    y += 34.0 * s;
    // TextBox (#2B2B2B fill, DimGray border) + Browse button, Lacrima's
    // "*,Auto" column pair with 10px spacing.
    let btn_w = 90.0 * s;
    let box_rect = Rect::from_min_size(
        Pos2::new(x, y),
        Vec2::new((right - x - btn_w - 10.0 * s).max(60.0 * s), 30.0 * s),
    );
    let mut tui = cui.new_child(
        UiBuilder::new()
            .max_rect(box_rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    tui.set_opacity(t);
    tui.add(
        egui::TextEdit::singleline(&mut cfg.recording_dir)
            .desired_width(box_rect.width())
            .hint_text("Folder for recorded videos")
            .font(FontId::proportional(16.0 * s)),
    );
    painter.rect_filled(
        box_rect,
        CornerRadius::same(4),
        Color32::from_rgb(0x2B, 0x2B, 0x2B).gamma_multiply(0.4 * t),
    );
    painter.rect_stroke(
        box_rect,
        CornerRadius::same(4),
        Stroke::new(1.0, Color32::from_gray(105).gamma_multiply(t)),
        StrokeKind::Outside,
    );
    let (clicked, _) = crate::flat_button(
        &mut cui,
        Pos2::new(box_rect.max.x + 10.0 * s, y - 1.0 * s),
        "Browse",
        s,
        true,
        Color32::from_rgb(0x22, 0x22, 0x22),
        Color32::WHITE,
        t,
    );
    if clicked {
        if let Some(dir) = rfd::FileDialog::new()
            .set_title("Folder for recorded videos")
            .pick_folder()
        {
            cfg.recording_dir = dir.to_string_lossy().into_owned();
        }
    }
    y += 62.0 * s;

    // --- Consoles -----------------------------------------------------
    header(&painter, &mut y, "Consoles");
    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_TOP,
        "Expand a console section to configure the supported emulator handlers for it.",
        FontId::proportional(16.0 * s),
        darkgray.gamma_multiply(t),
    );
    y += 40.0 * s;

    // Section cards: #151515 fill, #343434 border, radius 8, 12px gap
    // — Lacrima's EmulationSectionItem template.
    for (key, title, art) in CONSOLES {
        let open = emu.expanded.contains(*key);
        let head_h = 88.0 * s;
        let body_h = if open { 74.0 * s } else { 0.0 };
        let card = Rect::from_min_size(
            Pos2::new(x, y),
            Vec2::new(right - x, head_h + body_h),
        );
        painter.rect_filled(
            card,
            CornerRadius::same((8.0 * s) as u8),
            Color32::from_rgb(0x15, 0x15, 0x15).gamma_multiply(t),
        );
        painter.rect_stroke(
            card,
            CornerRadius::same((8.0 * s) as u8),
            Stroke::new(1.0, Color32::from_rgb(0x34, 0x34, 0x34).gamma_multiply(t)),
            StrokeKind::Inside,
        );
        let head = Rect::from_min_size(card.min, Vec2::new(card.width(), head_h));
        let resp = cui.interact(head, cui.id().with(("emu-section", key)), Sense::click());
        if resp.clicked() {
            if open {
                emu.expanded.remove(*key);
            } else {
                emu.expanded.insert((*key).to_string());
            }
        }
        if resp.hovered() {
            cui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        // 96x60 console-art thumb (border + corner clip), title, then
        // the +/- expander glyph on the right.
        let thumb = Rect::from_min_size(
            card.min + Vec2::new(14.0 * s, 14.0 * s),
            Vec2::new(96.0 * s, 60.0 * s),
        );
        painter.rect_filled(
            thumb,
            CornerRadius::same((8.0 * s) as u8),
            Color32::BLACK.gamma_multiply(t),
        );
        if !art.is_empty() {
            let src = library::CoverSrc::File(
                settings::data_root()
                    .map(|d| d.join("Assets").join("Consoles").join(format!("{art}.png")))
                    .unwrap_or_default(),
            );
            if let Some(tex) = emu.row.cover_tex(ui.ctx(), &src) {
                paint::paint_cover(
                    &painter,
                    tex.id(),
                    tex.size(),
                    thumb.shrink(1.0),
                    8.0 * s,
                    t,
                );
            }
        }
        painter.rect_stroke(
            thumb,
            CornerRadius::same((8.0 * s) as u8),
            Stroke::new(1.0, Color32::from_rgb(0x34, 0x34, 0x34).gamma_multiply(t)),
            StrokeKind::Inside,
        );
        painter.text(
            Pos2::new(thumb.max.x + 14.0 * s, head.center().y),
            Align2::LEFT_CENTER,
            *title,
            label.clone(),
            whitesmoke.gamma_multiply(t),
        );
        painter.text(
            Pos2::new(card.max.x - 14.0 * s - 14.0 * s, head.center().y),
            Align2::CENTER_CENTER,
            if open { "-" } else { "+" },
            FontId::proportional(24.0 * s),
            darkgray.gamma_multiply(t),
        );
        if open {
            // Handler list placeholder — the Set default / Browse /
            // Clear / Download rows land with the handler work; for
            // now the empty-state note fills the card body.
            let inner = Rect::from_min_size(
                Pos2::new(card.min.x + 14.0 * s, card.min.y + head_h),
                Vec2::new(card.width() - 28.0 * s, body_h - 14.0 * s),
            );
            painter.rect_filled(
                inner,
                CornerRadius::same((8.0 * s) as u8),
                Color32::from_rgb(0x1D, 0x1D, 0x1D).gamma_multiply(t),
            );
            painter.rect_stroke(
                inner,
                CornerRadius::same((8.0 * s) as u8),
                Stroke::new(1.0, Color32::from_rgb(0x34, 0x34, 0x34).gamma_multiply(t)),
                StrokeKind::Inside,
            );
            painter.text(
                inner.center(),
                Align2::CENTER_CENTER,
                "No emulator handlers configured for this section yet",
                FontId::proportional(16.0 * s),
                darkgray.gamma_multiply(t),
            );
        }
        y += card.height() + 12.0 * s;
    }

    // Scroll bookkeeping: clamp against the real content height and
    // show the thin scrollbar thumb when it overflows.
    let content_h = (y + scroll) - (panel.min.y + 95.0 * s);
    let max_scroll = (content_h - body.height()).max(0.0);
    scroll = scroll.min(max_scroll);
    ui.ctx().data_mut(|d| d.insert_persisted(scroll_id, scroll));
    if max_scroll > 1.0 {
        let thumb_h = (body.height() * (body.height() / content_h)).max(24.0);
        let thumb_y = body.min.y + (body.height() - thumb_h) * (scroll / max_scroll);
        let thumb = Rect::from_min_size(
            Pos2::new(body.max.x - 5.0, thumb_y),
            Vec2::new(3.0, thumb_h),
        );
        ui.painter().rect_filled(
            thumb,
            CornerRadius::same(2),
            Color32::from_white_alpha(70).gamma_multiply(t),
        );
    }
}
