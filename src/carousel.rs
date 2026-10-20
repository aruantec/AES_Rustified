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

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use eframe::egui::epaint::Vertex;
use eframe::egui::{self, Align2, Color32, FontId, Mesh, Pos2, Rect, Sense, Shape, Stroke, Vec2};

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
/// Scrollbar track thickness for the grid bars — the vertical bar is
/// the grid's main scroll affordance, so it gets a wider face; the
/// horizontal bar doubles the old 5px so the thumb is grabbable.
const SB_W_V: f32 = 15.0;
const SB_W_H: f32 = 10.0;
/// Vertical track inset from the view's right edge: must clear the
/// window's 8px resize border plus the hit-zone expansion, or the
/// resize cursor swallows every scrollbar grab.
const SB_INSET_V: f32 = 16.0;
/// Horizontal track inset from the view's bottom edge.
const SB_INSET_H: f32 = 3.0;
/// Reserved bottom zone for the horizontal bar's grid layout.
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
const ITEM_COMMIT_SECS: f32 = 0.45;
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
    /// "Delete" on the pointed item or the Delete key on the
    /// selection — Lacrima's DeletePointedItem, plural for the
    /// multi-selection (removes from the view, not from disk).
    Delete(Vec<PathBuf>),
    /// "Clear Album" — empties the loaded album's children
    /// (Lacrima's ClearAlbumCommand; files are never touched).
    Clear,
    /// ROM mode: "Clear Album Cache" — deletes the section's fetched
    /// title/cover sidecars and re-runs the resolver (Lacrima's
    /// ClearAlbumCacheCommand).
    ClearCache,
    /// "Export Playlist" — write the album's URL items to an .apl
    /// file (Lacrima's ExportOnlinePlaylistCommand).
    Export,
    /// ROM mode: "Add Roms" context menu — the caller picks files.
    AddRoms,
    /// ROM mode: "Scan Folder" context menu — recursive scan.
    ScanFolder,
    /// X360 ROM menu: "Patches" — the XeniaPatchesOverlay (PS3/PS4
    /// sections reuse it for their RPCS3/shadPS4 patch overlays).
    Patches(PathBuf),
    /// X360 ROM menu: "Custom Config" — the XeniaCustomConfigOverlay.
    CustomConfig(PathBuf),
    /// PS4 ROM menu: "Cheats" — the ShadPs4CheatsOverlay.
    Cheats(PathBuf),
    /// WIIU ROM menu: "Graphic Packs" — the CemuGraphicPacksOverlay.
    GraphicPacks(PathBuf),
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
    /// Tag duration in seconds — `None` for ROM items and items
    /// whose info hasn't resolved yet (the mini list shows "--:--").
    duration: Option<f64>,
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

/// Tracks the screen rect across frames. A window resize re-maps
/// every pointer coordinate — an in-flight press/drag is stale, and
/// a press caught while the OS resize grip is active must never arm
/// a card lift or a scroll.
#[derive(Default)]
struct ResizeWatch {
    last: Option<Rect>,
}

impl ResizeWatch {
    /// True while the screen rect is changing (each resize frame).
    fn update(&mut self, screen: Rect) -> bool {
        let changed = self.last.map(|r| r != screen).unwrap_or(false);
        self.last = Some(screen);
        changed
    }
}

/// In-flight item reorder: the card floated off its slot and follows
/// the pointer while neighbors slide aside. Committing applies the
/// actual `items` move — `display_slot` is exactly the post-move
/// permutation, so the landing is seamless.
/// `indices` is the dragged block (sorted, contains `index`) — the
/// current selection when the held card is selected, else just the
/// held card (ListboxItemDragBehavior's _draggedIndices).
struct ItemDrag {
    /// Index into `items` (reorder only runs unfiltered, so visible
    /// index == items index throughout).
    index: usize,
    /// Sorted dragged block — moves as one contiguous run.
    indices: Vec<usize>,
    /// Grab point kept constant: pointer - card center at lift.
    grab: Vec2,
    /// Floating primary card's current center.
    center: Pos2,
    /// Slot the primary card would land in now; the block lands
    /// contiguously around it.
    drop_target: usize,
    /// Per-item displacement blend 0..1 toward the shifted slot —
    /// eases the gap open instead of teleporting neighbors.
    swap: HashMap<usize, f32>,
    /// 0→1 glue ease — selected cards collapse from their scattered
    /// slots into a contiguous block (Lacrima's AnimateGlue).
    glue: f32,
    /// 0→1 lift-in (scale + shadow).
    lift: f32,
    /// Release glide — floats from pointer to the drop slots' centers.
    commit: Option<ItemCommit>,
}

struct ItemCommit {
    /// Every dragged card's center at release — they glide home as a
    /// group, `starts[k]` -> `targets[k]`.
    starts: Vec<Pos2>,
    /// Each dragged card's landing slot center after the drop.
    targets: Vec<Pos2>,
    progress: f32,
}

pub struct Carousel {
    items: Vec<Item>,
    /// Spring state in index units (also grid "selected" index).
    current: f32,
    target: f32,
    velocity: f32,
    sel: usize,
    /// Multi-selection (SelectionMode=Multiple) — item indices;
    /// `sel` is the primary. Painted on cards in the grid modes.
    sel_set: BTreeSet<usize>,
    /// Item the last plain/ctrl click landed on — Shift+click extends
    /// the range from it.
    sel_anchor: usize,
    /// Set on any user selection gesture — the app consumes it to
    /// decide which view owns the Delete key.
    sel_touched: bool,
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
    /// Last rendered scrollbar geometry + hot state — the horizontal
    /// bar docks at the bottom edge where the spectrum paints AFTER
    /// this view, so `repaint_scrollbar` redraws it on top.
    last_sb: Option<(SbGeo, bool)>,
    /// Screen-rect watch — a resize drops in-flight press/drag state
    /// and blocks new presses while coordinates re-map.
    resize: ResizeWatch,
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
    /// Last frame's grid geometry + view rect — arrow-key navigation
    /// scrolls the target card into view between paints.
    nav_geo: Option<(GridGeo, Rect)>,
    /// Brief focus flash (1→0) on Tab focus-in — a fading border
    /// around the view so the user sees where the keys now land.
    focus_flash: f32,
    /// Held lift (0→1) while Shift is down — the selection floats
    /// slightly to signal it can be reordered with arrows.
    pop: f32,
    /// ROM mode (the Emulation view): same widget, different menu
    /// vocabulary — Add Roms / Scan Folder / Play / Remove /
    /// Clear Section instead of the URL/playlist entries.
    pub rom_mode: bool,
    /// ROM mode + Xenia handler — shows the Lacrima "Patches" /
    /// "Custom Config" item menu entries (XeniaPatchesMenuItem /
    /// XeniaCustomConfigMenuItem).
    pub xenia_menu: bool,
    /// ROM mode + RPCS3 handler — shows the Lacrima "Patches" item
    /// entry (Rpcs3PatchesMenuItem).
    pub rpcs3_menu: bool,
    /// ROM mode + shadPS4 handler — shows the "Patches" + "Cheats"
    /// entries (ShadPs4PatchesMenuItem / ShadPs4CheatsMenuItem).
    pub shadps4_menu: bool,
    /// ROM mode + cemu handler — shows the "Graphic Packs" entry
    /// (ShowCurrentSectionCemuGraphicPacksMenuItem).
    pub cemu_menu: bool,
    /// Row floor for the horizontal card grid — compact transport
    /// mode sets 2 so the shrunken cover area still shows two rows
    /// (cards shrink to fit); normal layouts leave it at 1.
    pub h_min_rows: usize,
    /// Marquee the overlay title when it overflows (compact mode).
    /// Lacrima's MarqueeBehavior: scrolls left at 40px/s, re-entering
    /// from off the right edge (+40% text-width pad) after a 0.3s
    /// wrap pause.
    pub marquee_title: bool,
    marquee_off: f32,
    marquee_text: String,
    marquee_pause: Instant,
}

impl Carousel {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            current: 0.0,
            target: 0.0,
            velocity: 0.0,
            sel: 0,
            sel_set: BTreeSet::new(),
            sel_anchor: 0,
            sel_touched: false,
            press: None,
            wheel_settle_at: None,
            grid_current: 0.0,
            grid_target: 0.0,
            grid_vel: 0.0,
            sb_drag: None,
            sb_opacity: 0.0,
            sb_visible_until: Instant::now(),
            last_sb: None,
            resize: ResizeWatch::default(),
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
            nav_geo: None,
            focus_flash: 0.0,
            pop: 0.0,
            rom_mode: false,
            xenia_menu: false,
            rpcs3_menu: false,
            shadps4_menu: false,
            cemu_menu: false,
            h_min_rows: 1,
            marquee_title: false,
            marquee_off: 0.0,
            marquee_text: String::new(),
            marquee_pause: Instant::now(),
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
                    duration: None,
                }
            })
            .collect();
        for u in urls {
            self.push_url_item(u);
        }
        self.album_key = Some(album.dir.to_string_lossy().into_owned());
        self.album_title = album.title.clone();
        self.reset_view();
        (
            self.items.iter().map(|i| i.path.clone()).collect(),
            album.title.clone(),
        )
    }

    /// ROM mode load (Emulation view's section → game list): same
    /// item model, but covers come from the caller's resolver (a
    /// cached online box-art file when one was fetched, else the
    /// console-art fallback via the failed-decode path).
    /// `fallback` is the section's console art — NOT `album.covers`:
    /// the section tile can wear the first ROM's box art ("Use first
    /// item cover"), and sharing that made every coverless card show
    /// another game's cover.
    pub fn open_roms(
        &mut self,
        album: &Album,
        cover: &dyn Fn(&Path) -> Option<CoverSrc>,
        fallback: Option<CoverSrc>,
    ) {
        self.items = album
            .tracks
            .iter()
            .map(|p| Self::rom_item(p.clone(), cover, &fallback, &album.title))
            .collect();
        self.album_key = Some(album.dir.to_string_lossy().into_owned());
        self.album_title = album.title.clone();
        self.reset_view();
    }

    fn rom_item(
        path: PathBuf,
        cover: &dyn Fn(&Path) -> Option<CoverSrc>,
        fallback: &Option<CoverSrc>,
        section: &str,
    ) -> Item {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Item {
            cover: cover(&path)
                .or_else(|| fallback.clone())
                // Web([]) fails fast without IO — the card then falls
                // back through `item_tex` to the placeholder.
                .unwrap_or_else(|| CoverSrc::Web(Vec::new())),
            fallback: fallback.clone(),
            title: stem,
            artist: String::new(),
            album: section.to_string(),
            path,
            duration: None,
        }
    }

    /// Append a ROM item (Add Roms / Scan Folder results) — deduped
    /// like the URL path. Caller persists the new games list.
    pub fn add_rom(
        &mut self,
        path: PathBuf,
        cover: &dyn Fn(&Path) -> Option<CoverSrc>,
        fallback: &Option<CoverSrc>,
    ) -> bool {
        if self.items.iter().any(|i| i.path == path) {
            return false;
        }
        let section = self.album_title.clone();
        self.items
            .push(Self::rom_item(path, cover, fallback, &section));
        true
    }

    /// An online-fetched cover landed for `path` — swap the item's
    /// cover source so the decode pool picks the new file up.
    pub fn set_cover(&mut self, path: &Path, src: CoverSrc) {
        if let Some(i) = self.index_of(path) {
            self.items[i].cover = src;
        }
    }

    /// Shared post-load reset (spring, selection, drag/menu state)
    /// so `open` and `open_roms` can't drift apart.
    fn reset_view(&mut self) {
        self.current = 0.0;
        self.target = 0.0;
        self.velocity = 0.0;
        self.sel = 0;
        self.sel_set = BTreeSet::from([0]);
        self.sel_anchor = 0;
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
    }

    /// Push one URL item (deduped like Lacrima's IsMediaDuplicate —
    /// same string or same canonical YouTube video id). Caller persists
    /// the url into settings.
    pub fn add_url(&mut self, url: &str) -> bool {
        // Canonical form (Lacrima's GetCleanVideoLink): the same video
        // submitted as a playlist/mix link must not become a distinct
        // item — or a distinct stream-cache key — from the watch url.
        let url = crate::ytdlp::clean_video_link(url.trim());
        if url.is_empty() || self.is_duplicate(&url) {
            return false;
        }
        self.push_url_item(&url);
        true
    }

    fn push_url_item(&mut self, url: &str) {
        let id = crate::ytdlp::extract_video_id(url);
        self.items.push(Item {
            path: PathBuf::from(url),
            // web_for also picks up a cover the metadata editor saved
            // into the local cache for this url.
            cover: CoverSrc::web_for(url).unwrap_or_else(|| CoverSrc::Web(Vec::new())),
            fallback: None,
            title: id.unwrap_or_else(|| url.to_string()),
            artist: String::new(),
            album: self
                .items
                .first()
                .map(|i| i.album.clone())
                .unwrap_or_default(),
            duration: None,
        });
    }

    /// The cover source for a resolved stream thumbnail — an edited
    /// local cover in the meta cache wins over remote art.
    fn item_cover(path: &Path, thumb: &str) -> CoverSrc {
        let local = crate::meta_cache::cover_path(&path.to_string_lossy());
        if local.exists() {
            CoverSrc::File(local)
        } else {
            CoverSrc::Web(vec![thumb.to_string()])
        }
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

    /// The loaded album's file items — the counterpart to
    /// `url_items`, persisted as `ExcludedTracks` on Clear Album so a
    /// rescan doesn't resurrect them.
    pub fn file_items(&self) -> Vec<PathBuf> {
        self.items
            .iter()
            .filter(|i| !crate::library::is_url(&i.path))
            .map(|i| i.path.clone())
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

    /// Remove several items at once — the Delete key on a
    /// multi-selection. Indices resolve first, then removals run
    /// descending so earlier positions stay valid.
    pub fn remove_many(&mut self, paths: &[PathBuf]) {
        self.item_drag = None;
        let mut idxs: Vec<usize> = paths.iter().filter_map(|p| self.index_of(p)).collect();
        idxs.sort_unstable();
        idxs.dedup();
        for &i in idxs.iter().rev() {
            self.items.remove(i);
            // Remap the multi-selection: drop the removed item,
            // shift later members down.
            self.sel_set.remove(&i);
            let shifted: BTreeSet<usize> = self
                .sel_set
                .iter()
                .map(|&s| if s > i { s - 1 } else { s })
                .collect();
            self.sel_set = shifted;
        }
        if !idxs.is_empty() {
            // Primary + anchor shift down by every removal before them.
            let shift = |v: usize| v.saturating_sub(idxs.iter().filter(|&&i| i < v).count());
            self.sel = shift(self.sel);
            self.sel_anchor = shift(self.sel_anchor);
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
        self.sel_set.clear();
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
        self.sel_set.clear();
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
            item.duration = info.duration;
            if let Some(u) = &info.cover_url {
                item.cover = Self::item_cover(&item.path, u);
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
        Some((self.items.iter().map(|i| i.path.clone()).collect(), title))
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
            item.duration = info.duration;
            // Online items: the resolved thumbnail replaces the
            // guessed img.youtube.com candidate list (unless an edited
            // local cover exists — it wins).
            if let Some(u) = info.cover_url {
                item.cover = Self::item_cover(&item.path, &u);
            }
        }
    }

    /// Index of `path` in the loaded items (the playing track).
    pub fn index_of(&self, path: &Path) -> Option<usize> {
        self.items.iter().position(|i| i.path == path)
    }

    /// (index, path, duration) triples in loaded order — the mini
    /// shell's flat track list.
    pub fn item_paths(&self) -> impl Iterator<Item = (usize, &Path, Option<f64>)> {
        self.items
            .iter()
            .enumerate()
            .map(|(i, it)| (i, it.path.as_path(), it.duration))
    }

    /// (index, path, title, album, duration) rows — the mini
    /// shell's playlist view (Lacrima's CoverItems list).
    pub fn item_rows(
        &self,
    ) -> impl Iterator<Item = (usize, &Path, &str, &str, Option<f64>)> {
        self.items.iter().enumerate().map(|(i, it)| {
            (
                i,
                it.path.as_path(),
                it.title.as_str(),
                it.album.as_str(),
                it.duration,
            )
        })
    }

    /// Fill an item's duration once it becomes known (resolved stream
    /// `dur=`, late metadata) — the mini list re-renders from this.
    pub fn set_item_duration(&mut self, path: &Path, secs: f64) {
        if let Some(it) = self.items.iter_mut().find(|i| i.path == path) {
            if it.duration.is_none() {
                it.duration = Some(secs);
            }
        }
    }

    /// Summed tag durations of the loaded items — the mini footer's
    /// total play time.
    pub fn total_duration(&self) -> f64 {
        self.items.iter().filter_map(|i| i.duration).sum()
    }

    /// Number of loaded items (the mini shell's "n of m" strip).
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Move the items at `from` (any order) into a block starting at
    /// `to`, where `to` counts positions in the sequence of NON-moved
    /// items — the mini playlist's drag-reorder (Lacrima's
    /// ListboxItemDragBehavior commit).
    pub fn move_items(&mut self, from: &[usize], to: usize) {
        let mut idx: Vec<usize> = from.to_vec();
        idx.sort_unstable();
        idx.dedup();
        idx.retain(|&i| i < self.items.len());
        if idx.is_empty() {
            return;
        }
        let sel: std::collections::HashSet<usize> = idx.iter().copied().collect();
        let mut moving = Vec::with_capacity(idx.len());
        let mut rest = Vec::with_capacity(self.items.len() - idx.len());
        for (i, it) in self.items.drain(..).enumerate() {
            if sel.contains(&i) {
                moving.push(it);
            } else {
                rest.push(it);
            }
        }
        let at = to.min(rest.len());
        for (j, it) in moving.into_iter().enumerate() {
            rest.insert(at + j, it);
        }
        self.items = rest;
        // Same persist path as the hold-to-lift reorder — file items
        // too, via the library's per-album item_order.
        self.reorder_dirty = true;
    }

    /// Reorder `items` to a persisted path order (`settings.library.
    /// item_order`): listed paths take their saved slots in order,
    /// unknown items keep their scan order appended at the end —
    /// `album_order`'s merge semantics applied inside one album.
    pub fn apply_order(&mut self, order: &[String]) {
        if order.is_empty() || self.items.is_empty() {
            return;
        }
        let pos: std::collections::HashMap<&str, usize> = order
            .iter()
            .map(|s| s.as_str())
            .enumerate()
            .map(|(i, s)| (s, i))
            .collect();
        let mut keyed: Vec<(usize, usize, Item)> = self
            .items
            .drain(..)
            .enumerate()
            .map(|(i, it)| {
                let p = pos
                    .get(it.path.to_string_lossy().as_ref())
                    .copied()
                    .unwrap_or(usize::MAX);
                (p, i, it)
            })
            .collect();
        keyed.sort_by_key(|(p, i, _)| (*p, *i));
        self.items = keyed.into_iter().map(|(.., it)| it).collect();
    }

    /// Queue a scroll-to for `path`'s item — the playing item drives
    /// the selection marker and gets scrolled into view on the next
    /// frame (transport next/prev, auto-advance on track end). The
    /// selection itself lands NOW: `pending_reveal` only carries the
    /// scroll, which needs ui() geometry — contexts that never run
    /// carousel.ui (the mini shell, a just-restored session) would
    /// otherwise leave `sel` on items[0] and play_fallback would
    /// resolve the wrong track.
    pub fn reveal(&mut self, path: &Path) {
        if let Some(idx) = self.index_of(path) {
            self.sel = idx;
            self.sel_set = BTreeSet::from([idx]);
            self.sel_anchor = idx;
        }
        self.pending_reveal = Some(path.to_path_buf());
    }

    /// Any items loaded (gates arrow-key ownership when no view has
    /// claimed selection yet).
    pub fn has_items(&self) -> bool {
        !self.items.is_empty()
    }

    /// Arrow-key navigation (ListBox directional nav): move the
    /// selection by `dx`/`dy` in *visible* order — ±1 horizontally in
    /// every mode, ±columns vertically in the grids — collapsing the
    /// multi-selection and scrolling the landing item into view.
    pub fn arrow(&mut self, filter: &str, dx: i32, dy: i32) {
        let visible = self.visible(filter);
        if visible.is_empty() {
            return;
        }
        // Selection filtered out of view anchors at the centered item.
        let pos = visible
            .iter()
            .position(|&i| i == self.sel)
            .unwrap_or_else(|| {
                self.current.round().clamp(0.0, (visible.len() - 1) as f32) as usize
            });
        let cols = self
            .nav_geo
            .as_ref()
            .map(|(g, _)| g.columns.max(1))
            .unwrap_or(1);
        let delta = match self.mode {
            // The carousel row has no rows — both axes step ±1.
            Mode::Carousel => dx + dy,
            _ => dx + dy * cols as i32,
        };
        let new_pos = (pos as i32 + delta).clamp(0, visible.len() as i32 - 1) as usize;
        if new_pos == pos {
            return;
        }
        let idx = visible[new_pos];
        self.sel = idx;
        self.sel_anchor = idx;
        self.sel_set = BTreeSet::from([idx]);
        self.sel_touched = true;
        self.reveal_visible(new_pos);
    }

    /// Shift+arrow — move the selection one step in `items` (the
    /// drag-reorder's keyboard twin: the whole selection moves as a
    /// contiguous run, gated on an unfiltered view). Same stepping
    /// as `arrow` — ±1 horizontally, ±columns vertically in grids.
    pub fn move_selection(&mut self, filter: &str, dx: i32, dy: i32) {
        if (dx == 0 && dy == 0) || !filter.trim().is_empty() {
            return;
        }
        let n = self.items.len();
        if n < 2 {
            return;
        }
        let indices: Vec<usize> = if self.sel_set.is_empty() {
            vec![self.sel.min(n - 1)]
        } else {
            self.sel_set.iter().copied().filter(|&i| i < n).collect()
        };
        if indices.is_empty() {
            return;
        }
        let cols = self
            .nav_geo
            .as_ref()
            .map(|(g, _)| g.columns.max(1))
            .unwrap_or(1);
        let delta = match self.mode {
            Mode::Carousel => dx + dy,
            _ => dx + dy * cols as i32,
        };
        let pr = indices.iter().position(|&i| i == self.sel).unwrap_or(0);
        // Primary item's landing slot → block start, same formula as
        // the drag-commit.
        let drop_target = (self.sel as i32 + delta).clamp(0, n as i32 - 1) as usize;
        // Pull the selection out (descending keeps positions valid),
        // reinsert as a contiguous run.
        let mut pulled: Vec<Item> = Vec::with_capacity(indices.len());
        for &i in indices.iter().rev() {
            pulled.push(self.items.remove(i));
        }
        pulled.reverse();
        let insert = drop_target.saturating_sub(pr).min(self.items.len());
        let unchanged = indices == (insert..insert + indices.len()).collect::<Vec<_>>();
        for (j, it) in pulled.into_iter().enumerate() {
            self.items.insert(insert + j, it);
        }
        self.sel = insert + pr;
        self.sel_set = (insert..insert + indices.len()).collect();
        self.sel_anchor = insert + pr;
        self.reveal_visible(self.sel);
        if !unchanged {
            self.reorder_dirty = true;
        }
    }

    /// Tab focus-in — scroll the current selection into view (a
    /// filtered-out selection lands on the centered item instead).
    pub fn focus(&mut self, filter: &str) {
        let visible = self.visible(filter);
        if visible.is_empty() {
            return;
        }
        let pos = visible
            .iter()
            .position(|&i| i == self.sel)
            .unwrap_or_else(|| {
                self.current.round().clamp(0.0, (visible.len() - 1) as f32) as usize
            });
        let idx = visible[pos];
        self.sel = idx;
        self.sel_anchor = idx;
        self.sel_set = BTreeSet::from([idx]);
        self.sel_touched = true;
        self.focus_flash = 1.0;
        self.reveal_visible(pos);
    }

    /// Scroll so visible-position `pos` is on screen — carousel
    /// centers it, grids glide just enough to show the whole card
    /// (same math as `pending_reveal`).
    fn reveal_visible(&mut self, pos: usize) {
        match self.mode {
            Mode::Carousel => {
                self.target = pos as f32;
                self.wheel_settle_at = None;
            }
            _ => {
                if let Some((g, rect)) = &self.nav_geo {
                    if let Some(cr) = g.card_rect(pos, self.grid_current) {
                        let mut t = self.grid_target;
                        if g.horizontal {
                            if cr.min.x < rect.min.x {
                                t -= rect.min.x - cr.min.x;
                            } else if cr.max.x > rect.max.x {
                                t += cr.max.x - rect.max.x;
                            }
                        } else if cr.min.y < rect.min.y {
                            t -= rect.min.y - cr.min.y;
                        } else if cr.max.y > rect.max.y {
                            t += cr.max.y - rect.max.y;
                        }
                        self.grid_target = t.clamp(0.0, g.max_scroll);
                    }
                }
            }
        }
    }

    /// Ctrl+A — every visible item selected. Filter-hidden items stay
    /// out, so a following Delete can't remove what isn't shown.
    pub fn select_all(&mut self, filter: &str) {
        self.sel_set = self.visible(filter).into_iter().collect();
        if let Some(&first) = self.sel_set.iter().next() {
            self.sel = first;
            self.sel_anchor = first;
        }
        if !self.sel_set.is_empty() {
            self.sel_touched = true;
        }
    }

    /// A user selection gesture happened since the last check — the
    /// app uses it to give the cover view Delete-key ownership.
    pub fn take_sel_touched(&mut self) -> bool {
        std::mem::take(&mut self.sel_touched)
    }

    /// The primary selection's path — Enter plays it like a
    /// double-click.
    pub fn primary_path(&self) -> Option<PathBuf> {
        self.items.get(self.sel).map(|i| i.path.clone())
    }

    /// The first item's path — the transport play button's fallback
    /// when no card is selected (Lacrima's TogglePlay →
    /// SelectedMediaItem; an opened album selects index 0 anyway).
    pub fn first_path(&self) -> Option<PathBuf> {
        self.items.first().map(|i| i.path.clone())
    }

    /// Select-on-press (Explorer/ListBox semantics): the selection
    /// lands on mouse-down, not on release — gating it on `clicked()`
    /// made every selection feel delayed. Shift extends a range from
    /// the anchor, Ctrl toggles the item, plain press singles it.
    /// Ranges run over *visible* items — filtered-out tracks never
    /// get silently selected.
    fn select_vi(&mut self, vi: usize, mods: egui::Modifiers, visible: &[usize]) {
        let idx = visible[vi];
        self.sel = idx;
        if mods.shift {
            let anchor_vi = visible
                .iter()
                .position(|&i| i == self.sel_anchor)
                .unwrap_or(vi);
            let (a, b) = (anchor_vi.min(vi), anchor_vi.max(vi));
            self.sel_set = visible[a..=b].iter().copied().collect();
        } else if mods.ctrl || mods.command {
            if !self.sel_set.remove(&idx) {
                self.sel_set.insert(idx);
            }
            self.sel_anchor = idx;
        } else {
            self.sel_set = BTreeSet::from([idx]);
            self.sel_anchor = idx;
        }
        self.sel_touched = true;
    }

    /// Selected items' paths (the Delete key payload).
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        self.sel_set
            .iter()
            .filter_map(|&i| self.items.get(i))
            .map(|i| i.path.clone())
            .collect()
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
    fn item_tex(row: &mut AlbumRow, ctx: &egui::Context, item: &Item) -> egui::TextureHandle {
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
        let d = if axis_x {
            p.x - press.last.x
        } else {
            p.y - press.last.y
        };
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

    /// Where `index` displays while the dragged block targets
    /// `block_start` (ComputeDisplaySlot extended to a contiguous run
    /// — ListboxItemDragBehavior's ShiftItemsForSwap): dragged items
    /// own the block slots, the rest shift around them. After the
    /// commit this mapping IS the new item order, so the landing is
    /// seamless.
    fn display_slot(index: usize, dragged: &[usize], block_start: usize) -> usize {
        match dragged.binary_search(&index) {
            Ok(rank) => block_start + rank,
            Err(ins) => {
                let rank = index - ins;
                if rank < block_start {
                    rank
                } else {
                    rank + dragged.len()
                }
            }
        }
    }

    /// Block start for a drag: the primary's target slot minus its
    /// rank in the dragged set (Lacrima's blockToIndex clamp).
    fn block_start(d: &ItemDrag, count: usize) -> usize {
        let pr = d.indices.iter().position(|&i| i == d.index).unwrap_or(0);
        d.drop_target
            .min(count.saturating_sub(1))
            .saturating_sub(pr)
            .min(count.saturating_sub(d.indices.len()))
    }

    /// An item's displaced index-position while a drag hovers —
    /// `display_slot` blended by the animated swap factor so neighbors
    /// glide aside instead of teleporting.
    fn disp_index(&self, vi: usize) -> f32 {
        match &self.item_drag {
            Some(d) if d.indices.binary_search(&vi).is_err() => {
                let n = self.items.len();
                let disp = Self::display_slot(vi, &d.indices, Self::block_start(d, n)) as f32;
                // During the drop glide neighbors are fully at their
                // displaced slots — the asymptotic swap chase would
                // otherwise snap its last few percent at permute.
                let t = if d.commit.is_some() {
                    1.0
                } else {
                    d.swap.get(&vi).copied().unwrap_or(0.0)
                };
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
                let (tx, tz, rot, sc, _w, _h) = self.item_geo(vi as f32, &tex, cfg, s);
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

    /// (home offset, glued offset) of dragged item rank `k` relative
    /// to the primary card's slot — home is its own slot, glued is
    /// the contiguous slot `k - primary_rank` away from the primary's
    /// (Lacrima's _targetOffsetsRelPrimary). Uniform grid layout makes
    /// both scroll-independent; carousel-mode drags are singletons.
    fn drag_item_offsets(&self, d: &ItemDrag, k: usize, geo: Option<&GridGeo>) -> (Vec2, Vec2) {
        let Some(g) = geo else {
            return (Vec2::ZERO, Vec2::ZERO);
        };
        let pr = d.indices.iter().position(|&i| i == d.index).unwrap_or(0);
        let Some(pc) = g.card_rect(d.index, 0.0).map(|r| r.center()) else {
            return (Vec2::ZERO, Vec2::ZERO);
        };
        let home = g
            .card_rect(d.indices[k], 0.0)
            .map(|r| r.center() - pc)
            .unwrap_or(Vec2::ZERO);
        let glued = g
            .card_rect(d.index + k - pr, 0.0)
            .map(|r| r.center() - pc)
            .unwrap_or(Vec2::ZERO);
        (home, glued)
    }

    /// A dragged card's live offset from the primary's floating
    /// center: grid members ease from their own slots into the glued
    /// block; carousel members fan out horizontally under the pointer
    /// (a pile — the honest equivalent of collapsed margins on a
    /// curved layout).
    fn drag_member_offset(&self, d: &ItemDrag, k: usize, geo: Option<&GridGeo>, s: f32) -> Vec2 {
        let pr = d.indices.iter().position(|&i| i == d.index).unwrap_or(0);
        match geo {
            Some(_) => {
                let (home, glued) = self.drag_item_offsets(d, k, geo);
                let e = 1.0 - (1.0 - d.glue).powi(3);
                home + (glued - home) * e
            }
            None => Vec2::new((k as f32 - pr as f32) * 36.0 * s, 0.0),
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
            .map(|d| d.index >= n || d.indices.iter().any(|&i| i >= n))
            .unwrap_or(false)
        {
            self.item_drag = None;
        }

        // Drop glide: ease the card home, then permute the model —
        // the whole dragged block moves as one contiguous run,
        // selection following it (Lacrima's TryReorderItems).
        let landed = match self.item_drag.as_mut().and_then(|d| d.commit.as_mut()) {
            Some(c) => {
                c.progress += dt / ITEM_COMMIT_SECS;
                c.progress >= 1.0
            }
            None => false,
        };
        if landed {
            let d = self.item_drag.take().unwrap();
            let mut indices = d.indices.clone();
            indices.retain(|&i| i < n);
            if !indices.is_empty() {
                // Pull the dragged items out (descending indices keep
                // positions valid), preserving their relative order.
                let mut pulled: Vec<Item> = Vec::with_capacity(indices.len());
                for &i in indices.iter().rev() {
                    pulled.push(self.items.remove(i));
                }
                pulled.reverse();
                let pr = indices.iter().position(|&i| i == d.index).unwrap_or(0);
                let insert = d
                    .drop_target
                    .min(n - 1)
                    .saturating_sub(pr)
                    .min(self.items.len());
                let unchanged = indices == (insert..insert + indices.len()).collect::<Vec<_>>();
                for (j, it) in pulled.into_iter().enumerate() {
                    self.items.insert(insert + j, it);
                }
                self.sel = insert + pr;
                self.sel_set = (insert..insert + indices.len()).collect();
                self.sel_anchor = insert + pr;
                if !unchanged {
                    self.reorder_dirty = true;
                }
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

        let (pointer, down) = ctx.input(|i| (i.pointer.latest_pos(), i.pointer.primary_down()));

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
            let Some(vi) = pr.item.filter(|&vi| vi < n) else {
                return;
            };
            let Some(c) = self.slot_center(vi, mode, geo, grid_area, row, ctx, cfg, s) else {
                return;
            };
            let grab = pr.start - c;
            // Drag set: the selection when the held card is part of
            // it, else just the held card (Lacrima's _draggedIndices).
            let indices: Vec<usize> = if self.sel_set.contains(&vi) {
                self.sel_set.iter().copied().filter(|&i| i < n).collect()
            } else {
                vec![vi]
            };
            self.item_drag = Some(ItemDrag {
                index: vi,
                indices,
                grab,
                center: c,
                drop_target: vi,
                swap: HashMap::new(),
                glue: 0.0,
                lift: 0.0,
                commit: None,
            });
            self.press = None;
            ctx.request_repaint();
            return;
        }

        // Released (or view went inactive) — glide to the target slot.
        if !interactive || !down {
            // Pin the list's scroll springs for the glide — if the
            // carousel is still drifting (edge auto-scroll tail,
            // wheel settle), the slots move under the gliding card
            // and it has to slide again after landing.
            self.current = self.target;
            self.velocity = 0.0;
            self.grid_current = self.grid_target;
            self.grid_vel = 0.0;
            self.wheel_settle_at = None;
            let bs = {
                let d = self.item_drag.as_ref().unwrap();
                Self::block_start(d, n)
            };
            let len = self.item_drag.as_ref().unwrap().indices.len();
            // Every dragged card glides from where it floats now into
            // its own landing slot (the block's contiguous run).
            let mut starts = Vec::with_capacity(len);
            let mut targets = Vec::with_capacity(len);
            for k in 0..len {
                let off = {
                    let d = self.item_drag.as_ref().unwrap();
                    self.drag_member_offset(d, k, geo, s)
                };
                let c = self.item_drag.as_ref().unwrap().center;
                starts.push(c + off);
                targets.push(
                    self.slot_center(bs + k, mode, geo, grid_area, row, ctx, cfg, s)
                        .unwrap_or(c + off),
                );
            }
            self.item_drag.as_mut().unwrap().commit = Some(ItemCommit {
                starts,
                targets,
                progress: 0.0,
            });
            ctx.request_repaint();
            return;
        }
        let Some(p) = pointer else { return };

        {
            let d = self.item_drag.as_mut().unwrap();
            // Smoothed follow — the card trails the pointer instead
            // of hard-gluing, so the pickup glides out of its slot
            // and fast flicks stay fluid rather than teleporting.
            let follow = 1.0 - (-dt * 16.0).exp();
            d.center = lerp_pos(d.center, p + d.grab, follow);
            d.lift = (d.lift + dt / 0.14).min(1.0);
        }
        let center = self.item_drag.as_ref().unwrap().center;

        // Nearest slot center decides the drop target. Carousel slots
        // cluster near the middle — x-distance is the natural metric
        // on the arc; grid slots compare in 2D.
        let mut best = 0usize;
        let mut best_d = f32::MAX;
        for vi in 0..n {
            let Some(c) = self.slot_center(vi, mode, geo, grid_area, row, ctx, cfg, s) else {
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
            // Selected cards glue into a contiguous block (Lacrima's
            // AnimateGlue ~150ms ease-out).
            let g_want = (d.indices.len() > 1) as u8 as f32;
            d.glue += (g_want - d.glue) * (1.0 - (-dt / 0.05).exp());
            if g_want == 0.0 && d.glue < 0.01 {
                d.glue = 0.0;
            }
            // Neighbors ease toward their displaced slots.
            let bs = Self::block_start(d, n);
            for vi in 0..n {
                if d.indices.binary_search(&vi).is_ok() {
                    continue;
                }
                let disp = Self::display_slot(vi, &d.indices, bs);
                let want = if disp == vi { 0.0 } else { 1.0 };
                let cur = d.swap.get(&vi).copied().unwrap_or(0.0);
                let next = cur + (want - cur) * (1.0 - (-dt / ITEM_SWAP_TAU).exp());
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
                    self.target = (self.target + d_idx).clamp(0.0, (n - 1) as f32);
                    self.current = (self.current + d_idx).clamp(-0.5, (n - 1) as f32 + 0.5);
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
        playing_loading: bool,
        filter: &str,
        cfg: &settings::Carousel,
        mode: Mode,
        alpha: f32,
        interactive: bool,
        reorder_armed: bool,
        s: f32,
    ) -> Action {
        let mut action = Action::None;
        let ctx = ui.ctx().clone();
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        let painter = ui.painter().clone();

        // Window resize: every pointer coordinate re-maps, so any
        // press/drag in flight is stale (the resize-grip press used to
        // pop a card into drag mode as the window moved under it).
        // New presses are blocked while the screen rect keeps changing.
        let screen = ctx
            .input(|i| i.viewport().inner_rect)
            .unwrap_or(Rect::NOTHING);
        let resizing = self.resize.update(screen);
        if resizing {
            self.press = None;
            self.item_drag = None;
            self.sb_drag = None;
        }

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
        // Shift held = reorder armed — the selection lifts slightly,
        // signaling it can be moved. A still press on an item arms it
        // the same way (the hold-to-lift cue) and a live drag keeps
        // it up until the drop lands. Eases in fast, settles out.
        let press_armed = matches!(&self.press, Some(p) if !p.moved && p.item.is_some());
        let lift_target = if (reorder_armed && reorderable && !visible.is_empty())
            || (interactive && (press_armed || self.item_drag.is_some()))
        {
            1.0
        } else {
            0.0
        };
        let rate = dt / if lift_target > self.pop { 0.12 } else { 0.18 };
        self.pop = (self.pop + (lift_target - self.pop).clamp(-rate, rate)).clamp(0.0, 1.0);
        if !reorderable {
            self.item_drag = None;
            // Hidden items must never be deletable — prune the
            // selection to what the filter shows.
            let vis: BTreeSet<usize> = visible.iter().copied().collect();
            self.sel_set = self.sel_set.intersection(&vis).copied().collect();
        }
        let n = visible.len();
        if n == 0 {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                if self.rom_mode {
                    "No items"
                } else {
                    "No album loaded"
                },
                FontId::proportional(24.0 * s),
                Color32::from_rgb(0x78, 0x78, 0x78).gamma_multiply(alpha * 0.53),
            );
            if self.rom_mode && self.album_key.is_some() {
                painter.text(
                    rect.center() + Vec2::new(0.0, 30.0 * s),
                    Align2::CENTER_CENTER,
                    "Right-click to add games",
                    FontId::proportional(15.0 * s),
                    Color32::from_rgb(0x5A, 0x5A, 0x5A).gamma_multiply(alpha * 0.53),
                );
            }
            // Menu still reachable when an album is loaded but the
            // filter hides every item (Lacrima's menu lives on the
            // cover view itself, not on items).
            if interactive && self.album_key.is_some() {
                let resp = ui.interact(rect, ui.id().with("cover-view-empty"), Sense::click());
                resp.context_menu(|ui| {
                    if self.rom_mode {
                        if ui.button("Add Roms").clicked() {
                            action = Action::AddRoms;
                            ui.close();
                        }
                        if ui.button("Scan Folder").clicked() {
                            action = Action::ScanFolder;
                            ui.close();
                        }
                    } else {
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
                    }
                    if ui
                        .button(if self.rom_mode {
                            "Clear Section"
                        } else {
                            "Clear Album"
                        })
                        .clicked()
                    {
                        action = Action::Clear;
                        ui.close();
                    }
                    // Lacrima's "Clear Album Cache" — wipe the fetched
                    // sidecars so the resolver starts over.
                    if self.rom_mode && ui.button("Clear Section Cache").clicked() {
                        action = Action::ClearCache;
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
        // The playing item's tile spins while its stream resolves /
        // buffers — cleared the moment playback actually starts.
        let loading_vidx = if playing_loading { playing_vidx } else { None };
        if playing_loading {
            ctx.request_repaint();
        }

        // Layout-mode geometry used by both input and paint. Kept for
        // arrow-key navigation between frames.
        let geo = match mode {
            Mode::Carousel => None,
            Mode::VerticalGrid => Some(GridGeo::vertical(grid_area, n, cfg, s)),
            Mode::HorizontalGrid => {
                Some(GridGeo::horizontal(grid_area, n, cfg, s, self.h_min_rows))
            }
        };
        self.nav_geo = geo.clone().map(|g| (g, rect));
        // Queued reveal — the playing item owns the selection marker;
        // the carousel glides to center it, grids scroll just enough to
        // make its card fully visible (ScrollIntoView semantics).
        if let Some(path) = self.pending_reveal.take() {
            if let Some(idx) = self.index_of(&path) {
                self.sel = idx;
                // The playing item becoming primary collapses the
                // multi-selection to it (Avalonia's SelectedItem
                // setter clears the rest in multiple mode).
                self.sel_set = BTreeSet::from([idx]);
                self.sel_anchor = idx;
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
            self.step_grid(
                dt,
                g.max_scroll,
                self.press.is_some() && self.sb_drag.is_none(),
            );
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
            && (sb_hot || Instant::now() < self.sb_visible_until || self.grid_vel.abs() > 1.0);
        self.sb_opacity += ((wants_sb as u8) as f32 - self.sb_opacity) * (dt * 10.0).min(1.0);

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

        self.step(
            dt,
            self.press.is_some() && mode == Mode::Carousel,
            max_index,
        );

        // Manual press tracking — raw input can't be stolen by widget
        // routing (same approach as the album row's scrollbar).
        if interactive && !resizing && ctx.input(|i| i.pointer.primary_pressed()) {
            if let Some(p) = ctx.input(|i| i.pointer.press_origin()) {
                // A press that armed a window-resize grip belongs to
                // the OS resize — the root stamps the CURRENT frame
                // number when BeginResize fires (`insert_temp` is
                // session-lifetime, so a bare bool would stick and
                // block every later press).
                let grip_press = ctx
                    .data(|d| {
                        d.get_temp::<u64>(egui::Id::new("resize_grip_press"))
                    })
                    .map(|f| f == ctx.cumulative_frame_nr())
                    .unwrap_or(false);
                let over_slider = slider_rect.map(|r| r.contains(p)).unwrap_or(false);
                if rect.contains(p) && !over_slider && !grip_press {
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
                        // Item under the press — needed for
                        // select-on-press in every mode; it doubles as
                        // the hold-to-lift candidate only when
                        // reordering is possible (unfiltered).
                        let hit = match mode {
                            Mode::Carousel => {
                                self.hit_carousel(p, grid_area, &visible, row, &ctx, cfg, s)
                            }
                            _ => self.hit_grid(p, &visible, geo.as_ref().unwrap()),
                        };
                        if let Some(vi) = hit {
                            let mods = ctx.input(|i| i.modifiers);
                            self.select_vi(vi, mods, &visible);
                        }
                        let item = if reorderable { hit } else { None };
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
                self.sb_visible_until = Instant::now() + std::time::Duration::from_secs_f32(1.2);
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
                            self.target =
                                (press.anchor - dx / DRAG_PX_PER_ITEM).clamp(0.0, max_index);
                        }
                        _ => {
                            Self::press_update(press, p, dt, 1.0, axis_x);
                            let delta = if axis_x {
                                p.x - press.start.x
                            } else {
                                p.y - press.start.y
                            };
                            self.grid_target =
                                (press.anchor - delta).clamp(0.0, geo.as_ref().unwrap().max_scroll);
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
                    self.menu_item = hit.map(|vi| self.items[visible[vi]].path.clone());
                } else {
                    self.menu_item = None;
                }
            }
        }
        if interactive {
            resp.context_menu(|ui| {
                // Lacrima's cover-view menu: Add entries always,
                // item entries only while one is pointed. ROM mode
                // swaps in the emulation vocabulary (EmulationView's
                // item ContextMenu).
                if self.rom_mode {
                    if ui.button("Add Roms").clicked() {
                        action = Action::AddRoms;
                        ui.close();
                    }
                    if ui.button("Scan Folder").clicked() {
                        action = Action::ScanFolder;
                        ui.close();
                    }
                } else {
                    if ui.button("Add Url").clicked() {
                        action = Action::AddUrl;
                        ui.close();
                    }
                    if ui.button("Add Playlist").clicked() {
                        action = Action::AddPlaylist;
                        ui.close();
                    }
                }
                if let Some(p) = self.menu_item.clone() {
                    ui.separator();
                    if self.rom_mode && ui.button("Play").clicked() {
                        action = Action::Play(p.clone());
                        self.menu_item = None;
                        ui.close();
                    }
                    if ui.button("Edit Metadata").clicked() {
                        action = Action::Edit(p.clone());
                        self.menu_item = None;
                        ui.close();
                    }
                    // X360 + Xenia / PS3 + RPCS3 / PS4 + shadPS4 —
                    // Lacrima's per-game Patches menu item (Xenia adds
                    // Custom Config, shadPS4 adds Cheats).
                    if self.rom_mode
                        && (self.xenia_menu || self.rpcs3_menu || self.shadps4_menu)
                        && ui.button("Patches").clicked()
                    {
                        action = Action::Patches(p.clone());
                        self.menu_item = None;
                        ui.close();
                    }
                    if self.rom_mode && self.shadps4_menu && ui.button("Cheats").clicked() {
                        action = Action::Cheats(p.clone());
                        self.menu_item = None;
                        ui.close();
                    }
                    if self.rom_mode && self.xenia_menu && ui.button("Custom Config").clicked() {
                        action = Action::CustomConfig(p.clone());
                        self.menu_item = None;
                        ui.close();
                    }
                    // WIIU + cemu — Lacrima's "Graphic Packs" menu
                    // item (sits after Patches/Custom Config).
                    if self.rom_mode && self.cemu_menu && ui.button("Graphic Packs").clicked() {
                        action = Action::GraphicPacks(p.clone());
                        self.menu_item = None;
                        ui.close();
                    }
                    if ui
                        .button(if self.rom_mode { "Remove" } else { "Delete" })
                        .clicked()
                    {
                        // Pointing at a selected item deletes the whole
                        // selection (ListBox Delete semantics), else
                        // just the pointed item.
                        let paths = if self
                            .index_of(&p)
                            .map(|i| self.sel_set.contains(&i))
                            .unwrap_or(false)
                            && self.sel_set.len() > 1
                        {
                            self.selected_paths()
                        } else {
                            vec![p]
                        };
                        action = Action::Delete(paths);
                        self.menu_item = None;
                        ui.close();
                    }
                }
                ui.separator();
                if !self.rom_mode && self.has_url_items() && ui.button("Export Playlist").clicked()
                {
                    action = Action::Export;
                    ui.close();
                }
                if ui
                    .button(if self.rom_mode {
                        "Clear Section"
                    } else {
                        "Clear Album"
                    })
                    .clicked()
                {
                    action = Action::Clear;
                    ui.close();
                }
                if self.rom_mode && ui.button("Clear Section Cache").clicked() {
                    action = Action::ClearCache;
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
                        Mode::Carousel => {
                            self.hit_carousel(p, grid_area, &visible, row, &ctx, cfg, s)
                        }
                        _ => self.hit_grid(p, &visible, geo.as_ref().unwrap()),
                    };
                    if let Some(vi) = hit {
                        let idx = visible[vi];
                        // Selection already landed on press — the
                        // release only navigates / plays.
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
                loading_vidx,
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
            loading_vidx,
            cfg,
            global_alpha * mt,
            drift,
            interactive,
            s,
        );
        if mode != Mode::Carousel {
            self.last_sb = sb.map(|b| (b, sb_hot));
            if let Some(b) = &sb {
                if self.sb_opacity > 0.01 {
                    Self::paint_scrollbar(ui.painter(), b, sb_hot, self.sb_opacity);
                }
            }
        } else {
            self.last_sb = None;
        }

        // Tab focus-in flash — a soft border around the view that
        // eases out over ~0.9s, marking where the keys now land.
        if self.focus_flash > 0.0 {
            self.focus_flash = (self.focus_flash - dt / 0.9).max(0.0);
            let f = self.focus_flash * self.focus_flash * alpha;
            painter.rect_stroke(
                rect.shrink(2.0),
                10.0 * s,
                Stroke::new(6.0 * s, Color32::WHITE.gamma_multiply(0.10 * f)),
                egui::StrokeKind::Inside,
            );
            painter.rect_stroke(
                rect.shrink(2.0),
                10.0 * s,
                Stroke::new(2.0 * s, Color32::WHITE.gamma_multiply(0.55 * f)),
                egui::StrokeKind::Inside,
            );
        }

        // Repaint while anything is still animating — including a held
        // press (the direct-follow lag needs frames even when the
        // pointer pauses mid-drag) and an in-flight text crossfade.
        let animating = self.focus_flash > 0.0
            || self.pop > 0.0
            || self.velocity.abs() > 0.005
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

    /// Repaint the horizontal scrollbar after the spectrum — its
    /// track docks at the bottom edge of the cover view, exactly
    /// where `music_view` paints the spectrum strip *after* this
    /// view, so without this pass the spectrum covers the bar.
    /// Vertical bars live on the right edge and are never touched.
    pub fn repaint_scrollbar(&self, painter: &egui::Painter) {
        if let Some((b, hot)) = self.last_sb {
            if b.horizontal && self.sb_opacity > 0.01 {
                Self::paint_scrollbar(painter, &b, hot, self.sb_opacity);
            }
        }
    }

    /// Pill scrollbar like the album row's — a soft halo under a bright
    /// core, brighter while hot (hovered/dragged).
    fn paint_scrollbar(painter: &egui::Painter, sb: &SbGeo, hot: bool, opacity: f32) {
        let r = sb.track.width().min(sb.track.height()) * 0.5;
        painter.rect_filled(sb.track, r, Color32::WHITE.gamma_multiply(0.10 * opacity));
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
        loading_vidx: Option<usize>,
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
                self.paint_carousel(
                    ui,
                    r,
                    visible,
                    row,
                    playing_vidx,
                    loading_vidx,
                    cfg,
                    alpha,
                    s,
                );
                self.overlay_stack(ui, r, v, visible, alpha, interactive, s);
            }
            _ => {
                let mut geo = match mode {
                    Mode::VerticalGrid => GridGeo::vertical(rect, visible.len(), cfg, s),
                    _ => GridGeo::horizontal(rect, visible.len(), cfg, s, self.h_min_rows),
                };
                geo.origin.y += drift_y;
                self.paint_grid(
                    ui,
                    rect,
                    visible,
                    row,
                    playing_vidx,
                    loading_vidx,
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
    fn project(tx: f32, tz: f32, rot: f32, sc: f32, center: Pos2, x: f32, y: f32) -> (Pos2, f32) {
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
    fn item_size(tex: &egui::TextureHandle, cfg: &settings::Carousel, s: f32) -> (f32, f32) {
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
        loading_vidx: Option<usize>,
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
        let center = Pos2::new(rect.center().x, rect.center().y + cfg.vertical_offset * s);

        // Draw farthest first so z-order follows the animation
        // continuously instead of snapping at the halfway point.
        // Displaced neighbors evaluate at their eased slot position;
        // dragged-group members paint floating instead.
        let mut order: Vec<usize> = (0..visible.len())
            .filter(|&i| {
                self.item_drag
                    .as_ref()
                    .map(|d| d.indices.binary_search(&i).is_err())
                    .unwrap_or(true)
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
            // Shift held / held press — the selection (or the
            // pressed item) floats slightly, signaling it can be
            // reordered.
            let pe = if self.sel_set.contains(&idx)
                || matches!(&self.press, Some(p) if p.item == Some(vi))
            {
                self.pop
            } else {
                0.0
            };
            let sc = sc * (1.0 + 0.08 * pe);
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
                // Fade WITH the card — base_opacity drops as it slides
                // off the focal slot, so border and card vanish together.
                outline(
                    4.2 + 1.4 * pulse,
                    Color32::WHITE.gamma_multiply(0.22 * glow * base_opacity),
                );
                outline(
                    1.2 + 0.3 * pulse,
                    Color32::WHITE
                        .gamma_multiply(((225.0 + 30.0 * pulse) / 255.0).min(1.0) * base_opacity),
                );
                // Stream still resolving/buffering — spin over the
                // quad's center (Lacrima's busy overlay on the item).
                if Some(vi) == loading_vidx {
                    let cc = Pos2::new(
                        corners.iter().map(|p| p.x).sum::<f32>() / 4.0,
                        corners.iter().map(|p| p.y).sum::<f32>() / 4.0,
                    );
                    let rr = corners.iter().map(|p| p.distance(cc)).sum::<f32>() / 4.0 * 0.22;
                    let phase = ctx.input(|i| i.time) as f32 * 7.5;
                    spinner(&painter, cc, rr, phase, base_opacity, s);
                }
            }
        }

        // The held card floats on top — unrotated, glued to the
        // pointer, scaled/shadowed by its lift. During the drop glide
        // it eases from the release point into the slot. A dragged
        // selection paints as a glued pile (non-primary members
        // underneath, the grabbed card last/on top).
        if let Some(d) = &self.item_drag {
            let pr = d.indices.iter().position(|&i| i == d.index).unwrap_or(0);
            let e = d
                .commit
                .as_ref()
                .map(|c| ease_out_cubic(c.progress.min(1.0)));
            let centers: Vec<Pos2> = (0..d.indices.len())
                .map(|k| match (&d.commit, e) {
                    (Some(c), Some(t)) => lerp_pos(c.starts[k], c.targets[k], t),
                    _ => d.center + self.drag_member_offset(d, k, None, s),
                })
                .collect();
            let lift = d.lift * (1.0 - e.unwrap_or(0.0));
            let sc = 1.0 + (ITEM_DRAG_LIFT - 1.0) * lift;
            // Unclipped: a lifted card reads as floating above the
            // chrome — hard-clipping it at the cover edge would pop.
            let float_paint = ui.painter();
            for k in (0..d.indices.len())
                .filter(|&k| k != pr)
                .chain(std::iter::once(pr))
            {
                let item = &self.items[d.indices[k]];
                let tex = Self::item_tex(row, &ctx, item);
                let (w, h) = Self::item_size(&tex, cfg, s);
                // Drop-glide morph: `center` glides from the float
                // position toward the ARC center while the slot's
                // tx/tz/rot scale in — at t=1 the mesh is the exact
                // slot quad (a flat card at screen pos C is just
                // tx=0 with center=C), so the permute lands
                // seamlessly with no second slide.
                let (c, mtx, mtz, mrot, msc) = match e {
                    Some(t) => {
                        let slot = Self::block_start(d, self.items.len()) + k;
                        let (tx, tz, rot, ssc, _, _) = self.item_geo(slot as f32, &tex, cfg, s);
                        (
                            lerp_pos(centers[k], center, t),
                            tx * t,
                            tz * t,
                            rot * t,
                            sc + (ssc - sc) * t,
                        )
                    }
                    None => (centers[k], 0.0, 0.0, 0.0, sc),
                };
                let card_c = Self::project(mtx, mtz, mrot, msc, c, 0.0, 0.0).0;
                if lift > 0.01 {
                    float_paint.rect_filled(
                        Rect::from_center_size(
                            card_c + Vec2::new(0.0, 14.0 * s),
                            Vec2::new(w, h) * sc,
                        ),
                        16.0 * s,
                        Color32::BLACK.gamma_multiply(0.38 * lift * alpha),
                    );
                }
                let m = self.item_mesh(
                    mtx,
                    mtz,
                    mrot,
                    msc,
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

    /// Title marquee state (Lacrima's `MarqueeBehavior`, adjusted per
    /// user request): only runs when `marquee_title` (compact mode)
    /// and the text overflows the lane. A NEW title shows in place
    /// immediately — offset 0 — holds 2s, then scrolls left at
    /// 40px/s; on wrap it re-enters from off the right edge (+40% of
    /// text width) after a 0.3s pause, like Lacrima's re-entry.
    /// Returns true while the marquee is active (caller repaints).
    /// `now` is a parameter so tests can drive the hold/wrap timing.
    fn marquee_step(
        &mut self,
        title: &str,
        tw: f32,
        avail_w: f32,
        s: f32,
        dt: f32,
        now: Instant,
    ) -> bool {
        if !self.marquee_title || tw <= avail_w || avail_w <= 0.0 {
            self.marquee_text.clear();
            return false;
        }
        if self.marquee_text != title {
            self.marquee_text = title.to_owned();
            self.marquee_off = 0.0;
            self.marquee_pause = now + std::time::Duration::from_millis(2000);
        }
        if now >= self.marquee_pause {
            self.marquee_off -= 40.0 * s * dt;
            if self.marquee_off <= -tw {
                self.marquee_off = avail_w + tw * 0.4;
                self.marquee_pause = now + std::time::Duration::from_millis(300);
            }
        }
        true
    }

    /// The mini player's title marquee — same hold/scroll/re-entry as
    /// `marquee_step` but not gated on `marquee_title` (the mini bar
    /// always marquees when the title overflows). Returns the lane x
    /// offset while active, `None` when the title fits.
    pub(crate) fn mini_marquee(
        &mut self,
        title: &str,
        tw: f32,
        avail_w: f32,
        s: f32,
        dt: f32,
    ) -> Option<f32> {
        let saved = std::mem::replace(&mut self.marquee_title, true);
        let active = self.marquee_step(title, tw, avail_w, s, dt, Instant::now());
        self.marquee_title = saved;
        active.then_some(self.marquee_off)
    }

    /// One item's overlay text block — title, artist, album — drawn
    /// centered at `cx`, stacked upward from `cy`.
    #[allow(clippy::too_many_arguments)]
    fn overlay_text(
        &mut self,
        painter: &egui::Painter,
        ctx: &egui::Context,
        dt: f32,
        cx: f32,
        cy: f32,
        idx: usize,
        avail_w: f32,
        alpha: f32,
        s: f32,
    ) {
        let item = &self.items[idx];
        let (title, artist, album) =
            (item.title.clone(), item.artist.clone(), item.album.clone());
        let mut y = cy - 52.0 * s;
        let font = FontId::proportional(28.0 * s);
        let color = Color32::WHITE.gamma_multiply(alpha);
        // Title marquee: fitting titles stay centered; overflowing
        // ones show in place, hold, then scroll (see marquee_step).
        let galley = painter.layout_no_wrap(title.clone(), font.clone(), color);
        let tw = galley.rect.width();
        if self.marquee_step(&title, tw, avail_w, s, dt, Instant::now()) {
            ctx.request_repaint();
            let lane = Rect::from_center_size(
                Pos2::new(cx, y),
                Vec2::new(avail_w, galley.rect.height() * 1.2),
            );
            painter.with_clip_rect(lane).galley(
                Pos2::new(
                    lane.min.x + self.marquee_off,
                    lane.center().y - galley.rect.height() * 0.5,
                ),
                galley,
                color,
            );
        } else {
            painter.text(
                Pos2::new(cx, y),
                Align2::CENTER_CENTER,
                crate::ellipsize(painter, &title, &font, avail_w),
                font,
                color,
            );
        }
        y += 34.0 * s;
        if !artist.is_empty() {
            painter.text(
                Pos2::new(cx, y),
                Align2::CENTER_CENTER,
                crate::ellipsize(painter, &artist, &FontId::proportional(19.0 * s), avail_w),
                FontId::proportional(19.0 * s),
                Color32::WHITE.gamma_multiply(0.8 * alpha),
            );
            y += 26.0 * s;
        }
        if !album.is_empty() {
            painter.text(
                Pos2::new(cx, y),
                Align2::CENTER_CENTER,
                crate::ellipsize(painter, &album, &FontId::proportional(16.0 * s), avail_w),
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
            // Keep real margins on both sides — long titles ellipsis
            // rather than touching the cover area's edges (compact
            // mode marquees them instead).
            let ctx = ui.ctx().clone();
            let dt = ctx.input(|i| i.stable_dt).min(0.1);
            self.overlay_text(
                &painter,
                &ctx,
                dt,
                rect.center().x,
                cy,
                idx,
                rect.width() - 48.0 * s,
                alpha,
                s,
            );
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
            self.pill_slider(
                ui,
                slider,
                (visible.len() - 1) as f32,
                alpha,
                interactive,
                s,
            );
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
            Stroke::new(
                1.5,
                Color32::from_rgb(0xCC, 0xCC, 0xCC).gamma_multiply(alpha),
            ),
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
        loading_vidx: Option<usize>,
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
        let n_items = visible.len();

        // Drop hole — the slots the dragged block would land in.
        if let Some(d) = &self.item_drag {
            let bs = Self::block_start(d, n_items);
            for slot in bs..(bs + d.indices.len()).min(n_items) {
                let Some(hole) = geo.card_rect(slot, scroll) else {
                    continue;
                };
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
            if self
                .item_drag
                .as_ref()
                .map(|d| d.indices.binary_search(&vi).is_ok())
                .unwrap_or(false)
            {
                continue;
            }
            let Some(mut card) = geo.card_rect(vi, scroll) else {
                continue;
            };
            // Neighbors ease toward the slots the block vacated.
            if let Some(d) = &self.item_drag {
                let disp = Self::display_slot(vi, &d.indices, Self::block_start(d, n_items));
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
            // Shift held / held press — the selection (or the
            // pressed item) floats slightly off the grid over a soft
            // shadow, signaling it can be reordered.
            let pe = if self.sel_set.contains(&idx)
                || matches!(&self.press, Some(p) if p.item == Some(vi))
            {
                self.pop
            } else {
                0.0
            };
            if pe > 0.001 {
                card = Rect::from_center_size(
                    card.center() + Vec2::new(0.0, -4.0 * pe * s),
                    card.size() * (1.0 + 0.06 * pe),
                );
                painter.rect_filled(
                    card.translate(Vec2::new(0.0, 8.0 * s)),
                    CARD_RADIUS * s,
                    Color32::BLACK.gamma_multiply(0.3 * pe * alpha),
                );
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
                Some(vi) == loading_vidx,
                pulse,
                self.sel_set.contains(&idx),
                alpha,
                s,
            );
        }

        // The held card floats on top, glued to the pointer — a
        // dragged selection paints as its glued block, non-primary
        // members underneath the grabbed card.
        if let Some(d) = &self.item_drag {
            let pr = d.indices.iter().position(|&i| i == d.index).unwrap_or(0);
            let e = d
                .commit
                .as_ref()
                .map(|c| ease_out_cubic(c.progress.min(1.0)));
            let centers: Vec<Pos2> = (0..d.indices.len())
                .map(|k| match (&d.commit, e) {
                    (Some(c), Some(t)) => lerp_pos(c.starts[k], c.targets[k], t),
                    _ => d.center + self.drag_member_offset(d, k, Some(geo), s),
                })
                .collect();
            let lift = d.lift * (1.0 - e.unwrap_or(0.0));
            let sc = 1.0 + (ITEM_DRAG_LIFT - 1.0) * lift;
            // Unclipped — the lifted cards float over the chrome too.
            let float_paint = ui.painter();
            for k in (0..d.indices.len())
                .filter(|&k| k != pr)
                .chain(std::iter::once(pr))
            {
                let item = &self.items[d.indices[k]];
                let tex = Self::item_tex(row, &ctx, item);
                let card =
                    Rect::from_center_size(centers[k], Vec2::new(geo.card_w, geo.card_h) * sc);
                if lift > 0.01 {
                    float_paint.rect_filled(
                        card.translate(Vec2::new(0.0, 12.0 * s)),
                        (CARD_RADIUS + 4.0) * s,
                        Color32::BLACK.gamma_multiply(0.40 * lift * alpha),
                    );
                }
                Self::paint_card(
                    &float_paint,
                    &ctx,
                    card,
                    item,
                    &tex,
                    false,
                    false,
                    pulse,
                    false,
                    alpha,
                    s,
                );
            }
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
        loading: bool,
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
        for (px, uvx) in [(cover.min.x, u0), (cover.max.x, u0 + wr)] {
            for (py, uvy) in [(cover.min.y, v0), (cover.max.y, v0 + hr)] {
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

        // Stream resolving/buffering — spinner over the card center,
        // cleared the moment playback starts.
        if loading {
            let phase = ctx.input(|i| i.time) as f32 * 7.5;
            spinner(
                painter,
                card.center(),
                card.width().min(card.height()) * 0.14,
                phase,
                alpha,
                s,
            );
        }
    }
}

// ------------------------------------------------------------------
// Grid geometry — CardGridLayoutHelper + CardGridHorizontalLayout.
// ------------------------------------------------------------------
#[derive(Clone)]
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
    /// only when a card no longer fits vertically. `min_rows` floors
    /// the count (compact transport wants ≥2 visible): when it
    /// exceeds what the configured size fits, cards shrink so the
    /// rows are actually visible.
    fn horizontal(
        rect: Rect,
        count: usize,
        cfg: &settings::Carousel,
        s: f32,
        min_rows: usize,
    ) -> Self {
        let spacing = cfg.card_spacing.max(4.0) * s;
        let avail_h = (rect.height() - GRID_TOP * s - GRID_BOTTOM * s - SB_ZONE * s).max(120.0);
        let mut card_h = (cfg.card_size * CARD_RATIO * s).min(avail_h);
        let fit = ((avail_h + spacing) / (card_h + spacing)) as usize;
        let rows = fit.clamp(min_rows.clamp(1, count.max(1)), count.max(1));
        if rows > fit {
            card_h = ((avail_h - rows.saturating_sub(1) as f32 * spacing) / rows as f32)
                .max(24.0 * s);
        }
        let card_w = card_h / CARD_RATIO;
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
                    Pos2::new(
                        rect.min.x + H_PAD_X * s,
                        rect.max.y - SB_W_H * s - SB_INSET_H * s,
                    ),
                    Pos2::new(rect.max.x - H_PAD_X * s, rect.max.y - SB_INSET_H * s),
                ),
                true,
            )
        } else {
            (
                Rect::from_min_max(
                    Pos2::new(
                        rect.max.x - SB_W_V * s - SB_INSET_V * s,
                        rect.min.y + GRID_TOP * s,
                    ),
                    Pos2::new(rect.max.x - SB_INSET_V * s, rect.max.y - GRID_BOTTOM * s),
                ),
                false,
            )
        };
        let view = if horizontal {
            rect.width()
        } else {
            rect.height()
        };
        let frac = (scroll / self.max_scroll).clamp(0.0, 1.0);
        let view_frac = (view / (view + self.max_scroll)).min(1.0);
        let (thumb_len, travel) = if horizontal {
            ((track.width() * view_frac).max(24.0 * s), track.width())
        } else {
            ((track.height() * view_frac).max(24.0 * s), track.height())
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
#[derive(Clone, Copy)]
struct SbGeo {
    track: Rect,
    thumb: Rect,
    /// Generous grab zone around the thin visual track.
    hit: Rect,
    horizontal: bool,
    max_scroll: f32,
}

/// Busy spinner over an item while its stream resolves/buffers —
/// macOS style: twelve radial spokes, the fade trailing the head
/// clockwise; `phase` is the rotation accumulator in radians. The
/// head is CONTINUOUS (not floored to spoke steps) so the wheel
/// rotates smoothly instead of ticking ~10 times a second.
fn spinner(painter: &egui::Painter, c: Pos2, r: f32, phase: f32, alpha: f32, s: f32) {
    const N: usize = 12;
    let r = r.max(8.0 * s);
    // Faint disc so spokes read over busy covers.
    painter.circle_filled(
        c,
        r * 1.5,
        Color32::from_black_alpha((90.0 * alpha).min(255.0) as u8),
    );
    let w = (r * 0.20).max(1.6 * s);
    let head = phase / (std::f32::consts::TAU / N as f32);
    for i in 0..N {
        // Spoke age behind the head — the continuous trailing fade.
        let age = (head - i as f32).rem_euclid(N as f32) / N as f32;
        // Soft-ease the tail so the fade eases out instead of
        // cutting linearly to the dimmest spoke.
        let k = (1.0 - age).powf(1.4);
        let a = std::f32::consts::TAU * i as f32 / N as f32;
        let (dx, dy) = (a.cos(), a.sin());
        painter.line_segment(
            [
                Pos2::new(c.x + r * 0.45 * dx, c.y + r * 0.45 * dy),
                Pos2::new(c.x + r * dx, c.y + r * dy),
            ],
            Stroke::new(w, Color32::WHITE.gamma_multiply((0.12 + 0.88 * k) * alpha)),
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Compact transport floors the horizontal grid at 2 rows: when
    /// the configured card size only fits one, cards shrink until
    /// the required rows are actually visible (the compact bar
    /// shaved a card-height off the cover area).
    #[test]
    fn horizontal_grid_min_rows_shrinks_cards() {
        let cfg = settings::Carousel::default();
        // avail_h = 700 - (20+28+14) = 638; full card_h = 376*1.36 =
        // 511.36 -> one row fits naturally.
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 700.0));
        let one = GridGeo::horizontal(rect, 6, &cfg, 1.0, 1);
        assert_eq!(one.columns, 6);
        let two = GridGeo::horizontal(rect, 6, &cfg, 1.0, 2);
        assert_eq!(two.columns, 3);
        assert!(two.card_h < one.card_h);
        assert!(two.card_h * 2.0 + two.spacing <= 638.0 + 1e-4);
        // Plenty of room: the floor changes nothing.
        let tall = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 1200.0));
        let t = GridGeo::horizontal(tall, 6, &cfg, 1.0, 2);
        assert!((t.card_h - one.card_h).abs() < 1e-4);
        // A lone item never splits into phantom rows.
        let single = GridGeo::horizontal(rect, 1, &cfg, 1.0, 2);
        assert_eq!(single.columns, 1);
    }

    /// Regression: the resize-grip drag used to pop a card into
    /// reorder mode — the window moved under a held pointer and the
    /// re-mapped coords read as a drag gesture. The watch must flag
    /// every frame the screen rect changes (press/drag is dropped and
    /// new presses blocked) and settle when it stops.
    #[test]
    fn resize_watch_flags_screen_changes() {
        let mut w = ResizeWatch::default();
        let a = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        let b = Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 600.0));
        assert!(!w.update(a)); // first frame: baseline, not a resize
        assert!(!w.update(a)); // stable: no resize
        assert!(w.update(b)); // resize starts
        assert!(w.update(a)); // still dragging — keep blocking
        assert!(!w.update(a)); // settled
    }

    /// Select-on-press semantics: plain press singles, Shift extends
    /// from the anchor, Ctrl toggles — all applied on mouse-down.
    #[test]
    fn select_vi_covers_plain_shift_ctrl() {
        let mut c = Carousel::new();
        let visible: Vec<usize> = (0..6).collect();
        // Plain press singles.
        c.select_vi(2, egui::Modifiers::NONE, &visible);
        assert_eq!(c.sel, 2);
        assert_eq!(c.sel_set, BTreeSet::from([2]));
        // Shift extends a range from the anchor.
        c.select_vi(4, egui::Modifiers::SHIFT, &visible);
        assert_eq!(c.sel_set, BTreeSet::from([2, 3, 4]));
        // Ctrl toggles one item on/off.
        c.select_vi(0, egui::Modifiers::CTRL, &visible);
        assert!(c.sel_set.contains(&0));
        c.select_vi(0, egui::Modifiers::CTRL, &visible);
        assert!(!c.sel_set.contains(&0));
        assert!(c.sel_touched);
    }

    /// Marquee UX: a new title must be VISIBLE immediately (offset 0),
    /// hold before scrolling, then scroll left and re-enter from the
    /// right edge after a wrap — the previous behavior started the
    /// text off-screen so it slid in late.
    #[test]
    fn marquee_shows_new_title_then_scrolls() {
        use std::time::Duration;
        let mut c = Carousel::new();
        c.marquee_title = true;
        let t0 = Instant::now();
        let (tw, avail, s) = (400.0, 200.0, 1.0);

        // New title: visible in place, not off-screen.
        assert!(c.marquee_step("long title", tw, avail, s, 0.016, t0));
        assert_eq!(c.marquee_off, 0.0);

        // During the 2s hold the offset stays put.
        assert!(c.marquee_step("long title", tw, avail, s, 0.016, t0 + Duration::from_secs(1)));
        assert_eq!(c.marquee_off, 0.0);

        // After the hold it scrolls left.
        assert!(c.marquee_step("long title", tw, avail, s, 0.5, t0 + Duration::from_secs(3)));
        assert!(c.marquee_off < 0.0);

        // Past the tail it wraps to off-screen-right and pauses.
        let mut now = t0 + Duration::from_secs(3);
        for _ in 0..1000 {
            now += Duration::from_millis(16);
            c.marquee_step("long title", tw, avail, s, 0.016, now);
            if c.marquee_off > avail {
                break;
            }
        }
        assert!(c.marquee_off > avail, "never re-entered from the right");
        // During the wrap pause the offset holds.
        let off = c.marquee_off;
        assert!(c.marquee_step("long title", tw, avail, s, 0.016, now + Duration::from_millis(100)));
        assert_eq!(c.marquee_off, off);

        // A fitting title never engages and clears the state.
        assert!(!c.marquee_step("short", 100.0, avail, s, 0.016, now));
        assert!(c.marquee_text.is_empty());
    }

    /// The mini bar marquees regardless of the compact-mode
    /// `marquee_title` flag: an overflowing title engages and yields
    /// the lane offset, a fitting one doesn't, and the flag itself is
    /// left as it was.
    #[test]
    fn mini_marquee_ignores_compact_flag() {
        let mut c = Carousel::new();
        assert!(!c.marquee_title);
        // Overflowing title: active, offset reported, hold starts at 0.
        assert_eq!(c.mini_marquee("long title", 400.0, 200.0, 1.0, 0.016), Some(0.0));
        assert!(!c.marquee_title, "flag must be restored");
        // Fitting title: no marquee.
        assert_eq!(c.mini_marquee("short", 100.0, 200.0, 1.0, 0.016), None);
        // A set flag is preserved too.
        c.marquee_title = true;
        let _ = c.mini_marquee("long title", 400.0, 200.0, 1.0, 0.016);
        assert!(c.marquee_title);
    }

    /// Regression: a video submitted as a playlist/mix link stored the
    /// raw url as its path — which is also the stream-cache key, so the
    /// item built its own cache (the one wedged on itag=18 leftovers)
    /// instead of sharing the canonical video's. `add_url` must store
    /// the cleaned link (Lacrima's GetCleanVideoLink).
    #[test]
    fn add_url_stores_canonical_watch_link() {
        let mut c = Carousel::new();
        assert!(c.add_url(
            "https://www.youtube.com/watch?v=PiwDxH2HgIM&list=RDPiwDxH2HgIM&start_radio=1"
        ));
        assert_eq!(
            c.items.last().unwrap().path,
            PathBuf::from("https://www.youtube.com/watch?v=PiwDxH2HgIM")
        );
        // A later plain-link add of the same video still dedupes.
        assert!(!c.add_url("https://www.youtube.com/watch?v=PiwDxH2HgIM"));
        // Non-YouTube urls pass through untouched.
        assert!(c.add_url("https://example.com/radio.pls"));
        assert_eq!(
            c.items.last().unwrap().path,
            PathBuf::from("https://example.com/radio.pls")
        );
    }
}
