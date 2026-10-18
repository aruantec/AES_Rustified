//! The bottom albums strip (Lacrima's CompositionAlbumRowControl /
//! MusicListView): a horizontal row of album tiles, each showing a fan
//! of up to 4 covers stacked right-aligned that spread left on hover,
//! a count badge, and a title bar. Spring-physics scrolling with an
//! auto-fading scrollbar; selection gets a scale lift + white ring.
//!
//! Constants are ported from CompositionAlbumRowVisualHandler /
//! AlbumRowLayoutHelper.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};

use crate::library::{self, Album, CoverSrc};
use crate::paint;

// --- Layout (AlbumRowLayoutHelper) -------------------------------------
// All metrics are base values, multiplied by `scale` at draw time —
// Lacrima's ScalableDecorator zooms the whole strip uniformly.
const BASE_TILE_W: f32 = 240.0;
const BASE_TILE_H: f32 = 220.0;
const TITLE_BAR_H: f32 = 50.0;
const SPACING: f32 = 20.0;
pub const PAD_X: f32 = 20.0;
const PAD_Y: f32 = 12.0;
pub const SCROLLBAR_RESERVE: f32 = 28.0;
const SCROLLBAR_H: f32 = 8.0;
const SCROLLBAR_BOTTOM: f32 = 6.0;
const SCROLLBAR_HIT_H: f32 = 22.0;

// --- Tile visuals (CompositionAlbumRowVisualHandler) -------------------
const TILE_RADIUS: f32 = 12.0;
const COVER_RADIUS: f32 = 12.0;
const SELECT_LIFT: f32 = 0.072;
const HOVER_LIFT: f32 = 0.034;
const MAX_COVERS: usize = library::MAX_COVERS;
const FAN_MARGIN: f32 = 0.18; // fraction of the square cover size

const ROW_BG: Color32 = Color32::BLACK; // MusicListView Grid Background="Black"
const TILE_BG: Color32 = Color32::from_rgb(0x11, 0x11, 0x11);
const TILE_BORDER: Color32 = Color32::from_rgb(0x1A, 0x1A, 0x1A);
const TITLE_BG: Color32 = Color32::from_black_alpha(204);
const BADGE_BG: Color32 = Color32::from_rgb(0x22, 0x22, 0x22);

/// Cover decodes run on this many worker threads — a single
/// `image::load_from_memory` on embedded art can take tens of ms, so
/// doing it on the UI thread froze the view on entry.
const COVER_WORKERS: usize = 2;
/// Concurrent web-thumbnail fetches — enough to keep tiles filling
/// without saturating the connection when many URL albums are visible.
const WEB_WORKERS: usize = 3;

// --- Drag-to-reorder (CompositionAlbumRowControl) ----------------------
/// Pointer travel before a press becomes a drag (DragStartThreshold).
const DRAG_START_PX: f32 = 4.0;
/// Scale of the tile while it floats under the cursor (DragLiftScale).
const DRAG_LIFT: f32 = 1.04;
/// Glide-into-slot duration after release (DragCommitMs 300).
const COMMIT_SECS: f32 = 0.3;
/// Exponential time-constant for the other tiles sliding aside
/// (SwapAnimationSeconds 0.2 * 0.45).
const SWAP_TAU: f32 = 0.09;
/// Edge auto-scroll speed in px/s (UpdateDragAutoScroll scrollSpeed).
const DRAG_SCROLL_SPEED: f32 = 780.0;
/// Drop-slot hole color; Lacrima's #1A1A1A at alpha 180 (~0.7).
const HOLE: Color32 = Color32::from_rgb(0x1A, 0x1A, 0x1A);
const HOLE_ALPHA: f32 = 0.7;

/// What the row wants the app to do next frame.
#[allow(dead_code)] // Opened/Selected payloads get used by the album view next
pub enum Action {
    None,
    /// Single click selects an album. Carries the album's dir, not
    /// its index — the shown list can be a filter-clone, so an index
    /// would resolve against the wrong list in the caller.
    Selected(String),
    /// Double-clicked — will open the album's track view later.
    /// Dir-keyed for the same reason.
    Opened(String),
    /// "Delete Album" on the pointed tile (Lacrima's DeleteFolder —
    /// removes it from the list, not from disk). Carries the albums'
    /// dirs, not indices — the shown list can be filter-cloned. The
    /// Delete key pushes every selected album.
    DeleteAlbum(Vec<String>),
    /// "Export Playlist" on the pointed tile — write its URL items
    /// to an .apl file (Lacrima's ExportOnlinePlaylistCommand). When
    /// the pointed tile sits inside a multi-selection the payload is
    /// every selected album's dir (one .apl each), like Delete.
    Export(Vec<String>),
    /// "Export All Playlists" — every album holding URL items gets
    /// its own .apl in a picked folder, regardless of selection.
    ExportAll,
    /// "Import Playlist" — pick an .apl file to add as an album
    /// (Lacrima's ImportOnlinePlaylistCommand).
    Import,
    /// "Create Album" — append an empty virtual album and enter
    /// inline title edit on it (Lacrima's AddUserAlbum).
    Create,
    /// Inline title edit committed — carries the album's dir (stable
    /// key) and the new title.
    Rename(String, String),
    AddFolder,
    Rescan,
}

enum CoverTex {
    Ready(egui::TextureHandle),
    Failed,
}

/// File-version signature for `File`/`Tag` covers — mtime+len mixed.
/// A cover file re-saved after decode (the fetch ladder overwriting
/// `.img`, an editor Save) must invalidate the cached texture, not
/// serve the old decode forever. `Web` has no file → constant.
fn cover_sig(src: &CoverSrc) -> u64 {
    let p = match src {
        CoverSrc::File(p) | CoverSrc::Tag(p) => p,
        CoverSrc::Web(_) => return u64::MAX,
    };
    std::fs::metadata(p)
        .map(|m| {
            let t = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            m.len() ^ t.rotate_left(32)
        })
        .unwrap_or(0)
}

/// Per-album fan animation state: a spring-driven 0..1 spread factor
/// that all layer offsets derive from (equivalent to Lacrima's
/// per-layer lerp — every target moves proportionally).
#[derive(Clone, Copy, Default)]
struct Fan {
    spread: f32,
    vel: f32,
}

/// Underdamped-spring step: `pos` chases `target` carrying `vel`, so
/// motion has inertia and a gentle overshoot+settle instead of the
/// constant-rate lerp that reads as robotic.
fn spring_step(pos: f32, vel: &mut f32, target: f32, stiffness: f32, damping: f32, dt: f32) -> f32 {
    *vel += ((target - pos) * stiffness - *vel * damping) * dt;
    pos + *vel * dt
}

/// Post-release glide of a dropped block back into its slots
/// (Lacrima's AlbumRowDragCommitMessage + DragCommitSeconds). `offs`
/// captures each tile's offset from the primary tile's center at
/// release time so the whole group glides home together.
#[derive(Clone)]
struct Commit {
    start: Pos2,
    offs: Vec<Vec2>,
    progress: f32,
}

/// In-progress drag-reorder (Lacrima's _isDragging state): the pressed
/// tile is tracked, and past DRAG_START_PX it lifts and follows the
/// pointer while the other tiles slide aside to open the drop slot.
/// `indices` is the whole dragged block (sorted, contains `index`) —
/// the current selection when the grabbed tile is selected, else just
/// the grabbed tile (ListboxItemDragBehavior's _draggedIndices).
struct Drag {
    /// Album grabbed at press time (_currentDraggedIndex).
    index: usize,
    /// Sorted dragged block — index set moves as one contiguous run.
    indices: Vec<usize>,
    /// Press position — the move-threshold reference (_startPoint).
    press: Pos2,
    /// Tile-center ↔ pointer offset kept for the whole drag
    /// (_dragPointerOffset).
    grab_offset: Vec2,
    /// Latest visual center of the floating primary tile
    /// (_dragPosition).
    center: Pos2,
    /// Slot the PRIMARY tile would land in now
    /// (_lastValidSlotIndex); the block lands contiguously around it.
    drop_target: usize,
    /// Per-tile x shifts that open the gap, animated (_swapOffsets).
    swap_offsets: HashMap<usize, f32>,
    /// 0→1 glue ease — selected tiles collapse from their scattered
    /// slots into a contiguous block under the pointer (AnimateGlue).
    glue: f32,
    /// Past the threshold — before that the press is just a click.
    has_moved: bool,
    /// Set on release when the target differs from the start slot.
    commit: Option<Commit>,
}

impl Clone for Drag {
    fn clone(&self) -> Self {
        Self {
            index: self.index,
            indices: self.indices.clone(),
            press: self.press,
            grab_offset: self.grab_offset,
            center: self.center,
            drop_target: self.drop_target,
            swap_offsets: self.swap_offsets.clone(),
            glue: self.glue,
            has_moved: self.has_moved,
            commit: self.commit.clone(),
        }
    }
}

pub struct AlbumRow {
    current_scroll: f32,
    target_scroll: f32,
    spring_velocity: f32,
    scale: f32,
    hovered: i32,
    pub selected: i32,
    /// Multi-selection (SelectionMode=Multiple): album dir keys so a
    /// filtered `shown` list or removals can't shift membership.
    /// `selected` stays the primary tile for the single-select paths.
    sel_dirs: BTreeSet<String>,
    /// Dir the last plain/ctrl click landed on — Shift+click extends
    /// the range from it.
    sel_anchor: String,
    /// Set on any user selection gesture — the app consumes it to
    /// decide which view owns the Delete key.
    sel_touched: bool,
    /// Press on an already-selected tile keeps the group for a
    /// possible drag; a release that never moved collapses the
    /// selection to that tile (deferred single-click).
    defer_collapse: bool,
    hover_lift: HashMap<usize, (f32, f32)>,
    selection_fade: f32,
    scrollbar_opacity: f32,
    scrollbar_op_vel: f32,
    scrollbar_visible_until: Instant,
    scrollbar_drag: bool,
    /// Pointer x offset inside the thumb when the grab started — the
    /// thumb stays glued to the pointer rather than snapping to center.
    sb_grab_dx: f32,
    fans: Vec<Fan>,
    drag: Option<Drag>,
    /// Album index the context menu was opened on (PointedFolder).
    menu_album: Option<usize>,
    /// Cached cover decodes keyed by source, stamped with the file
    /// signature read at decode time (`cover_sig`) — a changed file
    /// re-decodes instead of serving the stale frame forever.
    textures: HashMap<CoverSrc, (CoverTex, u64)>,
    /// Lacrima's `PlaceholderGenerator` music-note cover, used for any
    /// layer with no real art (uploaded lazily on first use).
    placeholder: Option<egui::TextureHandle>,
    /// Work queue shared with the local decode threads.
    cover_queue: Arc<(Mutex<VecDeque<CoverSrc>>, Condvar)>,
    /// Separate bounded queue for web covers — they block on network
    /// timeouts, so a dedicated small pool keeps many URL albums
    /// filling without a thread-per-source storm or starving local
    /// decodes.
    web_queue: Arc<(Mutex<VecDeque<CoverSrc>>, Condvar)>,
    /// Finished decodes, drained on the UI thread (texture upload is
    /// cheap — it's the image decode that stalls).
    cover_results: mpsc::Receiver<(CoverSrc, u64, Option<egui::ColorImage>)>,
    /// Sources already queued or being decoded — avoids duplicate jobs.
    cover_pending: HashSet<CoverSrc>,
    /// Set when a drop glide lands and `albums` was reordered; the app
    /// consumes it via `take_order_dirty` to persist the new order.
    order_dirty: bool,
    /// Dir of the album whose title is being edited inline (Create
    /// Album / Rename Album) — a TextEdit overlays its title band.
    editing: Option<String>,
    edit_text: String,
    /// Whether the editor got its first-frame focus+select-all yet.
    edit_armed: bool,
    /// Dir of a tile that must be scrolled into view next frame —
    /// `edit_album` sets it so a freshly created/renamed tile is
    /// visible before the user types.
    pending_reveal: Option<String>,
    /// Last frame's view rect + tile metrics — arrow-key navigation
    /// glides the target tile into view between paints.
    nav_view: Option<(Rect, Metrics)>,
    /// Dirs of the list last rendered — `ui` may receive a
    /// filter-cloned list, so every outward-facing selection API
    /// resolves through this snapshot instead of raw indices.
    last_dirs: Vec<String>,
    /// Set by the caller when the shown list is a filtered subset —
    /// a reorder then can't map back to the real list, so drag/arrow
    /// reorders are dropped instead of silently reordering a clone
    /// (or worse, the real list at shown-space indices).
    pub filtered: bool,
    /// Settings toggle: tiles keep their bottom title bar painted.
    /// Off hides the bar + count badge behind a per-tile hover
    /// reveal (see `title_fade`) so the strip shows bare covers.
    pub show_titles: bool,
    /// Per-album title-bar fade (dir → 0..1): pinned at 1 while
    /// `show_titles` is on; otherwise eases in on hover and back
    /// out. Dir-keyed so filtering/reorders don't reset it.
    title_fade: HashMap<String, f32>,
    /// Brief focus flash (1→0) on Tab focus-in — a fading border
    /// around the strip so the user sees where the keys now land.
    focus_flash: f32,
    /// Held lift (0→1) while Shift is down — the selected tiles
    /// float slightly to signal they can be reordered with arrows.
    pop: f32,
}

#[derive(Clone)]
struct Metrics {
    tile_w: f32,
    tile_h: f32,
    tile_top: f32,
    content_w: f32,
    max_scroll: f32,
}

impl Default for AlbumRow {
    fn default() -> Self {
        Self::new()
    }
}

impl AlbumRow {
    pub fn new() -> Self {
        let (tx, cover_results) = mpsc::channel();
        let cover_queue =
            Arc::new((Mutex::new(VecDeque::<CoverSrc>::new()), Condvar::new()));
        let web_queue =
            Arc::new((Mutex::new(VecDeque::<CoverSrc>::new()), Condvar::new()));
        let spawn_pool = |queue: Arc<(Mutex<VecDeque<CoverSrc>>, Condvar)>,
                          workers: usize,
                          name: &'static str| {
            for _ in 0..workers {
                let queue = queue.clone();
                let tx = tx.clone();
                std::thread::Builder::new()
                    .name(name.into())
                    .spawn(move || loop {
                        let src = {
                            let (mutex, cv) = &*queue;
                            let mut g = mutex.lock().unwrap();
                            loop {
                                if let Some(s) = g.pop_front() {
                                    break s;
                                }
                                g = cv.wait(g).unwrap();
                            }
                        };
                        // Sig BEFORE decode — a mid-decode rewrite
                        // leaves the stored sig stale so the next
                        // check re-decodes (self-healing TOCTOU).
                        if tx
                            .send((src.clone(), cover_sig(&src), library::decode_cover(&src)))
                            .is_err()
                        {
                            return;
                        }
                    })
                    .expect("cover decode thread");
            }
        };
        spawn_pool(cover_queue.clone(), COVER_WORKERS, "aes-cover-decode");
        spawn_pool(web_queue.clone(), WEB_WORKERS, "aes-cover-web");
        Self {
            current_scroll: 0.0,
            target_scroll: 0.0,
            spring_velocity: 0.0,
            scale: 1.0,
            hovered: -1,
            selected: -1,
            hover_lift: HashMap::new(),
            selection_fade: 0.0,
            scrollbar_opacity: 0.0,
            scrollbar_op_vel: 0.0,
            scrollbar_visible_until: Instant::now(),
            scrollbar_drag: false,
            sb_grab_dx: 0.0,
            fans: Vec::new(),
            sel_dirs: BTreeSet::new(),
            sel_anchor: String::new(),
            sel_touched: false,
            defer_collapse: false,
            drag: None,
            menu_album: None,
            textures: HashMap::new(),
            placeholder: None,
            cover_queue,
            web_queue,
            cover_results,
            cover_pending: HashSet::new(),
            order_dirty: false,
            editing: None,
            edit_text: String::new(),
            edit_armed: false,
            pending_reveal: None,
            nav_view: None,
            last_dirs: Vec::new(),
            filtered: false,
            show_titles: true,
            title_fade: HashMap::new(),
            focus_flash: 0.0,
            pop: 0.0,
        }
    }

    /// Enter inline title edit on `dir` — the tile is scrolled into
    /// view next frame and the current title comes up fully selected
    /// so typing replaces it (Create Album / Rename Album).
    pub fn edit_album(&mut self, dir: &str, title: &str) {
        self.editing = Some(dir.to_string());
        self.edit_text = title.to_string();
        self.edit_armed = false;
        self.pending_reveal = Some(dir.to_string());
    }

    /// Whether `albums` was reordered since the last check.
    pub fn take_order_dirty(&mut self) -> bool {
        std::mem::take(&mut self.order_dirty)
    }

    /// The strip below the tiles where scrollbar grabs are accepted —
    /// from the tile bottom edge down past the row (also used to
    /// exclude the window-resize grip over the same area).
    pub fn scroll_zone(row: Rect, s: f32) -> Rect {
        let tile_top = PAD_Y * s
            + (row.height() - PAD_Y * 2.0 * s - SCROLLBAR_RESERVE * s - BASE_TILE_H * s)
                .max(0.0)
                * 0.5;
        Rect::from_min_max(
            Pos2::new(row.min.x, row.min.y + tile_top + BASE_TILE_H * s),
            Pos2::new(row.max.x, row.max.y + SCROLLBAR_HIT_H * s),
        )
    }

    /// Shared placeholder texture for layers with no real art.
    pub fn placeholder_tex(&mut self, ctx: &egui::Context) -> egui::TextureHandle {
        self.placeholder
            .get_or_insert_with(|| {
                ctx.load_texture(
                    "cover:placeholder",
                    library::placeholder_image(),
                    egui::TextureOptions::LINEAR,
                )
            })
            .clone()
    }

    /// Drop one cached cover texture (e.g. after a tag edit changed a
    /// file's embedded art) so the next paint re-decodes it.
    pub fn invalidate_cover(&mut self, src: &CoverSrc) {
        self.textures.remove(src);
    }

    /// Drop cached textures/fan state (e.g. after a rescan).
    pub fn reset(&mut self) {
        self.textures.clear();
        self.fans.clear();
        self.selected = -1;
        self.sel_dirs.clear();
        self.sel_anchor.clear();
        self.defer_collapse = false;
        self.hovered = -1;
        self.current_scroll = 0.0;
        self.target_scroll = 0.0;
        self.drag = None;
        self.editing = None;
        self.pending_reveal = None;
    }

    /// Title-bar reveal fades — pinned at 1 while `show_titles` is
    /// on; otherwise each shown tile's bar eases in while hovered and
    /// back out. Dir-keyed (via `last_dirs`) so filters/reorders
    /// don't reset it. Returns true while any fade is still moving so
    /// the caller keeps repainting — the app is event-driven and an
    /// unstopped fade would freeze on its last frame.
    fn title_fade_step(&mut self, dt: f32) -> bool {
        let mut moving = false;
        for (i, d) in self.last_dirs.iter().enumerate() {
            let tgt = if self.show_titles || i as i32 == self.hovered {
                1.0
            } else {
                0.0
            };
            let e = self
                .title_fade
                .entry(d.clone())
                .or_insert(if self.show_titles { 1.0 } else { 0.0 });
            *e += (tgt - *e) * (dt / 0.16).min(1.0);
            // Snap inside epsilon like the hover springs do — a pure
            // lerp asymptotes and hidden entries would never reach 0.
            if (*e - tgt).abs() <= 0.002 {
                *e = tgt;
            } else {
                moving = true;
            }
            *e = e.clamp(0.0, 1.0);
        }
        // Settled-hidden entries get pruned so scrolled-past or
        // filtered-out albums don't accrete in the map.
        self.title_fade.retain(|_, v| *v > 0.001);
        moving
    }

    fn metrics(&self, rect: Rect, count: usize) -> Metrics {
        let s = self.scale;
        let tile_w = BASE_TILE_W * s;
        let tile_h = BASE_TILE_H * s;
        let (pad_x, pad_y) = (PAD_X * s, PAD_Y * s);
        let content_w = if count == 0 {
            0.0
        } else {
            pad_x + count as f32 * tile_w + (count - 1) as f32 * SPACING * s + pad_x
        };
        Metrics {
            tile_w,
            tile_h,
            tile_top: pad_y
                + (rect.height() - pad_y * 2.0 - SCROLLBAR_RESERVE * s - tile_h)
                    .max(0.0)
                    * 0.5,
            content_w,
            max_scroll: (content_w - rect.width()).max(0.0),
        }
    }

    fn tile_rect(&self, rect: Rect, m: &Metrics, index: usize) -> Rect {
        let x = rect.min.x + PAD_X * self.scale
            + index as f32 * (m.tile_w + SPACING * self.scale)
            - self.current_scroll;
        Rect::from_min_size(
            Pos2::new(x, rect.min.y + m.tile_top),
            Vec2::new(m.tile_w, m.tile_h),
        )
    }

    fn hit_tile(&self, rect: Rect, m: &Metrics, count: usize, pos: Pos2) -> i32 {
        if count == 0 || pos.y < rect.min.y + m.tile_top || pos.y > rect.min.y + m.tile_top + m.tile_h {
            return -1;
        }
        let lx = pos.x - rect.min.x + self.current_scroll - PAD_X * self.scale;
        if lx < 0.0 {
            return -1;
        }
        let stride = m.tile_w + SPACING * self.scale;
        let i = (lx / stride) as i32;
        if i < 0 || i as usize >= count || lx - i as f32 * stride > m.tile_w {
            -1
        } else {
            i
        }
    }

    /// Where `index` displays while the dragged block targets
    /// `block_start` (AlbumRowLayoutHelper.ComputeDisplaySlot extended
    /// to a contiguous run — ListboxItemDragBehavior's
    /// ShiftItemsForSwap): dragged tiles own the block slots, the rest
    /// shift around them.
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
    fn block_start(d: &Drag, count: usize) -> usize {
        let pr = d
            .indices
            .iter()
            .position(|&i| i == d.index)
            .unwrap_or(0);
        d.drop_target
            .min(count.saturating_sub(1))
            .saturating_sub(pr)
            .min(count.saturating_sub(d.indices.len()))
    }

    /// Re-key index-addressed animation state after `albums` got a
    /// remove+insert move (Lacrima's SwapTileCoverState/MoveSnapshotItem —
    /// our textures are keyed by cover source so they need no remap).
    fn note_reorder(&mut self, dragged: &[usize], block_start: usize, primary_rank: usize) {
        if dragged
            .iter()
            .enumerate()
            .any(|(k, &i)| i != block_start + k)
        {
            let mut perm: Vec<usize> = (0..self.fans.len()).collect();
            let mut pulled: Vec<usize> = Vec::with_capacity(dragged.len());
            for &i in dragged.iter().rev() {
                pulled.push(perm.remove(i));
            }
            pulled.reverse();
            for (k, v) in pulled.into_iter().enumerate() {
                perm.insert(block_start + k, v);
            }
            let old = std::mem::take(&mut self.fans);
            self.fans = perm.into_iter().map(|i| old[i]).collect();
        }
        self.hover_lift.clear();
        self.hovered = -1;
        self.selected = (block_start + primary_rank) as i32;
    }

    /// Advance the drag-reorder state machine (ported from
    /// CompositionAlbumRowControl's pointer handlers). Runs every frame
    /// while a drag or the drop glide is in flight; the album move is
    /// applied to `albums` the moment the glide lands so indices never
    /// go stale.
    fn update_drag(
        &mut self,
        ctx: &egui::Context,
        rect: Rect,
        m: &Metrics,
        albums: &mut Vec<Album>,
        dt: f32,
        interactive: bool,
    ) {
        let count = albums.len();
        if count == 0
            || self
                .drag
                .as_ref()
                .map(|d| d.index >= count || d.indices.iter().any(|&i| i >= count))
                .unwrap_or(false)
        {
            self.drag = None;
            return;
        }
        let (pointer, down) =
            ctx.input(|i| (i.pointer.latest_pos(), i.pointer.primary_down()));

        // Drop glide: ease the whole block home, then reorder the
        // model — dragged indices out (descending keeps positions
        // valid), back in contiguously at the block start
        // (TryReorderItems).
        let landed = match self.drag.as_mut().and_then(|d| d.commit.as_mut()) {
            Some(c) => {
                c.progress += dt / COMMIT_SECS;
                c.progress >= 1.0
            }
            None => false,
        };
        if landed {
            let d = self.drag.take().unwrap();
            // A filtered strip renders a clone — a drop can't be mapped
            // back to the real list, so the reorder is dropped instead
            // of writing a shown-space permutation into the model.
            if !self.filtered {
                let bs = Self::block_start(&d, count);
                let already = d
                    .indices
                    .iter()
                    .enumerate()
                    .all(|(k, &i)| i == bs + k);
                if !already {
                    let mut pulled: Vec<Album> = Vec::with_capacity(d.indices.len());
                    for &i in d.indices.iter().rev() {
                        pulled.push(albums.remove(i));
                    }
                    pulled.reverse();
                    for (k, a) in pulled.into_iter().enumerate() {
                        albums.insert(bs + k, a);
                    }
                }
                let pr = d
                    .indices
                    .iter()
                    .position(|&i| i == d.index)
                    .unwrap_or(0);
                self.note_reorder(&d.indices, bs, pr);
                self.order_dirty = true;
            }
            return;
        }
        // Commit glide still in flight (or no drag at all) — nothing
        // else to do until it lands or a fresh press cancels it.
        if self.drag.as_ref().map(|d| d.commit.is_some()).unwrap_or(true) {
            return;
        }

        // Released mid-drag (or the view went inactive): never-moved
        // drags were plain clicks — a click on an already-selected
        // tile collapses the selection to it (the defer set on press).
        if !interactive || !down {
            enum Rel {
                Click,
                SnapBack,
                Glide { start: Pos2, offs: Vec<Vec2> },
            }
            let rel = {
                let d = self.drag.as_ref().unwrap();
                if !d.has_moved {
                    Rel::Click
                } else {
                    let bs = Self::block_start(d, count);
                    if d
                        .indices
                        .iter()
                        .enumerate()
                        .all(|(k, &i)| i == bs + k)
                    {
                        // Landing on the start slots snaps back.
                        Rel::SnapBack
                    } else {
                        // Freeze each tile's offset from the primary
                        // center so the group glides in formation.
                        let stride = m.tile_w + SPACING * self.scale;
                        let glue_e =
                            (d.glue.min(1.0) * std::f32::consts::FRAC_PI_2).sin();
                        let pr = d
                            .indices
                            .iter()
                            .position(|&i| i == d.index)
                            .unwrap_or(0);
                        let pc = self.tile_rect(rect, m, d.index).center();
                        let offs: Vec<Vec2> = d
                            .indices
                            .iter()
                            .enumerate()
                            .map(|(k, &i)| {
                                let home = self.tile_rect(rect, m, i).center() - pc;
                                let glued = Vec2::new(
                                    (k as f32 - pr as f32) * stride,
                                    0.0,
                                );
                                home + (glued - home) * glue_e
                            })
                            .collect();
                        Rel::Glide {
                            start: d.center,
                            offs,
                        }
                    }
                }
            };
            match rel {
                Rel::Click => {
                    if self.defer_collapse {
                        if let Some(a) = self
                            .drag
                            .as_ref()
                            .and_then(|d| albums.get(d.index))
                        {
                            let dir = a.dir.to_string_lossy().into_owned();
                            self.sel_dirs.clear();
                            self.sel_dirs.insert(dir.clone());
                            self.sel_anchor = dir;
                        }
                        self.defer_collapse = false;
                    }
                    self.drag = None;
                }
                Rel::SnapBack => {
                    self.drag = None;
                }
                Rel::Glide { start, offs } => {
                    self.drag.as_mut().unwrap().commit =
                        Some(Commit { start, offs, progress: 0.0 });
                }
            }
            return;
        }

        let Some(p) = pointer else { return };
        let scroll = self.current_scroll;
        let scale = self.scale;
        let stride = m.tile_w + SPACING * scale;
        let slot_cx =
            |i: usize| rect.min.x + PAD_X * scale + i as f32 * stride + m.tile_w * 0.5 - scroll;
        let slot_cy = rect.min.y + m.tile_top + m.tile_h * 0.5;

        let center;
        {
            let d = self.drag.as_mut().unwrap();
            if !d.has_moved {
                if p.distance(d.press) <= DRAG_START_PX {
                    return;
                }
                // Visual drag begins (ActivateVisualDrag): hover/fans
                // shut off so the row is still while tiles slide aside.
                d.has_moved = true;
                self.hovered = -1;
                for fan in &mut self.fans {
                    fan.spread = 0.0;
                    fan.vel = 0.0;
                }
            }
            d.center = p + d.grab_offset;
            center = d.center;
            // Selected tiles converge into a contiguous block under
            // the pointer — Lacrima's AnimateGlue (250ms sin ease).
            d.glue = (d.glue + dt / 0.25).min(1.0);

            // Nearest slot center decides the drop target
            // (FindNearestDropTargetIndex).
            let mut best = 0usize;
            let mut best_d = f32::MAX;
            for i in 0..count {
                let dx = slot_cx(i) - center.x;
                let dy = slot_cy - center.y;
                let dd = dx * dx + dy * dy;
                if dd < best_d {
                    best_d = dd;
                    best = i;
                }
            }
            d.drop_target = best;
        }

        // Edge auto-scroll (UpdateDragAutoScroll: quadratic falloff
        // inside a border zone, faster the more content remains).
        if m.max_scroll > 1.0 {
            let w = rect.width();
            let zone = (w * 0.16).clamp(64.0, 140.0);
            let lx = center.x - rect.min.x;
            let step = if lx < zone {
                -((zone - lx) / zone).powi(2)
            } else if lx > w - zone {
                ((lx - w + zone) / zone).powi(2)
            } else {
                0.0
            };
            if step.abs() >= 0.02 {
                let remaining = if step < 0.0 {
                    self.current_scroll
                } else {
                    m.max_scroll - self.current_scroll
                };
                let boost = 1.0 + (remaining / 1800.0).min(2.5);
                let next = (self.current_scroll + step * DRAG_SCROLL_SPEED * boost * dt)
                    .clamp(0.0, m.max_scroll);
                if (next - self.current_scroll).abs() >= 0.25 {
                    self.current_scroll = next;
                    self.target_scroll = next;
                }
            }
        }
        // While a tile floats, scroll follows input directly — no
        // spring (Lacrima's AlbumRowDirectScrollFollowMessage).
        self.current_scroll = self.target_scroll;
        self.spring_velocity = 0.0;
    }

    /// Upload covers the decode workers finished. Shared by the album
    /// row and the cover view so both see freshly decoded textures.
    fn drain_cover_results(&mut self, ctx: &egui::Context) {
        while let Ok((src, sig, img)) = self.cover_results.try_recv() {
            self.cover_pending.remove(&src);
            let entry = match img {
                Some(img) => CoverTex::Ready(ctx.load_texture(
                    format!("cover:{src:?}"),
                    img,
                    egui::TextureOptions::LINEAR,
                )),
                None => CoverTex::Failed,
            };
            self.textures.insert(src, (entry, sig));
        }
    }

    /// Send `src` to a decoder. Web covers route to the bounded web
    /// pool — one thread per fetch used to storm the network when many
    /// URL albums were visible at once.
    fn submit_cover(&self, src: CoverSrc) {
        let queue = if matches!(src, CoverSrc::Web(_)) {
            &self.web_queue
        } else {
            &self.cover_queue
        };
        queue.0.lock().unwrap().push_back(src);
        queue.1.notify_one();
    }

    /// Queue `src` for decode if needed and return its texture when
    /// ready — the cover view borrows the row's decode pool + cache.
    pub fn cover_tex(
        &mut self,
        ctx: &egui::Context,
        src: &CoverSrc,
    ) -> Option<egui::TextureHandle> {
        self.drain_cover_results(ctx);
        let sig = cover_sig(src);
        match self.textures.get(src) {
            Some((CoverTex::Ready(t), s)) if *s == sig => return Some(t.clone()),
            // The file changed since the last decode (or a stale
            // Failed) — drop it and let the decode re-run below.
            Some((_, s)) if *s != sig => {
                self.textures.remove(src);
                self.cover_pending.remove(src);
            }
            _ => {}
        }
        if self.cover_pending.insert(src.clone()) {
            self.submit_cover(src.clone());
            ctx.request_repaint();
        }
        None
    }

    /// Whether `src` failed to decode — callers fall back to another
    /// cover or the placeholder instead of waiting forever.
    pub fn cover_failed(&self, src: &CoverSrc) -> bool {
        matches!(
            self.textures.get(src),
            Some((CoverTex::Failed, s)) if *s == cover_sig(src)
        )
    }

    /// Upload covers the decode workers finished, then queue any still
    /// missing for albums in `range`. Decoding runs off the UI thread so
    /// appearing tiles never hitch; repaints keep flowing until the
    /// pending set drains.
    fn ensure_textures(&mut self, ctx: &egui::Context, albums: &[Album], range: (usize, usize)) {
        self.drain_cover_results(ctx);
        for album in albums
            .iter()
            .take(range.1.min(albums.len().saturating_sub(1)) + 1)
            .skip(range.0)
        {
            for src in &album.covers {
                let sig = cover_sig(src);
                if matches!(self.textures.get(src), Some((_, s)) if *s == sig)
                    || !self.cover_pending.insert(src.clone())
                {
                    continue;
                }
                self.submit_cover(src.clone());
            }
        }
        if !self.cover_pending.is_empty() {
            ctx.request_repaint();
        }
    }

    /// Paint + interact. `alpha` is the view transition fade. Takes the
    /// album list mutably so a completed drag-reorder can move its
    /// entry in place (Lacrima's ItemsSource MoveItem).
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        albums: &mut Vec<Album>,
        alpha: f32,
        interactive: bool,
        scale: f32,
        scanning: bool,
        reorder_armed: bool,
    ) -> Action {
        let mut action = Action::None;
        self.scale = scale;
        // Snapshot the rendered order — outward selection APIs and the
        // key handlers resolve through it so a filter-cloned `albums`
        // can't leak shown-space indices into the caller's real list.
        self.last_dirs = albums
            .iter()
            .map(|a| a.dir.to_string_lossy().into_owned())
            .collect();
        // Reorders are meaningless on a filtered subset — disarm the
        // lift cue (the drop commit itself is gated below).
        let reorder_armed = reorder_armed && !self.filtered;
        let dt = ui.ctx().input(|i| i.stable_dt).min(0.1);
        // Shift held = reorder armed — the selected tiles lift
        // slightly to signal they can be moved. A tile press that
        // could become a drag arms it the same way (the press
        // selects on down, so the cue lands on the grabbed tile).
        // Eases in fast, settles on release.
        let press_armed =
            matches!(&self.drag, Some(d) if !d.has_moved && d.commit.is_none());
        let lift_target = if (reorder_armed
            && !albums.is_empty()
            && self.selected >= 0)
            || press_armed
        {
            1.0
        } else {
            0.0
        };
        let rate = dt / if lift_target > self.pop { 0.12 } else { 0.18 };
        self.pop =
            (self.pop + (lift_target - self.pop).clamp(-rate, rate)).clamp(0.0, 1.0);
        if self.pop > 0.0 {
            ui.ctx().request_repaint();
        }
        let m = self.metrics(rect, albums.len());
        self.nav_view = Some((rect, m.clone()));
        if self.fans.len() != albums.len() {
            self.fans = (0..albums.len())
                .map(|_| Fan {
                    spread: 0.0,
                    vel: 0.0,
                })
                .collect();
        }
        // A pending reveal (Create Album / Rename Album) snaps the
        // tile fully into view this frame — no spring, so the title
        // editor is on screen the moment it grabs focus.
        if let Some(dir) = self.pending_reveal.take() {
            if let Some(i) = albums
                .iter()
                .position(|a| a.dir.to_string_lossy() == dir)
            {
                let stride = m.tile_w + SPACING * scale;
                let pad = PAD_X * scale;
                let x0 = pad + i as f32 * stride;
                let x1 = x0 + m.tile_w;
                let w = rect.width();
                let target = if x0 < self.current_scroll {
                    x0 - pad
                } else if x1 > self.current_scroll + w {
                    x1 + pad - w
                } else {
                    self.current_scroll
                };
                self.current_scroll = target.clamp(0.0, m.max_scroll);
                self.target_scroll = self.current_scroll;
                self.spring_velocity = 0.0;
            }
        }

        // --- Input ---------------------------------------------------
        let resp = ui.interact(rect, ui.id().with("album-row"), Sense::click());
        let pointer = ui.ctx().input(|i| i.pointer.latest_pos());
        let scroll_delta = if pointer.map(|p| rect.contains(p)).unwrap_or(false) {
            ui.ctx().input(|i| i.smooth_scroll_delta)
        } else {
            Vec2::ZERO
        };
        if interactive && scroll_delta != Vec2::ZERO {
            let d = scroll_delta.x + scroll_delta.y;
            self.target_scroll -= d;
            self.scrollbar_visible_until = Instant::now() + std::time::Duration::from_secs_f32(1.2);
        }

        // Scrollbar geometry.
        let s = self.scale;
        let track = Rect::from_min_max(
            Pos2::new(rect.min.x + PAD_X * s, rect.max.y - (SCROLLBAR_BOTTOM + SCROLLBAR_H) * s),
            Pos2::new(rect.max.x - PAD_X * s, rect.max.y - SCROLLBAR_BOTTOM * s),
        );
        let thumb_ratio = (rect.width() / m.content_w.max(rect.width())).min(1.0);
        let thumb_w = (track.width() * thumb_ratio).max(32.0 * s);
        let scroll_ratio = if m.max_scroll > 0.0 {
            self.current_scroll / m.max_scroll
        } else {
            0.0
        };
        let thumb = Rect::from_min_size(
            Pos2::new(
                track.min.x + (track.width() - thumb_w) * scroll_ratio,
                track.min.y,
            ),
            Vec2::new(thumb_w, SCROLLBAR_H * s),
        );
        // Grab zone: everything below the tile bottom edge, full row
        // width — a generous target that can never overlap a tile.
        let hit_rect = Self::scroll_zone(rect, s);
        let sb_resp = ui.interact(
            hit_rect,
            ui.id().with("album-row-scrollbar"),
            Sense::drag(),
        );
        let mut scrollbar_hover = false;
        if interactive && m.max_scroll > 1.0 {
            // Grab on press (not only past the drag threshold) and keep
            // the grab while the button is held, even off the strip.
            if sb_resp.is_pointer_button_down_on() && !self.scrollbar_drag {
                self.sb_grab_dx = pointer
                    .map(|p| p.x - thumb.min.x)
                    .unwrap_or(thumb_w * 0.5);
                self.scrollbar_drag = true;
            }
            if self.scrollbar_drag {
                if let Some(p) = pointer {
                    // Thumb stays glued at the grab offset — no snap.
                    let thumb_x = p.x - self.sb_grab_dx;
                    let t = ((thumb_x - track.min.x)
                        / (track.width() - thumb_w).max(1.0))
                    .clamp(0.0, 1.0);
                    self.target_scroll = t * m.max_scroll;
                    // A grabbed thumb tracks the pointer 1:1 — no spring.
                    self.current_scroll = self.target_scroll;
                    self.spring_velocity = 0.0;
                    self.scrollbar_visible_until =
                        Instant::now() + std::time::Duration::from_secs_f32(1.2);
                }
                if ui.ctx().input(|i| !i.pointer.primary_down()) {
                    self.scrollbar_drag = false;
                }
            }
            scrollbar_hover = sb_resp.hovered() || self.scrollbar_drag;
        }

        // Drag-reorder bookkeeping: threshold, drop target, edge
        // auto-scroll, release → commit glide → model move.
        self.update_drag(ui.ctx(), rect, &m, albums, dt, interactive);
        // The app is event-driven; a drag keeps animating without
        // pointer events (edge auto-scroll, the drop glide), so keep
        // frames coming while one's visually in flight.
        if self
            .drag
            .as_ref()
            .map(|d| d.has_moved || d.commit.is_some())
            .unwrap_or(false)
        {
            ui.ctx().request_repaint();
        }
        let drag_active = self.drag.as_ref().map(|d| d.has_moved).unwrap_or(false);

        let hovered = match pointer {
            Some(p)
                if interactive && rect.contains(p) && !scrollbar_hover && !drag_active =>
            {
                self.hit_tile(rect, &m, albums.len(), p)
            }
            _ => -1,
        };
        self.hovered = hovered;

        // Select on press (Lacrima's row selects on pointer-down) — not
        // on release like egui's clicked(), which makes selection lag.
        // Presses inside the scrollbar grab zone start a scrollbar drag
        // directly — raw input, independent of widget routing.
        if interactive
            && ui
                .ctx()
                .input(|i| i.pointer.primary_pressed())
        {
            if let Some(p) = ui.ctx().input(|i| i.pointer.press_origin()) {
                let on_scrollbar = m.max_scroll > 1.0 && hit_rect.contains(p);
                if on_scrollbar {
                    self.sb_grab_dx = p.x - thumb.min.x;
                    self.scrollbar_drag = true;
                } else if rect.contains(p) {
                    // Pressing while the drop glide still animates
                    // cancels the pending reorder — everything snaps
                    // back (Lacrima's SettlePointerState).
                    if self.drag.as_ref().map(|d| d.commit.is_some()).unwrap_or(false) {
                        self.drag = None;
                    }
                    let hit = self.hit_tile(rect, &m, albums.len(), p);
                    if hit >= 0 {
                        // Selection gestures (SelectionMode=Multiple):
                        // Ctrl toggles, Shift extends from the anchor,
                        // plain click selects — except a press on an
                        // already-selected tile, which keeps the group
                        // so it can be dragged (collapse defers to a
                        // no-move release).
                        let dir =
                            albums[hit as usize].dir.to_string_lossy().into_owned();
                        self.defer_collapse = false;
                        let mods = ui.ctx().input(|i| i.modifiers);
                        if mods.command {
                            if !self.sel_dirs.remove(&dir) {
                                self.sel_dirs.insert(dir.clone());
                            }
                            self.sel_anchor = dir.clone();
                        } else if mods.shift && !self.sel_anchor.is_empty() {
                            let ai = albums
                                .iter()
                                .position(|a| {
                                    a.dir.to_string_lossy() == self.sel_anchor
                                })
                                .unwrap_or(hit as usize);
                            let (lo, hi) =
                                (ai.min(hit as usize), ai.max(hit as usize));
                            self.sel_dirs = (lo..=hi)
                                .map(|i| {
                                    albums[i].dir.to_string_lossy().into_owned()
                                })
                                .collect();
                        } else if self.sel_dirs.contains(&dir) {
                            self.defer_collapse = true;
                        } else {
                            self.sel_dirs.clear();
                            self.sel_dirs.insert(dir.clone());
                            self.sel_anchor = dir.clone();
                        }
                        self.sel_touched = true;
                        self.selected = hit;
                        action = Action::Selected(dir.clone());
                        // Start tracking the press as a potential drag
                        // (BeginItemDrag); it becomes a visual drag past
                        // DRAG_START_PX. A selected tile drags the whole
                        // selection (_draggedIndices).
                        let hit = hit as usize;
                        let indices: Vec<usize> = if self.sel_dirs.contains(&dir) {
                            albums
                                .iter()
                                .enumerate()
                                .filter(|(_, a)| {
                                    self.sel_dirs
                                        .contains(a.dir.to_string_lossy().as_ref())
                                })
                                .map(|(i, _)| i)
                                .collect()
                        } else {
                            vec![hit]
                        };
                        let center = self.tile_rect(rect, &m, hit).center();
                        self.drag = Some(Drag {
                            index: hit,
                            indices,
                            press: p,
                            grab_offset: center - p,
                            center,
                            drop_target: hit,
                            swap_offsets: HashMap::new(),
                            glue: 0.0,
                            has_moved: false,
                            commit: None,
                        });
                    } else if !ui.ctx().input(|i| i.modifiers).command {
                        // Empty-row click clears the selection.
                        self.sel_dirs.clear();
                        self.sel_anchor.clear();
                        self.selected = -1;
                        self.sel_touched = true;
                    }
                }
            }
        }
        if interactive && resp.double_clicked() {
            if let Some(p) = ui.ctx().input(|i| i.pointer.interact_pos()) {
                let hit = self.hit_tile(rect, &m, albums.len(), p);
                if hit >= 0 {
                    self.selected = hit;
                    action = Action::Opened(
                        albums[hit as usize].dir.to_string_lossy().into_owned(),
                    );
                }
            }
        }
        // Right-click captures the pointed tile for the context menu's
        // item entries (Lacrima's PointedFolder).
        if interactive && resp.secondary_clicked() {
            self.menu_album = ui
                .ctx()
                .input(|i| i.pointer.interact_pos())
                .map(|p| self.hit_tile(rect, &m, albums.len(), p))
                .filter(|&h| h >= 0)
                .map(|h| h as usize);
            // Explorer semantics: right-clicking an unselected tile
            // selects just it; right-clicking a selected one keeps the
            // group so the menu acts on the whole selection. Without
            // this the menu ran on a STALE sel_dirs (right-click never
            // touches selection) — a partial leftover set labelled
            // "Export Playlists" but exported only the pointed album.
            if let Some(i) = self.menu_album {
                let dir = albums[i].dir.to_string_lossy().into_owned();
                if !self.sel_dirs.contains(&dir) {
                    self.sel_dirs.clear();
                    self.sel_dirs.insert(dir.clone());
                    self.sel_anchor = dir;
                    self.selected = i as i32;
                    self.sel_touched = true;
                }
            }
        }

        // --- Physics (ported: spring 680 / zeta 0.9, rubber-band 12/s) --
        if self.target_scroll < 0.0 {
            self.target_scroll += -self.target_scroll * (12.0 * dt).min(1.0);
            if self.target_scroll.abs() < 0.5 {
                self.target_scroll = 0.0;
            }
        } else if self.target_scroll > m.max_scroll {
            let over = self.target_scroll - m.max_scroll;
            self.target_scroll -= over * (12.0 * dt).min(1.0);
            if over < 0.5 {
                self.target_scroll = m.max_scroll;
            }
        }
        let dist = self.target_scroll - self.current_scroll;
        let stiffness = 680.0f32;
        let damping = 2.0 * stiffness.sqrt() * 0.9;
        self.spring_velocity += (dist * stiffness - self.spring_velocity * damping) * dt;
        self.current_scroll += self.spring_velocity * dt;
        if dist.abs() < 0.01 && self.spring_velocity.abs() < 0.01 {
            self.current_scroll = self.target_scroll;
            self.spring_velocity = 0.0;
        }

        // Fan spread + hover lift — near-critical springs so hover has
        // inertia and a gentle settle without visible bounce (the old
        // constant-rate lerp read as robotic).
        for (i, fan) in self.fans.iter_mut().enumerate() {
            let tgt = if i as i32 == self.hovered { 1.0 } else { 0.0 };
            fan.spread = spring_step(fan.spread, &mut fan.vel, tgt, 120.0, 20.0, dt);
            if (fan.spread - tgt).abs() < 0.002 && fan.vel.abs() < 0.02 {
                fan.spread = tgt;
                fan.vel = 0.0;
            }
            let lift_t = if i as i32 == self.hovered { 1.0 } else { 0.0 };
            let mut ls = self.hover_lift.get(&i).copied().unwrap_or((0.0, 0.0));
            ls.0 = spring_step(ls.0, &mut ls.1, lift_t, 160.0, 23.0, dt);
            if ls.0 <= 0.001 && lift_t == 0.0 && ls.1.abs() < 0.02 {
                self.hover_lift.remove(&i);
            } else {
                self.hover_lift.insert(i, ls);
            }
        }
        // The app is event-driven — while any hover spring is still
        // moving, keep frames coming or it integrates in bursts.
        let hover_moving = self
            .fans
            .iter()
            .enumerate()
            .any(|(i, f)| {
                let tgt = if i as i32 == self.hovered { 1.0 } else { 0.0 };
                (f.spread - tgt).abs() > 0.002 || f.vel.abs() > 0.02
            })
            || self.hover_lift.iter().any(|(&i, &(p, v))| {
                let tgt = if i as i32 == self.hovered { 1.0 } else { 0.0 };
                (p - tgt).abs() > 0.002 || v.abs() > 0.02
            });
        if hover_moving {
            ui.ctx().request_repaint();
        }
        // Title-bar reveal — same keepalive contract as the springs:
        // a fade mid-flight needs frames or it freezes part-way.
        if self.title_fade_step(dt) {
            ui.ctx().request_repaint();
        }
        // Swap-offset slide: non-dragged tiles ease toward the slots
        // that open a gap under the dragged tile (AnimateSwapOffsets —
        // frozen while the drop glide runs since they've arrived).
        if self
            .drag
            .as_ref()
            .map(|d| d.has_moved && d.commit.is_none())
            .unwrap_or(false)
        {
            let scroll = self.current_scroll;
            let stride = m.tile_w + SPACING * s;
            let slot_x = |i: usize| rect.min.x + PAD_X * s + i as f32 * stride - scroll;
            let step = 1.0 - (-dt / SWAP_TAU).exp();
            let d = self.drag.as_mut().unwrap();
            let bs = Self::block_start(&d, albums.len());
            for i in 0..albums.len() {
                if d.indices.binary_search(&i).is_ok() {
                    continue;
                }
                let slot = Self::display_slot(i, &d.indices, bs);
                let target = if slot == i {
                    0.0
                } else {
                    slot_x(slot) - slot_x(i)
                };
                let cur = d.swap_offsets.get(&i).copied().unwrap_or(0.0);
                let next = cur + (target - cur) * step;
                if target == 0.0 && next.abs() < 0.5 {
                    d.swap_offsets.remove(&i);
                } else {
                    d.swap_offsets.insert(i, next);
                }
            }
        }
        // Selection border fade.
        let fade_t = if self.selected >= 0 { 1.0 } else { 0.0 };
        let rate = if fade_t > self.selection_fade { 18.0 } else { 22.0 };
        self.selection_fade += (fade_t - self.selection_fade) * (dt * rate).min(1.0);
        self.selection_fade = self.selection_fade.clamp(0.0, 1.0);
        // Scrollbar opacity spring (stiffness 45, critically damped).
        let wants_bar = m.max_scroll > 1.0
            && (self.scrollbar_drag
                || scrollbar_hover
                || Instant::now() < self.scrollbar_visible_until
                || dist.abs() > 0.5);
        let op_t = if wants_bar { 1.0 } else { 0.0 };
        let op_stiff = 45.0f32;
        let op_damp = 2.0 * op_stiff.sqrt();
        self.scrollbar_op_vel += ((op_t - self.scrollbar_opacity) * op_stiff
            - self.scrollbar_op_vel * op_damp)
            * dt;
        self.scrollbar_opacity = (self.scrollbar_opacity + self.scrollbar_op_vel * dt)
            .clamp(0.0, 1.0);

        // --- Draw ----------------------------------------------------
        // Lifted tiles (Shift reorder cue / drags) poke a few px
        // above the strip — give the clip that headroom so the
        // float isn't sliced off at the top edge.
        let clip = Rect::from_min_max(
            rect.min - Vec2::new(0.0, 18.0 * s),
            rect.max,
        );
        let painter = ui.painter().with_clip_rect(clip);
        painter.rect_filled(rect, 0.0, ROW_BG.gamma_multiply(alpha));

        if albums.is_empty() {
            // While the scan streams in, say so — an empty row with no
            // feedback reads as "no library" during a long first walk.
            let hint = if scanning {
                "Scanning library…"
            } else {
                "Add a music folder to get started — right-click for options"
            };
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                hint,
                FontId::proportional(20.0 * s),
                Color32::from_rgba_unmultiplied(0x78, 0x78, 0x78, (0x88 as f32 * alpha) as u8),
            );
            if interactive {
                resp.context_menu(|ui| self.context_menu(ui, &mut action, albums));
            }
            return action;
        }

        // Visible index range with a 1-tile buffer.
        let stride = m.tile_w + SPACING * s;
        let first = (((self.current_scroll - PAD_X * s) / stride) as i32 - 1).max(0) as usize;
        let last = (((self.current_scroll + rect.width() - PAD_X * s) / stride) as i32 + 1)
            .clamp(0, albums.len() as i32 - 1)
            .max(0) as usize;
        self.ensure_textures(ui.ctx(), albums, (first, last));

        // Owned snapshot — the draw calls below borrow &mut self.
        let dragged = self
            .drag
            .clone()
            .filter(|d| d.has_moved && d.index < albums.len());
        // Drop-slot holes: dashed outline at each slot the dragged
        // block will land in (Lacrima's DrawPlaceholder, dash 8 on /
        // 8 off).
        if let Some(d) = dragged.as_ref() {
            let bs = Self::block_start(d, albums.len());
            for k in 0..d.indices.len() {
                let hole = self.tile_rect(rect, &m, bs + k);
                dashed_rect(
                    &painter,
                    hole,
                    8.0 * s,
                    2.0 * s,
                    HOLE.gamma_multiply(HOLE_ALPHA * alpha),
                );
            }
        }

        for i in first..=last {
            if i as i32 == self.selected
                || dragged
                    .as_ref()
                    .map(|d| d.indices.binary_search(&i).is_ok())
                    .unwrap_or(false)
            {
                continue;
            }
            let off = if dragged.is_some() {
                self.drag
                    .as_ref()
                    .and_then(|d| d.swap_offsets.get(&i).copied())
                    .unwrap_or(0.0)
            } else {
                0.0
            };
            self.draw_tile(ui.ctx(), &painter, rect, &m, albums, i, off, None, alpha);
        }
        if self.selected >= 0
            && (self.selected as usize) < albums.len()
            && !dragged
                .as_ref()
                .map(|d| d.indices.binary_search(&(self.selected as usize)).is_ok())
                .unwrap_or(false)
        {
            let i = self.selected as usize;
            if i >= first && i <= last {
                let off = if dragged.is_some() {
                    self.drag
                        .as_ref()
                        .and_then(|d| d.swap_offsets.get(&i).copied())
                        .unwrap_or(0.0)
                } else {
                    0.0
                };
                self.draw_tile(ui.ctx(), &painter, rect, &m, albums, i, off, None, alpha);
            }
        }
        // The dragged block floats on top (DrawDraggedTile): the
        // primary is pointer-glued at DRAG_LIFT, the rest ride
        // contiguous beside it, all gliding home during commit.
        if let Some(d) = dragged.as_ref() {
            let stride = m.tile_w + SPACING * s;
            let pr = d
                .indices
                .iter()
                .position(|&i| i == d.index)
                .unwrap_or(0);
            let glue_e = (d.glue.min(1.0) * std::f32::consts::FRAC_PI_2).sin();
            let bs = Self::block_start(d, albums.len());
            let prim_center = self.tile_rect(rect, &m, d.index).center();
            for (k, &i) in d.indices.iter().enumerate() {
                let glued = Vec2::new((k as f32 - pr as f32) * stride, 0.0);
                let (center, lift) = match &d.commit {
                    Some(c) => {
                        let t = 1.0 - (1.0 - c.progress.clamp(0.0, 1.0)).powi(3);
                        let target =
                            self.tile_rect(rect, &m, bs + k).center();
                        (
                            c.start + c.offs[k] + (target - c.start - c.offs[k]) * t,
                            DRAG_LIFT + (1.0 - DRAG_LIFT) * t,
                        )
                    }
                    None => {
                        let home =
                            self.tile_rect(rect, &m, i).center() - prim_center;
                        (d.center + home + (glued - home) * glue_e, DRAG_LIFT)
                    }
                };
                self.draw_tile(
                    ui.ctx(),
                    &painter,
                    rect,
                    &m,
                    albums,
                    i,
                    0.0,
                    Some((center, lift)),
                    alpha,
                );
            }
        }

        // Inline title edit (Create Album / Rename Album): a TextEdit
        // sits over the tile's title band until Enter/click-away commit
        // or Esc cancels. It goes through `ui.put`, so clicks inside
        // the field reach the editor, not the tile's hit test below.
        if interactive {
            if let Some(dir) = self.editing.clone() {
                if let Some(i) = albums
                    .iter()
                    .position(|a| a.dir.to_string_lossy() == dir)
                {
                    let base = self.tile_rect(rect, &m, i);
                    let cover_h = m.tile_h - TITLE_BAR_H * scale;
                    let title_rect = Rect::from_min_max(
                        Pos2::new(base.min.x, base.min.y + cover_h),
                        base.max,
                    );
                    let edit_rect = Rect::from_center_size(
                        title_rect.center(),
                        Vec2::new(
                            (title_rect.width() - 12.0 * scale).max(48.0),
                            26.0 * scale,
                        ),
                    );
                    let r = ui.put(
                        edit_rect,
                        egui::TextEdit::singleline(&mut self.edit_text)
                            .font(FontId::proportional(18.0 * scale))
                            .text_color(Color32::WHITE),
                    );
                    if !self.edit_armed {
                        // Focus once with the whole title selected —
                        // typing replaces it outright.
                        let mut st = egui::text_edit::TextEditState::load(
                            ui.ctx(),
                            r.id,
                        )
                        .unwrap_or_default();
                        st.cursor.set_char_range(Some(
                            egui::text::CCursorRange::two(
                                egui::text::CCursor::new(0),
                                egui::text::CCursor::new(
                                    self.edit_text.chars().count(),
                                ),
                            ),
                        ));
                        st.store(ui.ctx(), r.id);
                        r.request_focus();
                        self.edit_armed = true;
                    }
                    let esc = ui
                        .ctx()
                        .input(|i| i.key_pressed(egui::Key::Escape))
                        && r.has_focus();
                    if esc {
                        self.editing = None;
                    } else if r.lost_focus() {
                        let text = self.edit_text.trim().to_string();
                        if !text.is_empty() {
                            action = Action::Rename(dir, text);
                        }
                        self.editing = None;
                    }
                } else {
                    self.editing = None;
                }
            }
        }

        // Scrollbar thumb (spring-faded, blurred pill in Lacrima; two
        // layered rects approximate the glow).
        if self.scrollbar_opacity > 0.01 && m.max_scroll > 1.0 {
            let a = self.scrollbar_opacity * alpha;
            painter.rect_filled(
                thumb.expand2(Vec2::new(2.0 * s, s)),
                4.0 * s,
                Color32::WHITE.gamma_multiply(0.25 * a),
            );
            painter.rect_filled(thumb, 4.0 * s, Color32::WHITE.gamma_multiply(0.7 * a));
        }

        // Tab focus-in flash — a soft border around the strip that
        // eases out over ~0.9s, marking where the keys now land.
        if self.focus_flash > 0.0 {
            self.focus_flash = (self.focus_flash - dt / 0.9).max(0.0);
            let f = self.focus_flash * self.focus_flash * alpha;
            let fr = rect.shrink(2.0 * s);
            painter.rect_stroke(
                fr,
                8.0 * s,
                Stroke::new(6.0 * s, Color32::WHITE.gamma_multiply(0.10 * f)),
                egui::StrokeKind::Inside,
            );
            painter.rect_stroke(
                fr,
                8.0 * s,
                Stroke::new(2.0 * s, Color32::WHITE.gamma_multiply(0.55 * f)),
                egui::StrokeKind::Inside,
            );
            ui.ctx().request_repaint();
        }

        if interactive {
            resp.context_menu(|ui| self.context_menu(ui, &mut action, albums));
        }
        action
    }

    /// Keep the selection sane after an album was removed from the
    /// list (`DeleteFolder`). `selected` indexes the last SHOWN list
    /// (`last_dirs`), so the removal is tracked in that space — and
    /// the snapshot drops the dir too, keeping multi-deletes in one
    /// frame consistent.
    pub fn removed(&mut self, dir: &str) {
        if let Some(r) = self.last_dirs.iter().position(|d| d == dir) {
            self.last_dirs.remove(r);
            if self.selected == r as i32 {
                self.selected = -1;
            } else if self.selected > r as i32 {
                self.selected -= 1;
            }
        }
    }

    /// Drop `dir` from the multi-selection (it left the list).
    pub fn deselect_dir(&mut self, dir: &str) {
        self.sel_dirs.remove(dir);
        if self.sel_anchor == dir {
            self.sel_anchor.clear();
        }
    }

    /// Arrow-key navigation (ListBox directional nav on a horizontal
    /// strip — every arrow steps ±1): collapse the selection to the
    /// neighbor tile and glide it into view. Steps through the last
    /// SHOWN list (`last_dirs`) — under an active filter that's the
    /// filtered order, matching the tiles on screen.
    pub fn arrow(&mut self, dir: i32) {
        if self.last_dirs.is_empty() {
            return;
        }
        let n = self.last_dirs.len() as i32;
        // No selection yet: the first arrow enters at an edge.
        let base = if self.selected >= 0 {
            self.selected
        } else if dir > 0 {
            -1
        } else {
            n
        };
        let new = (base + dir).clamp(0, n - 1);
        if new == self.selected && !self.sel_dirs.is_empty() {
            return;
        }
        self.selected = new;
        let dir_key = self.last_dirs[new as usize].clone();
        self.sel_dirs.clear();
        self.sel_dirs.insert(dir_key.clone());
        self.sel_anchor = dir_key;
        self.sel_touched = true;
        self.scroll_tile_into_view(new);
    }

    /// Tab focus-in — ensure a selection exists (first shown tile when
    /// none) and reveal it.
    pub fn focus(&mut self) {
        if self.last_dirs.is_empty() {
            return;
        }
        if self.selected < 0 {
            self.selected = 0;
            let dir = self.last_dirs[0].clone();
            self.sel_dirs.insert(dir.clone());
            self.sel_anchor = dir;
        }
        self.sel_touched = true;
        self.focus_flash = 1.0;
        self.scroll_tile_into_view(self.selected);
    }

    /// Shift+arrow — move the selection one step along the strip
    /// (the drag-reorder's keyboard twin: the selection moves as a
    /// contiguous run, `note_reorder` keeps fans/selection aligned).
    pub fn move_selection(&mut self, albums: &mut Vec<Album>, dir: i32) {
        let count = albums.len();
        // A filtered strip renders a clone — there is no meaningful
        // reorder target, so the move is dropped rather than corrupting
        // the real list at shown-space indices.
        if dir == 0 || count < 2 || self.selected < 0 || self.filtered {
            return;
        }
        // Selected tile indices (dir-keyed selection → positions).
        let indices: Vec<usize> = if self.sel_dirs.is_empty() {
            vec![self.selected as usize]
        } else {
            albums
                .iter()
                .enumerate()
                .filter(|(_, a)| self.sel_dirs.contains(a.dir.to_string_lossy().as_ref()))
                .map(|(i, _)| i)
                .collect()
        };
        if indices.is_empty() {
            return;
        }
        let k = indices.len();
        let bs = (indices[0] as i32 + dir)
            .clamp(0, count as i32 - k as i32) as usize;
        if indices
            .iter()
            .enumerate()
            .all(|(j, &i)| i == bs + j)
        {
            return;
        }
        let pr = indices
            .iter()
            .position(|&i| i == self.selected as usize)
            .unwrap_or(0);
        let mut pulled: Vec<Album> = Vec::with_capacity(k);
        for &i in indices.iter().rev() {
            pulled.push(albums.remove(i));
        }
        pulled.reverse();
        for (j, a) in pulled.into_iter().enumerate() {
            albums.insert(bs + j, a);
        }
        self.note_reorder(&indices, bs, pr);
        self.order_dirty = true;
        self.scroll_tile_into_view(self.selected);
    }

    /// Spring the strip just enough for tile `index` to be fully
    /// visible (same math as pending_reveal, eased via target_scroll
    /// instead of snapping).
    fn scroll_tile_into_view(&mut self, index: i32) {
        if let Some((rect, m)) = self.nav_view.clone() {
            let stride = m.tile_w + SPACING * self.scale;
            let pad = PAD_X * self.scale;
            let x0 = pad + index as f32 * stride;
            let x1 = x0 + m.tile_w;
            let w = rect.width();
            let target = if x0 < self.target_scroll {
                x0 - pad
            } else if x1 > self.target_scroll + w {
                x1 + pad - w
            } else {
                self.target_scroll
            };
            self.target_scroll = target.clamp(0.0, m.max_scroll);
        }
    }

    /// Ctrl+A — every shown album selected (keyed by dir, like the
    /// click gestures, so filtering/reordering can't shift members).
    /// Under an active filter that's the filtered set — the same
    /// semantics as a filtered ListBox.
    pub fn select_all(&mut self) {
        self.sel_dirs = self.last_dirs.iter().cloned().collect();
        self.sel_anchor = self.last_dirs.first().cloned().unwrap_or_default();
        if !self.sel_dirs.is_empty() {
            self.sel_touched = true;
        }
    }

    /// The primary selection's album dir — Enter opens it like a
    /// double-click. `selected` indexes the last SHOWN list, so the
    /// dir comes from `last_dirs`, never a caller-side index (the
    /// filtered-open regression: an index into the filtered clone
    /// resolved against the full list opened a different album).
    pub fn selected_dir(&self) -> Option<String> {
        if self.selected < 0 {
            return None;
        }
        self.last_dirs.get(self.selected as usize).cloned()
    }

    /// A user selection gesture happened since the last check — the
    /// app uses it to give the row Delete-key ownership.
    pub fn take_sel_touched(&mut self) -> bool {
        std::mem::take(&mut self.sel_touched)
    }

    /// Selected albums' dirs in shown order (the Delete key payload).
    pub fn selected_dirs(&self) -> Vec<String> {
        self.last_dirs
            .iter()
            .filter(|d| self.sel_dirs.contains(d.as_str()))
            .cloned()
            .collect()
    }

    fn context_menu(&mut self, ui: &mut egui::Ui, action: &mut Action, albums: &[Album]) {
        if ui.button("Add music folder").clicked() {
            *action = Action::AddFolder;
            ui.close();
        }
        if ui.button("Rescan library").clicked() {
            *action = Action::Rescan;
            ui.close();
        }
        if ui.button("Import Playlist").clicked() {
            *action = Action::Import;
            ui.close();
        }
        // Every url-holding album exports its own .apl into a picked
        // folder — independent of what happens to be selected.
        if albums.iter().any(|a| a.url_count > 0)
            && ui.button("Export All Playlists").clicked()
        {
            *action = Action::ExportAll;
            ui.close();
        }
        if ui.button("Create Album").clicked() {
            *action = Action::Create;
            ui.close();
        }
        if let Some(i) = self.menu_album {
            ui.separator();
            if ui.button("Rename Album").clicked() {
                let dir = albums[i].dir.to_string_lossy().into_owned();
                let title = albums[i].title.clone();
                self.edit_album(&dir, &title);
                self.menu_album = None;
                ui.close();
            }
            if albums.get(i).map(|a| a.url_count > 0).unwrap_or(false) {
                let hit = albums[i].dir.to_string_lossy().into_owned();
                let multi = self.sel_dirs.contains(hit.as_str()) && self.sel_dirs.len() > 1;
                let label = if multi { "Export Playlists" } else { "Export Playlist" };
                if ui.button(label).clicked() {
                    // Pointing at a member of a multi-selection exports
                    // the whole selection (Delete's rule); .apl only
                    // holds URL items, so non-url albums are skipped.
                    let dirs: Vec<String> = if multi {
                        albums
                            .iter()
                            .filter(|a| {
                                a.url_count > 0
                                    && self.sel_dirs.contains(a.dir.to_string_lossy().as_ref())
                            })
                            .map(|a| a.dir.to_string_lossy().into_owned())
                            .collect()
                    } else {
                        vec![hit]
                    };
                    *action = Action::Export(dirs);
                    self.menu_album = None;
                    ui.close();
                }
            }
            if ui.button("Delete Album").clicked() {
                *action = Action::DeleteAlbum(vec![
                    albums[i].dir.to_string_lossy().into_owned(),
                ]);
                self.menu_album = None;
                ui.close();
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_tile(
        &mut self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        row: Rect,
        m: &Metrics,
        albums: &[Album],
        index: usize,
        offset_x: f32,
        placement: Option<(Pos2, f32)>,
        alpha: f32,
    ) {
        let album = &albums[index];
        let base = self
            .tile_rect(row, m, index)
            .translate(Vec2::new(offset_x, 0.0));
        // Selection visuals: every album in the multi-selection gets
        // the ring + lift. `selected` alone only marks the primary
        // when no multi-selection is populated (legacy single path) —
        // a Ctrl-toggled-off tile must not keep the ring.
        let dir_key = album.dir.to_string_lossy();
        let is_selected = self.sel_dirs.contains(dir_key.as_ref())
            || (self.sel_dirs.is_empty() && index as i32 == self.selected);
        let fade = if placement.is_none() && is_selected {
            self.selection_fade
        } else {
            0.0
        };
        // `placement` is the dragged tile's (center, scale) override;
        // normal tiles derive both from their slot + hover/selection.
        let (center, scale) = if let Some(p) = placement {
            p
        } else {
            let lift = self.hover_lift.get(&index).copied().unwrap_or((0.0, 0.0)).0;
            let eased = 1.0 - (1.0 - lift).powi(3);
            // Shift held — the selected tiles float slightly off the
            // strip, signaling they can be reordered with arrows.
            let pe = if is_selected { self.pop } else { 0.0 };
            (
                base.center() + Vec2::new(0.0, -4.0 * pe * self.scale),
                (if is_selected {
                    1.0 + SELECT_LIFT * fade
                } else {
                    1.0 + HOVER_LIFT * eased
                }) * (1.0 + 0.06 * pe),
            )
        };
        let tile = Rect::from_center_size(center, base.size() * scale);
        let pe = if is_selected && placement.is_none() {
            self.pop
        } else {
            0.0
        };
        if pe > 0.001 {
            painter.rect_filled(
                tile.translate(Vec2::new(0.0, 8.0 * self.scale)),
                TILE_RADIUS * self.scale,
                Color32::BLACK.gamma_multiply(0.3 * pe * alpha),
            );
        }

        painter.rect_filled(tile, TILE_RADIUS * self.scale, TILE_BG.gamma_multiply(alpha));
        painter.rect_stroke(
            tile,
            TILE_RADIUS * self.scale,
            Stroke::new(2.0 * self.scale, TILE_BORDER.gamma_multiply(alpha)),
            egui::StrokeKind::Inside,
        );

        // Cover fan: covers clipped to the cover area (rect clip is
        // fine — the rounded corners come from each cover mesh itself).
        // Titles off: the cover claims the band's strip too — the
        // title bar becomes a hover-revealed overlay on its bottom
        // edge instead of a layout region.
        let band_h = TITLE_BAR_H * self.scale * scale;
        let cover_h = if self.show_titles {
            m.tile_h * scale - band_h
        } else {
            m.tile_h * scale
        };
        let cover_rect = Rect::from_min_size(tile.min, Vec2::new(tile.width(), cover_h));
        let item_size = tile.width().max(cover_h);
        let base_x = tile.width() - item_size;
        let spread = self.fans.get(index).map(|f| f.spread).unwrap_or(0.0);
        let cover_painter = painter.with_clip_rect(cover_rect);
        // Titles off: dest matches the cover region so the mesh's
        // rounded bottom corners land on the tile's bottom edge — a
        // square dest would clip the arcs into a straight cut.
        let dest_h = if self.show_titles { item_size } else { cover_h };
        // Resolve the album's top cover first: fan layers without their
        // own art draw the album cover (Lacrima's UseFolderCover layers),
        // not the note placeholder.
        let n = album.covers.len().min(MAX_COVERS);
        let top_tex = album.covers.get(n - 1).and_then(|src| {
            match self.textures.get(src) {
                Some((CoverTex::Ready(t), _)) => Some(t.clone()),
                _ => None,
            }
        });
        let placeholder = self.placeholder_tex(ctx);
        let mut draw_i = 0usize;
        for (i, src) in album.covers.iter().take(n).enumerate() {
            let own = match self.textures.get(src) {
                Some((CoverTex::Ready(t), _)) => Some(t.clone()),
                _ => None,
            };
            let tex = own
                .or_else(|| top_tex.clone())
                .unwrap_or_else(|| placeholder.clone());
            // Layers that can only render the shared placeholder collapse
            // into the top cover — identical note images fanning out read
            // as rendering artifacts.
            if i + 1 != n && tex.id() == placeholder.id() {
                continue;
            }
            // Last layer is the top cover; bottom layers stay put.
            let x = base_x - draw_i as f32 * FAN_MARGIN * item_size * spread;
            draw_i += 1;
            let dest = Rect::from_min_size(
                Pos2::new(cover_rect.min.x + x, cover_rect.min.y),
                Vec2::new(item_size, dest_h),
            );
            paint::paint_cover(
                &cover_painter,
                tex.id(),
                tex.size(),
                dest,
                COVER_RADIUS * self.scale,
                alpha,
            );
        }
        // No cover sources at all (empty created album, artless
        // folder) — the note placeholder fills the cover area.
        if n == 0 {
            let dest = Rect::from_min_size(
                Pos2::new(cover_rect.min.x + base_x, cover_rect.min.y),
                Vec2::new(item_size, dest_h),
            );
            paint::paint_cover(
                &cover_painter,
                placeholder.id(),
                placeholder.size(),
                dest,
                COVER_RADIUS * self.scale,
                alpha,
            );
        }

        // Title bar + album title (centered, wrapped, clipped). With
        // titles off it overlays the cover's bottom edge, faded in
        // per-tile while hovered (`title_fade`).
        let title_rect =
            Rect::from_min_max(Pos2::new(tile.min.x, tile.max.y - band_h), tile.max);
        let title_a = if self.show_titles {
            alpha
        } else {
            alpha * self
                .title_fade
                .get(dir_key.as_ref())
                .copied()
                .unwrap_or(0.0)
        };
        if title_a > 0.001 {
            // Overlay bar: bottom corners rounded to the tile's radius
            // so it doesn't poke past the cover's rounded arcs.
            let bar_cr = if self.show_titles {
                egui::CornerRadius::ZERO
            } else {
                let r = (TILE_RADIUS * self.scale).round() as u8;
                egui::CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: r,
                    se: r,
                }
            };
            painter.rect_filled(title_rect, bar_cr, TITLE_BG.gamma_multiply(title_a));
        }
        // While the inline editor is open on this tile the painted
        // title stays out — it would bleed around the TextEdit's frame.
        let title_editing =
            self.editing.as_deref() == Some(album.dir.to_string_lossy().as_ref());
        if !title_editing && title_a > 0.001 {
            // Wrap to at most 2 rows with an ellipsis tail.
            let mut title_job = egui::text::LayoutJob::single_section(
                album.title.clone(),
                egui::TextFormat::simple(
                    FontId::proportional(18.0 * self.scale),
                    Color32::WHITE.gamma_multiply(title_a),
                ),
            );
            title_job.wrap = egui::text::TextWrapping {
                max_width: tile.width() - 16.0 * self.scale,
                max_rows: 2,
                break_anywhere: true,
                overflow_character: Some('…'),
            };
            let title_galley = painter.layout_job(title_job);
            let tp = Pos2::new(
                tile.center().x - title_galley.size().x * 0.5,
                title_rect.center().y - title_galley.size().y * 0.5,
            );
            painter.with_clip_rect(title_rect).galley(tp, title_galley, Color32::WHITE);
        }

        // Track-count badge, top-right — files + url items (Lacrima
        // counts all album Children, not just disk tracks).
        let count = album.tracks.len() + album.url_count;
        if count > 0 {
            let text = count.to_string();
            let font = FontId::proportional(20.0 * self.scale);
            let tw = painter
                .layout_no_wrap(text.clone(), font.clone(), Color32::WHITE)
                .size()
                .x;
            let badge_w = (24.0 * self.scale).max(tw + 12.0 * self.scale);
            let badge = Rect::from_min_size(
                Pos2::new(
                    tile.max.x - badge_w - 12.0 * self.scale,
                    tile.min.y + 12.0 * self.scale,
                ),
                Vec2::new(badge_w, 24.0 * self.scale),
            );
            painter.rect_filled(
                badge,
                12.0 * self.scale,
                BADGE_BG.gamma_multiply(0.7 * alpha),
            );
            painter.text(
                badge.center(),
                Align2::CENTER_CENTER,
                text,
                font,
                Color32::WHITE.gamma_multiply(alpha),
            );
        }

        // Selection ring: crisp white border, drawn un-clipped so it can
        // bleed outside the tile (Lacrima draws it after Restore).
        if is_selected && fade > 0.001 {
            painter.rect_stroke(
                tile,
                TILE_RADIUS * self.scale,
                Stroke::new(2.1 * self.scale, Color32::WHITE.gamma_multiply(0.9 * fade * alpha)),
                egui::StrokeKind::Inside,
            );
        }
    }
}

/// Dashed outline for the drop-slot hole — egui has no path effects,
/// so the dashes are segments along each edge (`dash` on, `dash` off).
fn dashed_rect(painter: &egui::Painter, rect: Rect, dash: f32, width: f32, color: Color32) {
    let stroke = Stroke::new(width, color);
    for (a, b) in [
        (rect.min, Pos2::new(rect.max.x, rect.min.y)),
        (Pos2::new(rect.max.x, rect.min.y), rect.max),
        (rect.max, Pos2::new(rect.min.x, rect.max.y)),
        (Pos2::new(rect.min.x, rect.max.y), rect.min),
    ] {
        let len = (b - a).length();
        if len < 1.0 {
            continue;
        }
        let dir = (b - a) / len;
        let mut t = 0.0;
        while t < len {
            let seg = (t + dash).min(len);
            painter.line_segment([a + dir * t, a + dir * seg], stroke);
            t += dash * 2.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The filtered-open regression: `selected` indexes the SHOWN
    /// (possibly filter-cloned) list — Enter must resolve through
    /// `last_dirs` to a dir, so a filtered strip opens the album the
    /// user pointed at, not the full-list item at the same index.
    #[test]
    fn selected_dir_resolves_shown_space() {
        let mut row = AlbumRow::new();
        // Full list a,b,c — selected the second tile.
        row.last_dirs = vec!["a".into(), "b".into(), "c".into()];
        row.selected = 1;
        assert_eq!(row.selected_dir().as_deref(), Some("b"));
        // Filtered down to ["c"]: index 0 must yield "c", not "a".
        row.last_dirs = vec!["c".into()];
        row.selected = 0;
        assert_eq!(row.selected_dir().as_deref(), Some("c"));
        // Out of range (filter shrank past the selection) — None,
        // never a wrong-album index.
        row.selected = 5;
        assert_eq!(row.selected_dir(), None);
        row.selected = -1;
        assert_eq!(row.selected_dir(), None);
    }

    /// Deletion bookkeeping runs in shown space too — `removed(dir)`
    /// shifts `selected` against `last_dirs` so several removes in one
    /// frame stay consistent.
    #[test]
    fn removed_shifts_selection_in_shown_space() {
        let mut row = AlbumRow::new();
        row.last_dirs = vec!["a".into(), "b".into(), "c".into()];
        row.selected = 2; // "c" highlighted
        row.removed("a"); // removed before the selection -> shift left
        assert_eq!(row.selected, 1);
        assert_eq!(row.selected_dir().as_deref(), Some("c"));
        row.removed("b");
        assert_eq!(row.selected, 0);
        assert_eq!(row.selected_dir().as_deref(), Some("c"));
        row.removed("c"); // the selected album itself leaves
        assert_eq!(row.selected, -1);
        assert_eq!(row.selected_dir(), None);
    }

    /// Filtered strips can't reorder — the real list isn't the shown
    /// one, so both reorder paths are gated on `filtered`.
    #[test]
    fn move_selection_drops_while_filtered() {
        let mut row = AlbumRow::new();
        row.last_dirs = vec!["a".into(), "b".into(), "c".into()];
        row.selected = 0;
        row.sel_dirs.insert("a".into());
        let mut full: Vec<Album> = vec![
            Album::placeholder("a"),
            Album::placeholder("b"),
            Album::placeholder("c"),
        ];
        row.filtered = true;
        row.move_selection(&mut full, 1);
        assert!(!row.order_dirty);
        assert_eq!(
            full.iter()
                .map(|a| a.dir.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        // Unfiltered, the same move lands (`fans` mirrors the list —
        // ui() maintains that; the test seeds it directly).
        row.filtered = false;
        row.fans = (0..full.len()).map(|_| Fan::default()).collect();
        row.move_selection(&mut full, 1);
        assert!(row.order_dirty);
        assert_eq!(
            full[1].dir.to_string_lossy().as_ref(),
            "a"
        );
    }

    /// Title-bar reveal: pinned at 1 while `show_titles` is on;
    /// off, each tile's bar eases in only while it's the hovered one
    /// and fully-hidden entries get pruned (dir-keyed).
    #[test]
    fn title_fades_follow_show_and_hover() {
        let mut row = AlbumRow::new();
        row.last_dirs = vec!["a".into(), "b".into()];
        assert!(row.show_titles);
        // On: every shown tile's fade pins at 1.
        for _ in 0..30 {
            row.title_fade_step(0.016);
        }
        assert!(row.title_fade.values().all(|&v| v > 0.99));
        // Off + hovering "b": "a" fades out to pruning, "b" stays lit.
        row.show_titles = false;
        row.hovered = 1;
        assert!(row.title_fade_step(0.016)); // still moving
        for _ in 0..60 {
            row.title_fade_step(0.016);
        }
        assert!(row.title_fade.get("a").is_none());
        assert!(*row.title_fade.get("b").unwrap() > 0.99);
        // Back on: hidden tiles fade IN — reinserted at 0 only when
        // they're unhovered... under `show_titles` the insert seed is
        // 1 so a mid-list tile never blinks black.
        row.show_titles = true;
        row.hovered = -1;
        for _ in 0..60 {
            row.title_fade_step(0.016);
        }
        assert!(row.title_fade.values().all(|&v| v > 0.99));
        assert!(!row.title_fade_step(0.016)); // settled -> no repaint
    }
}
