//! The loaded album's track view — a port of Lacrima's
//! `CompositionCoverControl` covering all three `CoverLayoutMode`s:
//! the perspective card carousel (`CompositionCarouselVisualHandler`),
//! the vertical card grid and the horizontal card grid
//! (`CardGridLayoutHelper` / `CardGridHorizontalLayout`), plus the
//! `PillProgressSlider` position pill and the centered title overlay.
//!
//! Physics are the same spring the compositor uses: stiffness 78,
//! damping 2*sqrt(78)*0.98, direct-follow (tau 35ms) while pointer
//! scrolling, velocity fling on release, wheel impulses clamped to
//! ±14 items/s.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use eframe::egui::epaint::Vertex;
use eframe::egui::{
    self, Align2, Color32, FontId, Mesh, Pos2, Rect, Sense, Shape, Stroke, Vec2,
};

use crate::album_row::AlbumRow;
use crate::library::{Album, CoverSrc, TrackInfo};
use crate::settings;

// ------------------------------------------------------------------
// Lacrima tuning constants (CompositionCarouselVisualHandler)
// ------------------------------------------------------------------
/// Item frame — Lacrima's ItemWidth x ItemHeight (240x200).
const BASE_W: f32 = 240.0;
const BASE_H: f32 = 200.0;
const SIDE_ROT: f32 = 0.95;
const PROJECTION_DIST: f32 = 1000.0;
const VISIBLE_RANGE: f32 = 5.0;
/// Px of pointer travel per item index while dragging.
const DRAG_PX_PER_ITEM: f32 = 250.0;
const DRAG_START_PX: f32 = 8.0;
/// Wheel impulse scale + clamp (items/sec).
const WHEEL_IMPULSE: f32 = 4.5;
const MAX_INDEX_VEL: f32 = 14.0;
/// Spring stiffness / damping (OnAnimationFrameUpdate) — 2*sqrt(78)*0.98.
const SPRING_K: f32 = 78.0;
const SPRING_C: f32 = 17.3103;
/// Direct-follow time constant while pointer scrolling.
const FOLLOW_TAU: f32 = 0.035;
/// Idle window after the last wheel notch before the target snaps.
const WHEEL_SETTLE_MS: u64 = 180;
const FLING_STALE_MS: u128 = 120;
/// Card grid geometry (CardGridLayoutHelper / HorizontalLayout).
const CARD_W: f32 = 200.0;
const CARD_H: f32 = 272.0;
const CARD_RATIO: f32 = CARD_H / CARD_W;
const CARD_RADIUS: f32 = 12.0;
const GRID_PAD_X: f32 = 28.0;
const GRID_TOP: f32 = 20.0;
const GRID_BOTTOM: f32 = 28.0;
const SCROLLBAR_RESERVE: f32 = 40.0;
const H_PAD_X: f32 = 36.0;
const TITLE_AREA: f32 = 0.24;
/// Scrollbar track width and reserved edge zone for the grid bars.
const SB_W: f32 = 5.0;
const SB_ZONE: f32 = 14.0;
/// Wheel multiplier for grid scrolling (egui's ~24-50px/notch is too
/// sluggish for long card lists).
const GRID_WHEEL_MULT: f32 = 3.0;
/// Layout-mode crossfade duration (carousel <-> card grids).
const MODE_FADE_SECS: f32 = 0.3;

// ------------------------------------------------------------------
// Item drag-reorder (hold-to-lift; drag-scroll stays primary)
// ------------------------------------------------------------------
/// How long a press must stay put before the item lifts — short
/// enough to feel instant, long enough that drag-scrolls never
/// trigger it (Fluent/iOS pick-up convention).
const REORDER_HOLD_MS: u128 = 280;
/// Floating card scale while held (the album row's DRAG_LIFT analog).
const ITEM_DRAG_LIFT: f32 = 1.05;
/// Drop glide duration — the card eases home, then the move commits.
const ITEM_COMMIT_SECS: f32 = 0.26;
/// Chase speed for neighbors sliding aside (exponential, seconds).
const ITEM_SWAP_TAU: f32 = 0.085;
/// Edge auto-scroll zone (px) while a card is held.
const DRAG_EDGE_ZONE: f32 = 90.0;
/// Auto-scroll speed at the zone edge (grid px/s / carousel items/s)
/// — scaled by item count so long albums still cross quickly.
const DRAG_EDGE_GRID_SPEED: f32 = 700.0;
const DRAG_EDGE_CAR_SPEED: f32 = 8.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Carousel,
    VerticalGrid,
    HorizontalGrid,
}

pub enum Action {
    None,
    /// Double-clicked a track item — play that file.
    Play(PathBuf),
    /// Right-clicked a track item -> "Edit Metadata" (Lacrima's
    /// `OpenMetadataCommand` context menu entry).
    Edit(PathBuf),
    /// "Add Url" context menu — open the URL prompt overlay.
    AddUrl,
    /// "Add Playlist" context menu — open the playlist URL prompt.
    AddPlaylist,
    /// "Delete" context menu on a pointed item (Lacrima's
    /// DeletePointedItem — removes it from the view, not from disk).
    Delete(PathBuf),
    /// "Clear Album" — empties the loaded album's children
    /// (Lacrima's ClearAlbumCommand; files are never touched).
    Clear,
    /// "Export Playlist" — write the album's URL items to an .apl
    /// file (Lacrima's ExportOnlinePlaylistCommand).
    Export,
}

/// One carousel/grid item = one track of the loaded album.
struct Item {
    path: PathBuf,
    /// Track's own embedded art.
    cover: CoverSrc,
    /// Shared folder artwork file (cover.jpg …) — Lacrima applies a
    /// local artwork file to every item in the folder, but a sibling
    /// track's embedded art never leaks onto items without art.
    fallback: Option<CoverSrc>,
    title: String,
    artist: String,
    album: String,
}

struct Press {
    start: Pos2,
    last: Pos2,
    last_move: Instant,
    /// When the press went down — the hold clock for pick-up.
    at: Instant,
    /// Index/scroll position captured at press time.
    anchor: f32,
    /// Smoothed velocity (items or px per second).
    vel: f32,
    moved: bool,
    /// Item under the press point (visible index) — the lift
    /// candidate; `None` when the press began over empty space.
    item: Option<usize>,
}

/// In-flight item reorder: the card floated off its slot and follows
/// the pointer while neighbors slide aside. Committing applies the
/// actual `items` move — `display_slot` is exactly the post-move
/// permutation, so the landing is seamless.
struct ItemDrag {
    /// Index into `items` (reorder only runs unfiltered, so visible
    /// index == items index throughout).
    index: usize,
    /// Grab point kept constant: pointer - card center at lift.
    grab: Vec2,
    /// Floating card's current center.
    center: Pos2,
    /// Slot the card would land in now.
    drop_target: usize,
    /// Per-item displacement blend 0..1 toward the shifted slot —
    /// eases the gap open instead of teleporting neighbors.
    swap: HashMap<usize, f32>,
    /// 0→1 lift-in (scale + shadow).
    lift: f32,
    /// Release glide — floats from pointer to the drop slot's center.
    commit: Option<ItemCommit>,
}

struct ItemCommit {
    from: Pos2,
    to: Pos2,
    progress: f32,
}

pub struct Carousel {
    items: Vec<Item>,
    /// Spring state in index units (also grid "selected" index).
    current: f32,
    target: f32,
    velocity: f32,
    sel: usize,
    press: Option<Press>,
    wheel_settle_at: Option<Instant>,
    /// Grid-mode scroll offset (px) + spring.
    grid_current: f32,
    grid_target: f32,
    grid_vel: f32,
    /// Grid scrollbar thumb grab offset (px along the thumb), while
    /// the scrollbar is being dragged.
    sb_drag: Option<f32>,
    /// Scrollbar auto-fade, matching the album row's behavior.
    sb_opacity: f32,
    sb_visible_until: Instant,
    /// Playing-item pulse phase (3.1 rad/s while a track plays).
    pulse: f32,
    /// Pill slider drag/hover preview.
    preview: Option<usize>,
    /// Queued scroll-to for a playing item (transport next/prev or
    /// auto-advance) — applied on the next `ui()` where the layout
    /// geometry exists.
    pending_reveal: Option<PathBuf>,
    /// Item under the cursor when the context menu opened.
    menu_item: Option<PathBuf>,
    /// In-flight hold-to-lift item reorder.
    item_drag: Option<ItemDrag>,
    /// Set when a drop committed an items move — the app consumes it
    /// via `take_reorder_dirty` to persist the new url order.
    reorder_dirty: bool,
    /// Loaded album's dir string — persistence key for its URL items
    /// (Lacrima: URL children belong to LoadedAlbum; AddUrl is gated
    /// on one being loaded).
    album_key: Option<String>,
    /// Loaded album's title — survives Clear Album (suggested .apl
    /// export name, like Lacrima's LoadedAlbum.Title).
    album_title: String,
    /// Global fade when a new album's items arrive (0.35s).
    transition_alpha: f32,
    /// Layout-mode crossfade: the mode `ui()` was last called with,
    /// the outgoing ghost mode, and fade progress 0→1.
    mode: Mode,
    prev_mode: Option<Mode>,
    mode_t: f32,
}

impl Carousel {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            current: 0.0,
            target: 0.0,
            velocity: 0.0,
            sel: 0,
            press: None,
            wheel_settle_at: None,
            grid_current: 0.0,
            grid_target: 0.0,
            grid_vel: 0.0,
            sb_drag: None,
            sb_opacity: 0.0,
            sb_visible_until: Instant::now(),
            pulse: 0.0,
            preview: None,
            pending_reveal: None,
            menu_item: None,
            item_drag: None,
            reorder_dirty: false,
            album_key: None,
            album_title: String::new(),
            transition_alpha: 1.0,
            mode: Mode::Carousel,
            prev_mode: None,
            mode_t: 1.0,
        }
    }

    /// Load an album's tracks as the cover items (Lacrima's
    /// `LoadedAlbum` -> `CoverItems = browseAlbum.Children`), with the
    /// album's persisted URL items appended after its files. Returns
    /// the spawn args for `library::spawn_track_info` so tag metadata
    /// can land asynchronously.
    pub fn open(&mut self, album: &Album, urls: &[String]) -> (Vec<PathBuf>, String) {
        let fallback = album
            .covers
            .iter()
            .find(|c| matches!(c, CoverSrc::File(_)))
            .cloned();
        self.items = album
            .tracks
            .iter()
            .map(|p| {
                let stem = p
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.display().to_string());
                Item {
                    path: p.clone(),
                    cover: CoverSrc::Tag(p.clone()),
                    fallback: fallback.clone(),
                    title: stem,
                    artist: String::new(),
                    album: album.title.clone(),
                }
            })
            .collect();
        for u in urls {
            self.push_url_item(u);
        }
        self.album_key = Some(album.dir.to_string_lossy().into_owned());
        self.album_title = album.title.clone();
        self.current = 0.0;
        self.target = 0.0;
        self.velocity = 0.0;
        self.sel = 0;
        self.grid_current = 0.0;
        self.grid_target = 0.0;
        self.grid_vel = 0.0;
        self.sb_drag = None;
        self.press = None;
        self.item_drag = None;
        self.preview = None;
        self.pending_reveal = None;
        self.menu_item = None;
        self.transition_alpha = 0.0;
        (
            self.items.iter().map(|i| i.path.clone()).collect(),
            album.title.clone(),
        )
    }

    /// Push one URL item (deduped like Lacrima's IsMediaDuplicate —
    /// same string or same canonical YouTube video id). Caller persists
    /// the url into settings.
    pub fn add_url(&mut self, url: &str) -> bool {
        let url = url.trim();
        if url.is_empty() || self.is_duplicate(url) {
            return false;
        }
        self.push_url_item(url);
        true
    }

    fn push_url_item(&mut self, url: &str) {
        let id = crate::ytdlp::extract_video_id(url);
        self.items.push(Item {
            path: PathBuf::from(url),
            cover: CoverSrc::Web(
                id.as_deref()
                    .map(crate::ytdlp::thumbnail_candidates)
                    .unwrap_or_default(),
            ),
            fallback: None,
            title: id.unwrap_or_else(|| url.to_string()),
            artist: String::new(),
            album: self.items.first().map(|i| i.album.clone()).unwrap_or_default(),
        });
    }

    /// Lacrima's IsMediaDuplicate — same path string, or both YouTube
    /// links resolving to the same video id.
    fn is_duplicate(&self, url: &str) -> bool {
        let new_id = crate::ytdlp::extract_video_id(url);
        self.items.iter().any(|i| {
            let p = i.path.to_string_lossy();
            p == url || (new_id.is_some() && new_id == crate::ytdlp::extract_video_id(&p))
        })
    }

    /// The loaded album's persistence key for URL items.
    pub fn album_key(&self) -> Option<&str> {
        self.album_key.as_deref()
    }

    /// The loaded album's URL items in order — what's persisted into
    /// Settings.Library.UrlItems (Lacrima serializes url children with
    /// the album).
    pub fn url_items(&self) -> Vec<String> {
        self.items
            .iter()
            .filter(|i| crate::library::is_url(&i.path))
            .map(|i| i.path.to_string_lossy().into_owned())
            .collect()
    }

    /// The loaded url items' cover sources in item order — the album
    /// tile's web fan mirrors this so resolved thumbnails and item
    /// order changes propagate to the tile automatically.
    pub fn url_covers(&self) -> Vec<CoverSrc> {
        self.items
            .iter()
            .filter(|i| crate::library::is_url(&i.path))
            .map(|i| i.cover.clone())
            .collect()
    }

    /// Remove an item (DeletePointedItem) — view-side only; a file
    /// comes back on the next album open, url items stay gone via the
    /// persisted list.
    pub fn remove(&mut self, path: &Path) {
        self.item_drag = None;
        if let Some(i) = self.index_of(path) {
            self.items.remove(i);
            let max = self.items.len().saturating_sub(1) as f32;
            self.current = self.current.clamp(0.0, max);
            self.target = self.target.clamp(0.0, max);
            self.sel = self.sel.min(self.items.len().saturating_sub(1));
            self.menu_item = None;
        }
    }

    /// Empty the loaded album's children but keep the album loaded
    /// (Lacrima's ClearAlbum: `LoadedAlbum.Children.Clear()` — the
    /// view returns to empty, Add Url stays available).
    pub fn clear_items(&mut self) {
        self.items.clear();
        self.item_drag = None;
        self.menu_item = None;
        self.current = 0.0;
        self.target = 0.0;
        self.velocity = 0.0;
        self.sel = 0;
        self.grid_current = 0.0;
        self.grid_target = 0.0;
        self.grid_vel = 0.0;
        self.sb_drag = None;
        self.press = None;
        self.preview = None;
        self.pending_reveal = None;
    }

    /// Any online (URL) items loaded — gates "Export Playlist".
    pub fn has_url_items(&self) -> bool {
        self.items.iter().any(|i| crate::library::is_url(&i.path))
    }

    /// The loaded URL items as .apl entries (title/artist/album as
    /// displayed; duration/track numbers aren't tracked for online
    /// items — Lacrima's fields export as 0).
    pub fn export_items(&self) -> Vec<crate::apl::Item> {
        self.items
            .iter()
            .filter(|i| crate::library::is_url(&i.path))
            .map(|i| crate::apl::Item {
                url: i.path.to_string_lossy().into_owned(),
                title: Some(i.title.clone()),
                artist: Some(i.artist.clone()),
                album: Some(i.album.clone()),
                ..Default::default()
            })
            .collect()
    }

    /// The loaded album's title — the suggested .apl export name.
    pub fn album_title(&self) -> String {
        self.album_title.clone()
    }

    /// Keep the loaded album's label in sync when the tile's inline
    /// title edit commits while the album is open (Rename Album).
    pub fn set_album_title(&mut self, title: &str) {
        self.album_title = title.to_string();
    }

    /// Unload the album entirely (the album row's Delete Album on the
    /// loaded album — Lacrima sets LoadedAlbum = null).
    pub fn close(&mut self) {
        self.items.clear();
        self.item_drag = None;
        self.album_key = None;
        self.album_title = String::new();
        self.menu_item = None;
        self.current = 0.0;
        self.target = 0.0;
        self.sel = 0;
    }

    /// The item's own cover source — for the now-playing disc art of
    /// URL items (Web candidates instead of tag reads).
    pub fn cover_for(&self, path: &Path) -> Option<CoverSrc> {
        self.index_of(path).map(|i| self.items[i].cover.clone())
    }

    /// Apply one item's metadata (per-item workers deliver labels as
    /// they resolve). Same number-prefix rule as `set_info` so file
    /// tracks can come through this path too.
    pub fn apply_info(&mut self, path: &Path, info: &TrackInfo) {
        if let Some(i) = self.index_of(path) {
            let item = &mut self.items[i];
            item.title = match info.number {
                Some(n) => format!("{:02}. {}", n, info.title),
                None => info.title.clone(),
            };
            item.artist = info.artist.clone();
            item.album = info.album.clone();
            if let Some(u) = &info.cover_url {
                item.cover = CoverSrc::Web(vec![u.clone()]);
            }
        }
    }

    /// Re-spawn args for `spawn_track_info` after tags were edited —
    /// the loaded items' paths plus the album title fallback.
    pub fn info_refresh_args(&self) -> Option<(Vec<PathBuf>, String)> {
        if self.items.is_empty() {
            return None;
        }
        let title = self.items[0].album.clone();
        Some((
            self.items.iter().map(|i| i.path.clone()).collect(),
            title,
        ))
    }

    /// Tag metadata arrived from the worker — upgrade item labels
    /// (same order the paths were spawned with). A tagged track number
    /// prefixes the title the way music players display it.
    pub fn set_info(&mut self, infos: Vec<TrackInfo>) {
        for (item, info) in self.items.iter_mut().zip(infos) {
            item.title = match info.number {
                Some(n) => format!("{:02}. {}", n, info.title),
                None => info.title,
            };
            item.artist = info.artist;
            item.album = info.album;
            // Online items: the resolved thumbnail replaces the
            // guessed img.youtube.com candidate list.
            if let Some(u) = info.cover_url {
                item.cover = CoverSrc::Web(vec![u]);
            }
        }
    }

    /// Index of `path` in the loaded items (the playing track).
    pub fn index_of(&self, path: &Path) -> Option<usize> {
        self.items.iter().position(|i| i.path == path)
    }

    /// Queue a scroll-to for `path`'s item — the playing item drives
    /// the selection marker and gets scrolled into view on the next
    /// frame (transport next/prev, auto-advance on track end).
    pub fn reveal(&mut self, path: &Path) {
        self.pending_reveal = Some(path.to_path_buf());
    }

    /// The item `dir` steps after `playing` in album order — the
    /// transport's Previous/Next target. `wrap` (RepeatAll) loops at
    /// the ends; otherwise the edge item returns `None`.
    pub fn step_path(&self, playing: &Path, dir: i32, wrap: bool) -> Option<PathBuf> {
        let n = self.items.len();
        if n == 0 {
            return None;
        }
        let i = self.index_of(playing)? as i32 + dir;
        let j = if wrap {
            i.rem_euclid(n as i32)
        } else if i < 0 || i >= n as i32 {
            return None;
        } else {
            i
        };
        Some(self.items[j as usize].path.clone())
    }

    /// Title/artist/album for the top-left metadata block — the item's
    /// tags when `path` is a loaded track, filename fallbacks otherwise.
    pub fn display_for(&self, path: &Path) -> (String, String, String) {
        if let Some(i) = self.index_of(path) {
            let it = &self.items[i];
            return (it.title.clone(), it.artist.clone(), it.album.clone());
        }
        (
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            String::new(),
            String::new(),
        )
    }

    /// The item the overlay labels should describe — the slider
    /// preview when hovering it, else the visually centered item.
    fn overlay_index(&self, visible: &[usize]) -> Option<usize> {
        if let Some(p) = self.preview {
            return visible.get(p).copied().or_else(|| visible.last().copied());
        }
        let i = self.current.round() as isize;
        if i < 0 || i as usize >= visible.len() {
            return None;
        }
        Some(visible[i as usize])
    }

    /// Item indices passing `filter` (MusicView's CoverItems filter —
    /// matches title, artist or album, case-insensitive).
    fn visible(&self, filter: &str) -> Vec<usize> {
        let q = filter.trim().to_lowercase();
        if q.is_empty() {
            return (0..self.items.len()).collect();
        }
        self.items
            .iter()
            .enumerate()
            .filter(|(_, i)| {
                i.title.to_lowercase().contains(&q)
                    || i.artist.to_lowercase().contains(&q)
                    || i.album.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Best texture for an item: its own embedded art, the album's
    /// top cover when that failed, else the shared placeholder.
    fn item_tex(
        row: &mut AlbumRow,
        ctx: &egui::Context,
        item: &Item,
    ) -> egui::TextureHandle {
        if let Some(t) = row.cover_tex(ctx, &item.cover) {
            return t;
        }
        if row.cover_failed(&item.cover) {
            if let Some(fb) = &item.fallback {
                if let Some(t) = row.cover_tex(ctx, fb) {
                    return t;
                }
            }
        }
        row.placeholder_tex(ctx)
    }

    // ----------------------------------------------------------------
    // Physics — CompositionCarouselVisualHandler::OnAnimationFrameUpdate
    // ----------------------------------------------------------------
    fn step(&mut self, dt: f32, pointer_scrolling: bool, max_index: f32) {
        let distance = self.target - self.current;
        if pointer_scrolling {
            // Track the pointer-driven target with a fast exponential
            // so covers glide at display refresh even when pointer
            // events arrive slower than vsync.
            let a = 1.0 - (-dt / FOLLOW_TAU).exp();
            let prev = self.current;
            self.current += distance * a;
            self.velocity = (self.current - prev) / dt.max(1e-6);
        } else {
            self.velocity += (distance * SPRING_K - self.velocity * SPRING_C) * dt;
            self.current += self.velocity * dt;
            // Settle: round a fractional target once nearly still.
            // Skipped while an item drag is active — the edge
            // auto-scroll feeds small fractional deltas that this
            // rounding would eat before the spring could move.
            if self.item_drag.is_none() {
                let nearest = self.target.round();
                if (self.target - nearest).abs() > 0.0001
                    && distance.abs() < 0.08
                    && self.velocity.abs() < 0.18
                {
                    self.target = nearest.clamp(0.0, max_index);
                }
            }
        }
        let distance = self.target - self.current;
        if distance.abs() < 0.0005 && self.velocity.abs() < 0.005 {
            self.current = self.target;
            self.velocity = 0.0;
        }
        self.pulse += dt * 3.1;
        if self.transition_alpha < 1.0 {
            self.transition_alpha = (self.transition_alpha + dt / 0.35).min(1.0);
        }
        // Wheel settle: last notch +180ms snaps the target to nearest.
        if let Some(deadline) = self.wheel_settle_at {
            if Instant::now() >= deadline {
                self.wheel_settle_at = None;
                self.target = self.target.round().clamp(0.0, max_index);
            }
        }
    }

    /// Grid scroll spring — same model, px units, stiffer feel.
    fn step_grid(&mut self, dt: f32, max_scroll: f32, direct: bool) {
        if direct {
            let a = 1.0 - (-dt / FOLLOW_TAU).exp();
            let prev = self.grid_current;
            self.grid_current += (self.grid_target - self.grid_current) * a;
            self.grid_vel = (self.grid_current - prev) / dt.max(1e-6);
        } else {
            let d = self.grid_target - self.grid_current;
            self.grid_vel += (d * 140.0 - self.grid_vel * 22.5) * dt;
            self.grid_current += self.grid_vel * dt;
            if d.abs() < 0.05 && self.grid_vel.abs() < 0.5 {
                self.grid_current = self.grid_target;
                self.grid_vel = 0.0;
            }
        }
        self.grid_target = self.grid_target.clamp(0.0, max_scroll.max(0.0));
        self.grid_current = self.grid_current.clamp(0.0, max_scroll.max(0.0) + 1.0);
    }

    /// Shared press bookkeeping: anchor + smoothed velocity.
    fn press_update(press: &mut Press, p: Pos2, dt: f32, px_per_unit: f32, axis_x: bool) {
        let d = if axis_x { p.x - press.last.x } else { p.y - press.last.y };
        if dt > 1e-6 {
            let instant = -d / (px_per_unit * dt);
            // ~45ms exponential window keeps the release fling stable
            // across bursty pointer event delivery.
            let a = 1.0 - (-dt / 0.045).exp();
            press.vel += (instant - press.vel) * a;
        }
        press.last = p;
        press.last_move = Instant::now();
    }

    /// Whether an items move was committed since the last check —
    /// the app persists the new url order (tile fan follows too).
    pub fn take_reorder_dirty(&mut self) -> bool {
        std::mem::take(&mut self.reorder_dirty)
    }

    /// Where `index` displays while `drag` hovers `target`: the held
    /// card owns the target slot, the rest shift around it. After the
    /// commit this mapping IS the new item order, so the landing is
    /// seamless (same scheme as the album row's ComputeDisplaySlot).
    fn display_slot(index: usize, drag: usize, target: usize, count: usize) -> usize {
        let target = target.min(count.saturating_sub(1));
        if index == drag {
            return target;
        }
        let rank = index - usize::from(index > drag);
        if rank < target {
            rank
        } else {
            rank + 1
        }
    }

    /// An item's displaced index-position while a drag hovers —
    /// `display_slot` blended by the animated swap factor so neighbors
    /// glide aside instead of teleporting.
    fn disp_index(&self, vi: usize) -> f32 {
        match &self.item_drag {
            Some(d) if vi != d.index => {
                let n = self.items.len();
                let disp =
                    Self::display_slot(vi, d.index, d.drop_target, n) as f32;
                let t = d.swap.get(&vi).copied().unwrap_or(0.0);
                vi as f32 + (disp - vi as f32) * t
            }
            _ => vi as f32,
        }
    }

    /// Screen center of the (undisplaced) slot `vi` — a grid card's
    /// rect center, or the carousel item's projected center.
    #[allow(clippy::too_many_arguments)]
    fn slot_center(
        &self,
        vi: usize,
        mode: Mode,
        geo: Option<&GridGeo>,
        grid_area: Rect,
        row: &mut AlbumRow,
        ctx: &egui::Context,
        cfg: &settings::Carousel,
        s: f32,
    ) -> Option<Pos2> {
        match mode {
            Mode::Carousel => {
                let item = self.items.get(vi)?;
                let tex = Self::item_tex(row, ctx, item);
                let (tx, tz, rot, sc, _w, _h) =
                    self.item_geo(vi as f32, &tex, cfg, s);
                let center = Pos2::new(
                    grid_area.center().x,
                    grid_area.center().y + cfg.vertical_offset * s,
                );
                Some(Self::project(tx, tz, rot, sc, center, 0.0, 0.0).0)
            }
            _ => geo
                .and_then(|g| g.card_rect(vi, self.grid_current))
                .map(|r| r.center()),
        }
    }

    /// Hold-to-lift item reorder: a press that stays put for
    /// REORDER_HOLD_MS floats the item off its slot; the card then
    /// tracks the pointer 1:1 (grab offset preserved), neighbors ease
    /// aside to open the gap, edge zones auto-scroll, and release
    /// glides the card home before `items` is permuted. `reorderable`
    /// is `filter.is_empty()` — a filtered view's permutation would be
    /// ambiguous.
    #[allow(clippy::too_many_arguments)]
    fn update_item_drag(
        &mut self,
        ctx: &egui::Context,
        grid_area: Rect,
        row: &mut AlbumRow,
        geo: Option<&GridGeo>,
        mode: Mode,
        cfg: &settings::Carousel,
        dt: f32,
        interactive: bool,
        reorderable: bool,
        s: f32,
    ) {
        let n = self.items.len();
        if self
            .item_drag
            .as_ref()
            .map(|d| d.index >= n)
            .unwrap_or(false)
        {
            self.item_drag = None;
        }

        // Drop glide: ease the card home, then permute the model.
        let landed = match self.item_drag.as_mut().and_then(|d| d.commit.as_mut()) {
            Some(c) => {
                c.progress += dt / ITEM_COMMIT_SECS;
                c.progress >= 1.0
            }
            None => false,
        };
        if landed {
            let d = self.item_drag.take().unwrap();
            let to = d.drop_target.min(n.saturating_sub(1));
            if n > 0 && d.index != to {
                let it = self.items.remove(d.index);
                self.items.insert(to, it);
                self.sel = to;
                self.reorder_dirty = true;
            }
            return;
        }
        if self
            .item_drag
            .as_ref()
            .map(|d| d.commit.is_some())
            .unwrap_or(false)
        {
            return;
        }

        let (pointer, down) =
            ctx.input(|i| (i.pointer.latest_pos(), i.pointer.primary_down()));

        if self.item_drag.is_none() {
            // Still-holding press -> lift. Any earlier movement already
            // committed the gesture to scroll (press.moved).
            if !(interactive && reorderable && down) || n == 0 {
                return;
            }
            let Some(pr) = &self.press else { return };
            if pr.moved || pr.at.elapsed().as_millis() < REORDER_HOLD_MS {
                return;
            }
            let Some(vi) = pr.item.filter(|&vi| vi < n) else { return };
            let Some(c) =
                self.slot_center(vi, mode, geo, grid_area, row, ctx, cfg, s)
            else {
                return;
            };
            let grab = pr.start - c;
            self.item_drag = Some(ItemDrag {
                index: vi,
                grab,
                center: c,
                drop_target: vi,
                swap: HashMap::new(),
                lift: 0.0,
                commit: None,
            });
            self.press = None;
            ctx.request_repaint();
            return;
        }

        // Released (or view went inactive) — glide to the target slot.
        if !interactive || !down {
            let (from, to) = {
                let d = self.item_drag.as_ref().unwrap();
                (d.center, d.drop_target.min(n.saturating_sub(1)))
            };
            let home = self
                .slot_center(to, mode, geo, grid_area, row, ctx, cfg, s)
                .unwrap_or(from);
            self.item_drag.as_mut().unwrap().commit =
                Some(ItemCommit { from, to: home, progress: 0.0 });
            ctx.request_repaint();
            return;
        }
        let Some(p) = pointer else { return };

        {
            let d = self.item_drag.as_mut().unwrap();
            d.center = p + d.grab;
            d.lift = (d.lift + dt / 0.14).min(1.0);
        }
        let center = self.item_drag.as_ref().unwrap().center;

        // Nearest slot center decides the drop target. Carousel slots
        // cluster near the middle — x-distance is the natural metric
        // on the arc; grid slots compare in 2D.
        let mut best = 0usize;
        let mut best_d = f32::MAX;
        for vi in 0..n {
            let Some(c) =
                self.slot_center(vi, mode, geo, grid_area, row, ctx, cfg, s)
            else {
                continue;
            };
            let dd = match mode {
                Mode::Carousel => (c.x - center.x).abs(),
                _ => (c - center).length_sq(),
            };
            if dd < best_d {
                best_d = dd;
                best = vi;
            }
        }
        {
            let d = self.item_drag.as_mut().unwrap();
            d.drop_target = best;
            // Neighbors ease toward their displaced slots.
            for vi in 0..n {
                if vi == d.index {
                    continue;
                }
                let disp = Self::display_slot(vi, d.index, d.drop_target, n);
                let want = if disp == vi { 0.0 } else { 1.0 };
                let cur = d.swap.get(&vi).copied().unwrap_or(0.0);
                let next =
                    cur + (want - cur) * (1.0 - (-dt / ITEM_SWAP_TAU).exp());
                if want == 0.0 && next < 0.01 {
                    d.swap.remove(&vi);
                } else {
                    d.swap.insert(vi, next);
                }
            }
        }

        // Edge auto-scroll while a card is held — quadratic ramp, same
        // feel as the album row's drag scroll. Speed scales with item
        // count (up to 3x) so a 200-item album doesn't crawl.
        let zone = DRAG_EDGE_ZONE * s;
        let boost = 1.0 + (n as f32 / 50.0).min(2.0);
        match mode {
            Mode::Carousel => {
                let a = p.x;
                let (lo, hi) = (grid_area.min.x, grid_area.max.x);
                let f = if a < lo + zone {
                    -(((lo + zone - a) / zone).min(1.0).powi(2))
                } else if a > hi - zone {
                    ((a - (hi - zone)) / zone).min(1.0).powi(2)
                } else {
                    0.0
                };
                if f != 0.0 {
                    // Drive current with the same delta — 1:1 scroll
                    // at the edge speed instead of a laggy spring
                    // chase (which the settle would pin anyway).
                    let d_idx = f * DRAG_EDGE_CAR_SPEED * boost * dt;
                    self.target = (self.target + d_idx)
                        .clamp(0.0, (n - 1) as f32);
                    self.current = (self.current + d_idx)
                        .clamp(-0.5, (n - 1) as f32 + 0.5);
                    self.velocity = 0.0;
                    self.wheel_settle_at = None;
                }
            }
            _ => {
                if let Some(g) = geo {
                    let (a, lo, hi) = if g.horizontal {
                        (p.x, grid_area.min.x, grid_area.max.x)
                    } else {
                        (p.y, grid_area.min.y, grid_area.max.y)
                    };
                    let f = if a < lo + zone {
                        -(((lo + zone - a) / zone).min(1.0).powi(2))
                    } else if a > hi - zone {
                        ((a - (hi - zone)) / zone).min(1.0).powi(2)
                    } else {
                        0.0
                    };
                    if f != 0.0 {
                        // Gentler boost for card grids — px speed
                        // scales less aggressively than the carousel's
                        // index speed.
                        let gboost = 1.0 + (n as f32 / 60.0).min(1.5);
                        self.grid_target = (self.grid_target
                            + f * DRAG_EDGE_GRID_SPEED * gboost * dt)
                            .clamp(0.0, g.max_scroll);
                        self.grid_current = self.grid_target;
                        self.grid_vel = 0.0;
                    }
                }
            }
        }
        ctx.request_repaint();
    }

    // ----------------------------------------------------------------
    // Entry point — paints the cover view and handles input.
    // `playing` is the currently loaded track path (glow + pulse).
    // ----------------------------------------------------------------
    /// `rect` is the visible cover view (input gating, scrollbar dock);
    /// `grid_area` is the layout/paint region for the grid modes. They
    /// differ while the EQ drawer lifts the transport band — the grid
    /// keeps its row/column count and the band simply covers whatever
    /// slides under it (Avalonia's ClipToBounds=False cover row).
    #[allow(clippy::too_many_arguments)]
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        grid_area: Rect,
        row: &mut AlbumRow,
        playing: Option<&Path>,
        filter: &str,
        cfg: &settings::Carousel,
        mode: Mode,
        alpha: f32,
        interactive: bool,
        s: f32,
    ) -> Action {
        let mut action = Action::None;
        let ctx = ui.ctx().clone();
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        let painter = ui.painter().clone();

        // Layout-mode switch: the outgoing view stays painted as a
        // fading ghost while the new one fades in (see the paint
        // dispatch below). `mode_t` drives the crossfade.
        if mode != self.mode {
            self.prev_mode = Some(std::mem::replace(&mut self.mode, mode));
            self.mode_t = 0.0;
            // A held card belongs to the old layout — drop it.
            self.item_drag = None;
        }
        if self.mode_t < 1.0 {
            self.mode_t = (self.mode_t + dt / MODE_FADE_SECS).min(1.0);
            if self.mode_t >= 1.0 {
                self.prev_mode = None;
            }
        }

        let visible = self.visible(filter);
        // Reorder permutes `items` by visible position — under a
        // filter that mapping is ambiguous, so the gesture is off.
        let reorderable = filter.trim().is_empty();
        if !reorderable {
            self.item_drag = None;
        }
        let n = visible.len();
        if n == 0 {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "No album loaded",
                FontId::proportional(24.0 * s),
                Color32::from_rgb(0x78, 0x78, 0x78).gamma_multiply(alpha * 0.53),
            );
            // Menu still reachable when an album is loaded but the
            // filter hides every item (Lacrima's menu lives on the
            // cover view itself, not on items).
            if interactive && self.album_key.is_some() {
                let resp =
                    ui.interact(rect, ui.id().with("cover-view-empty"), Sense::click());
                resp.context_menu(|ui| {
                    if ui.button("Add Url").clicked() {
                        action = Action::AddUrl;
                        ui.close();
                    }
                    if ui.button("Add Playlist").clicked() {
                        action = Action::AddPlaylist;
                        ui.close();
                    }
                    if self.has_url_items() && ui.button("Export Playlist").clicked() {
                        action = Action::Export;
                        ui.close();
                    }
                    if ui.button("Clear Album").clicked() {
                        action = Action::Clear;
                        ui.close();
                    }
                });
            }
            return action;
        }
        let max_index = (n - 1) as f32;
        self.target = self.target.clamp(0.0, max_index);
        self.current = self.current.clamp(-0.5, max_index + 0.5);
        let playing_idx = playing.and_then(|p| self.index_of(p));
        let playing_vidx = playing_idx.and_then(|pi| visible.iter().position(|&v| v == pi));

        // Layout-mode geometry used by both input and paint.
        let geo = match mode {
            Mode::Carousel => None,
            Mode::VerticalGrid => Some(GridGeo::vertical(grid_area, n, cfg, s)),
            Mode::HorizontalGrid => Some(GridGeo::horizontal(grid_area, n, cfg, s)),
        };
        // Queued reveal — the playing item owns the selection marker;
        // the carousel glides to center it, grids scroll just enough to
        // make its card fully visible (ScrollIntoView semantics).
        if let Some(path) = self.pending_reveal.take() {
            if let Some(idx) = self.index_of(&path) {
                self.sel = idx;
                if let Some(vi) = visible.iter().position(|&v| v == idx) {
                    match mode {
                        Mode::Carousel => {
                            self.target = vi as f32;
                            self.wheel_settle_at = None;
                        }
                        _ => {
                            let g = geo.as_ref().unwrap();
                            if let Some(cr) = g.card_rect(vi, self.grid_current) {
                                let mut t = self.grid_target;
                                if g.horizontal {
                                    if cr.min.x < rect.min.x {
                                        t -= rect.min.x - cr.min.x;
                                    } else if cr.max.x > rect.max.x {
                                        t += cr.max.x - rect.max.x;
                                    }
                                } else {
                                    if cr.min.y < rect.min.y {
                                        t -= rect.min.y - cr.min.y;
                                    } else if cr.max.y > rect.max.y {
                                        t += cr.max.y - rect.max.y;
                                    }
                                }
                                self.grid_target = t.clamp(0.0, g.max_scroll);
                            }
                        }
                    }
                }
            }
        }
        if let Some(g) = &geo {
            self.step_grid(dt, g.max_scroll, self.press.is_some() && self.sb_drag.is_none());
        }
        let pointer = ctx.input(|i| i.pointer.latest_pos());
        // Grid scrollbar geometry — `None` in carousel mode or when the
        // content fits without scrolling.
        let sb = geo
            .as_ref()
            .and_then(|g| g.scrollbar(rect, self.grid_current, s));
        // Hot while dragged or hovered — brightens the thumb.
        let sb_hot = self.sb_drag.is_some()
            || pointer
                .map(|p| sb.as_ref().map(|g| g.hit.contains(p)).unwrap_or(false))
                .unwrap_or(false);
        // Auto-fade like the album-row bar: hot, recently scrolled, or
        // still moving keeps it up; idle fades it out.
        let wants_sb = sb.is_some()
            && (sb_hot
                || Instant::now() < self.sb_visible_until
                || self.grid_vel.abs() > 1.0);
        self.sb_opacity +=
            ((wants_sb as u8) as f32 - self.sb_opacity) * (dt * 10.0).min(1.0);

        // --- Input --------------------------------------------------
        // The pill slider's rect — computed here so wheel/press input
        // can skip it before the widget itself is painted.
        let slider_rect = (mode == Mode::Carousel && n > 1)
            .then(|| {
                Rect::from_center_size(
                    Pos2::new(
                        grid_area.center().x,
                        grid_area.center().y + (190.0 + 62.0) * s,
                    ),
                    Vec2::new(520.0 * s, 28.0 * s),
                )
            })
            // Same rule as the paint side — a slider that doesn't fit
            // isn't there, for hit-testing either.
            .filter(|r| rect.contains_rect(*r));
        let scroll_in = pointer
            .map(|p| rect.contains(p) && !slider_rect.map(|r| r.contains(p)).unwrap_or(false))
            .unwrap_or(false);
        let scroll_delta = if scroll_in {
            ctx.input(|i| i.smooth_scroll_delta)
        } else {
            Vec2::ZERO
        };

        match mode {
            Mode::Carousel => {
                if interactive && scroll_delta != Vec2::ZERO {
                    // ~50px of wheel delta per notch => 1 item, like
                    // Avalonia's Delta.Y = ±1.
                    let notches = scroll_delta.y / 50.0;
                    self.target = (self.target - notches).clamp(0.0, max_index);
                    self.velocity = (self.velocity - notches * WHEEL_IMPULSE)
                        .clamp(-MAX_INDEX_VEL, MAX_INDEX_VEL);
                    self.wheel_settle_at =
                        Some(Instant::now() + std::time::Duration::from_millis(WHEEL_SETTLE_MS));
                    ctx.request_repaint();
                }
            }
            _ => {
                if interactive && scroll_delta != Vec2::ZERO {
                    let g = geo.as_ref().unwrap();
                    let d = if mode == Mode::HorizontalGrid {
                        scroll_delta.x + scroll_delta.y
                    } else {
                        scroll_delta.y
                    };
                    self.grid_target =
                        (self.grid_target - d * GRID_WHEEL_MULT).clamp(0.0, g.max_scroll);
                    self.sb_visible_until =
                        Instant::now() + std::time::Duration::from_secs_f32(1.2);
                    ctx.request_repaint();
                }
            }
        }

        self.step(dt, self.press.is_some() && mode == Mode::Carousel, max_index);

        // Manual press tracking — raw input can't be stolen by widget
        // routing (same approach as the album row's scrollbar).
        if interactive && ctx.input(|i| i.pointer.primary_pressed()) {
            if let Some(p) = ctx.input(|i| i.pointer.press_origin()) {
                let over_slider =
                    slider_rect.map(|r| r.contains(p)).unwrap_or(false);
                if rect.contains(p) && !over_slider {
                    // Press inside the scrollbar's grab zone owns the
                    // drag — store the offset within the thumb (or the
                    // whole thumb when grabbing the track) so it never
                    // jumps on grab.
                    let on_sb = sb.as_ref().map(|g| g.hit.contains(p)).unwrap_or(false);
                    if on_sb {
                        let g = sb.as_ref().unwrap();
                        let (t, len) = if g.horizontal {
                            (p.x - g.thumb.min.x, g.thumb.width())
                        } else {
                            (p.y - g.thumb.min.y, g.thumb.height())
                        };
                        self.sb_drag = Some(t.clamp(0.0, len));
                    } else {
                        // Item under the press — the hold-to-lift
                        // candidate. Only resolved when reordering is
                        // possible (unfiltered); the hit test projects
                        // quads, so skip it otherwise.
                        let item = if reorderable {
                            match mode {
                                Mode::Carousel => self.hit_carousel(
                                    p, grid_area, &visible, row, &ctx, cfg, s,
                                ),
                                _ => self.hit_grid(
                                    p,
                                    &visible,
                                    geo.as_ref().unwrap(),
                                ),
                            }
                        } else {
                            None
                        };
                        self.press = Some(Press {
                            start: p,
                            last: p,
                            last_move: Instant::now(),
                            at: Instant::now(),
                            anchor: match mode {
                                Mode::Carousel => self.target,
                                _ => self.grid_target,
                            },
                            vel: 0.0,
                            moved: false,
                            item,
                        });
                    }
                }
            }
        }
        let primary_down = ctx.input(|i| i.pointer.primary_down());
        if !primary_down {
            self.sb_drag = None;
        }
        // Scrollbar drag: the thumb follows the pointer 1:1 — the
        // grabbed offset stays fixed under it, no spring lag.
        if self.sb_drag.is_some() && primary_down {
            if let (Some(p), Some(g)) = (pointer, &sb) {
                let grab = self.sb_drag.unwrap();
                let (tlen, span) = if g.horizontal {
                    (g.thumb.width(), (p.x - g.track.min.x - grab))
                } else {
                    (g.thumb.height(), (p.y - g.track.min.y - grab))
                };
                let travel = if g.horizontal {
                    g.track.width() - tlen
                } else {
                    g.track.height() - tlen
                };
                let frac = (span / travel.max(1.0)).clamp(0.0, 1.0);
                self.grid_target = frac * g.max_scroll;
                self.grid_current = self.grid_target;
                self.grid_vel = 0.0;
                self.sb_visible_until =
                    Instant::now() + std::time::Duration::from_secs_f32(1.2);
                ctx.request_repaint();
            }
        } else if self.press.is_some() && primary_down {
            let press = self.press.as_mut().unwrap();
            if let Some(p) = pointer {
                let axis_x = mode != Mode::VerticalGrid;
                let d = if axis_x {
                    p.x - press.start.x
                } else {
                    p.y - press.start.y
                };
                if !press.moved && d.abs() > DRAG_START_PX {
                    press.moved = true;
                }
                if press.moved {
                    match mode {
                        Mode::Carousel => {
                            Self::press_update(press, p, dt, DRAG_PX_PER_ITEM, true);
                            let dx = p.x - press.start.x;
                            self.target = (press.anchor - dx / DRAG_PX_PER_ITEM)
                                .clamp(0.0, max_index);
                        }
                        _ => {
                            Self::press_update(press, p, dt, 1.0, axis_x);
                            let delta = if axis_x {
                                p.x - press.start.x
                            } else {
                                p.y - press.start.y
                            };
                            self.grid_target = (press.anchor - delta)
                                .clamp(0.0, geo.as_ref().unwrap().max_scroll);
                        }
                    }
                } else {
                    press.last = p;
                    press.last_move = Instant::now();
                }
            }
        } else if let Some(press) = self.press.take() {
            // Release: fling with the smoothed velocity, then let the
            // spring settle to the projected index.
            let recent = press.last_move.elapsed().as_millis() < FLING_STALE_MS;
            let fling = if recent { press.vel } else { 0.0 };
            match mode {
                Mode::Carousel => {
                    if press.moved {
                        self.velocity = fling.clamp(-MAX_INDEX_VEL, MAX_INDEX_VEL);
                        self.target = (self.current + press.vel * 0.15)
                            .round()
                            .clamp(0.0, max_index);
                    }
                }
                _ => {
                    if press.moved {
                        let g = geo.as_ref().unwrap();
                        self.grid_vel = fling;
                        self.grid_target =
                            (self.grid_current + press.vel * 0.15).clamp(0.0, g.max_scroll);
                    }
                }
            }
        }

        // Hold-to-lift item reorder — converts a still press into a
        // floating card, or runs the drop glide after release.
        self.update_item_drag(
            &ctx,
            grid_area,
            row,
            geo.as_ref(),
            mode,
            cfg,
            dt,
            interactive,
            reorderable,
            s,
        );

        // Click / double-click on items (egui suppresses the click when
        // the press moved past its threshold, so a scroll never selects).
        let resp = ui.interact(rect, ui.id().with("cover-view"), Sense::click());
        // Right-click an item -> context menu with "Edit Metadata"
        // (MusicView's OpenMetadataCommand, gated on IsItemPointed).
        if interactive && resp.secondary_clicked() {
            if let Some(p) = ctx.input(|i| i.pointer.interact_pos()) {
                let skip = slider_rect.map(|r| r.contains(p)).unwrap_or(false);
                if !skip {
                    let hit = match mode {
                        Mode::Carousel => {
                            self.hit_carousel(p, grid_area, &visible, row, &ctx, cfg, s)
                        }
                        _ => self.hit_grid(p, &visible, geo.as_ref().unwrap()),
                    };
                    self.menu_item =
                        hit.map(|vi| self.items[visible[vi]].path.clone());
                } else {
                    self.menu_item = None;
                }
            }
        }
        if interactive {
            resp.context_menu(|ui| {
                // Lacrima's cover-view menu: Add entries always,
                // item entries only while one is pointed.
                if ui.button("Add Url").clicked() {
                    action = Action::AddUrl;
                    ui.close();
                }
                if ui.button("Add Playlist").clicked() {
                    action = Action::AddPlaylist;
                    ui.close();
                }
                if let Some(p) = self.menu_item.clone() {
                    ui.separator();
                    if ui.button("Edit Metadata").clicked() {
                        action = Action::Edit(p.clone());
                        self.menu_item = None;
                        ui.close();
                    }
                    if ui.button("Delete").clicked() {
                        action = Action::Delete(p);
                        self.menu_item = None;
                        ui.close();
                    }
                }
                ui.separator();
                if self.has_url_items() && ui.button("Export Playlist").clicked() {
                    action = Action::Export;
                    ui.close();
                }
                if ui.button("Clear Album").clicked() {
                    action = Action::Clear;
                    ui.close();
                }
            });
        }
        // A lifted card's release registers as a click (the pointer
        // never crossed the drag threshold) — suppress it so a reorder
        // never re-selects or plays.
        if interactive && self.item_drag.is_none() && (resp.clicked() || resp.double_clicked()) {
            if let Some(p) = ctx.input(|i| i.pointer.interact_pos()) {
                let skip = slider_rect.map(|r| r.contains(p)).unwrap_or(false);
                if !skip {
                    let hit = match mode {
                        Mode::Carousel => self.hit_carousel(
                            p,
                            grid_area,
                            &visible,
                            row,
                            &ctx,
                            cfg,
                            s,
                        ),
                        _ => self.hit_grid(p, &visible, geo.as_ref().unwrap()),
                    };
                    if let Some(vi) = hit {
                        let idx = visible[vi];
                        self.sel = idx;
                        if resp.double_clicked() {
                            // Snap to the item and play it.
                            if mode == Mode::Carousel {
                                self.current = vi as f32;
                                self.target = vi as f32;
                                self.velocity = 0.0;
                            }
                            action = Action::Play(self.items[idx].path.clone());
                        } else if mode == Mode::Carousel {
                            self.target = (vi as f32).clamp(0.0, max_index);
                            self.wheel_settle_at = None;
                        }
                    }
                }
            }
        }

        // --- Paint --------------------------------------------------
        let global_alpha = alpha * self.transition_alpha;
        let mt = ease_out_cubic(self.mode_t);
        if let Some(pm) = self.prev_mode {
            // Outgoing view fades out in place.
            self.paint_mode(
                ui,
                grid_area,
                rect,
                pm,
                &visible,
                row,
                playing_vidx,
                cfg,
                global_alpha * (1.0 - mt),
                0.0,
                false,
                s,
            );
        }
        // Incoming view fades in and drifts up into place.
        let drift = (1.0 - mt) * 26.0 * s;
        self.paint_mode(
            ui,
            grid_area,
            rect,
            mode,
            &visible,
            row,
            playing_vidx,
            cfg,
            global_alpha * mt,
            drift,
            interactive,
            s,
        );
        if mode != Mode::Carousel {
            if let Some(b) = &sb {
                if self.sb_opacity > 0.01 {
                    Self::paint_scrollbar(ui.painter(), b, sb_hot, self.sb_opacity);
                }
            }
        }

        // Repaint while anything is still animating — including a held
        // press (the direct-follow lag needs frames even when the
        // pointer pauses mid-drag) and an in-flight text crossfade.
        let animating = self.velocity.abs() > 0.005
            || (self.target - self.current).abs() > 0.0005
            || self.grid_vel.abs() > 0.5
            || (self.grid_target - self.grid_current).abs() > 0.05
            || playing_idx.is_some()
            || self.transition_alpha < 1.0
            || self.wheel_settle_at.is_some()
            || self.press.is_some()
            || self.item_drag.is_some()
            || self.sb_opacity > 0.01
            || self.mode_t < 1.0;
        if animating {
            ctx.request_repaint();
        }
        action
    }

    // ================================================================
    // GRID SCROLLBAR
    // ================================================================

    /// Pill scrollbar like the album row's — a soft halo under a bright
    /// core, brighter while hot (hovered/dragged).
    fn paint_scrollbar(painter: &egui::Painter, sb: &SbGeo, hot: bool, opacity: f32) {
        let r = sb.track.width().min(sb.track.height()) * 0.5;
        painter.rect_filled(
            sb.track,
            r,
            Color32::WHITE.gamma_multiply(0.10 * opacity),
        );
        let hot_mul = if hot { 1.0 } else { 0.75 };
        painter.rect_filled(
            sb.thumb.expand(1.5),
            r + 1.5,
            Color32::WHITE.gamma_multiply(0.22 * hot_mul * opacity),
        );
        painter.rect_filled(
            sb.thumb,
            r,
            Color32::WHITE.gamma_multiply(0.7 * hot_mul * opacity),
        );
    }

    // ================================================================
    // MODE PAINT / CROSSFADE
    // ================================================================

    /// Paints one layout mode — used for both the live view and the
    /// outgoing ghost during a mode crossfade. `drift_y` offsets the
    /// whole view vertically (the incoming view rises into place).
    #[allow(clippy::too_many_arguments)]
    fn paint_mode(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        view: Rect,
        mode: Mode,
        visible: &[usize],
        row: &mut AlbumRow,
        playing_vidx: Option<usize>,
        cfg: &settings::Carousel,
        alpha: f32,
        drift_y: f32,
        interactive: bool,
        s: f32,
    ) {
        match mode {
            Mode::Carousel => {
                let r = rect.translate(Vec2::new(0.0, drift_y));
                let v = view.translate(Vec2::new(0.0, drift_y));
                self.paint_carousel(ui, r, visible, row, playing_vidx, cfg, alpha, s);
                self.overlay_stack(ui, r, v, visible, alpha, interactive, s);
            }
            _ => {
                let mut geo = match mode {
                    Mode::VerticalGrid => GridGeo::vertical(rect, visible.len(), cfg, s),
                    _ => GridGeo::horizontal(rect, visible.len(), cfg, s),
                };
                geo.origin.y += drift_y;
                self.paint_grid(
                    ui,
                    rect,
                    visible,
                    row,
                    playing_vidx,
                    &geo,
                    self.grid_current,
                    alpha * cfg.card_opacity,
                    s,
                );
            }
        }
    }

    // ================================================================
    // CAROUSEL MODE
    // ================================================================

    /// Perspective transform for item `i` (CompositionCarouselVisual
    /// Handler's matrix): returns (translation x, z, rotation Y, scale,
    /// item w/h in points).
    #[allow(clippy::too_many_arguments)]
    fn item_transform(
        &self,
        i: f32,
        w: f32,
        h: f32,
        base_w: f32,
        cfg: &settings::Carousel,
        s: f32,
    ) -> (f32, f32, f32, f32) {
        let diff = i - self.current;
        let abs = diff.abs();
        let ease = (diff * 2.2).tanh();
        let mut rot = -ease * SIDE_ROT;
        let stack = diff.signum() * (abs - 0.45).max(0.0).powf(1.1) * cfg.stack_spacing * s;
        let mut tx = (ease * cfg.side_translation * s + stack) * cfg.spacing * cfg.scale;
        let tz = -(abs.powf(0.8) * 220.0 * s * cfg.spacing * cfg.scale);
        let pop = 0.18 * (-abs * abs * 6.0).exp();
        let scale = (1.0 + pop - abs * 0.06).max(0.1);
        // Wide covers translate more and rotate less so they don't
        // collide with the center card.
        let wide = (w / base_w.max(1.0) - 1.0).max(0.0);
        tx *= 1.0 + wide * 0.35;
        rot /= 1.0 + wide * 0.85;
        let _ = h;
        (tx, tz, rot, scale)
    }

    /// Full local->screen transform: matrix T * RotY * Scale applied to
    /// (x, y, 0), then perspective projection about `center`.
    fn project(
        tx: f32,
        tz: f32,
        rot: f32,
        sc: f32,
        center: Pos2,
        x: f32,
        y: f32,
    ) -> (Pos2, f32) {
        let xs = x * sc;
        let ys = y * sc;
        let cos = rot.cos();
        let sin = rot.sin();
        // RotationY: vx = x*cos + z*sin ; vz = -x*sin + z*cos (z = 0).
        let vx = xs * cos + tx;
        let vy = ys;
        let vz = -xs * sin + tz;
        let f = PROJECTION_DIST / (PROJECTION_DIST - vz).max(1e-3);
        (Pos2::new(center.x + vx * f, center.y + vy * f), vz)
    }

    /// Item quad size — the 240x200 frame scaled by ItemScale. With
    /// UseFullCoverSize the width slides to baseH*aspect (Lacrima's
    /// fullCoverSizeFactor treated as 0/1); off keeps the landscape
    /// frame and the cover is fill-cropped into it.
    fn item_size(
        tex: &egui::TextureHandle,
        cfg: &settings::Carousel,
        s: f32,
    ) -> (f32, f32) {
        let base_w = BASE_W * s * cfg.scale;
        let base_h = BASE_H * s * cfg.scale;
        let [tw, th] = tex.size();
        if !cfg.use_full_cover_size || tw == 0 || th == 0 {
            return (base_w, base_h);
        }
        let aspect = (tw as f32 / th as f32).clamp(0.01, 1.35);
        (base_h * aspect, base_h)
    }

    /// Build the perspective mesh for one item — vertical strips so the
    /// rotated cards read as 3D without bending (DrawQuad).
    #[allow(clippy::too_many_arguments)]
    fn item_mesh(
        &self,
        tx: f32,
        tz: f32,
        rot: f32,
        sc: f32,
        center: Pos2,
        w: f32,
        h: f32,
        tex_size: [usize; 2],
        texture: egui::TextureId,
        opacity: f32,
        reflection: bool,
    ) -> Mesh {
        let segs = (4.0 + (rot.abs() * 8.0).ceil()).clamp(4.0, 10.0) as usize;
        // UniformToFill uv crop.
        let (tw, th) = (tex_size[0] as f32, tex_size[1] as f32);
        let sc_fill = (w / tw).max(h / th);
        let wr = w / sc_fill;
        let hr = h / sc_fill;
        let xo = ((tw - wr) * 0.5) / tw;
        let yo = ((th - hr) * 0.5) / th;
        let uw = wr / tw;
        let vh = hr / th;

        let col = Color32::from_white_alpha((opacity.clamp(0.0, 1.0) * 255.0) as u8);
        let mut mesh = Mesh::with_texture(texture);
        // Mirror across the card's bottom edge + gap: y=+h/2 (card
        // bottom, uv bottom) lands just below the card, y=-h/2 the
        // farthest down — a correct upside-down reflection.
        let refl_y = |y: f32| -y + h + 25.0;
        for si in 0..=segs {
            let u = si as f32 / segs as f32;
            let x = -w * 0.5 + w * u;
            let uu = xo + uw * u;
            for (y, vv) in [(-h * 0.5, yo), (h * 0.5, yo + vh)] {
                let ly = if reflection { refl_y(y) } else { y };
                let (pos, _) = Self::project(tx, tz, rot, sc, center, x, ly);
                mesh.vertices.push(Vertex {
                    pos,
                    uv: Pos2::new(uu, vv),
                    color: col,
                });
            }
            if si < segs {
                let b = si as u32 * 2;
                mesh.indices
                    .extend_from_slice(&[b, b + 1, b + 2, b + 1, b + 3, b + 2]);
            }
        }
        mesh
    }

    /// Projected corner positions of item quad (for hit testing and the
    /// selection border), TL TR BR BL.
    #[allow(clippy::too_many_arguments)]
    fn item_corners(
        &self,
        tx: f32,
        tz: f32,
        rot: f32,
        sc: f32,
        center: Pos2,
        w: f32,
        h: f32,
    ) -> [Pos2; 4] {
        let c = |x: f32, y: f32| Self::project(tx, tz, rot, sc, center, x, y).0;
        [
            c(-w * 0.5, -h * 0.5),
            c(w * 0.5, -h * 0.5),
            c(w * 0.5, h * 0.5),
            c(-w * 0.5, h * 0.5),
        ]
    }

    /// Transform + size for index-position `i` (fractional while a
    /// displaced neighbor glides between slots).
    #[allow(clippy::too_many_arguments)]
    fn item_geo(
        &self,
        i: f32,
        tex: &egui::TextureHandle,
        cfg: &settings::Carousel,
        s: f32,
    ) -> (f32, f32, f32, f32, f32, f32) {
        let (w, h) = Self::item_size(tex, cfg, s);
        // Wide-cover compensation is relative to the base frame width
        // (Lacrima's widthRatio = itemW / itemWidth).
        let frame_w = BASE_W * s * cfg.scale;
        let (tx, tz, rot, sc) = self.item_transform(i, w, h, frame_w, cfg, s);
        (tx, tz, rot, sc, w, h)
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_carousel(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        visible: &[usize],
        row: &mut AlbumRow,
        playing_vidx: Option<usize>,
        cfg: &settings::Carousel,
        alpha: f32,
        s: f32,
    ) {
        let ctx = ui.ctx().clone();
        // Clip to the cover area — when the EQ drawer lifts the
        // transport band the shrunk rect must cut the covers at its
        // edge, never let them stray over the bands (the top bar is a
        // front overlay, but the band edges aren't).
        let painter = ui.painter().clone().with_clip_rect(rect);
        let center = Pos2::new(
            rect.center().x,
            rect.center().y + cfg.vertical_offset * s,
        );

        let held = self.item_drag.as_ref().map(|d| d.index);
        // Draw farthest first so z-order follows the animation
        // continuously instead of snapping at the halfway point.
        // Displaced neighbors evaluate at their eased slot position.
        let mut order: Vec<usize> = (0..visible.len())
            .filter(|&i| {
                Some(i) != held
                    && (self.disp_index(i) - self.current).abs() < VISIBLE_RANGE
            })
            .collect();
        order.sort_by(|&a, &b| {
            let da = (self.disp_index(a) - self.current).abs();
            let db = (self.disp_index(b) - self.current).abs();
            db.partial_cmp(&da).unwrap_or(std::cmp::Ordering::Equal)
        });

        for vi in order {
            let idx = visible[vi];
            let item = &self.items[idx];
            let tex = Self::item_tex(row, &ctx, item);
            let ei = self.disp_index(vi);
            let (tx, tz, rot, sc, w, h) = self.item_geo(ei, &tex, cfg, s);
            let diff = (ei - self.current).abs();
            let base_opacity = (1.0 - diff * 0.2) * alpha;

            // Reflection below the card — flipped mesh, tiny alpha.
            let falloff = if diff < 4.8 {
                (1.0 - diff / 4.8).powf(1.2)
            } else {
                0.0
            };
            let refl_alpha = (base_opacity * 0.45 + 0.55) * 0.072 * falloff;
            if refl_alpha > 0.0015 {
                let rm = self.item_mesh(
                    tx,
                    tz,
                    rot,
                    sc,
                    center,
                    w,
                    h,
                    tex.size(),
                    tex.id(),
                    refl_alpha,
                    true,
                );
                painter.add(Shape::mesh(rm));
            }
            let m = self.item_mesh(
                tx,
                tz,
                rot,
                sc,
                center,
                w,
                h,
                tex.size(),
                tex.id(),
                base_opacity,
                false,
            );
            painter.add(Shape::mesh(m));

            // Playing item: pulsing white glow border on the projected
            // quad (DrawProjectedSelectionBorder) — a faint outer halo
            // plus a crisp hairline, not a thick frame.
            if Some(vi) == playing_vidx {
                let corners = self.item_corners(tx, tz, rot, sc, center, w, h);
                let pulse = 0.5 + 0.5 * self.pulse.sin();
                let glow = 0.55 + 0.45 * pulse;
                let outline = |width: f32, c: Color32| {
                    painter.add(Shape::closed_line(
                        corners.to_vec(),
                        Stroke::new(width * s.max(1.0), c),
                    ));
                };
                outline(
                    4.2 + 1.4 * pulse,
                    Color32::WHITE.gamma_multiply(0.22 * glow * alpha),
                );
                outline(
                    1.2 + 0.3 * pulse,
                    Color32::WHITE.gamma_multiply(
                        ((225.0 + 30.0 * pulse) / 255.0).min(1.0) * alpha,
                    ),
                );
            }
        }

        // The held card floats on top — unrotated, glued to the
        // pointer, scaled/shadowed by its lift. During the drop glide
        // it eases from the release point into the slot.
        if let Some(d) = &self.item_drag {
            let item = &self.items[d.index];
            let tex = Self::item_tex(row, &ctx, item);
            let (w, h) = Self::item_size(&tex, cfg, s);
            let e = d
                .commit
                .as_ref()
                .map(|c| ease_out_cubic(c.progress.min(1.0)));
            let c = match (&d.commit, e) {
                (Some(c), Some(t)) => lerp_pos(c.from, c.to, t),
                _ => d.center,
            };
            let lift = d.lift * (1.0 - e.unwrap_or(0.0));
            let sc = 1.0 + (ITEM_DRAG_LIFT - 1.0) * lift;
            // Unclipped: a lifted card reads as floating above the
            // chrome — hard-clipping it at the cover edge would pop.
            let float_paint = ui.painter();
            if lift > 0.01 {
                float_paint.rect_filled(
                    Rect::from_center_size(
                        c + Vec2::new(0.0, 14.0 * s),
                        Vec2::new(w, h) * sc,
                    ),
                    16.0 * s,
                    Color32::BLACK.gamma_multiply(0.38 * lift * alpha),
                );
            }
            let m = self.item_mesh(
                0.0,
                0.0,
                0.0,
                sc,
                c,
                w,
                h,
                tex.size(),
                tex.id(),
                alpha,
                false,
            );
            float_paint.add(Shape::mesh(m));
        }
    }

    /// Point-in-quad hit test, center item first then outward — the
    /// same priority the control uses so overlapping cards pick the
    /// front one.
    fn hit_carousel(
        &self,
        p: Pos2,
        rect: Rect,
        visible: &[usize],
        row: &mut AlbumRow,
        ctx: &egui::Context,
        cfg: &settings::Carousel,
        s: f32,
    ) -> Option<usize> {
        let center = Pos2::new(rect.center().x, rect.center().y + cfg.vertical_offset * s);
        let center_i = self.current.round() as i32;
        let mut order = vec![center_i];
        for off in 1..=(VISIBLE_RANGE as i32 + 1) {
            order.push(center_i + off);
            order.push(center_i - off);
        }
        for vi in order {
            if vi < 0 || vi as usize >= visible.len() {
                continue;
            }
            let item = &self.items[visible[vi as usize]];
            let tex = Self::item_tex(row, ctx, item);
            let (tx, tz, rot, sc, w, h) = self.item_geo(vi as f32, &tex, cfg, s);
            let q = self.item_corners(tx, tz, rot, sc, center, w, h);
            if point_in_quad(p, q) {
                return Some(vi as usize);
            }
        }
        None
    }

    /// One item's overlay text block — title, artist, album — drawn
    /// centered at `cx`, stacked upward from `cy`.
    fn overlay_text(
        &self,
        painter: &egui::Painter,
        cx: f32,
        cy: f32,
        idx: usize,
        alpha: f32,
        s: f32,
    ) {
        let item = &self.items[idx];
        let mut y = cy - 52.0 * s;
        painter.text(
            Pos2::new(cx, y),
            Align2::CENTER_CENTER,
            &item.title,
            FontId::proportional(28.0 * s),
            Color32::WHITE.gamma_multiply(alpha),
        );
        y += 34.0 * s;
        if !item.artist.is_empty() {
            painter.text(
                Pos2::new(cx, y),
                Align2::CENTER_CENTER,
                &item.artist,
                FontId::proportional(19.0 * s),
                Color32::WHITE.gamma_multiply(0.8 * alpha),
            );
            y += 26.0 * s;
        }
        if !item.album.is_empty() {
            painter.text(
                Pos2::new(cx, y),
                Align2::CENTER_CENTER,
                &item.album,
                FontId::proportional(16.0 * s),
                Color32::WHITE.gamma_multiply(0.6 * alpha),
            );
        }
    }

    /// The centered title lines + position pill (MusicView's overlay
    /// StackPanel, Margin 0,380,0,0).
    /// `view` is the visible cover area — the pill slider's interact
    /// widget only exists while its painted rect still fits inside it
    /// (an EQ-lifted band covering the slider must not leave an
    /// invisible hit-target over the transport).
    fn overlay_stack(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        view: Rect,
        visible: &[usize],
        alpha: f32,
        interactive: bool,
        s: f32,
    ) {
        // StackPanel: centered, Margin 0,380,0,0 => center +190.
        let cy = rect.center().y + 190.0 * s;
        let painter = ui.painter().with_clip_rect(rect);
        if let Some(idx) = self.overlay_index(visible) {
            self.overlay_text(&painter, rect.center().x, cy, idx, alpha, s);
        }

        // Pill slider: 520x28, track 16 (Margin 0,12,0,0 below text).
        // Only when it fully fits inside the cover area — a shrunk
        // rect (EQ drawer open) would leave an invisible interact
        // widget over the transport band stealing its clicks.
        let slider = Rect::from_center_size(
            Pos2::new(rect.center().x, cy + 62.0 * s),
            Vec2::new(520.0 * s, 28.0 * s),
        );
        if visible.len() > 1 && view.contains_rect(slider) {
            self.pill_slider(ui, slider, (visible.len() - 1) as f32, alpha, interactive, s);
        }
    }

    /// PillProgressSlider: capsule outline + gradient fill; drag sets
    /// the carousel target live (ExecuteDuringDrag) and hover previews
    /// the overlay title.
    fn pill_slider(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        max: f32,
        alpha: f32,
        interactive: bool,
        s: f32,
    ) {
        let painter = ui.painter().clone();
        let track_h = (16.0 * s).min(rect.height() - 4.0).max(8.0);
        let outer = Rect::from_min_max(
            Pos2::new(rect.min.x + 2.0, rect.center().y - track_h * 0.5),
            Pos2::new(rect.max.x - 2.0, rect.center().y + track_h * 0.5),
        );
        let r = track_h * 0.5;
        painter.rect_stroke(
            outer,
            r,
            Stroke::new(1.5, Color32::from_rgb(0xCC, 0xCC, 0xCC).gamma_multiply(alpha)),
            egui::StrokeKind::Inside,
        );
        let inner = outer.shrink(0.75 + track_h * 0.125);
        let ir = inner.height() * 0.5;
        if inner.width() <= 1.0 {
            return;
        }
        let resp = ui.interact(rect, ui.id().with("carousel-pill"), Sense::click_and_drag());
        if interactive {
            // Only while actually dragging — a bare hover must not
            // preview titles (the text tracks the carousel itself).
            let pos = resp
                .interact_pointer_pos()
                .or_else(|| ui.ctx().input(|i| i.pointer.latest_pos()));
            if resp.is_pointer_button_down_on() && pos.is_some() {
                let p = pos.unwrap();
                let v = ((p.x - inner.min.x) / inner.width()).clamp(0.0, 1.0) * max;
                if resp.is_pointer_button_down_on() {
                    // Drag scrubs the carousel live; a bare click snaps
                    // to the nearest item (SetCarouselIndexCommand).
                    self.target = if resp.dragged() {
                        v.clamp(0.0, max)
                    } else {
                        v.round().clamp(0.0, max)
                    };
                    self.wheel_settle_at = None;
                }
                self.preview = Some(v.round() as usize);
            } else {
                self.preview = None;
            }
        } else {
            self.preview = None;
        }
        let pct = (self.current / max.max(0.001)).clamp(0.0, 1.0);
        let fill_w = if pct <= 0.0 {
            0.0
        } else {
            (inner.width() * pct).clamp(ir * 2.0, inner.width())
        };
        if fill_w > 0.0 {
            let fill = Rect::from_min_size(inner.min, Vec2::new(fill_w, inner.height()));
            let top = Color32::from_rgb(0xF5, 0xF5, 0xF5).gamma_multiply(0.71 * alpha);
            let bot = Color32::from_rgb(0xF5, 0xF5, 0xF5).gamma_multiply(0.39 * alpha);
            painter.add(Shape::mesh(capsule_mesh(fill, ir, top, bot)));
        }
    }

    // ================================================================
    // GRID MODES
    // ================================================================

    fn hit_grid(&self, p: Pos2, visible: &[usize], geo: &GridGeo) -> Option<usize> {
        for (vi, _idx) in visible.iter().enumerate() {
            let _ = _idx;
            if let Some(r) = geo.card_rect(vi, self.grid_current) {
                if r.contains(p) {
                    return Some(vi);
                }
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_grid(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        visible: &[usize],
        row: &mut AlbumRow,
        playing_vidx: Option<usize>,
        geo: &GridGeo,
        scroll: f32,
        alpha: f32,
        s: f32,
    ) {
        let ctx = ui.ctx().clone();
        // Clip to the cover area — scrolled cards must never paint
        // over the top bar or the transport band (Lacrima keeps the
        // control below MainTopBar's own row).
        let painter = ui.painter().clone().with_clip_rect(rect);
        let pulse = 0.5 + 0.5 * self.pulse.sin();
        let held = self.item_drag.as_ref().map(|d| d.index);
        let n_items = visible.len();

        // Drop hole — the slot the held card would land in.
        if let Some(d) = &self.item_drag {
            if let Some(hole) =
                geo.card_rect(d.drop_target.min(n_items.saturating_sub(1)), scroll)
            {
                if hole.intersects(rect) {
                    painter.rect_filled(
                        hole.shrink(3.0 * s),
                        CARD_RADIUS * s,
                        Color32::WHITE.gamma_multiply(0.05 * alpha),
                    );
                    painter.rect_stroke(
                        hole.shrink(3.0 * s),
                        CARD_RADIUS * s,
                        Stroke::new(
                            1.4 * s.max(1.0),
                            Color32::WHITE.gamma_multiply(0.28 * alpha),
                        ),
                        egui::StrokeKind::Inside,
                    );
                }
            }
        }

        for (vi, &idx) in visible.iter().enumerate() {
            if Some(vi) == held {
                continue;
            }
            let Some(mut card) = geo.card_rect(vi, scroll) else {
                continue;
            };
            // Neighbors ease toward the slot the held card vacated.
            if let Some(d) = &self.item_drag {
                let disp =
                    Self::display_slot(vi, d.index, d.drop_target, n_items);
                let t = d.swap.get(&vi).copied().unwrap_or(0.0);
                if t > 0.001 && disp != vi {
                    if let Some(to) = geo.card_rect(disp, scroll) {
                        card = Rect::from_min_max(
                            lerp_pos(card.min, to.min, t),
                            lerp_pos(card.max, to.max, t),
                        );
                    }
                }
            }
            if !card.intersects(rect) {
                continue;
            }
            let item = &self.items[idx];
            let tex = Self::item_tex(row, &ctx, item);
            Self::paint_card(
                &painter,
                &ctx,
                card,
                item,
                &tex,
                Some(vi) == playing_vidx,
                pulse,
                idx == self.sel,
                alpha,
                s,
            );
        }

        // The held card floats on top, glued to the pointer.
        if let Some(d) = &self.item_drag {
            let item = &self.items[d.index];
            let tex = Self::item_tex(row, &ctx, item);
            let e = d
                .commit
                .as_ref()
                .map(|c| ease_out_cubic(c.progress.min(1.0)));
            let c = match (&d.commit, e) {
                (Some(c), Some(t)) => lerp_pos(c.from, c.to, t),
                _ => d.center,
            };
            let lift = d.lift * (1.0 - e.unwrap_or(0.0));
            let sc = 1.0 + (ITEM_DRAG_LIFT - 1.0) * lift;
            let card = Rect::from_center_size(
                c,
                Vec2::new(geo.card_w, geo.card_h) * sc,
            );
            // Unclipped — the lifted card floats over the chrome too.
            let float_paint = ui.painter();
            if lift > 0.01 {
                float_paint.rect_filled(
                    card.translate(Vec2::new(0.0, 12.0 * s)),
                    (CARD_RADIUS + 4.0) * s,
                    Color32::BLACK.gamma_multiply(0.40 * lift * alpha),
                );
            }
            Self::paint_card(
                &float_paint, &ctx, card, item, &tex, false, pulse, false, alpha, s,
            );
        }
    }

    /// One grid card — rounded frame, fill-cropped cover, gradient
    /// title strip, wrapped title text, selected/playing accents.
    /// Used for slotted cards and the floating drag card alike.
    #[allow(clippy::too_many_arguments)]
    fn paint_card(
        painter: &egui::Painter,
        ctx: &egui::Context,
        card: Rect,
        item: &Item,
        tex: &egui::TextureHandle,
        playing: bool,
        pulse: f32,
        selected: bool,
        alpha: f32,
        s: f32,
    ) {
        let title_h = card.height() * TITLE_AREA;
        let cover = Rect::from_min_max(card.min, Pos2::new(card.max.x, card.max.y - title_h));

        // Card: rounded clip, cover fill-cropped top, blurred-cover
        // stand-in + gradient title strip (CardDisplayBaker).
        painter.add(Shape::rect_filled(
            card,
            CARD_RADIUS * s,
            Color32::from_rgb(0x1E, 0x1E, 0x1E).gamma_multiply(alpha),
        ));
        let clip = painter.with_clip_rect(card.shrink(0.5));
        // Cover: fill-crop via a two-triangle uv mesh.
        let (tw, th) = (tex.size()[0] as f32, tex.size()[1] as f32);
        let fill = (cover.width() / tw).max(cover.height() / th);
        let wr = cover.width() / fill / tw;
        let hr = cover.height() / fill / th;
        let u0 = (1.0 - wr) * 0.5;
        let v0 = (1.0 - hr) * 0.5;
        let mut mesh = Mesh::with_texture(tex.id());
        let col = Color32::from_white_alpha((alpha * 255.0) as u8);
        for (px, uvx) in [
            (cover.min.x, u0),
            (cover.max.x, u0 + wr),
        ] {
            for (py, uvy) in [
                (cover.min.y, v0),
                (cover.max.y, v0 + hr),
            ] {
                mesh.vertices.push(Vertex {
                    pos: Pos2::new(px, py),
                    uv: Pos2::new(uvx, uvy),
                    color: col,
                });
            }
        }
        mesh.indices.extend_from_slice(&[0, 1, 2, 1, 3, 2]);
        clip.add(Shape::mesh(mesh));
        // Title strip: black gradient over the cover tail.
        let trect = Rect::from_min_max(Pos2::new(card.min.x, cover.max.y), card.max);
        let mut gm = Mesh::default();
        let top = Color32::BLACK.gamma_multiply(0.04 * alpha);
        let bot = Color32::BLACK.gamma_multiply(0.69 * alpha);
        for (px, cx) in [(trect.min.x, top), (trect.max.x, top)] {
            gm.vertices.push(Vertex {
                pos: Pos2::new(px, trect.min.y),
                uv: Pos2::ZERO,
                color: cx,
            });
        }
        for (px, cx) in [(trect.min.x, bot), (trect.max.x, bot)] {
            gm.vertices.push(Vertex {
                pos: Pos2::new(px, trect.max.y),
                uv: Pos2::ZERO,
                color: cx,
            });
        }
        gm.indices.extend_from_slice(&[0, 1, 2, 1, 3, 2]);
        clip.add(Shape::mesh(gm));
        // Title text: up to 2 wrapped lines with ellipsis,
        // shadow + white (the baked card's title area).
        let font = FontId::proportional((card.width() * 0.09).clamp(13.0 * s, 17.0 * s));
        let mut job = egui::text::LayoutJob::default();
        job.wrap = egui::text::TextWrapping {
            max_width: card.width() - 24.0 * s,
            max_rows: 2,
            break_anywhere: true,
            overflow_character: Some('…'),
        };
        job.append(
            &item.title,
            0.0,
            // PLACEHOLDER so each galley() call's color wins —
            // an explicit white here would ignore the alpha fade.
            egui::text::TextFormat::simple(font, Color32::PLACEHOLDER),
        );
        let galley = ctx.fonts_mut(|f| f.layout_job(job));
        let tpos = Pos2::new(
            card.min.x + 12.0 * s,
            trect.center().y - galley.size().y * 0.5,
        );
        clip.galley(
            tpos + Vec2::new(0.0, s.max(1.0)),
            galley.clone(),
            Color32::BLACK.gamma_multiply(0.55 * alpha),
        );
        clip.galley(tpos, galley, Color32::WHITE.gamma_multiply(0.96 * alpha));

        // Selected / playing accents.
        if playing {
            painter.rect_stroke(
                card.expand(1.0),
                (CARD_RADIUS + 1.0) * s,
                Stroke::new(
                    (2.0 + 0.7 * pulse) * s.max(1.0),
                    Color32::WHITE.gamma_multiply((0.55 + 0.35 * pulse) * alpha),
                ),
                egui::StrokeKind::Inside,
            );
        } else if selected {
            painter.rect_stroke(
                card.expand(0.5),
                (CARD_RADIUS + 0.5) * s,
                Stroke::new(
                    1.5 * s.max(1.0),
                    Color32::WHITE.gamma_multiply(0.35 * alpha),
                ),
                egui::StrokeKind::Inside,
            );
        }
    }
}

// ------------------------------------------------------------------
// Grid geometry — CardGridLayoutHelper + CardGridHorizontalLayout.
// ------------------------------------------------------------------
struct GridGeo {
    card_w: f32,
    card_h: f32,
    spacing: f32,
    pad_left: f32,
    pad_top: f32,
    columns: usize,
    horizontal: bool,
    max_scroll: f32,
    /// Viewport top-left the card offsets are relative to.
    origin: Pos2,
}

impl GridGeo {
    /// Vertical card grid. Card Size is a fixed px base — the slider
    /// scales cards smoothly; columns only drop when a card no longer
    /// fits, so sizes never jump (the content block centers in the
    /// free width).
    fn vertical(rect: Rect, count: usize, cfg: &settings::Carousel, s: f32) -> Self {
        let spacing = cfg.card_spacing.max(4.0) * s;
        let avail_w = (rect.width() - GRID_PAD_X * 2.0 * s - SCROLLBAR_RESERVE * s).max(80.0);
        let card_w = (cfg.card_size * s).min(avail_w);
        let card_h = card_w * CARD_RATIO;
        let columns = (((avail_w + spacing) / (card_w + spacing)) as usize).max(1);
        let content_w = columns as f32 * card_w + (columns.saturating_sub(1)) as f32 * spacing;
        let pad_left = GRID_PAD_X * s + (avail_w - content_w).max(0.0) * 0.5;
        let rows = count.div_ceil(columns);
        let content_h = GRID_TOP * s
            + rows as f32 * card_h
            + (rows.saturating_sub(1)) as f32 * spacing
            + GRID_BOTTOM * s;
        Self {
            card_w,
            card_h,
            spacing,
            pad_left,
            pad_top: GRID_TOP * s,
            columns,
            horizontal: false,
            max_scroll: (content_h - rect.height()).max(0.0),
            origin: rect.min,
        }
    }

    /// Horizontal card grid — same fixed-size model, rows collapse
    /// only when a card no longer fits vertically.
    fn horizontal(rect: Rect, count: usize, cfg: &settings::Carousel, s: f32) -> Self {
        let spacing = cfg.card_spacing.max(4.0) * s;
        let avail_h = (rect.height() - GRID_TOP * s - GRID_BOTTOM * s - SB_ZONE * s).max(120.0);
        let card_h = (cfg.card_size * CARD_RATIO * s).min(avail_h);
        let card_w = card_h / CARD_RATIO;
        let rows = (((avail_h + spacing) / (card_h + spacing)) as usize)
            .clamp(1, count.max(1));
        let content_h = rows as f32 * card_h + (rows.saturating_sub(1)) as f32 * spacing;
        let pad_top = GRID_TOP * s + (avail_h - content_h).max(0.0) * 0.5;
        let columns = count.max(1).div_ceil(rows);
        let content_w = H_PAD_X * 2.0 * s
            + columns as f32 * card_w
            + (columns.saturating_sub(1)) as f32 * spacing;
        Self {
            card_w,
            card_h,
            spacing,
            pad_left: H_PAD_X * s,
            pad_top,
            columns,
            horizontal: true,
            max_scroll: (content_w - rect.width()).max(0.0),
            origin: rect.min,
        }
    }

    /// Overlay scrollbar geometry for this grid — a thin track on the
    /// far edge (right for vertical, bottom for horizontal) with a
    /// generous hit zone. `None` when the content fits.
    fn scrollbar(&self, rect: Rect, scroll: f32, s: f32) -> Option<SbGeo> {
        if self.max_scroll <= 0.5 {
            return None;
        }
        let (track, horizontal) = if self.horizontal {
            (
                Rect::from_min_max(
                    Pos2::new(rect.min.x + H_PAD_X * s, rect.max.y - SB_W * s - 3.0 * s),
                    Pos2::new(rect.max.x - H_PAD_X * s, rect.max.y - 3.0 * s),
                ),
                true,
            )
        } else {
            (
                Rect::from_min_max(
                    Pos2::new(rect.max.x - SB_W * s - 3.0 * s, rect.min.y + GRID_TOP * s),
                    Pos2::new(rect.max.x - 3.0 * s, rect.max.y - GRID_BOTTOM * s),
                ),
                false,
            )
        };
        let view = if horizontal { rect.width() } else { rect.height() };
        let frac = (scroll / self.max_scroll).clamp(0.0, 1.0);
        let view_frac = (view / (view + self.max_scroll)).min(1.0);
        let (thumb_len, travel) = if horizontal {
            (
                (track.width() * view_frac).max(24.0 * s),
                track.width(),
            )
        } else {
            (
                (track.height() * view_frac).max(24.0 * s),
                track.height(),
            )
        };
        let thumb_len = thumb_len.min(travel);
        let thumb = if horizontal {
            Rect::from_min_size(
                Pos2::new(track.min.x + (travel - thumb_len) * frac, track.min.y),
                Vec2::new(thumb_len, track.height()),
            )
        } else {
            Rect::from_min_size(
                Pos2::new(track.min.x, track.min.y + (travel - thumb_len) * frac),
                Vec2::new(track.width(), thumb_len),
            )
        };
        let hit = track.expand(6.0 * s);
        Some(SbGeo {
            track,
            thumb,
            hit,
            horizontal,
            max_scroll: self.max_scroll,
        })
    }

    /// Card rect for visible item `vi`, with the scroll offset applied.
    /// Grid offsets are absolute — callers add the viewport origin.
    fn card_rect(&self, vi: usize, scroll: f32) -> Option<Rect> {
        if self.columns == 0 {
            return None;
        }
        let (col, row_) = (vi % self.columns, vi / self.columns);
        let (x, y) = if self.horizontal {
            (
                self.pad_left + col as f32 * (self.card_w + self.spacing) - scroll,
                self.pad_top + row_ as f32 * (self.card_h + self.spacing),
            )
        } else {
            (
                self.pad_left + col as f32 * (self.card_w + self.spacing),
                self.pad_top + row_ as f32 * (self.card_h + self.spacing) - scroll,
            )
        };
        Some(Rect::from_min_size(
            Pos2::new(self.origin.x + x, self.origin.y + y),
            Vec2::new(self.card_w, self.card_h),
        ))
    }
}

/// Grid scrollbar geometry (see `GridGeo::scrollbar`).
struct SbGeo {
    track: Rect,
    thumb: Rect,
    /// Generous grab zone around the thin visual track.
    hit: Rect,
    horizontal: bool,
    max_scroll: f32,
}

/// Ease-out cubic — the mode crossfade eases into its final alpha.
fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// Point lerp — drag cards ease along the line between slots.
fn lerp_pos(a: Pos2, b: Pos2, t: f32) -> Pos2 {
    a + (b - a) * t
}

/// Convex-quad point test (TL TR BR BL order).
fn point_in_quad(p: Pos2, q: [Pos2; 4]) -> bool {
    let mut sign = 0.0f32;
    for i in 0..4 {
        let a = q[i];
        let b = q[(i + 1) % 4];
        let cross = (b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x);
        if cross.abs() < 1e-6 {
            continue;
        }
        if sign == 0.0 {
            sign = cross.signum();
        } else if cross.signum() != sign {
            return false;
        }
    }
    true
}

/// Capsule outline with a top→bottom color gradient (the pill fill):
/// the outline is triangulated as a fan and each vertex is colored by
/// its y position, which yields the vertical gradient for free.
fn capsule_mesh(rect: Rect, r: f32, top: Color32, bot: Color32) -> Mesh {
    let mut mesh = Mesh::default();
    let steps = 8usize;
    let cy = rect.center().y;
    let x0 = rect.min.x + r;
    let x1 = rect.max.x - r;
    let h = rect.height().max(1.0);
    let col_at = |y: f32| {
        let t = ((y - rect.min.y) / h).clamp(0.0, 1.0);
        let (t1, b1) = (1.0 - t, t);
        Color32::from_rgba_unmultiplied(
            (top.r() as f32 * t1 + bot.r() as f32 * b1) as u8,
            (top.g() as f32 * t1 + bot.g() as f32 * b1) as u8,
            (top.b() as f32 * t1 + bot.b() as f32 * b1) as u8,
            (top.a() as f32 * t1 + bot.a() as f32 * b1) as u8,
        )
    };
    // Ring: (x0,top) → left arc down → (x1,bottom) → right arc up.
    let mut ring = vec![Pos2::new(x0, rect.min.y)];
    for i in 1..steps {
        let a = -std::f32::consts::FRAC_PI_2 - (i as f32 / steps as f32) * std::f32::consts::PI;
        ring.push(Pos2::new(x0 + a.cos() * r, cy + a.sin() * r));
    }
    ring.push(Pos2::new(x0, rect.max.y));
    ring.push(Pos2::new(x1, rect.max.y));
    for i in 1..steps {
        let a = std::f32::consts::FRAC_PI_2 - (i as f32 / steps as f32) * std::f32::consts::PI;
        ring.push(Pos2::new(x1 + a.cos() * r, cy + a.sin() * r));
    }
    ring.push(Pos2::new(x1, rect.min.y));
    for p in &ring {
        mesh.vertices.push(Vertex {
            pos: *p,
            uv: Pos2::ZERO,
            color: col_at(p.y),
        });
    }
    // Fan from vertex 0 — the capsule is convex.
    for i in 1..(ring.len() - 1) {
        mesh.indices.extend_from_slice(&[0, i as u32, i as u32 + 1]);
    }
    mesh
}
