//! Re/Decompilation view — a catalog of decomp, recomp, source-port
//! and tool projects, rendered in the AES card-grid language.
//!
//! The catalog itself is rebuilt from the Recompendium dataset
//! (`catalog.rs`): one zipball fetch → normalized `catalog.json`
//! under `<data>/Decomps/` → entries + fresh per-repo stats
//! (`pushedAt`, release tag, stars) with zero extra API calls. The
//! built-in `PROJECTS` manifest below stays as the offline fallback.
//!
//! Installs live in `<data>/Decomps/<slug>`; `.oxide_pushed` (written
//! at download time) vs the catalog's `pushedAt` is the update badge.
//! Source is copied OVER the existing tree — never a wipe — so a
//! baserom or build outputs parked inside survive an update.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Instant;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, Pos2, Rect, Sense, Stroke,
    StrokeKind, Vec2,
};

use crate::album_row::AlbumRow;
use crate::carousel::{self, Mode};
use crate::emulation::catalog::{self, Entry, Kind};
use crate::emulation::{cap_shader, xenia};
use crate::library::CoverSrc;
use crate::search_pill::SearchPill;
use crate::settings;

/// The install marker written next to the extracted source — holds
/// the remote `pushed_at` seen at download time.
const MARKER: &str = ".oxide_pushed";
const USER_AGENT: &str = "AES_Oxide-DecompCatalog/1.0";

/// One fallback-manifest entry — `repo` is `owner/repo` on GitHub.
pub struct Project {
    pub slug: &'static str,
    pub name: &'static str,
    pub repo: &'static str,
    pub console: &'static str,
    pub kind: Kind,
    pub blurb: &'static str,
    /// The box-art search phrase — the game's retail cover, not the
    /// repo's (which rarely has one).
    pub cover_q: &'static str,
}

/// Offline fallback — big-name, actively-maintained projects shown
/// while the upstream catalog is missing or unfetchable.
pub const PROJECTS: &[Project] = &[
    Project {
        slug: "sm64",
        name: "Super Mario 64",
        repo: "n64decomp/sm64",
        console: "N64",
        kind: Kind::Decomp,
        blurb: "The original N64 decomp — every PC port descends from it.",
        cover_q: "Super Mario 64 box art",
    },
    Project {
        slug: "oot",
        name: "Ocarina of Time",
        repo: "zeldaret/oot",
        console: "N64",
        kind: Kind::Decomp,
        blurb: "Matching decomp of The Legend of Zelda: Ocarina of Time.",
        cover_q: "Zelda Ocarina of Time box art",
    },
    Project {
        slug: "mm",
        name: "Majora's Mask",
        repo: "zeldaret/mm",
        console: "N64",
        kind: Kind::Decomp,
        blurb: "Matching decomp of Majora's Mask — feeds the N64 recomp.",
        cover_q: "Zelda Majora's Mask box art",
    },
    Project {
        slug: "tp",
        name: "Twilight Princess",
        repo: "zeldaret/tp",
        console: "GCN",
        kind: Kind::Decomp,
        blurb: "Matching decomp of Twilight Princess (GameCube).",
        cover_q: "Zelda Twilight Princess box art",
    },
    Project {
        slug: "banjo",
        name: "Banjo-Kazooie",
        repo: "n64decomp/banjo-kazooie",
        console: "N64",
        kind: Kind::Decomp,
        blurb: "Matching decomp of Banjo-Kazooie.",
        cover_q: "Banjo-Kazooie box art",
    },
    Project {
        slug: "perfect_dark",
        name: "Perfect Dark",
        repo: "n64decomp/perfect_dark",
        console: "N64",
        kind: Kind::Decomp,
        blurb: "Matching decomp of Rare's Perfect Dark.",
        cover_q: "Perfect Dark box art",
    },
    Project {
        slug: "papermario",
        name: "Paper Mario",
        repo: "pmret/papermario",
        console: "N64",
        kind: Kind::Decomp,
        blurb: "Matching decomp of Paper Mario.",
        cover_q: "Paper Mario box art",
    },
    Project {
        slug: "jak",
        name: "Jak and Daxter",
        repo: "open-goal/jak-project",
        console: "PS2",
        kind: Kind::Decomp,
        blurb: "GOAL decomp + native PC ports of the Jak trilogy.",
        cover_q: "Jak and Daxter box art",
    },
    Project {
        slug: "pokered",
        name: "Pokémon Red & Blue",
        repo: "pret/pokered",
        console: "GB",
        kind: Kind::Disasm,
        blurb: "The pret disassembly that started it all.",
        cover_q: "Pokemon Red box art",
    },
    Project {
        slug: "pokecrystal",
        name: "Pokémon Crystal",
        repo: "pret/pokecrystal",
        console: "GBC",
        kind: Kind::Disasm,
        blurb: "pret's disassembly of Pokémon Crystal.",
        cover_q: "Pokemon Crystal box art",
    },
    Project {
        slug: "sonic-cd",
        name: "Sonic CD (2011)",
        repo: "Rubberduckycooly/Sonic-CD-11-Decompilation",
        console: "Multi",
        kind: Kind::Decomp,
        blurb: "RSDK decomp of the Sonic CD remaster.",
        cover_q: "Sonic CD box art",
    },
    Project {
        slug: "sonic-1-2",
        name: "Sonic 1 & 2 (2013)",
        repo: "Rubberduckycooly/Sonic-1-2-2013-Decompilation",
        console: "Multi",
        kind: Kind::Decomp,
        blurb: "RSDK decomp of the Sonic 1 & 2 remasters.",
        cover_q: "Sonic the Hedgehog 2 box art",
    },
    Project {
        slug: "sonic-mania",
        name: "Sonic Mania",
        repo: "Rubberduckycooly/Sonic-Mania-Decompilation",
        console: "Multi",
        kind: Kind::Decomp,
        blurb: "RSDKv5 decomp of Sonic Mania.",
        cover_q: "Sonic Mania box art",
    },
    Project {
        slug: "devilution",
        name: "Diablo (devilution)",
        repo: "diasurgical/devilution",
        console: "PC",
        kind: Kind::Decomp,
        blurb: "The reconstructed Diablo source tree.",
        cover_q: "Diablo 1996 box art",
    },
    Project {
        slug: "devilutionx",
        name: "DevilutionX",
        repo: "diasurgical/devilutionX",
        console: "PC",
        kind: Kind::Port,
        blurb: "The playable port — ships actual binaries.",
        cover_q: "Diablo 1996 box art",
    },
    Project {
        slug: "fallout1-ce",
        name: "Fallout 1 CE",
        repo: "alexbatalov/fallout1-ce",
        console: "PC",
        kind: Kind::Decomp,
        blurb: "Community edition decomp of Fallout.",
        cover_q: "Fallout 1997 box art",
    },
    Project {
        slug: "fallout2-ce",
        name: "Fallout 2 CE",
        repo: "alexbatalov/fallout2-ce",
        console: "PC",
        kind: Kind::Decomp,
        blurb: "Community edition decomp of Fallout 2.",
        cover_q: "Fallout 2 box art",
    },
    Project {
        slug: "dethrace",
        name: "Carmageddon",
        repo: "dethrace-labs/dethrace",
        console: "PC",
        kind: Kind::Decomp,
        blurb: "Reverse-engineered Carmageddon — dethrace.",
        cover_q: "Carmageddon box art",
    },
    Project {
        slug: "z64recomp",
        name: "Majora's Mask Recompiled",
        repo: "Zelda64Recomp/Zelda64Recomp",
        console: "PC",
        kind: Kind::Recomp,
        blurb: "N64-to-native static recompilation of Majora's Mask.",
        cover_q: "Zelda Majora's Mask box art",
    },
];

/// The fallback manifest as catalog entries.
fn fallback_entries() -> Vec<Entry> {
    PROJECTS
        .iter()
        .map(|p| Entry {
            slug: p.slug.to_string(),
            name: p.name.to_string(),
            kind: p.kind,
            console: p.console.to_lowercase(),
            status: String::new(),
            repo: p.repo.to_string(),
            homepage: String::new(),
            developer: String::new(),
            toolchain: String::new(),
            fork_label: String::new(),
            fork_url: String::new(),
            description: p.blurb.to_string(),
            requirements: String::new(),
            tags: Vec::new(),
            featured: false,
            cover_q: p.cover_q.to_string(),
            stars: None,
            pushed_at: String::new(),
            version: String::new(),
            license: String::new(),
            language: String::new(),
            year: None,
            progress: None,
            abandoned: false,
            takedown: false,
            author: String::new(),
            game: String::new(),
            links: Vec::new(),
            search_blob: String::new(),
        })
        .collect()
}

/// Lowercased haystack for `Entry::matches` — built once when the
/// catalog lands in the view (fetch, cache load, or fallback).
fn prepare_entries(entries: &mut [Entry]) {
    for e in entries.iter_mut() {
        if e.cover_q.is_empty() {
            e.cover_q = format!("{} box art", e.name);
        }
        e.search_blob = format!(
            "{} {} {} {} {} {} {} {} {}",
            e.name,
            e.repo,
            e.console,
            catalog::console_label(&e.console),
            e.developer,
            e.toolchain,
            e.author,
            e.game,
            e.tags.join(" ")
        )
        .to_lowercase();
    }
}

/// `<data>/Decomps/<slug>` — installed project trees live here.
pub fn decomps_root() -> Option<PathBuf> {
    crate::settings::data_root().map(|d| d.join("Decomps"))
}

fn install_dir(slug: &str) -> Option<PathBuf> {
    decomps_root().map(|d| d.join(slug))
}

/// The `pushed_at` the installed copy was downloaded at, if installed.
/// The marker's second line carries the release tag (or `source`) —
/// `installed_stamp` reads line 1 only so both stay independent.
fn installed_stamp(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(MARKER))
        .ok()
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
        .filter(|s| !s.is_empty())
}

/// The release tag a binary install came from (`source` installs and
/// missing markers both return `None`).
fn installed_release(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(MARKER))
        .ok()
        .and_then(|s| {
            let t = s.lines().nth(1)?.trim();
            (!t.is_empty() && t != "source").then(|| t.to_string())
        })
}

/// Remote facts the update check collects per project.
#[derive(Clone, Default)]
pub(crate) struct RemoteInfo {
    /// ISO-8601 UTC — lexically comparable for update checks.
    pushed_at: String,
    err: Option<String>,
}

/// What a card's action button does right now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Not installed — download the release/source.
    Install,
    /// Installed, remote is newer — re-download.
    Update,
    /// Installed and current — open the folder.
    Open,
}

/// The card's resolved action: installed marker + remote check in,
/// button label out. `pushed_at` strings are ISO-8601 so a lexical
/// compare is a real ordering. A failed check never dead-ends the
/// card — Install will surface its own error through the worker.
pub fn card_action(installed: Option<&str>, remote: Option<&RemoteInfo>) -> Action {
    match installed {
        None => Action::Install,
        Some(_) => match remote {
            Some(r) if r.err.is_none() && !r.pushed_at.is_empty() => {
                if r.pushed_at.as_str() > installed.unwrap_or("") {
                    Action::Update
                } else {
                    Action::Open
                }
            }
            // No remote data (unchecked or errored) — installed is
            // installed; offer the folder.
            _ => Action::Open,
        },
    }
}

/// Result of a catalog fetch/cache-load, delivered to the view.
type CatalogResult = Result<catalog::Catalog, String>;

/// Maturity chip rows in the filter drawer — the raw upstream
/// `status` strings each chip matches (`fully` and `stable` share
/// the "Complete" label). Label/color come from
/// `catalog::status_label`, ordered Experimental → Complete like a
/// progression.
const STATUS_OPTS: &[&[&str]] = &[
    &["experimental"],
    &["beta"],
    &["usable"],
    &["playable"],
    &["fully", "stable"],
];

/// List ordering — `Catalog` keeps the fetched dataset's order.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SortKey {
    #[default]
    Catalog,
    Stars,
    Name,
    Year,
    Updated,
}

const SORT_OPTS: &[(&str, SortKey)] = &[
    ("Catalog", SortKey::Catalog),
    ("Stars", SortKey::Stars),
    ("Name", SortKey::Name),
    ("Year", SortKey::Year),
    ("Updated", SortKey::Updated),
];

/// Chip order for the platform filter — grouped by maker like the
/// site's category row (Nintendo → Sony → Sega → Xbox → PC/misc);
/// ids not listed sort after, alphabetically.
const CONSOLE_ORDER: &[&str] = &[
    "nes", "snes", "n64", "gamecube", "wii", "wiiu", "switch", "switch2", "virtualboy", "gb", "gbc",
    "gba", "nds", "3ds", "ps1", "ps2", "ps3", "ps4", "ps5", "psp", "vita", "megadrive", "genesis",
    "segacd", "megacd", "32x", "sms", "gamegear", "saturn", "dreamcast", "xbox", "x360", "xboxone",
    "xsx", "dos", "pc", "mac", "pc98", "mobile", "arcade", "neogeo", "other",
];

/// View state — the shared item list, the loaded catalog, the
/// kind/search filters, remote check results and in-flight
/// downloads keyed by slug. The list itself is the same
/// `carousel::Carousel` the music/emulation views drive: one widget
/// for the perspective carousel and both card grids, its spring
/// scrolling, scrollbar, marquee and menus.
pub struct DecompView {
    /// The shared item list — every item's `path` is its slug.
    pub cards: carousel::Carousel,
    /// Texture/decode pool the shared list draws covers through.
    row: AlbumRow,
    /// slug → `entries` index — card paths are slugs, so the info
    /// pane resolves the focused item through this.
    slug_idx: HashMap<String, usize>,
    /// slug → installed-stamp cache — refreshed on catalog load,
    /// download completion and a staleness timer (every frame would
    /// mean ~900 fs stats).
    installed: HashMap<String, Option<String>>,
    installed_at: Instant,
    /// Working catalog — the fallback manifest until the fetched or
    /// cached `catalog.json` lands.
    entries: Vec<Entry>,
    catalog: Option<catalog::Catalog>,
    catalog_rx: Option<mpsc::Receiver<CatalogResult>>,
    catalog_loading: bool,
    catalog_started: bool,
    /// slug → remote info. Filled from catalog stats in catalog mode;
    /// by the per-repo check worker in fallback mode.
    remote: HashMap<String, RemoteInfo>,
    check_rx: Option<mpsc::Receiver<(String, RemoteInfo)>>,
    check_running: bool,
    downloads: HashMap<String, crate::emulation::emu_dl::Shared>,
    notice: Option<String>,
    /// One-shot fallback check — armed only when no catalog arrives.
    checked: bool,
    /// Kind filter tab — `None` shows everything.
    filter: Option<Kind>,
    /// Filter drawer — open state plus the platform/status/sort
    /// options it owns (search stays in the top-right pill).
    pub filter_open: bool,
    /// Selected console ids (lowercased) — the empty set shows
    /// every platform (entries with no console only match when
    /// unfiltered).
    platforms: HashSet<String>,
    /// Index into `STATUS_OPTS` — `None` shows every maturity.
    status: Option<usize>,
    /// List ordering — `sync_items` rebuilds on change.
    sort: SortKey,
    /// The search pill's filter text lives in `search.text`.
    pub search: SearchPill,
    /// slugs whose cover fetch was already queued this session.
    cover_requested: HashSet<String>,
    cover_req_tx: mpsc::Sender<(String, String, Option<PathBuf>)>,
    cover_req_rx: Option<mpsc::Receiver<(String, String, Option<PathBuf>)>>,
    cover_res_rx: mpsc::Receiver<(String, bool)>,
    cover_res_tx: mpsc::Sender<(String, bool)>,
    /// The band's layout toggle wrote a persisted setting — the app
    /// consumes this and saves.
    pub settings_dirty: bool,

    // ---- Game capture session ------------------------------------
    // The same machinery the emulation view drives: a `xenia::Session`
    // with `Kind::Decomp` resolves the game's HWND (launcher-aware),
    // hides the real window and feeds WGC frames into the same
    // opaque presentation layer — list ⇄ capture switch, fullscreen,
    // pause/power controls.
    /// The running game session — WGC frames land in `frame_tex`
    /// (CPU mirror) or `gpu_cap` (zero-copy ring).
    pub session: Option<xenia::Session>,
    /// Install dir of the running/launching game — Play on the same
    /// card un-dismisses instead of relaunching.
    session_dir: Option<PathBuf>,
    /// Display name for the launch/status line.
    session_name: String,
    /// Spawn runs on a worker (CreateProcess + job setup) — the
    /// session lands here.
    launch_rx: Option<mpsc::Receiver<Result<xenia::Session, String>>>,
    /// Play on a different card while a session runs — queued until
    /// the running game closes (Lacrima's pending-launch shape).
    pending_launch: Option<(PathBuf, String)>,
    /// ⇄ list/capture switch — the grid stays live while the
    /// capture runs underneath (Lacrima's dismissed viewport).
    pub viewport_dismissed: bool,
    /// Game-fullscreen — the capture covers the whole view (the app
    /// window itself goes fullscreen via the F12 path).
    pub game_fs: bool,
    /// Whether WE put the window into fullscreen for `game_fs` —
    /// leaving restores the previous windowed/maximized state.
    fs_drove: bool,
    fs_seen: bool,
    fs_prev_max: bool,
    /// OS cursor hidden while the game owns the screen.
    cursor_hidden: bool,
    /// Presentation-rect tween state — fullscreen transitions glide
    /// between `upper` and `content` over a fixed 0.45s cubic.
    pres_anim: Rect,
    pres_from: Rect,
    pres_t: f32,
    pres_fs: bool,
    /// The opaque capture layer's fade over the grid (0.3s like
    /// Lacrima's DoubleTransition).
    cap_t: f32,
    /// egui texture captured frames upload into (CPU mirror path).
    frame_tex: Option<egui::TextureHandle>,
    /// Last frame seq consumed — ring seq on the zero-copy path,
    /// slot seq on the CPU mirror.
    frame_seq: u64,
    slot_seq: u64,
    /// Persistent frame staging buffer — swapped with the capture
    /// slot so neither side allocates per frame.
    frame_buf: Vec<u8>,
    /// GPU capture pipelines — assigned at boot when the backend is
    /// wgpu (same `CaptureShader` the emulation view uses).
    pub cap_shader: Option<cap_shader::CaptureShader>,
    /// The imported zero-copy ring; `None` puts us on the CPU mirror.
    gpu_cap: Option<cap_shader::ImportedRing>,
    /// Generation whose import already failed — don't retry per frame.
    gpu_failed_gen: u64,
    gpu_err: Option<String>,
    /// Aspect-fill blend — 0 = Uniform (fit), 1 = cover crop; the
    /// tween glides instead of snapping near the fill threshold.
    crop_t: f32,
    crop_from: f32,
    crop_target: f32,
    crop_prog: f32,
    /// UI close request parked for the caller — it owns the confirm
    /// prompt that guards every session stop.
    close_req: bool,
    /// Live capture render options — the "DECOMP" slot in the
    /// shared `cfg.render` map; the settings panel edits this copy
    /// and view() writes it through per change (AutoSave shape).
    render: settings::RenderOpts,
    /// One-shot load flag — `render` hydrates from cfg on the first
    /// view() pass (DecompView::new runs before cfg is in scope).
    render_loaded: bool,
    /// Capture-settings panel open — the same RenderOptionsOverlay
    /// the emulation view shows.
    pub render_open: bool,
    /// Set when `render` changed so view() writes it into cfg.
    render_dirty: bool,
    /// Stats-overlay + pillarbox state — the same bundle the
    /// emulation capture draw uses.
    fx: crate::emulation::CapFx,
    /// Whether the saved aspect_key was applied to this session's
    /// window (the HWND resolves asynchronously after launch).
    aspect_applied: bool,
    /// Uninstall request parked for the caller's delete prompt —
    /// `(slug, name)` so the dialog can name the project.
    uninstall_req: Option<(String, String)>,
    /// Uninstall asked for the RUNNING game — armed behind the close
    /// prompt, re-fires as `uninstall_req` once the session (or its
    /// in-flight launch) is fully gone.
    pending_uninstall: Option<(String, String)>,
    /// Input tunnel — pointer/keys over the capture PostMessage into
    /// the hidden game window so its own UI (launcher menus, option
    /// dialogs) is reachable. Esc or the band button releases it.
    pub input_tun: bool,
    /// The live frame mapping the tunnel needs — `(dest, uv)` —
    /// refreshed every frame the capture draw computes it.
    tun_map: Option<(Rect, Rect)>,
    /// Held mouse-button mask + tracked modifier state so tunneled
    /// events carry correct key flags.
    tun_buttons: u8,
    tun_mods: u8,
    /// Diagnostics — forwarded-event count + last post result, shown
    /// in the capture's bottom-right badge.
    pub tun_sent: u32,
    pub tun_dbg: Option<String>,
}

/// Where a card's Play goes (same shape as the emulation view's
/// PlayRoute): the running game only un-dismisses the capture
/// viewport; a different game queues behind a session close; no
/// session launches straight away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecompPlayRoute {
    UnDismiss,
    QueuePending,
    Launch,
}

impl Default for DecompView {
    fn default() -> Self {
        let (req_tx, req_rx) = mpsc::channel();
        let (res_tx, res_rx) = mpsc::channel();
        let mut cards = carousel::Carousel::new();
        cards.decomp_menu = true;
        // The horizontal grid's two-row floor — same floor the compact
        // music view applies.
        cards.h_min_rows = 2;
        Self {
            cards,
            row: AlbumRow::new(),
            slug_idx: HashMap::new(),
            installed: HashMap::new(),
            installed_at: Instant::now() - std::time::Duration::from_secs(60),
            entries: fallback_entries(),
            catalog: None,
            catalog_rx: None,
            catalog_loading: false,
            catalog_started: false,
            remote: HashMap::new(),
            check_rx: None,
            check_running: false,
            downloads: HashMap::new(),
            notice: None,
            checked: false,
            filter: None,
            filter_open: false,
            platforms: HashSet::new(),
            status: None,
            sort: SortKey::Catalog,
            search: SearchPill::default(),
            cover_requested: HashSet::new(),
            cover_req_tx: req_tx,
            cover_req_rx: Some(req_rx),
            cover_res_rx: res_rx,
            cover_res_tx: res_tx,
            settings_dirty: false,
            session: None,
            session_dir: None,
            session_name: String::new(),
            launch_rx: None,
            pending_launch: None,
            viewport_dismissed: false,
            game_fs: false,
            fs_drove: false,
            fs_seen: false,
            fs_prev_max: false,
            cursor_hidden: false,
            pres_anim: Rect::NOTHING,
            pres_from: Rect::NOTHING,
            pres_t: 1.0,
            pres_fs: false,
            cap_t: 0.0,
            frame_tex: None,
            frame_seq: 0,
            slot_seq: 0,
            frame_buf: Vec::new(),
            cap_shader: None,
            gpu_cap: None,
            gpu_failed_gen: 0,
            gpu_err: None,
            crop_t: 0.0,
            crop_from: 0.0,
            crop_target: 0.0,
            crop_prog: 1.0,
            close_req: false,
            uninstall_req: None,
            pending_uninstall: None,
            render: settings::RenderOpts::default(),
            render_loaded: false,
            render_open: false,
            render_dirty: false,
            fx: crate::emulation::CapFx::default(),
            aspect_applied: false,
            input_tun: false,
            tun_map: None,
            tun_buttons: 0,
            tun_mods: 0,
            tun_sent: 0,
            tun_dbg: None,
        }
    }
}

impl DecompView {
    pub fn new() -> Self {
        let mut v = Self::default();
        prepare_entries(&mut v.entries);
        v.rebuild_index();
        v.sync_items();
        v.apply_subset();
        v.refresh_installed();
        v
    }

    /// slug → entries index — rebuilt whenever `entries` is swapped.
    fn rebuild_index(&mut self) {
        self.slug_idx = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.slug.clone(), i))
            .collect();
    }

    /// `entries` indices in the current sort — `sync_items` pushes
    /// them in this order so the list matches the drawer's Sort row.
    fn sort_order(&self) -> Vec<usize> {
        let mut v: Vec<usize> = (0..self.entries.len()).collect();
        let e = &self.entries;
        match self.sort {
            SortKey::Catalog => {}
            SortKey::Stars => v.sort_by(|&a, &b| {
                e[b]
                    .stars
                    .unwrap_or(0)
                    .cmp(&e[a].stars.unwrap_or(0))
                    .then_with(|| e[a].name.cmp(&e[b].name))
            }),
            SortKey::Name => {
                v.sort_by(|&a, &b| e[a].name.to_lowercase().cmp(&e[b].name.to_lowercase()))
            }
            SortKey::Year => v.sort_by(|&a, &b| {
                e[b]
                    .year
                    .unwrap_or(0)
                    .cmp(&e[a].year.unwrap_or(0))
                    .then_with(|| e[a].name.cmp(&e[b].name))
            }),
            SortKey::Updated => v.sort_by(|&a, &b| e[b].pushed_at.cmp(&e[a].pushed_at)),
        }
        v
    }

    /// Push `entries` into the shared list — each item's `path` is
    /// the slug (identity for selection, covers, actions), `keywords`
    /// carries the search haystack so `visible()`'s text filter keeps
    /// matching repo/dev/toolchain/tags, `artist`/`album` stay clean
    /// marquee lines.
    fn sync_items(&mut self) {
        let items = self
            .sort_order()
            .iter()
            .map(|&i| {
                let e = &self.entries[i];
                carousel::Item {
                    path: PathBuf::from(&e.slug),
                    cover: cover_cache(&e.slug)
                        .filter(|c| c.is_file())
                        .map(CoverSrc::File)
                        .unwrap_or_else(|| CoverSrc::Web(Vec::new())),
                    fallback: None,
                    title: e.name.clone(),
                    artist: if !e.developer.is_empty() {
                        e.developer.clone()
                    } else {
                        e.author.clone()
                    },
                    album: format!("{} · {}", catalog::console_label(&e.console), e.kind.label()),
                    keywords: e.search_blob.clone(),
                    duration: None,
                    marker: None,
                    progress: None,
                }
            })
            .collect();
        self.cards.open_items(items, "decomp".to_string());
    }

    /// `Carousel::set_subset` for every active filter at once —
    /// kind tab ∩ platform chips ∩ status chip; `None` when nothing
    /// is filtering so the whole list shows.
    fn apply_subset(&mut self) {
        let status = self.status;
        let active =
            self.filter.is_some() || !self.platforms.is_empty() || status.is_some();
        self.cards.set_subset(active.then(|| {
            self.entries
                .iter()
                .filter(|e| {
                    self.filter.map_or(true, |k| e.kind == k)
                        && (self.platforms.is_empty()
                            || self.platforms.contains(&e.console.to_lowercase()))
                        && status.map_or(true, |i| {
                            STATUS_OPTS[i].contains(&e.status.as_str())
                        })
                })
                .map(|e| PathBuf::from(&e.slug))
                .collect()
        }));
    }

    /// Any drawer filter actually narrowing the list — lights the
    /// band's filter icon while the drawer is closed.
    fn filters_active(&self) -> bool {
        !self.platforms.is_empty() || self.status.is_some()
    }

    /// After a filter change: refresh the subset and re-anchor the
    /// focus on the selection (a filtered-out selection lands on the
    /// first shown) — the same contract the kind tabs follow.
    fn apply_filters(&mut self) {
        self.apply_subset();
        self.cards.focus(&self.search.text);
    }

    /// The platform chips — consoles present in the catalog,
    /// `(id, label)` pairs ordered by `CONSOLE_ORDER` then label.
    /// Entries with no console get no chip (they only show when no
    /// platform is selected). Ids are lowercased — the catalog uses
    /// "n64" but the fallback manifest writes "N64".
    fn platform_options(&self) -> Vec<(String, String)> {
        let mut seen = HashSet::new();
        for e in &self.entries {
            if !e.console.is_empty() {
                seen.insert(e.console.to_lowercase());
            }
        }
        let rank = |id: &str| {
            CONSOLE_ORDER
                .iter()
                .position(|c| *c == id)
                .unwrap_or(usize::MAX)
        };
        let mut v: Vec<(String, String)> = seen
            .into_iter()
            .map(|id| {
                let label = catalog::console_label(&id);
                (id, label)
            })
            .collect();
        v.sort_by(|a, b| rank(&a.0).cmp(&rank(&b.0)).then_with(|| a.1.cmp(&b.1)));
        v
    }

    /// Tab switch: restrict the subset and re-anchor the focus on the
    /// selection (a filtered-out selection lands on the first shown).
    fn set_kind_filter(&mut self, k: Option<Kind>) {
        self.filter = k;
        self.apply_filters();
    }

    /// Sort row: rebuild the list in the new order (open_items resets
    /// the scroll/selection — explicit user reorder, that's fine).
    /// `subset` survives `open_items`, so filters stay applied.
    fn set_sort(&mut self, k: SortKey) {
        if self.sort != k {
            self.sort = k;
            self.sync_items();
        }
    }

    /// Re-stat every install marker — throttled so a frame never pays
    /// ~900 fs stats; called on catalog load, download completion and
    /// when the cache has gone stale.
    fn refresh_installed(&mut self) {
        self.installed = self
            .entries
            .iter()
            .map(|e| {
                (
                    e.slug.clone(),
                    install_dir(&e.slug).as_deref().and_then(installed_stamp),
                )
            })
            .collect();
        self.installed_at = Instant::now();
    }

    /// Consume the layout-toggle's settings write.
    pub fn take_settings_dirty(&mut self) -> bool {
        std::mem::take(&mut self.settings_dirty)
    }

    /// Where a card's Play goes — same routing the emulation view
    /// uses: the running game only un-dismisses the capture
    /// viewport; a different game queues a pending launch behind a
    /// close; no session launches straight away.
    fn play_route(&self, dir: &Path) -> DecompPlayRoute {
        if self.session.is_some() || self.launch_rx.is_some() {
            if self.session_dir.as_deref() == Some(dir) {
                DecompPlayRoute::UnDismiss
            } else {
                DecompPlayRoute::QueuePending
            }
        } else {
            DecompPlayRoute::Launch
        }
    }

    /// Card/pane "Play": launch the installed game as a captured
    /// session (or route through the running one).
    pub fn play_dir(&mut self, dir: &Path, name: String) {
        match self.play_route(dir) {
            DecompPlayRoute::UnDismiss => self.viewport_dismissed = false,
            DecompPlayRoute::QueuePending => {
                self.pending_launch = Some((dir.to_path_buf(), name));
                self.request_close();
            }
            DecompPlayRoute::Launch => self.launch(dir, name),
        }
    }

    /// Find the game's exe and spawn the capture session on a
    /// worker — the process + resolve worker land via `launch_rx`.
    fn launch(&mut self, dir: &Path, name: String) {
        self.notice = None;
        xenia::session_log(&format!("decomp launch dir={} exe=find", dir.display()));
        match find_exe(dir) {
            Some(exe) => {
                let root = dir.to_path_buf();
                let exe_s = exe.to_string_lossy().into_owned();
                let (tx, rx) = mpsc::channel();
                // LowLatencyCapture → the capture pool depth for the
                // session about to start (the panel has no row for
                // it; the DECOMP render slot carries the default).
                let low_latency = self.render.low_latency;
                std::thread::spawn(move || {
                    let _ = tx.send(xenia::Session::start(
                        &exe_s,
                        &exe,
                        low_latency,
                        None,
                        xenia::Kind::Decomp,
                        None,
                        Some(root),
                    ));
                });
                self.launch_rx = Some(rx);
                self.session_dir = Some(dir.to_path_buf());
                self.session_name = name.clone();
                self.notice = Some(format!("Launching {name}…"));
            }
            None => {
                self.notice = Some("No executable found — opening the folder".into());
                open_in_file_manager(dir);
            }
        }
    }

    /// Ask the running session to stop the game — or cancel a
    /// launch still in flight: dropping the receiver fails the
    /// spawn worker's send and the orphaned `Session` reaps the
    /// process it just started (Drop kills the child + job tree).
    pub fn stop_session(&mut self) {
        if let Some(s) = &self.session {
            s.stop();
        } else if self.launch_rx.is_some() {
            self.launch_rx = None;
            self.session_dir = None;
        }
    }

    /// Flag a close request — the caller turns it into the
    /// Switch-style "Close game?" confirm instead of stopping
    /// directly.
    pub fn request_close(&mut self) {
        self.close_req = true;
    }

    /// Consume a parked close request — once, so a stale flag can't
    /// reopen the prompt after it's been answered.
    pub fn take_close_request(&mut self) -> bool {
        std::mem::replace(&mut self.close_req, false)
    }

    /// Card/pane "Uninstall" — parks a `(slug, name)` request the
    /// caller routes through the delete-confirm prompt (same shape
    /// as `request_close`). Uninstalling the RUNNING game goes
    /// through the close prompt first — its install dir can't be
    /// deleted mid-session — then re-arms once teardown lands.
    fn request_uninstall(&mut self, e: &Entry) {
        let req = (e.slug.clone(), e.name.clone());
        if self.session_dir == install_dir(&e.slug) {
            self.pending_uninstall = Some(req);
            self.request_close();
        } else {
            self.uninstall_req = Some(req);
        }
    }

    /// Consume a parked uninstall request — once, same reason as
    /// `take_close_request`.
    pub fn take_uninstall_request(&mut self) -> Option<(String, String)> {
        self.uninstall_req.take()
    }

    /// The pending_uninstall → uninstall_req handoff once the dir
    /// frees (session torn down, launch canceled or failed). Called
    /// by view() right after each spot that clears `session_dir`.
    fn settle_uninstall(&mut self) {
        if self.session_dir.is_none() && self.launch_rx.is_none() {
            if let Some(req) = self.pending_uninstall.take() {
                self.uninstall_req = Some(req);
            }
        }
    }

    /// Confirmed uninstall — drop the install tree, drop a queued
    /// Play for it, and refresh the stamp cache so cards and the
    /// pane flip back to Install. Refuses while the game still owns
    /// a session or in-flight launch (the running path re-arms after
    /// teardown instead, so this can't race the filesystem locks).
    pub fn uninstall(&mut self, slug: &str) {
        let Some(dir) = install_dir(slug) else {
            return;
        };
        if self.session_dir.as_deref() == Some(dir.as_path()) {
            return;
        }
        if self
            .pending_launch
            .as_ref()
            .is_some_and(|(d, _)| d == &dir)
        {
            self.pending_launch = None;
        }
        let _ = std::fs::remove_dir_all(&dir);
        self.refresh_installed();
    }

    /// The ⇄ switch — dismissing also drops game-fullscreen and the
    /// input tunnel so the app window comes back; un-dismiss is the
    /// toggle's other half.
    pub fn toggle_viewport(&mut self) {
        if self.viewport_dismissed {
            self.viewport_dismissed = false;
        } else {
            self.viewport_dismissed = true;
            self.game_fs = false;
            self.input_tun = false;
        }
    }

    /// The input-tunnel toggle — the band's pointer icon and Ctrl+M:
    /// while lit, clicks/keys over the capture PostMessage into the
    /// game's hidden window.
    pub fn toggle_input_tunnel(&mut self) {
        self.input_tun = !self.input_tun;
    }

    /// Entering also un-dismisses the viewport: `game_fs` alone is a
    /// request — the capture layer must be up for it to mean
    /// anything.
    pub fn toggle_fullscreen(&mut self) {
        self.game_fs = !self.game_fs;
        if self.game_fs {
            self.viewport_dismissed = false;
        }
    }

    /// Count of items the shared list currently shows (subset + text
    /// filter applied) — the band's "n of m shown".
    fn shown_count(&self) -> usize {
        self.cards.visible(&self.search.text).len()
    }

    /// Per-kind counts for the tabs.
    fn kind_counts(&self) -> HashMap<Kind, usize> {
        let mut m = HashMap::new();
        for e in &self.entries {
            *m.entry(e.kind).or_insert(0) += 1;
        }
        m
    }

    /// Kick the catalog worker: serve the disk cache first (instant
    /// entries), then fetch upstream when the cache is stale or
    /// missing — the fetched catalog arrives as a second message.
    /// `force` skips the freshness check (manual refresh).
    fn spawn_catalog(&mut self, force: bool, ctx: &egui::Context) {
        if self.catalog_loading {
            return;
        }
        self.catalog_loading = true;
        self.catalog_started = true;
        let (tx, rx) = mpsc::channel();
        self.catalog_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let cached = catalog::load_cached();
            let stale = cached.as_ref().map(catalog::is_stale).unwrap_or(true);
            if let Some(c) = &cached {
                if tx.send(Ok(c.clone())).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
            if cached.is_some() && !stale && !force {
                return;
            }
            match catalog::fetch() {
                Ok(c) => {
                    let _ = tx.send(Ok(c));
                    ctx.request_repaint();
                }
                Err(e) => {
                    // A stale cache still beats the fallback manifest.
                    if cached.is_none() {
                        let _ = tx.send(Err(e));
                        ctx.request_repaint();
                    }
                }
            }
        });
    }

    /// Fallback-mode check: one `repos/{o}/{r}` GET per project,
    /// paced to stay under GitHub's secondary rate limit. In catalog
    /// mode `pushed_at` comes free with the data — this only runs
    /// when the catalog never arrived.
    pub fn check_updates(&mut self, ctx: &egui::Context) {
        if self.check_running {
            return;
        }
        if self.catalog.is_some() {
            // Catalog mode: refreshing means re-fetching the dataset.
            self.spawn_catalog(true, ctx);
            return;
        }
        self.check_running = true;
        let (tx, rx) = mpsc::channel();
        self.check_rx = Some(rx);
        let repos: Vec<(String, String)> = self
            .entries
            .iter()
            .filter(|e| !e.repo.is_empty())
            .map(|e| (e.slug.clone(), e.repo.clone()))
            .collect();
        std::thread::spawn(move || {
            let agent = crate::emulation::emu_dl::agent();
            for (i, (slug, repo)) in repos.iter().enumerate() {
                if i > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(350));
                }
                let url = format!("https://api.github.com/repos/{repo}");
                let info = match agent
                    .get(&url)
                    .header("User-Agent", USER_AGENT)
                    .header("Accept", "application/vnd.github+json")
                    .call()
                {
                    Ok(mut resp) => match resp.body_mut().read_to_string() {
                        Ok(text) => RemoteInfo {
                            pushed_at: serde_json::from_str::<serde_json::Value>(&text)
                                .ok()
                                .and_then(|v| v.get("pushed_at")?.as_str().map(|s| s.to_string()))
                                .unwrap_or_default(),
                            err: None,
                        },
                        Err(e) => RemoteInfo {
                            err: Some(format!("{e}")),
                            ..Default::default()
                        },
                    },
                    Err(e) => RemoteInfo {
                        err: Some(format!("{e}")),
                        ..Default::default()
                    },
                };
                if tx.send((slug.clone(), info)).is_err() {
                    return;
                }
            }
        });
    }

    /// Arrow-key navigation — the shared list's directional walk:
    /// ±1 horizontally, ±columns vertically in the grids.
    pub fn move_selection(&mut self, dx: i32, dy: i32) {
        self.cards.arrow(&self.search.text, dx, dy);
    }

    /// Enter — runs the focused entry's action (Install/Update/
    /// Play/Open), same as its pane buttons.
    pub fn activate_selected(&mut self) {
        if let Some(p) = self.cards.focused_path(&self.search.text) {
            self.activate_slug(&p.to_string_lossy());
        }
    }

    /// The card's resolved action, run for `entry`.
    fn activate(&mut self, e: &Entry) {
        if e.repo.is_empty() {
            return;
        }
        match card_action(
            install_dir(&e.slug)
                .as_deref()
                .and_then(|d| installed_stamp(d))
                .as_deref(),
            self.remote.get(&e.slug),
        ) {
            Action::Install | Action::Update => self.download(e),
            Action::Open => {
                if let Some(dir) = install_dir(&e.slug) {
                    self.play_dir(&dir, e.name.clone());
                }
            }
        }
    }

    fn activate_slug(&mut self, slug: &str) {
        if let Some(i) = self.entries.iter().position(|e| e.slug == slug) {
            let e = self.entries[i].clone();
            self.activate(&e);
        }
    }

    /// Release/zipball → extract → copy into `<data>/Decomps/<slug>`
    /// on a worker; `DlProgress` carries stage/frac for the card.
    fn download(&mut self, e: &Entry) {
        if self.downloads.contains_key(&e.slug) {
            return;
        }
        let dl: crate::emulation::emu_dl::Shared = std::sync::Arc::new(std::sync::Mutex::new(
            crate::emulation::emu_dl::DlProgress {
                stage: format!("Fetching {}…", e.name),
                ..Default::default()
            },
        ));
        let shared = dl.clone();
        let repo = e.repo.clone();
        let slug = e.slug.clone();
        std::thread::spawn(move || {
            let res = download_work(&shared, &repo, &slug);
            let mut g = shared.lock().unwrap_or_else(|e| e.into_inner());
            g.done = Some(res);
        });
        self.downloads.insert(e.slug.clone(), dl);
    }

    /// Adopt a fetched/cached catalog: entries, per-repo stats as the
    /// remote map, search blobs.
    fn adopt_catalog(&mut self, mut c: catalog::Catalog) {
        prepare_entries(&mut c.entries);
        self.remote = c
            .entries
            .iter()
            .map(|e| {
                (
                    e.slug.clone(),
                    RemoteInfo {
                        pushed_at: e.pushed_at.clone(),
                        err: None,
                    },
                )
            })
            .collect();
        self.entries = std::mem::take(&mut c.entries);
        self.notice = None;
        self.catalog = Some(c);
        self.rebuild_index();
        self.sync_items();
        self.apply_subset();
        self.refresh_installed();
    }

    /// Per-frame bookkeeping: kick the catalog + cover workers, drain
    /// all channels, retire finished downloads, keep repaints flowing.
    fn pump(&mut self, ctx: &egui::Context) {
        if !self.catalog_started {
            self.spawn_catalog(false, ctx);
        }
        if let Some(req_rx) = self.cover_req_rx.take() {
            let res_tx = self.cover_res_tx.clone();
            let c = ctx.clone();
            std::thread::spawn(move || cover_work(req_rx, res_tx, c));
        }
        let mut catalog_msgs = Vec::new();
        let mut catalog_dead = false;
        if let Some(rx) = &self.catalog_rx {
            loop {
                match rx.try_recv() {
                    Ok(r) => catalog_msgs.push(r),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        catalog_dead = true;
                        break;
                    }
                }
            }
        }
        for m in catalog_msgs {
            match m {
                Ok(c) => self.adopt_catalog(c),
                Err(e) => self.notice = Some(e),
            }
        }
        if catalog_dead {
            self.catalog_rx = None;
            self.catalog_loading = false;
        }
        // Fallback mode: no catalog after the worker finished → run
        // the old per-repo check once.
        if self.catalog_started
            && !self.catalog_loading
            && self.catalog.is_none()
            && !self.checked
        {
            self.checked = true;
            self.check_updates(ctx);
        }
        loop {
            match self.cover_res_rx.try_recv() {
                // The worker cached art to disk — swap the item's
                // cover source so the row's decode pool picks it up.
                Ok((slug, true)) => {
                    if let Some(c) = cover_cache(&slug).filter(|f| f.is_file()) {
                        self.cards
                            .set_cover(&PathBuf::from(&slug), CoverSrc::File(c));
                    }
                }
                Ok((_, false)) => {}
                Err(_) => break,
            }
        }
        if let Some(rx) = &self.check_rx {
            let mut dead = false;
            loop {
                match rx.try_recv() {
                    Ok((slug, info)) => {
                        self.remote.insert(slug, info);
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        dead = true;
                        break;
                    }
                }
            }
            if dead {
                self.check_rx = None;
                self.check_running = false;
            }
        }
        let mut finished = Vec::new();
        for (slug, dl) in &self.downloads {
            let g = dl.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(res) = &g.done {
                finished.push((slug.clone(), res.clone()));
            }
        }
        for (slug, res) in finished {
            self.downloads.remove(&slug);
            match res {
                Ok(path) => {
                    self.notice = Some(format!("Installed to {path}"));
                    self.refresh_installed();
                }
                Err(e) => self.notice = Some(e),
            }
        }
        if self.check_running || self.catalog_loading || !self.downloads.is_empty() {
            ctx.request_repaint();
        }
    }
}

/// Best OS-matching asset from a `releases/latest` `assets` array —
/// a Windows x64 build is required; symbols, checksums, source dumps
/// and foreign-OS artifacts are rejected up front, and `arch_ok`
/// (the emu_dl gate) filters the arm/riscv builds release listings
/// mix into the same page. x64 markers only *score* — projects that
/// ship exactly one windows zip carry no arch marker at all.
fn pick_os_asset(assets: &[serde_json::Value]) -> Option<String> {
    let mut best: Option<(i32, &str)> = None;
    for a in assets {
        let Some(n) = a.get("name").and_then(|v| v.as_str()).map(str::to_lowercase) else {
            continue;
        };
        let Some(url) = a.get("browser_download_url").and_then(|v| v.as_str()) else {
            continue;
        };
        if [
            "linux", "macos", "darwin", "osx", "android", "ios", "freebsd", "appimage",
            "flatpak", ".deb", ".rpm", ".dmg", ".pkg", "symbols", ".sig", ".sha", "checksum",
            "source", "-src", "debug", ".pdb", ".txt", ".json",
        ]
        .iter()
        .any(|m| n.contains(m))
        {
            continue;
        }
        if !crate::emulation::emu_dl::arch_ok(&n) {
            continue;
        }
        // An explicit windows marker is required — a linux x64
        // tarball otherwise scores just as well as the win build.
        if ![
            "windows", "win64", "win32", "-win", "win-", "msvc", ".exe", ".msi", "-pc.",
        ]
        .iter()
        .any(|m| n.contains(m))
        {
            continue;
        }
        let mut score = 0;
        if n.contains("x64") || n.contains("x86_64") || n.contains("amd64") {
            score += 2;
        }
        if n.ends_with(".zip") || n.ends_with(".7z") {
            score += 1;
        }
        if score > best.map(|(s, _)| s).unwrap_or(-1) {
            best = Some((score, url));
        }
    }
    best.map(|(_, u)| u.to_string())
}

/// Whether a downloaded asset is an archive we can extract —
/// `extract_archive` covers zip/7z and the tar family; anything
/// else (a bare .exe, an AppImage-less binary) is copied as-is.
fn is_archive(name: &str) -> bool {
    let n = name.to_lowercase();
    [
        ".zip", ".7z", ".tar", ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.zst",
    ]
    .iter()
    .any(|e| n.ends_with(e))
}

/// Card label for the action button click path — kept separate so the
/// click handler and Enter share `activate`'s decision.
fn download_work(
    dl: &crate::emulation::emu_dl::Shared,
    repo: &str,
    slug: &str,
) -> Result<String, String> {
    let root = decomps_root().ok_or("Could not resolve the data directory")?;
    // Per-slug staging — concurrent installs share nothing.
    let stage_dir = root.join("dl_tmp").join(slug);
    std::fs::create_dir_all(&stage_dir).map_err(|e| format!("Decomps dir not writable: {e}"))?;

    // Repo info first — the marker stores the pushed_at we fetched
    // at download time, so the next check compares honestly.
    crate::emulation::emu_dl::set(dl, "Checking remote…", 0.0);
    let agent = crate::emulation::emu_dl::agent();
    let info_url = format!("https://api.github.com/repos/{repo}");
    let pushed_at = agent
        .get(&info_url)
        .header("User-Agent", USER_AGENT)
        .call()
        .ok()
        .and_then(|mut r| r.body_mut().read_to_string().ok())
        .and_then(|text| {
            serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("pushed_at")?.as_str().map(|s| s.to_string()))
        })
        .unwrap_or_default();

    let dest = install_dir(slug).ok_or("Could not resolve the install dir")?;
    // Releases first — a matching OS binary beats the source zipball
    // when the project publishes builds. Most decomps ship none, so
    // the source path stays the fallback.
    let mode = match try_release(dl, &agent, repo, &stage_dir, &dest) {
        Some(tag) => tag,
        None => {
            source_download(dl, repo, slug, &stage_dir, &dest)?;
            "source".to_string()
        }
    };
    let _ = std::fs::write(
        dest.join(MARKER),
        format!("{}\n{}", pushed_at.trim(), mode),
    );
    let _ = std::fs::remove_dir_all(&stage_dir);
    crate::emulation::emu_dl::set(dl, "Done", 1.0);
    Ok(dest.to_string_lossy().into_owned())
}

/// `releases/latest` → the best OS asset → download + unpack (or
/// copy, for bare binaries) into `dest`. Returns the release tag on
/// success; `None` on no-release, no-matching-asset, or any failure
/// — the caller falls back to the source zipball either way.
fn try_release(
    dl: &crate::emulation::emu_dl::Shared,
    agent: &ureq::Agent,
    repo: &str,
    stage_dir: &Path,
    dest: &Path,
) -> Option<String> {
    crate::emulation::emu_dl::set(dl, "Checking release…", 0.0);
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let mut resp = agent
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .call()
        .ok()?;
    let v: serde_json::Value =
        serde_json::from_str(&resp.body_mut().read_to_string().ok()?).ok()?;
    let tag = v.get("tag_name")?.as_str()?.to_string();
    let asset = pick_os_asset(v.get("assets")?.as_array()?)?;
    let fname = asset.rsplit('/').next().unwrap_or("asset.bin");
    let file = stage_dir.join(fname);
    crate::emulation::emu_dl::download(dl, &format!("Downloading {tag}…"), &asset, &file).ok()?;

    crate::emulation::emu_dl::set(dl, "Installing…", 0.0);
    std::fs::create_dir_all(dest).ok()?;
    let ok = if is_archive(fname) {
        let extracted = stage_dir.join("extracted");
        let _ = std::fs::remove_dir_all(&extracted);
        std::fs::create_dir_all(&extracted).is_ok()
            && crate::emulation::emu_dl::extract_archive(&file, &extracted).is_ok()
            && {
                let src = crate::emulation::emu_dl::normalize_root(&extracted);
                crate::emulation::emu_dl::copy_dir_contents(&src, dest, "").is_ok()
            }
    } else {
        std::fs::copy(&file, dest.join(fname)).is_ok()
    };
    ok.then_some(tag)
}

/// The source path: repo zipball → extract → copy over `dest`.
fn source_download(
    dl: &crate::emulation::emu_dl::Shared,
    repo: &str,
    slug: &str,
    stage_dir: &Path,
    dest: &Path,
) -> Result<(), String> {
    let zip = stage_dir.join(format!("{slug}.zip"));
    crate::emulation::emu_dl::download(
        dl,
        &format!("Downloading {slug} source…"),
        &format!("https://api.github.com/repos/{repo}/zipball"),
        &zip,
    )?;

    crate::emulation::emu_dl::set(dl, "Extracting…", 0.0);
    let extracted = stage_dir.join("extracted");
    let _ = std::fs::remove_dir_all(&extracted);
    std::fs::create_dir_all(&extracted).map_err(|e| format!("Extract failed: {e}"))?;
    crate::emulation::emu_dl::extract_archive(&zip, &extracted)?;
    let src = crate::emulation::emu_dl::normalize_root(&extracted);
    std::fs::create_dir_all(dest).map_err(|e| format!("Install failed: {e}"))?;
    // Copy OVER the tree — baseroms and build output survive updates.
    crate::emulation::emu_dl::copy_dir_contents(&src, dest, "")?;
    Ok(())
}

fn open_in_file_manager(dir: &Path) {
    let _ = if cfg!(windows) {
        std::process::Command::new("explorer").arg(dir).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(dir).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(dir).spawn()
    };
}

/// Cache path for a project's box art — `Decomps/covers/<slug>.png`.
fn cover_cache(slug: &str) -> Option<PathBuf> {
    decomps_root().map(|d| d.join("covers").join(format!("{slug}.png")))
}

/// The box-art worker: requests arrive per visible card, disk cache
/// first, `boxart_bytes` on a miss cached back to disk. The row's
/// decode pool owns pixels — the worker only reports whether a cover
/// file now exists so the view can `set_cover` the item.
fn cover_work(
    rx: mpsc::Receiver<(String, String, Option<PathBuf>)>,
    tx: mpsc::Sender<(String, bool)>,
    ctx: egui::Context,
) {
    while let Ok((slug, query, cache)) = rx.recv() {
        let ok = cache.as_ref().is_some_and(|f| f.is_file())
            || (!query.is_empty()
                && crate::emulation::emu_meta::boxart_bytes(&query)
                    .map(|b| {
                        if let Some(f) = &cache {
                            if let Some(d) = f.parent() {
                                let _ = std::fs::create_dir_all(d);
                            }
                            let _ = std::fs::write(f, &b);
                        }
                        true
                    })
                    .unwrap_or(false));
        if tx.send((slug, ok)).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

/// Contain-fit `iw`×`ih` inside `slot` (letterboxed, centered) —
/// box art isn't a uniform aspect, so the slot never crops.
fn fit_rect(slot: Rect, iw: f32, ih: f32) -> Rect {
    if iw <= 0.0 || ih <= 0.0 {
        return slot;
    }
    let k = (slot.width() / iw).min(slot.height() / ih);
    Rect::from_center_size(slot.center(), Vec2::new(iw * k, ih * k))
}

/// Plural tab label per kind.
fn tab_label(kind: Kind) -> &'static str {
    match kind {
        Kind::Decomp => "Decomps",
        Kind::Recomp => "Recomps",
        Kind::Disasm => "Disassemblies",
        Kind::Port => "Ports",
        Kind::Mod => "Mods",
        Kind::Tool => "Tools",
    }
}

/// The Re/Decompilation view: kind tabs + the shared item list over
/// black with the AES bottom band — same geometry as the emulation
/// view, and the list itself IS the emulation/music `Carousel`.
pub fn view(
    ui: &mut egui::Ui,
    content: Rect,
    alpha: f32,
    interactive: bool,
    view: &mut DecompView,
    emu_cfg: &mut settings::Emulation,
    cfg: &settings::Carousel,
    s: f32,
) {
    view.pump(ui.ctx());

    let band_h = crate::TRANSPORT_BAND_H * s;
    let band = Rect::from_min_max(
        Pos2::new(content.min.x, content.max.y - band_h),
        content.max,
    );
    let upper = Rect::from_min_max(content.min, Pos2::new(content.max.x, band.min.y));
    let painter = ui.painter().clone();

    painter.rect_filled(upper, 0.0, Color32::BLACK.gamma_multiply(alpha));

    // --- Capture session housekeeping (the emulation view's form) --
    // The spawn worker hands the session over; the game lands on the
    // capture layer — the presentation fades in over the grid.
    if let Some(rx) = &view.launch_rx {
        match rx.try_recv() {
            Ok(Ok(s)) => {
                // Fresh capture — the old ring/seqs belong to the
                // dead session.
                view.gpu_cap = None;
                view.gpu_failed_gen = 0;
                view.gpu_err = None;
                view.slot_seq = 0;
                view.frame_seq = 0;
                view.aspect_applied = false;
                view.session = Some(s);
                view.launch_rx = None;
                view.notice = None;
                view.viewport_dismissed = false;
            }
            Ok(Err(e)) => {
                xenia::session_log(&format!("decomp launch failed: {e}"));
                view.notice = Some(e);
                view.launch_rx = None;
                view.session_dir = None;
            }
            Err(mpsc::TryRecvError::Empty) => ui.ctx().request_repaint(),
            Err(mpsc::TryRecvError::Disconnected) => {
                xenia::session_log("decomp launch worker died before producing a session");
                view.notice = Some("Launch failed (worker died — see session.log)".into());
                view.launch_rx = None;
                view.session_dir = None;
            }
        }
    }
    // Session teardown once the game reports exit — the last
    // captured frame stays mounted so the layer can fade out over
    // the grid instead of snapping away.
    if view.session.as_mut().is_some_and(|s| s.finished()) {
        view.session = None;
        view.session_dir = None;
        view.viewport_dismissed = false;
        view.game_fs = false;
        view.input_tun = false;
        // An uninstall requested for this game re-arms now that its
        // install dir is free — BEFORE a queued Play can re-park a
        // session on it for another stretch.
        view.settle_uninstall();
        // The queued Play fires only once the replaced session is
        // fully reaped (TryLaunchPendingEmulatorRequest).
        if let Some((dir, name)) = view.pending_launch.take() {
            view.launch(&dir, name);
        }
    }
    // A stopped/canceled/failed launch frees the install dir the
    // same way teardown does — the armed uninstall surfaces now.
    view.settle_uninstall();
    // A confirmed close during an in-flight launch cleared the rx
    // (stop_session) — the queued game fires now that nothing is
    // running or starting. A canceled prompt leaves the session
    // (or its launch) alive, which blocks this arm.
    if view.session.is_none() && view.launch_rx.is_none() {
        if let Some((dir, name)) = view.pending_launch.take() {
            view.launch(&dir, name);
        }
    }

    // The view's render options — a fixed "DECOMP" slot in the same
    // per-section `cfg.render` map the emulation view uses (loaded
    // once; the settings panel edits the live copy afterwards).
    if !view.render_loaded {
        view.render_loaded = true;
        view.render = emu_cfg
            .render
            .get("DECOMP")
            .cloned()
            .unwrap_or_default();
    }
    // Live render options → the capture thread's swizzle reads these
    // atomics each frame, so panel edits land on the very next
    // captured frame (same push the emulation view does).
    if let Some(session) = &view.session {
        use std::sync::atomic::Ordering::Relaxed;
        let r = session.render();
        r.brightness.store(view.render.brightness.to_bits(), Relaxed);
        r.saturation.store(view.render.saturation.to_bits(), Relaxed);
        let [tr, tg, tb] = view.render.tint;
        r.tint
            .store((tr as u32) << 16 | (tg as u32) << 8 | tb as u32, Relaxed);
    }
    // Write-through — the DECOMP slot persists like the per-section
    // SectionSettings do.
    if view.render_dirty {
        view.render_dirty = false;
        emu_cfg.render.insert("DECOMP".into(), view.render.clone());
        view.settings_dirty = true;
    }
    // DisableVSync — the presentation is repaint-driven, so the
    // equivalent is a continuous repaint while the capture shows:
    // frames present at display rate regardless of arrival timing.
    if view.render.disable_vsync && view.session.is_some() && !view.viewport_dismissed {
        ui.ctx().request_repaint();
    }

    // SelectedCaptureAspectRatio — once the session resolves its
    // render window, resize it to the saved aspect ("handler"
    // leaves the game's own; the context menu re-picks live).
    #[cfg(windows)]
    if !view.aspect_applied {
        if let Some(session) = &view.session {
            let hwnd = session.hwnd();
            if !hwnd.is_invalid() {
                if let Some(ratio) = crate::emulation::aspect_ratio_of(&view.render.aspect_key) {
                    xenia::apply_aspect_ratio(hwnd, ratio);
                }
                view.aspect_applied = true;
            }
        }
    }
    #[cfg(not(windows))]
    {
        view.aspect_applied = true;
    }
    // The opaque capture layer fades in/out over the grid — 0.3s,
    // same duration as the emulation view.
    let cap_target = view.session.is_some() && !view.viewport_dismissed;
    let dt = ui.ctx().input(|i| i.stable_dt).min(0.1);
    let before = view.cap_t;
    view.cap_t = (view.cap_t + if cap_target { dt / 0.3 } else { -dt / 0.3 }).clamp(0.0, 1.0);
    let cap_a = crate::ease_in_out_cubic(view.cap_t);
    if view.cap_t > 0.0 || view.cap_t != before {
        ui.ctx().request_repaint();
    } else if view.session.is_some() {
        // Hidden session — only poll occasionally for exit.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
    }
    if view.session.is_none() && view.cap_t <= 0.0 {
        view.frame_tex = None;
        view.gpu_cap = None;
        view.frame_seq = 0;
        view.slot_seq = 0;
        view.frame_buf = Vec::new();
    }

    // Game-fullscreen rides on the app's real fullscreen window mode
    // (same contract as the emulation view): entering sends
    // Fullscreen(true), leaving restores the previous
    // windowed/maximized state — but only when we drove it.
    let (win_fs, win_max) = ui.ctx().input(|i| {
        (
            i.viewport().fullscreen.unwrap_or(false),
            i.viewport().maximized.unwrap_or(false),
        )
    });
    if view.game_fs && !win_fs && !view.fs_drove {
        view.fs_drove = true;
        view.fs_seen = false;
        view.fs_prev_max = win_max;
        // set_fullscreen pairs Fullscreen with Decorations(false) —
        // the bare command leaves winit's 1px shadow inset.
        crate::set_fullscreen(ui.ctx(), true);
    }
    if view.fs_drove && win_fs {
        view.fs_seen = true;
    }
    if view.fs_drove && view.fs_seen && !win_fs {
        view.fs_drove = false;
        view.fs_seen = false;
        view.game_fs = false;
    }
    if !view.game_fs && view.fs_drove && win_fs {
        view.fs_drove = false;
        view.fs_seen = false;
        crate::set_fullscreen(ui.ctx(), false);
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Maximized(view.fs_prev_max));
    }
    let fs = view.game_fs && win_fs && view.session.is_some() && !view.viewport_dismissed;
    let pres_target = if fs { content } else { upper };
    // Presentation motion: the rect follows its target 1:1; a
    // fullscreen flip engages a fixed 0.45s cubic tween (same
    // profile the emulation view uses).
    {
        if !view.pres_anim.is_finite() {
            view.pres_anim = pres_target;
            view.pres_fs = fs;
            view.pres_t = 1.0;
        }
        if fs != view.pres_fs {
            view.pres_fs = fs;
            view.pres_from = view.pres_anim;
            view.pres_t = 0.0;
        }
        if view.pres_t < 1.0 {
            view.pres_t = (view.pres_t + dt / 0.45).min(1.0);
            let e = crate::ease_in_out_cubic(view.pres_t);
            view.pres_anim = if view.pres_t < 1.0 {
                crate::emulation::lerp_rect(view.pres_from, pres_target, e)
            } else {
                pres_target
            };
            ui.ctx().request_repaint();
        } else {
            view.pres_anim = pres_target;
        }
    }
    let pres_rect = view.pres_anim;
    // Hide the OS cursor while the game owns the screen — except
    // while the input tunnel needs a pointer to aim with.
    let want_hidden = fs && !view.input_tun;
    if want_hidden != view.cursor_hidden {
        view.cursor_hidden = want_hidden;
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::CursorVisible(!want_hidden));
    }
    // The grid/pane stay mounted under the capture layer — painted
    // only while the layer is translucent, interactive only once
    // it's mostly faded out.
    let covered = cap_a >= 0.999;
    let content_interactive = interactive && cap_a < 0.5;
    // In game-fullscreen the chrome (band, search, tabs) is not
    // painted at all.
    let chrome_hidden = fs && cap_a > 0.5;
    // A pill left open when Play fired would keep eating keys under
    // the capture layer — close it as the layer takes over.
    if covered && view.search.is_open() {
        view.search.close(ui.ctx());
    }

    // --- Kind tabs (settings-overlay style) ------------------------
    // Text tabs in a row — active is white with an underline,
    // inactive gray, hover lighter; a small count trails the label.
    let counts = view.kind_counts();
    let tab_font = FontId::proportional(19.0 * s);
    let tab_gap = 26.0 * s;
    let tab_row_h = 34.0 * s;
    let mut set_filter: Option<Option<Kind>> = None;
    let tabs: Vec<(Option<Kind>, String, usize)> = std::iter::once((None, "All".to_string(), view.entries.len()))
        .chain(Kind::CHIPS.iter().filter_map(|k| {
            counts
                .get(k)
                .map(|&n| (Some(*k), tab_label(*k).to_string(), n))
        }))
        .collect();
    let mut tab_x = upper.min.x + 40.0 * s;
    let tab_y = upper.min.y + 12.0 * s;
    let tabs_right = content.max.x - 300.0 * s; // keep clear of the search pill
    if !covered {
        for (this, label, n) in &tabs {
            let lg = painter.layout_no_wrap(label.clone(), tab_font.clone(), Color32::WHITE);
            let ng = painter.layout_no_wrap(
                format!(" {n}"),
                FontId::proportional(13.0 * s),
                Color32::WHITE,
            );
            let w = lg.size().x + ng.size().x + 4.0 * s;
            if tab_x + w > tabs_right {
                break;
            }
            let rect = Rect::from_min_size(Pos2::new(tab_x, tab_y), Vec2::new(w, tab_row_h));
            let resp = if content_interactive {
                ui.interact(rect, ui.scope_id().with(("decomp-tab", this.map(|k| k.label()))), Sense::click())
            } else {
                ui.interact(rect, ui.scope_id().with(("decomp-tab", this.map(|k| k.label()))), Sense::hover())
            };
            let active = view.filter == *this;
            let color = if active {
                Color32::WHITE
            } else if resp.hovered() {
                Color32::from_gray(200)
            } else {
                Color32::from_gray(140)
            };
            painter.galley(
                Pos2::new(rect.min.x, rect.min.y + 4.0 * s),
                lg,
                color.gamma_multiply(alpha),
            );
            painter.galley(
                Pos2::new(rect.min.x + rect.width() - ng.size().x - 2.0 * s, rect.min.y + 10.0 * s),
                ng,
                Color32::from_gray(120).gamma_multiply(alpha),
            );
            if active {
                painter.line_segment(
                    [
                        Pos2::new(rect.min.x, rect.max.y - 2.0 * s),
                        Pos2::new(rect.min.x + rect.width(), rect.max.y - 2.0 * s),
                    ],
                    Stroke::new(2.0 * s, Color32::WHITE.gamma_multiply(alpha)),
                );
            }
            if resp.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            if resp.clicked() {
                set_filter = Some(*this);
            }
            tab_x += w + tab_gap;
        }
        if let Some(f) = set_filter {
            view.set_kind_filter(f);
        }
    }

    // --- The shared item list ---------------------------------------
    // The exact widget the music and emulation views drive — the
    // perspective carousel and both card grids with their scroll
    // physics, scrollbar, marquee and context menus. The info pane
    // docks below and follows the focused card.
    let pane_h = 360.0 * s;
    let pane = Rect::from_min_max(
        Pos2::new(upper.min.x, upper.max.y - pane_h),
        upper.max,
    );
    let grid = Rect::from_min_max(
        Pos2::new(upper.min.x, tab_y + tab_row_h + 10.0 * s),
        Pos2::new(upper.max.x, pane.min.y - 4.0 * s),
    );

    // --- Filter drawer (EQ-style damped reveal) -------------------
    // The shelf is anchored at the footer separator — its top edge
    // lifts up over the info pane, which keeps its layout behind
    // the drawer, same as the audio view's EQ drawer.
    let filter_t = {
        let id = ui.scope_id().with("decomp-filter-drawer");
        let mut t = ui
            .ctx()
            .data_mut(|d| d.get_temp::<f32>(id))
            .unwrap_or(0.0);
        let target = if view.filter_open { 1.0 } else { 0.0 };
        let dt = ui.ctx().input(|i| i.stable_dt).min(0.1);
        t += (target - t) * (1.0 - (-dt * 8.0).exp());
        if (target - t).abs() < 0.0015 {
            t = target;
        }
        ui.ctx().data_mut(|d| d.insert_temp(id, t));
        if t != target {
            ui.ctx().request_repaint();
        }
        t
    };
    // Never smaller than the pane — the open drawer covers the whole
    // selected-entry area, extending over the list only when the
    // options need more room.
    let drawer_full = filter_drawer_height(&painter, view, upper.width(), upper.height(), s)
        .max(pane.height())
        .min(upper.height());
    let drawer = Rect::from_min_max(
        Pos2::new(upper.min.x, upper.max.y - drawer_full * filter_t),
        upper.max,
    );

    // Lazy cover fetches — the list reports which items are on
    // screen; the worker caches bytes to disk and `set_cover` swaps
    // the source into the row's decode pool when one lands.
    for p in view.cards.onscreen_paths(&view.search.text) {
        let slug = p.to_string_lossy().into_owned();
        if cover_cache(&slug).is_some_and(|c| c.is_file())
            || !view.cover_requested.insert(slug.clone())
        {
            continue;
        }
        if let Some(&ei) = view.slug_idx.get(&slug) {
            let _ = view.cover_req_tx.send((
                slug,
                view.entries[ei].cover_q.clone(),
                cover_cache(&view.entries[ei].slug),
            ));
        }
    }
    // Card status markers — the widget paints the dot + the download
    // strip: green installed, amber update, blue while downloading.
    {
        let installed = &view.installed;
        let remote = &view.remote;
        let dls = &view.downloads;
        view.cards.update_markers(&move |p| {
            let slug = p.to_string_lossy();
            if let Some(dl) = dls.get(slug.as_ref()) {
                let g = dl.lock().unwrap_or_else(|e| e.into_inner());
                return (
                    Some(Color32::from_rgb(70, 130, 200)),
                    Some(g.frac.clamp(0.0, 1.0)),
                );
            }
            let stamp = installed.get(slug.as_ref()).and_then(|s| s.as_deref());
            match card_action(stamp, remote.get(slug.as_ref())) {
                Action::Update => (Some(Color32::from_rgb(255, 190, 90)), None),
                Action::Open => (Some(Color32::from_rgb(120, 220, 120)), None),
                Action::Install => (None, None),
            }
        });
    }

    let mode = match emu_cfg.decomp_layout_mode.min(2) {
        1 => Mode::VerticalGrid,
        2 => Mode::HorizontalGrid,
        _ => Mode::Carousel,
    };
    // The focused card wears the pulsing selection ring and feeds
    // the info pane — the same item the overlay labels describe.
    let focused = view.cards.focused_path(&view.search.text);
    // The card menu's "Uninstall" shows only on installed slugs —
    // the stamp cache is already fresh enough for menu state.
    view.cards.decomp_installed = view
        .installed
        .iter()
        .filter(|(_, s)| s.is_some())
        .map(|(slug, _)| PathBuf::from(slug))
        .collect();
    // An opaque capture layer owns `upper` — skip the list draw and
    // its input entirely (it still runs underneath via `cap_t`'s
    // fade, same as the emulation view).
    let action = if !covered {
        view.cards.ui(
            ui,
            grid,
            grid,
            &mut view.row,
            focused.as_deref(),
            false,
            &view.search.text,
            cfg,
            mode,
            alpha,
            content_interactive,
            false,
            s,
        )
    } else {
        carousel::Action::None
    };
    match action {
        // Double-click / "Play" and "Install / Update" both resolve
        // to the entry's card action — Install→download, Update→
        // download, Open→launch.
        carousel::Action::Play(p) | carousel::Action::Download(p) => {
            view.activate_slug(&p.to_string_lossy());
        }
        carousel::Action::OpenFolder(p) => {
            if let Some(dir) = install_dir(&p.to_string_lossy()) {
                open_in_file_manager(&dir);
            }
        }
        carousel::Action::Uninstall(p) => {
            let slug = p.to_string_lossy().into_owned();
            if let Some(&i) = view.slug_idx.get(&slug) {
                let e = view.entries[i].clone();
                view.request_uninstall(&e);
            }
        }
        // The catalog menu's "Refresh catalog" reuses the ROM menu's
        // "Scan Folder" slot.
        carousel::Action::ScanFolder => view.check_updates(ui.ctx()),
        _ => {}
    }
    if view.catalog_loading && view.entries.is_empty() {
        painter.text(
            grid.center(),
            Align2::CENTER_CENTER,
            "Loading catalog…",
            FontId::proportional(15.0 * s),
            Color32::from_gray(180).gamma_multiply(alpha),
        );
    }

    // --- Selected-entry info pane (always docked at the bottom) ------
    if !covered {
        let sel_idx = focused
            .as_deref()
            .and_then(|p| view.slug_idx.get(&*p.to_string_lossy()).copied());
        info_pane(ui, pane, view, sel_idx, alpha, content_interactive, s);

        // The drawer paints last, over the pane — it slides up from the
        // footer separator and covers whatever sits behind it.
        if filter_t > 0.001 {
            painter.rect_filled(drawer, 0.0, Color32::BLACK.gamma_multiply(alpha));
            painter.hline(
                drawer.min.x..=drawer.max.x,
                drawer.min.y,
                Stroke::new(
                    s.max(1.0),
                    Color32::from_white_alpha((51.0 * alpha) as u8),
                ),
            );
            filter_drawer(ui, &painter, drawer, drawer_full, view, alpha, content_interactive, s);
        }

        // The item search pill — docked in the top-right corner with a
        // small top/right margin so the cards never slide under it.
        if !chrome_hidden {
            view.search.ui(
                ui,
                content.max.x - 18.0 * s,
                upper.min.y + 10.0 * s,
                "Search",
                true,
                false,
                alpha,
                content_interactive,
                s,
            );
        }
    }

    // --- Capture layer ---------------------------------------------
    // The same presentation the emulation view uses: an opaque black
    // backing fading in over the grid, the newest WGC frame inside
    // (aspect-fit with the near-fill crop glide), a centered status
    // line until the first frame lands. In game-fullscreen the rect
    // glides from `upper` to `content`.
    if view.cap_t > 0.0 {
        draw_capture(ui, &painter, pres_rect, pres_target, view, alpha * cap_a, s);
    }
    // The input tunnel — while it owns the session, pointer/key
    // events inside the frame rect PostMessage to the hidden game
    // window so its own UI is reachable. Esc releases it (the global
    // chain runs after — the forward loop never sends Escape).
    crate::emulation::tunnel_input(
        ui,
        view.session.as_ref(),
        crate::emulation::TunIo {
            on: &mut view.input_tun,
            map: view.tun_map,
            frame: view
                .frame_tex
                .as_ref()
                .map(|t| (t.size()[0] as f64, t.size()[1] as f64)),
            buttons: &mut view.tun_buttons,
            mods: &mut view.tun_mods,
            sent: &mut view.tun_sent,
            dbg: &mut view.tun_dbg,
        },
        interactive && cap_a > 0.5,
    );
    // Click/double-click/context gestures ride the capture rect — a
    // double-click toggles game-fullscreen, the right-click menu
    // mirrors the band's session controls (same shape as the
    // emulation view's capture gestures).
    if view.session.is_some() && cap_a > 0.5 {
        let cap =
            ui.interact(pres_rect, ui.scope_id().with("decomp-capture"), Sense::click());
        if cap.double_clicked() && !view.input_tun {
            view.toggle_fullscreen();
        }
        let mut close_game = false;
        cap.context_menu(|ui| {
            if ui
                .button(if view.game_fs {
                    "Exit Fullscreen"
                } else {
                    "Fullscreen"
                })
                .clicked()
            {
                view.toggle_fullscreen();
                ui.close();
            }
            // SetCaptureAspectRatio — resizes the game window so the
            // capture's native aspect changes; saved to the DECOMP
            // render slot like the per-section emu one.
            ui.menu_button("Aspect Ratio", |ui| {
                for (key, label) in [
                    ("handler", "Handler Default"),
                    ("4:3", "4:3"),
                    ("16:9", "16:9"),
                    ("21:9", "21:9"),
                    ("32:9", "32:9"),
                    ("1:1", "1:1"),
                ] {
                    if ui
                        .selectable_label(view.render.aspect_key == key, label)
                        .clicked()
                    {
                        if view.render.aspect_key != key {
                            view.render.aspect_key = key.to_string();
                            view.render_dirty = true;
                            #[cfg(windows)]
                            if let Some(session) = &view.session {
                                let hwnd = session.hwnd();
                                if !hwnd.is_invalid() {
                                    if let Some(ratio) =
                                        crate::emulation::aspect_ratio_of(key)
                                    {
                                        xenia::apply_aspect_ratio(hwnd, ratio);
                                    }
                                    view.aspect_applied = true;
                                }
                            }
                        }
                        ui.close();
                    }
                }
            });
            if ui.button("Close Game").clicked() {
                close_game = true;
                ui.close();
            }
        });
        if close_game {
            view.request_close();
        }
    }

    // --- Capture render-options panel (the settings overlay) ------
    // Same RenderOptionsOverlay the emulation view shows — slides in
    // from the right over the item area; band + pane chrome stay.
    if !chrome_hidden {
        let item_area = Rect::from_min_max(content.min, Pos2::new(content.max.x, band.min.y));
        if crate::emulation::render_panel(
            ui,
            &mut view.render_open,
            &mut view.render,
            view.session.is_some(),
            view.cap_shader.as_ref(),
            item_area,
            alpha,
            interactive,
            s,
        ) {
            view.render_dirty = true;
        }
    }

    // --- Bottom band -----------------------------------------------
    if !chrome_hidden {
    painter.rect_filled(
        band,
        0.0,
        Color32::from_black_alpha((230.0 * alpha) as u8),
    );
    painter.rect_filled(
        band,
        0.0,
        Color32::from_black_alpha((230.0 * alpha) as u8),
    );
    let line = Stroke::new(s.max(1.0), Color32::from_white_alpha((51.0 * alpha) as u8));
    painter.hline(content.min.x..=content.max.x, band.min.y, line);
    let cy = band.center().y;
    painter.text(
        Pos2::new(band.min.x + 40.0 * s, cy - 12.0 * s),
        Align2::LEFT_CENTER,
        "RE/DECOMPILATION",
        FontId::proportional(16.0 * s),
        Color32::WHITE.gamma_multiply(alpha),
    );
    let installed = view.installed.values().flatten().count();
    let updates = view
        .entries
        .iter()
        .filter(|e| {
            card_action(
                view.installed.get(&e.slug).and_then(|s| s.as_deref()),
                view.remote.get(&e.slug),
            ) == Action::Update
        })
        .count();
    let sub = view
        .notice
        .clone()
        .unwrap_or_else(|| {
            format!(
                "{} of {} projects · {} installed{}{}",
                view.shown_count(),
                view.entries.len(),
                installed,
                if updates > 0 {
                    format!(" · {updates} update{}", if updates == 1 { "" } else { "s" })
                } else {
                    String::new()
                },
                if view.catalog_loading {
                    " · fetching catalog…"
                } else if view.check_running {
                    " · checking…"
                } else {
                    ""
                },
            )
        });
    painter.text(
        Pos2::new(band.min.x + 40.0 * s, cy + 14.0 * s),
        Align2::LEFT_CENTER,
        crate::ellipsize(&painter, &sub, &FontId::proportional(12.0 * s), band.width() * 0.6),
        FontId::proportional(12.0 * s),
        Color32::from_gray(150).gamma_multiply(alpha),
    );

    // Right cluster: the filter-drawer toggle left of the layout
    // icon — a funnel, lit while the drawer is open or a platform/
    // status filter is narrowing the list.
    let frect = Rect::from_center_size(
        Pos2::new(band.max.x - 110.0 * s, cy),
        Vec2::splat(60.0 * s),
    );
    // List controls — dead while the capture owns the screen (the
    // session controls stay live; they're the escape hatch).
    let fr = if content_interactive {
        ui.interact(frect, ui.scope_id().with("decomp-filter"), Sense::click())
    } else {
        ui.interact(frect, ui.scope_id().with("decomp-filter"), Sense::hover())
    }
    .on_hover_text("Filters — platform, status, sort");
    if fr.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    crate::transport::icon_filter(
        &painter,
        frect.shrink(10.0 * s),
        view.filter_open || view.filters_active(),
        alpha,
    );
    if fr.clicked() {
        view.filter_open = !view.filter_open;
    }

    // Right cluster: the card-layout toggle — the same 60px icon
    // slot and glyph the music band uses for its view modes. It
    // persists Emulation.DecompLayoutMode — independent of the
    // emulation view's own CoverLayoutMode.
    let trect = Rect::from_center_size(Pos2::new(band.max.x - 50.0 * s, cy), Vec2::splat(60.0 * s));
    let tr = if content_interactive {
        ui.interact(trect, ui.scope_id().with("decomp-layout"), Sense::click())
    } else {
        ui.interact(trect, ui.scope_id().with("decomp-layout"), Sense::hover())
    }
    .on_hover_text(match emu_cfg.decomp_layout_mode.min(2) {
        0 => "Carousel — click for the vertical card grid",
        1 => "Vertical cards — click for horizontal rows",
        _ => "Horizontal cards — click for the carousel strip",
    });
    if tr.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    crate::transport::icon_layout(
        &painter,
        trect.shrink(10.0 * s),
        emu_cfg.decomp_layout_mode.min(2),
        alpha,
    );
    if tr.clicked() {
        emu_cfg.decomp_layout_mode = (emu_cfg.decomp_layout_mode.min(2) + 1) % 3;
        view.settings_dirty = true;
    }

    // Session controls — Lacrima's IsEmulatorRunning set, same form
    // as the emulation band's center cluster: the ⇄ list/capture
    // switch left of the pause zone, pause/play (process suspend/
    // resume) dead-center, the input tunnel right — 110px pitch with
    // the 80px pause slot. The power glyph stays in the right
    // cluster next to the filter icon (close-confirm via the
    // caller's prompt).
    let mut toggle_viewport = false;
    let mut toggle_pause = false;
    let mut toggle_tun = false;
    let mut close_game = false;
    if let Some(session) = &view.session {
        let pc = band.center();

        let vp = Rect::from_center_size(
            Pos2::new(pc.x - 110.0 * s, cy),
            Vec2::splat(60.0 * s),
        );
        let r = if interactive {
            ui.interact(vp, ui.scope_id().with("decomp-viewport"), Sense::click())
        } else {
            ui.interact(vp, ui.scope_id().with("decomp-viewport"), Sense::hover())
        }
        .on_hover_text(if view.viewport_dismissed {
            "Back to Game"
        } else {
            "Switch to List"
        });
        toggle_viewport = r.clicked();
        if r.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        crate::transport::icon_viewport(&painter, vp, !view.viewport_dismissed, alpha);

        let pp = Rect::from_center_size(pc, Vec2::splat(80.0 * s));
        let r = if interactive {
            ui.interact(pp, ui.scope_id().with("decomp-pause"), Sense::click())
        } else {
            ui.interact(pp, ui.scope_id().with("decomp-pause"), Sense::hover())
        }
        .on_hover_text(if session.is_paused() {
            "Resume Game"
        } else {
            "Pause Game"
        });
        toggle_pause = r.clicked();
        if r.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        crate::transport::icon_play(&painter, pp, !session.is_paused(), alpha);

        // Input tunnel — while lit, clicks/keys inside the frame
        // reach the game's hidden window so its own dialogs are
        // usable in the captured stream. On Linux the same applies
        // when the session runs embedded in the nested compositor —
        // an external-window fallback owns its input itself.
        let tun_ok = if cfg!(windows) {
            true
        } else {
            session.embedded()
        };
        if tun_ok {
            let tb = Rect::from_center_size(
                Pos2::new(pc.x + 110.0 * s, cy),
                Vec2::splat(60.0 * s),
            );
            let r = if interactive {
                ui.interact(tb, ui.scope_id().with("decomp-tun"), Sense::click())
            } else {
                ui.interact(tb, ui.scope_id().with("decomp-tun"), Sense::hover())
            }
            .on_hover_text(if view.input_tun {
                "Stop Controlling Game (Esc)"
            } else {
                "Control Game — clicks and keys reach the game"
            });
            toggle_tun = r.clicked();
            if r.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            crate::transport::icon_pointer(&painter, tb, view.input_tun, alpha);
        }

        let pw = Rect::from_center_size(
            Pos2::new(band.max.x - 230.0 * s, cy),
            Vec2::splat(60.0 * s),
        );
        let r = if interactive {
            ui.interact(pw, ui.scope_id().with("decomp-close"), Sense::click())
        } else {
            ui.interact(pw, ui.scope_id().with("decomp-close"), Sense::hover())
        }
        .on_hover_text("Close Game");
        close_game = r.clicked();
        if r.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        // Same 10px inset the filter/layout glyphs get — the power
        // arc fills its canvas edge-to-edge, so an unshrunk box
        // reads oversized next to them.
        crate::transport::icon_power(&painter, pw.shrink(10.0 * s), alpha);

        // Capture-settings toggle (Lacrima's ToggleRenderOptions) —
        // the sliders glyph between close and the filter icon,
        // same cluster order as the emulation band.
        let rb = Rect::from_center_size(
            Pos2::new(band.max.x - 170.0 * s, cy),
            Vec2::splat(60.0 * s),
        );
        let r = if interactive {
            ui.interact(rb, ui.scope_id().with("decomp-render"), Sense::click())
        } else {
            ui.interact(rb, ui.scope_id().with("decomp-render"), Sense::hover())
        }
        .on_hover_text("Capture Settings");
        if r.clicked() {
            view.render_open = !view.render_open;
        }
        if r.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        crate::transport::icon_sliders(
            &painter,
            rb.shrink(10.0 * s),
            view.render_open,
            alpha,
        );
    }
    if toggle_viewport {
        view.toggle_viewport();
    }
    if toggle_pause {
        if let Some(session) = &view.session {
            session.set_paused(!session.is_paused());
        }
    }
    if toggle_tun {
        view.toggle_input_tunnel();
    }
    if close_game {
        view.request_close();
    }
    }
}


// ---- Capture presentation --------------------------------------------------

/// The capture presentation layer — the same form the emulation
/// view's `draw_session` uses: an opaque black backing (never a
/// compositor hole), the newest WGC frame aspect-fit with the
/// near-fill crop glide, and a centered status line while the game
/// window resolves. No letterbox art, pillar crop, stretch setting
/// or tunnel — decomp games render at the window's own aspect.
fn draw_capture(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    rect: Rect,
    target: Rect,
    view: &mut DecompView,
    alpha: f32,
    s: f32,
) {
    painter.rect_filled(rect, 0.0, Color32::BLACK.gamma_multiply(alpha));

    let Some(session) = &view.session else {
        view.tun_map = None;
        // Session ended — the last frame (if any) still fades out.
        // The imported GPU ring stays alive (its wgpu textures hold
        // the shared allocation) so the zero-copy path fades too.
        let (fw, fh) = if let Some(g) = &view.gpu_cap {
            (g.size[0] as usize, g.size[1] as usize)
        } else if let Some(t) = &view.frame_tex {
            (t.size()[0], t.size()[1])
        } else {
            return;
        };
        let (dest, uv) = crate::emulation::frame_dest_uv(
            rect,
            fw,
            fh,
            view.crop_t,
            view.render.stretch,
            view.fx.pillar_cur,
        );
        let seq = view.frame_seq;
        let adjust = crate::emulation::render_adjust(&view.render);
        let cb = view
            .gpu_cap
            .as_ref()
            .map(|gc| {
                (
                    gc.bgs[(seq % 3) as usize].clone(),
                    gc.fence.clone(),
                    gc.size,
                )
            })
            .and_then(|(bg, fence, size)| {
                view.cap_shader.as_mut().and_then(|cs| {
                    cs.callback_gpu(
                        &view.render.shader,
                        dest,
                        uv,
                        alpha,
                        size,
                        seq as f32,
                        ui.ctx().pixels_per_point(),
                        bg,
                        // The writer is gone — the fence was already
                        // signaled, but the wait is cheap + strictly
                        // correct cross-API ordering.
                        Some((fence, seq)),
                        adjust,
                    )
                })
            });
        match cb {
            Some(cb) => {
                painter.add(eframe::egui_wgpu::Callback::new_paint_callback(dest, cb));
            }
            None => {
                if let Some(tex) = &view.frame_tex {
                    painter.image(tex.id(), dest, uv, Color32::WHITE.gamma_multiply(alpha));
                }
            }
        }
        return;
    };
    let status = session.status();
    let game_name = view.session_name.clone();
    // Remove-side-pillars gates the frame's bar detection (no
    // letterbox-art fill here — decomp entries have no .meta art).
    let pillar_active = view.render.remove_pillars;

    // Zero-copy import — the capture publishes a new ring generation
    // on every size change; a failed import pins the session to the
    // CPU mirror until a new generation appears. The capture side's
    // own failure (no ring at all) surfaces the same way.
    if view.gpu_err.is_none() {
        if let Some(e) = session
            .gpu_state()
            .err
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            view.gpu_err = Some(e);
        }
    }
    if view.cap_shader.is_some() {
        let share = session
            .gpu_state()
            .share
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(share) = share {
            if view.gpu_cap.as_ref().map(|g| g.generation) != Some(share.generation)
                && view.gpu_failed_gen != share.generation
            {
                match view.cap_shader.as_mut().map(|cs| cs.import_share(&share)) {
                    Some(Ok(r)) => {
                        view.gpu_cap = Some(r);
                        view.gpu_failed_gen = 0;
                        view.gpu_err = None;
                    }
                    Some(Err(e)) => {
                        view.gpu_failed_gen = share.generation;
                        view.gpu_err = Some(e);
                    }
                    None => {}
                }
            }
        }
    }
    // The capture's CPU readback only earns its keep when something
    // actually needs pixels there — display on the mirror path, or
    // pillarbox detection.
    session
        .render()
        .want_cpu
        .store(pillar_active || view.gpu_cap.is_none(), Ordering::Relaxed);

    // Pull the newest captured frame — on the zero-copy path only
    // the ring seq matters (stats + shader FrameCount); on the CPU
    // mirror the slot's pixels are swapped in and uploaded. Skipped
    // while the layer is fully faded out so a dismissed capture
    // doesn't burn uploads.
    if alpha > 0.02 {
        let gpu_live = view.gpu_cap.is_some();
        let mut new_seq: Option<u64> = None;
        // The CPU slot is consumed when the display needs its pixels
        // (mirror path) or pillarbox detection does.
        if !gpu_live || pillar_active {
            // Swap the frame into `frame_buf` — the slot gets the old
            // buffer back so the capture thread's scratch<->slot
            // ping-pong stays allocation-free.
            let mut slot = session.frames().lock().unwrap_or_else(|p| p.into_inner());
            if slot.seq != view.slot_seq
                && slot.w > 0
                && slot.h > 0
                && slot.rgba.len() == slot.w * slot.h * 4
            {
                let (w, h) = (slot.w, slot.h);
                std::mem::swap(&mut slot.rgba, &mut view.frame_buf);
                let seq = slot.seq;
                drop(slot);
                view.slot_seq = seq;
                if !gpu_live {
                    let img = egui::ColorImage {
                        size: [w, h],
                        source_size: Vec2::new(w as f32, h as f32),
                        // Opaque frames — premult == unmult.
                        pixels: bytemuck::cast_slice::<u8, egui::Color32>(&view.frame_buf)
                            .to_vec(),
                    };
                    match &mut view.frame_tex {
                        Some(tex) if tex.size() == [w, h] => {
                            tex.set(img, egui::TextureOptions::LINEAR)
                        }
                        _ => {
                            view.frame_tex = Some(ui.ctx().load_texture(
                                "decomp-capture",
                                img,
                                egui::TextureOptions::LINEAR,
                            ));
                        }
                    }
                    // The GPU shader path samples its own texture —
                    // upload the same frame straight from the staging
                    // buffer (dedup'd by seq inside).
                    if !view.render.shader.is_empty() {
                        if let Some(cs) = &mut view.cap_shader {
                            cs.upload_frame(seq, &view.frame_buf, w as u32, h as u32);
                        }
                    }
                    new_seq = Some(seq);
                }
                // Remove-side-pillars: bar detection on the mirrored
                // pixels + the 6-stable-frame debounce (no locked
                // per-ROM crop — decomp entries have no .meta).
                if pillar_active {
                    crate::emulation::fx_pillar_detect(&mut view.fx, &view.frame_buf, w, h);
                }
            }
        }
        // Zero-copy path — the ring's signaled seq IS the frame
        // counter (and the fence value to wait on before sampling).
        if gpu_live {
            let seq = session.gpu_state().seq.load(Ordering::Acquire);
            if seq != view.frame_seq {
                new_seq = Some(seq);
            }
        }
        if let Some(seq) = new_seq {
            view.frame_seq = seq;
            // Stats-overlay bookkeeping — capture fps + the rolling
            // frametime ring.
            crate::emulation::fx_stats_tick(&mut view.fx);
        }
    }

    // Ease the aspect-fill blend — the crop/letterbox glides instead
    // of snapping when the destination aspect crosses the ~12% fill
    // threshold (band resizes, fullscreen transitions).
    let frame_dims = if let Some(g) = &view.gpu_cap {
        Some((g.size[0] as usize, g.size[1] as usize))
    } else {
        view.frame_tex.as_ref().map(|t| (t.size()[0], t.size()[1]))
    };
    if let Some((fw, fh)) = frame_dims {
        // The pillar crop replaces the frame's aspect with the
        // measured content's — the fill decision uses the debounced
        // TARGET insets so it can't flip mid-tween (crop_t bounce).
        let eff =
            ((fw as f32 - view.fx.pillar_tgt.0 as f32 - view.fx.pillar_tgt.1 as f32).max(1.0))
                / fh as f32;
        // `want` uses the TARGET aspect, not the animating rect's —
        // otherwise it flips as the easing aspect sweeps through the
        // fill band mid-transition and crop_t reverses.
        let dst = target.width() / target.height();
        let want = match view.render.stretch {
            1 | 2 => 1.0, // UniformToFill / Fill always cover
            _ => ((eff / dst - 1.0).abs() < 0.12) as u8 as f32,
        };
        let dt = ui.ctx().input(|i| i.stable_dt).min(0.1);
        // Tween the pillar insets toward the debounced target — off
        // tweens back to (0,0) so the bars slide back in.
        crate::emulation::fx_pillar_tween(&mut view.fx, pillar_active, dt, ui.ctx());
        if want != view.crop_target {
            view.crop_from = view.crop_t;
            view.crop_target = want;
            view.crop_prog = 0.0;
        }
        if view.crop_prog < 1.0 {
            view.crop_prog = (view.crop_prog + dt / 0.55).min(1.0);
            let e = crate::ease_in_out_cubic(view.crop_prog);
            view.crop_t = view.crop_from + (view.crop_target - view.crop_from) * e;
            if view.crop_prog < 1.0 {
                ui.ctx().request_repaint();
            } else {
                view.crop_t = view.crop_target;
            }
        }
        let (dest, uv) = crate::emulation::frame_dest_uv(
            rect,
            fw,
            fh,
            view.crop_t,
            view.render.stretch,
            view.fx.pillar_cur,
        );
        // The tunnel maps screen → frame → client through this same
        // transform — refresh it wherever the draw moved to.
        view.tun_map = Some((dest, uv));
        // The zero-copy ring drives the draw when imported — the
        // newest signaled slot is sampled in place after the D3D12
        // queue waits on the cross-API fence. Otherwise the CPU
        // mirror path: named shader via PaintCallback or plain blit.
        let seq = view.frame_seq;
        let adjust = crate::emulation::render_adjust(&view.render);
        let cb = match view.gpu_cap.as_ref().map(|gc| {
            (
                gc.bgs[(seq % 3) as usize].clone(),
                gc.fence.clone(),
                gc.size,
            )
        }) {
            Some((bg, fence, size)) => view.cap_shader.as_mut().and_then(|cs| {
                cs.callback_gpu(
                    &view.render.shader,
                    dest,
                    uv,
                    alpha,
                    size,
                    seq as f32,
                    ui.ctx().pixels_per_point(),
                    bg,
                    Some((fence, seq)),
                    adjust,
                )
            }),
            _ => view.cap_shader.as_mut().and_then(|cs| {
                cs.callback(
                    &view.render.shader,
                    dest,
                    uv,
                    alpha,
                    [fw as u32, fh as u32],
                    view.frame_seq as f32,
                    ui.ctx().pixels_per_point(),
                )
            }),
        };
        match cb {
            Some(cb) => {
                // Must be the egui_wgpu::Callback wrapper — the
                // renderer downcasts the Arc to that exact type.
                painter.add(eframe::egui_wgpu::Callback::new_paint_callback(dest, cb));
            }
            None => {
                if let Some(tex) = &view.frame_tex {
                    painter.image(tex.id(), dest, uv, Color32::WHITE.gamma_multiply(alpha));
                } else if view.gpu_cap.is_some() {
                    // The interop path is live but its pipeline failed
                    // — surface it instead of a silent black frame.
                    view.gpu_err = Some("capture pipeline build failed".to_string());
                }
            }
        }
    }

    // Centered status — the launch gate while resolving, failure/
    // exit notices; nothing overlays a live capture.
    let msg = match &status {
        xenia::Status::Resolving => Some(format!("Launching {game_name}…")),
        xenia::Status::Failed(e) => Some(format!("Capture failed: {e}")),
        xenia::Status::Exited => Some("Game exited".into()),
        xenia::Status::Capturing => {
            (view.frame_seq == 0).then(|| format!("Waiting for frames — {game_name}"))
        }
    };
    if let Some(msg) = msg {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            msg,
            FontId::proportional(24.0 * s),
            Color32::from_gray(160).gamma_multiply(alpha),
        );
    }

    // Statistics overlay (Lacrima's PortalCaptureStatsOverlay) —
    // the same bottom-left panel the emulation capture draws.
    if view.render.stats_overlay && matches!(&status, xenia::Status::Capturing) {
        crate::emulation::draw_stats_overlay(
            painter,
            rect,
            &mut view.fx,
            &view.render,
            view.gpu_cap.is_some(),
            &view.gpu_err,
            alpha,
            s,
        );
    }

    // Input-tunnel state badge — bottom-right, doubles as the
    // diagnostic (the counter must rise on input; if it doesn't, our
    // side is dead; if it does, the game side ignores them).
    if view.input_tun {
        let label = match &view.tun_dbg {
            Some(e) => format!("INPUT → GAME — {e}"),
            None => format!("INPUT → GAME — {} msgs", view.tun_sent),
        };
        let pos = Pos2::new(rect.max.x - 16.0 * s, rect.max.y - 16.0 * s);
        painter.text(
            pos,
            Align2::RIGHT_BOTTOM,
            label,
            FontId::proportional(15.0 * s),
            Color32::from_rgb(0x60, 0xD8, 0x80).gamma_multiply(alpha),
        );
    }
}

/// `painter.layout_job` equivalent — the card painter doesn't carry
/// a Ui, so lay the title job out through the painter's context.
fn ui_fonts_job(painter: &egui::Painter, job: egui::text::LayoutJob) -> std::sync::Arc<egui::Galley> {
    painter.ctx().fonts_mut(|f| f.layout_job(job))
}

// ---- Selected-entry info pane ---------------------------------------------

/// The docked pane below the cards — the focused entry's cover,
/// catalog metadata, setup notes and the Install/Update/Play/Open
/// actions, all live as the focus moves.
fn info_pane(
    ui: &mut egui::Ui,
    pane: Rect,
    view: &mut DecompView,
    e_idx: Option<usize>,
    alpha: f32,
    interactive: bool,
    s: f32,
) {
    let painter = ui.painter().clone();
    painter.line_segment(
        [pane.left_top(), pane.right_top()],
        Stroke::new(
            s.max(1.0),
            Color32::from_white_alpha((40.0 * alpha) as u8),
        ),
    );

    let e = e_idx.map(|i| view.entries[i].clone());
    let Some(e) = e else {
        painter.text(
            pane.center(),
            Align2::CENTER_CENTER,
            "Select a project — its details and actions show here",
            FontId::proportional(13.0 * s),
            Color32::from_gray(140).gamma_multiply(alpha),
        );
        return;
    };

    // The pane needs the cover even when the card itself scrolled
    // out of view.
    if !cover_cache(&e.slug).is_some_and(|c| c.is_file())
        && view.cover_requested.insert(e.slug.clone())
    {
        let _ = view.cover_req_tx.send((
            e.slug.clone(),
            e.cover_q.clone(),
            cover_cache(&e.slug),
        ));
    }

    let pad_x = 40.0 * s;
    let dir = install_dir(&e.slug);
    let stamp = dir.as_deref().and_then(installed_stamp);
    let disp = dir
        .as_deref()
        .and_then(installed_release)
        .or_else(|| stamp.clone());
    let act = card_action(stamp.as_deref(), view.remote.get(&e.slug));
    let dl = view.downloads.get(&e.slug).cloned();
    let installed = stamp.is_some();
    let (status_txt, status_c) = if let Some(dl) = &dl {
        (
            dl.lock().unwrap_or_else(|e| e.into_inner()).stage.clone(),
            Color32::from_rgb(120, 180, 255),
        )
    } else {
        match (disp.as_deref(), act) {
            (Some(st), Action::Update) => (
                format!("Installed · {st} — update available"),
                Color32::from_rgb(255, 190, 90),
            ),
            (Some(st), _) => (
                format!("Installed · {st}"),
                Color32::from_rgb(120, 200, 120),
            ),
            (None, _) if e.repo.is_empty() => (
                "Listed — no repository yet".to_string(),
                Color32::from_gray(150),
            ),
            (None, _) => (String::new(), Color32::from_gray(150)),
        }
    };

    // --- Left column: the cover, full pane height ------------------
    let slot_h = pane.height() - 28.0 * s;
    let slot = Rect::from_min_size(
        Pos2::new(pane.min.x + pad_x, pane.min.y + 14.0 * s),
        Vec2::new(slot_h * 0.74, slot_h),
    );
    painter.rect_filled(
        slot,
        CornerRadius::same((8.0 * s).round() as u8),
        Color32::from_white_alpha((6.0 * alpha) as u8),
    );
    let pane_tex = cover_cache(&e.slug)
        .filter(|c| c.is_file())
        .and_then(|c| view.row.cover_tex(ui.ctx(), &CoverSrc::File(c)));
    if let Some(tex) = &pane_tex {
        let dest = fit_rect(
            slot.shrink(1.0),
            tex.size()[0] as f32,
            tex.size()[1] as f32,
        );
        painter.image(
            tex.id(),
            dest,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE.gamma_multiply(alpha),
        );
    }
    painter.rect_stroke(
        slot,
        CornerRadius::same((8.0 * s).round() as u8),
        Stroke::new(
            s.max(1.0),
            Color32::from_white_alpha((30.0 * alpha) as u8),
        ),
        StrokeKind::Inside,
    );

    // --- Info column (fills the rest of the pane) -------------------
    let tx = slot.max.x + 18.0 * s;
    let tw = (pane.max.x - pad_x - tx).max(60.0 * s);
    // The action row sits under the info text, pinned to the pane's
    // bottom edge — the status line rides right above it.
    let bh = 36.0 * s;
    let gap = 10.0 * s;
    let row_top = pane.max.y - 14.0 * s - bh;
    let mut ty = pane.min.y + 24.0 * s;
    let lh = 26.0 * s;

    let title = if e.featured {
        format!("★ {}", e.name)
    } else {
        e.name.clone()
    };
    painter.text(
        Pos2::new(tx, ty),
        Align2::LEFT_CENTER,
        crate::ellipsize(&painter, &title, &FontId::proportional(22.0 * s), tw),
        FontId::proportional(22.0 * s),
        Color32::WHITE.gamma_multiply(alpha),
    );
    ty += lh + 3.0 * s;

    // Badges: console · kind · version + a colored status flag.
    let mut badges = format!(
        "{} · {}",
        catalog::console_label(&e.console),
        e.kind.label()
    );
    if !e.version.is_empty() {
        badges.push_str(&format!(" · {}", e.version));
    }
    let flag = if e.takedown {
        Some(("Takedown", Color32::from_rgb(255, 110, 110)))
    } else if e.abandoned {
        Some(("Abandoned", Color32::from_rgb(255, 160, 110)))
    } else {
        catalog::status_label(&e.status)
    };
    let badges_txt = crate::ellipsize(&painter, &badges, &FontId::proportional(14.0 * s), tw * 0.7);
    let bw = painter
        .layout_no_wrap(badges_txt.clone(), FontId::proportional(14.0 * s), Color32::WHITE)
        .size()
        .x;
    painter.text(
        Pos2::new(tx, ty),
        Align2::LEFT_CENTER,
        badges_txt,
        FontId::proportional(14.0 * s),
        Color32::from_gray(170).gamma_multiply(alpha),
    );
    if let Some((ft, fc)) = flag {
        painter.text(
            Pos2::new(tx + bw + 10.0 * s, ty),
            Align2::LEFT_CENTER,
            ft,
            FontId::proportional(14.0 * s),
            fc.gamma_multiply(alpha),
        );
    }
    ty += lh;

    // Repo + homepage — small clickable links.
    let mut link_clicks: Vec<String> = Vec::new();
    if !e.repo.is_empty() {
        let g = painter.layout_no_wrap(
            e.repo.clone(),
            FontId::monospace(14.0 * s),
            Color32::from_rgb(120, 180, 255),
        );
        let rr = Rect::from_min_size(Pos2::new(tx, ty - g.size().y * 0.5), g.size());
        let resp = if interactive {
            ui.interact(rr, ui.scope_id().with("decomp-pane-repo"), Sense::click())
        } else {
            ui.interact(rr, ui.scope_id().with("decomp-pane-repo"), Sense::hover())
        };
        let col = if resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            Color32::from_rgb(170, 210, 255)
        } else {
            Color32::from_rgb(120, 180, 255)
        };
        painter.galley(rr.min, g, col.gamma_multiply(alpha));
        if resp.clicked() {
            link_clicks.push(format!("https://github.com/{}", e.repo));
        }
        let mut lx = rr.max.x + 14.0 * s;
        if !e.homepage.is_empty() {
            let hg = painter.layout_no_wrap(
                "homepage".to_string(),
                FontId::proportional(13.0 * s),
                Color32::from_rgb(120, 180, 255),
            );
            let hr = Rect::from_min_size(Pos2::new(lx, ty - hg.size().y * 0.5), hg.size());
            let resp = if interactive {
                ui.interact(hr, ui.scope_id().with("decomp-pane-home"), Sense::click())
            } else {
                ui.interact(hr, ui.scope_id().with("decomp-pane-home"), Sense::hover())
            };
            let col = if resp.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                Color32::from_rgb(170, 210, 255)
            } else {
                Color32::from_rgb(120, 180, 255)
            };
            painter.galley(hr.min, hg, col.gamma_multiply(alpha));
            if resp.clicked() {
                link_clicks.push(e.homepage.clone());
            }
            lx = hr.max.x + 14.0 * s;
        }
        // First couple of curated links ride the same row.
        for (label, url) in e.links.iter().take(2) {
            let lg = painter.layout_no_wrap(
                label.clone(),
                FontId::proportional(13.0 * s),
                Color32::from_rgb(120, 180, 255),
            );
            if lx + lg.size().x > tx + tw {
                break;
            }
            let lr = Rect::from_min_size(Pos2::new(lx, ty - lg.size().y * 0.5), lg.size());
            let resp = if interactive {
                ui.interact(lr, ui.scope_id().with(("decomp-pane-link", url)), Sense::click())
            } else {
                ui.interact(lr, ui.scope_id().with(("decomp-pane-link", url)), Sense::hover())
            };
            let col = if resp.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                Color32::from_rgb(170, 210, 255)
            } else {
                Color32::from_rgb(120, 180, 255)
            };
            painter.galley(lr.min, lg, col.gamma_multiply(alpha));
            if resp.clicked() {
                link_clicks.push(url.clone());
            }
            lx = lr.max.x + 14.0 * s;
        }
    }
    ty += lh;

    // Meta — year · dev · author · toolchain · progress, one line.
    let mut meta: Vec<String> = Vec::new();
    if let Some(yr) = e.year {
        meta.push(yr.to_string());
    }
    if !e.developer.is_empty() {
        meta.push(e.developer.clone());
    }
    if !e.author.is_empty() && e.author != e.developer {
        meta.push(e.author.clone());
    }
    if !e.toolchain.is_empty() {
        meta.push(e.toolchain.clone());
    }
    if let Some(p) = e.progress {
        meta.push(format!("{p}%"));
    }
    if let Some(st) = e.stars {
        meta.push(format!("★ {st}"));
    }
    if !e.language.is_empty() {
        meta.push(e.language.clone());
    }
    if !meta.is_empty() {
        painter.text(
            Pos2::new(tx, ty),
            Align2::LEFT_CENTER,
            crate::ellipsize(
                &painter,
                &meta.join("  ·  "),
                &FontId::proportional(13.5 * s),
                tw,
            ),
            FontId::proportional(13.5 * s),
            Color32::from_gray(150).gamma_multiply(alpha),
        );
        ty += lh;
    }

    // Description — wrapped, up to two lines with ellipsis.
    if !e.description.is_empty() {
        let mut job = egui::text::LayoutJob::default();
        job.wrap = egui::text::TextWrapping {
            max_width: tw,
            max_rows: 3,
            break_anywhere: false,
            overflow_character: Some('…'),
        };
        job.append(
            &e.description,
            0.0,
            egui::text::TextFormat::simple(
                FontId::proportional(14.0 * s),
                Color32::PLACEHOLDER,
            ),
        );
        let g = ui_fonts_job(&painter, job);
        painter.galley(
            Pos2::new(tx, ty - 8.0 * s),
            g.clone(),
            Color32::from_gray(205).gamma_multiply(alpha),
        );
        ty += g.size().y + 5.0 * s;
    }

    // Setup requirements — amber, what the user needs to supply.
    if !e.requirements.is_empty() && ty + lh < row_top - 18.0 * s {
        painter.text(
            Pos2::new(tx, ty),
            Align2::LEFT_CENTER,
            crate::ellipsize(
                &painter,
                &format!("Setup: {}", e.requirements),
                &FontId::proportional(13.5 * s),
                tw,
            ),
            FontId::proportional(13.5 * s),
            Color32::from_rgb(235, 190, 120).gamma_multiply(alpha),
        );
        ty += lh;
    }

    // Tags ride the last line when there's room.
    if !e.tags.is_empty() && ty + lh < row_top - 18.0 * s {
        painter.text(
            Pos2::new(tx, ty),
            Align2::LEFT_CENTER,
            crate::ellipsize(
                &painter,
                &e.tags.join("  ·  "),
                &FontId::proportional(12.5 * s),
                tw,
            ),
            FontId::proportional(12.5 * s),
            Color32::from_gray(120).gamma_multiply(alpha),
        );
    }

    // --- Action row (bottom of the info column) -------------------
    // Install status line right above the row.
    if !status_txt.is_empty() {
        painter.text(
            Pos2::new(tx, row_top - 6.0 * s),
            Align2::LEFT_BOTTOM,
            crate::ellipsize(
                &painter,
                &status_txt,
                &FontId::proportional(12.0 * s),
                tw,
            ),
            FontId::proportional(12.0 * s),
            status_c.gamma_multiply(alpha),
        );
    }
    let mut bx = tx;
    if let Some(dl) = &dl {
        let g = dl.lock().unwrap_or_else(|e| e.into_inner());
        let btn = Rect::from_min_size(Pos2::new(bx, row_top), Vec2::new(170.0 * s, bh));
        painter.rect_filled(
            btn,
            CornerRadius::same((bh * 0.5).round() as u8),
            Color32::from_white_alpha((10.0 * alpha) as u8),
        );
        let fill_w = (btn.width() * g.frac.clamp(0.0, 1.0)).max(0.0);
        if fill_w > 1.0 {
            let mut fr = btn;
            fr.max.x = fr.min.x + fill_w;
            painter.rect_filled(
                fr,
                CornerRadius::same((bh * 0.5).round() as u8),
                Color32::from_rgb(70, 130, 200).gamma_multiply(0.8 * alpha),
            );
        }
        painter.text(
            btn.center(),
            Align2::CENTER_CENTER,
            crate::ellipsize(
                &painter,
                &g.stage,
                &FontId::proportional(13.0 * s),
                btn.width() - 12.0 * s,
            ),
            FontId::proportional(13.0 * s),
            Color32::from_gray(220).gamma_multiply(alpha),
        );
        ui.ctx().request_repaint();
    } else {
        let mut ops: Vec<(&str, PaneOp, bool)> = Vec::new();
        if e.repo.is_empty() {
            ops.push(("No repository", PaneOp::Download, false));
        } else {
            ops.push(match act {
                Action::Install => ("Install", PaneOp::Download, true),
                _ => ("Play", PaneOp::Play, true),
            });
            if installed {
                ops.push(("Open folder", PaneOp::OpenFolder, true));
                ops.push(("Uninstall", PaneOp::Uninstall, true));
            }
            if act == Action::Update {
                ops.push(("Update", PaneOp::Download, true));
            }
        }
        for (label, op, enabled) in ops {
            let btn =
                Rect::from_min_size(Pos2::new(bx, row_top), Vec2::new(170.0 * s, bh));
            if pane_button(
                ui, &painter, btn, label, label, enabled, interactive, alpha, s,
            ) {
                run_pane_op(view, &e, &dir, op);
            }
            bx += 170.0 * s + gap;
        }
    }
    // CC BY-SA attribution for the upstream dataset docks to the
    // pane's bottom-right corner, clear of the action row.
    painter.text(
        Pos2::new(pane.max.x - 10.0 * s, pane.max.y - 8.0 * s),
        Align2::RIGHT_BOTTOM,
        "Catalog data: recompendium.com · CC BY-SA 4.0",
        FontId::proportional(11.0 * s),
        Color32::from_gray(100).gamma_multiply(alpha),
    );
    for url in link_clicks {
        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
    }
}

/// What the pane's buttons do — kept separate from `Action` since
/// Play is pane-only.
#[derive(Clone, Copy, PartialEq)]
enum PaneOp {
    Download,
    Play,
    OpenFolder,
    Uninstall,
}

// ---- Filter drawer -------------------------------------------------------

/// The drawer's full (unclipped) content height — the STATUS and
/// SORT rows plus however many rows the uniform platform grid
/// needs, capped at just over half the area so the list never
/// disappears entirely.
fn filter_drawer_height(
    painter: &egui::Painter,
    view: &DecompView,
    width: f32,
    area_h: f32,
    s: f32,
) -> f32 {
    let chip_h = 34.0 * s;
    let gap = 10.0 * s;
    let font = FontId::proportional(16.0 * s);
    let avail = width - 2.0 * 40.0 * s - 120.0 * s;
    let plats = view.platform_options();
    let cw = plats
        .iter()
        .map(|(_, l)| painter.layout_no_wrap(l.clone(), font.clone(), Color32::WHITE).size().x)
        .fold(0.0, f32::max)
        + 26.0 * s;
    let per_row = ((avail + gap) / (cw + gap)).floor().max(1.0) as usize;
    let rows = plats.len().div_ceil(per_row).max(1) as f32;
    let need = 18.0 * s
        + chip_h
        + 12.0 * s
        + chip_h
        + 14.0 * s
        + rows * (chip_h + 8.0 * s)
        - 8.0 * s
        + 14.0 * s;
    need.min((area_h * 0.65).max(200.0 * s))
}

/// One filter chip — a small `pane_button`-style pill; `active`
/// brightens the fill and (for the maturity chips) tints the edge
/// and label with the status color.
#[allow(clippy::too_many_arguments)]
fn filter_chip(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    rect: Rect,
    id: &str,
    label: &str,
    active: bool,
    tint: Option<Color32>,
    interactive: bool,
    alpha: f32,
    s: f32,
) -> bool {
    let resp = if interactive {
        ui.interact(rect, ui.scope_id().with(("decomp-chip", id)), Sense::click())
    } else {
        ui.interact(rect, ui.scope_id().with(("decomp-chip", id)), Sense::hover())
    };
    let hot = interactive && resp.hovered();
    let down = hot && resp.is_pointer_button_down_on();
    let (fill, edge) = if active {
        (36.0, 175.0)
    } else if down {
        (28.0, 140.0)
    } else if hot {
        (20.0, 110.0)
    } else {
        (9.0, 60.0)
    };
    let rad = CornerRadius::same((rect.height() * 0.5).round() as u8);
    painter.rect_filled(rect, rad, Color32::from_white_alpha((fill * alpha) as u8));
    painter.rect_stroke(
        rect,
        rad,
        Stroke::new(
            s.max(1.0),
            if active {
                tint.unwrap_or(Color32::WHITE)
            } else {
                Color32::WHITE
            }
            .gamma_multiply(edge / 255.0 * alpha),
        ),
        StrokeKind::Inside,
    );
    let tc = if active {
        tint.unwrap_or(Color32::WHITE)
    } else {
        Color32::from_gray(200)
    };
    let font = FontId::proportional(16.0 * s);
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        crate::ellipsize(painter, label, &font, rect.width() - 14.0 * s),
        font,
        tc.gamma_multiply(alpha),
    );
    if hot {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    resp.clicked() && interactive
}

/// The drawer's content — laid out in the full shelf height and
/// clipped by the animated rect so it reveals from the pane's edge
/// upward like the EQ drawer. Chips compose through the same
/// `apply_filters`/`set_sort` path the kind tabs use.
fn filter_drawer(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    rect: Rect,
    full_h: f32,
    view: &mut DecompView,
    alpha: f32,
    interactive: bool,
    s: f32,
) {
    let content = Rect::from_min_max(Pos2::new(rect.min.x, rect.max.y - full_h), rect.max);
    let clip = painter.with_clip_rect(rect);
    let font = FontId::proportional(16.0 * s);
    let cap_font = FontId::proportional(13.0 * s);
    let chip_h = 34.0 * s;
    let gap = 10.0 * s;
    let pad_x = 40.0 * s;
    let cap_w = 120.0 * s;
    let x0 = content.min.x + pad_x;
    let chip_x = x0 + cap_w;
    let avail_w = (content.max.x - pad_x - chip_x).max(0.0);
    let cap_col = Color32::from_gray(130).gamma_multiply(alpha);
    let mut y = content.min.y + 18.0 * s;
    let uniform_w = |labels: &[String]| {
        labels
            .iter()
            .map(|l| painter.layout_no_wrap(l.clone(), font.clone(), Color32::WHITE).size().x)
            .fold(0.0, f32::max)
            + 26.0 * s
    };
    let caption = |text: &str, cy: f32| {
        clip.text(
            Pos2::new(x0, cy),
            Align2::LEFT_CENTER,
            text,
            cap_font.clone(),
            cap_col,
        );
    };

    // ---- STATUS — one maturity at a time, off = all ------------
    caption("STATUS", y + chip_h * 0.5);
    let labels: Vec<String> = STATUS_OPTS
        .iter()
        .map(|set| {
            catalog::status_label(set[0])
                .map(|(l, _)| l.to_string())
                .unwrap_or_else(|| set[0].to_uppercase())
        })
        .collect();
    let cw = uniform_w(&labels);
    let mut x = chip_x;
    for (i, set) in STATUS_OPTS.iter().enumerate() {
        let r = Rect::from_min_size(Pos2::new(x, y), Vec2::new(cw, chip_h));
        let tint = catalog::status_label(set[0]).map(|(_, c)| c);
        if filter_chip(
            ui,
            &clip,
            r,
            &format!("decomp-status-{i}"),
            &labels[i],
            view.status == Some(i),
            tint,
            interactive,
            alpha,
            s,
        ) {
            view.status = if view.status == Some(i) {
                None
            } else {
                Some(i)
            };
            view.apply_filters();
        }
        x += cw + gap;
    }
    if view.filters_active() {
        let label = "Clear filters";
        let w = painter
            .layout_no_wrap(label.to_string(), font.clone(), Color32::WHITE)
            .size()
            .x
            + 26.0 * s;
        let r = Rect::from_min_size(
            Pos2::new(content.max.x - pad_x - w, y),
            Vec2::new(w, chip_h),
        );
        if filter_chip(
            ui, &clip, r, "decomp-clear", label, false, None, interactive, alpha, s,
        ) {
            view.platforms.clear();
            view.status = None;
            view.apply_filters();
        }
    }
    y += chip_h + 12.0 * s;

    // ---- SORT — single-select, one always active ---------------
    caption("SORT", y + chip_h * 0.5);
    let sort_labels: Vec<String> = SORT_OPTS.iter().map(|(l, _)| l.to_string()).collect();
    let cw = uniform_w(&sort_labels);
    let mut x = chip_x;
    for (i, (label, key)) in SORT_OPTS.iter().enumerate() {
        let r = Rect::from_min_size(Pos2::new(x, y), Vec2::new(cw, chip_h));
        if filter_chip(
            ui,
            &clip,
            r,
            &format!("decomp-sort-{i}"),
            label,
            view.sort == *key,
            None,
            interactive,
            alpha,
            s,
        ) {
            view.set_sort(*key);
        }
        x += cw + gap;
    }
    y += chip_h + 14.0 * s;

    // ---- PLATFORMS — uniform-width grid -------------------------
    // Every cell gets the widest label's width, so the grid reads
    // as a tidy matrix instead of the site's ragged chips. When the
    // capped drawer can't fit every row the grid wheel-scrolls.
    caption("PLATFORMS", y + chip_h * 0.5);
    let plats = view.platform_options();
    if plats.is_empty() {
        return;
    }
    let plat_labels: Vec<String> = plats.iter().map(|(_, l)| l.clone()).collect();
    let cw = uniform_w(&plat_labels);
    let per_row = ((avail_w + gap) / (cw + gap)).floor().max(1.0) as usize;
    let rows = plats.len().div_ceil(per_row);
    let row_step = chip_h + 8.0 * s;
    let grid_h = rows as f32 * row_step - 8.0 * s;
    let grid_area = Rect::from_min_max(
        Pos2::new(chip_x, y),
        Pos2::new(content.max.x - pad_x, content.max.y - 12.0 * s),
    );
    let overflow = (grid_h - grid_area.height()).max(0.0);
    let scroll = if overflow > 0.5 {
        let id = ui.scope_id().with("decomp-plat-scroll");
        let mut off = ui
            .ctx()
            .data_mut(|d| d.get_temp::<f32>(id))
            .unwrap_or(0.0);
        if ui
            .interact(rect, ui.scope_id().with("decomp-plat-hit"), Sense::hover())
            .hovered()
        {
            off -= ui.ctx().input(|i| i.smooth_scroll_delta.y);
        }
        let off = off.clamp(0.0, overflow);
        ui.ctx().data_mut(|d| d.insert_temp(id, off));
        off
    } else {
        0.0
    };
    let gclip = clip.with_clip_rect(grid_area);
    for (i, (id, label)) in plats.iter().enumerate() {
        let (row, col) = (i / per_row, i % per_row);
        let r = Rect::from_min_size(
            Pos2::new(
                chip_x + col as f32 * (cw + gap),
                y - scroll + row as f32 * row_step,
            ),
            Vec2::new(cw, chip_h),
        );
        let active = view.platforms.contains(id);
        if filter_chip(
            ui,
            &gclip,
            r,
            &format!("decomp-plat-{id}"),
            label,
            active,
            None,
            interactive,
            alpha,
            s,
        ) {
            if active {
                view.platforms.remove(id);
            } else {
                view.platforms.insert(id.clone());
            }
            view.apply_filters();
        }
    }
}

/// One pane pill button — white-alpha fill/edge like the settings
/// buttons, three states (idle/hover/pressed) plus a disabled dim.
/// Returns true on click.
#[allow(clippy::too_many_arguments)]
fn pane_button(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    rect: Rect,
    id: &str,
    label: &str,
    enabled: bool,
    interactive: bool,
    alpha: f32,
    s: f32,
) -> bool {
    let resp = if interactive && enabled {
        ui.interact(rect, ui.scope_id().with(("decomp-pane-btn", id)), Sense::click())
    } else {
        ui.interact(rect, ui.scope_id().with(("decomp-pane-btn", id)), Sense::hover())
    };
    let hot = enabled && resp.hovered();
    let down = hot && resp.is_pointer_button_down_on();
    let (fill, edge) = if !enabled {
        (4.0, 30.0)
    } else if down {
        (30.0, 160.0)
    } else if hot {
        (22.0, 120.0)
    } else {
        (10.0, 70.0)
    };
    painter.rect_filled(
        rect,
        CornerRadius::same((rect.height() * 0.5).round() as u8),
        Color32::from_white_alpha((fill * alpha) as u8),
    );
    painter.rect_stroke(
        rect,
        CornerRadius::same((rect.height() * 0.5).round() as u8),
        Stroke::new(
            s.max(1.0),
            Color32::from_white_alpha((edge * alpha) as u8),
        ),
        StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(15.0 * s),
        (if enabled {
            Color32::from_gray(235)
        } else {
            Color32::from_gray(130)
        })
        .gamma_multiply(alpha),
    );
    if hot {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    resp.clicked() && enabled
}

/// What a pane button click does — Download reuses the card's
/// resolved action; Play launches the installed exe (folder fallback
/// when no binary is found); OpenFolder opens the install dir.
fn run_pane_op(view: &mut DecompView, e: &Entry, dir: &Option<PathBuf>, op: PaneOp) {
    match op {
        PaneOp::Download => view.activate(e),
        PaneOp::Play => {
            if let Some(d) = dir {
                view.play_dir(d, e.name.clone());
            }
        }
        PaneOp::OpenFolder => {
            if let Some(d) = dir {
                open_in_file_manager(d);
            }
        }
        PaneOp::Uninstall => view.request_uninstall(e),
    }
}

/// The binary `Play` launches: shallow scan for a `.exe`, install
/// root first, skipping uninstallers; the largest file wins — the
/// built binary tends to outweigh launchers and tools.
fn find_exe(dir: &Path) -> Option<PathBuf> {
    let mut found: Vec<(u8, u64, PathBuf)> = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0u8)];
    while let Some((d, depth)) = stack.pop() {
        if depth > 2 {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                stack.push((p, depth + 1));
            } else if p
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("exe"))
            {
                let stem = p
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_lowercase();
                if stem.contains("uninstall") || stem.starts_with("unins") {
                    continue;
                }
                let size = ent.metadata().map(|m| m.len()).unwrap_or(0);
                found.push((depth, size, p));
            }
        }
    }
    // Shallower first, larger second.
    found.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    found.into_iter().next().map(|(_, _, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The card's action from marker + remote: missing install →
    /// Install, newer remote stamp → Update, same stamp → Open, a
    /// failed check never blocks Open on an installed copy.
    #[test]
    fn card_action_resolves_state() {
        // Nothing installed.
        assert_eq!(card_action(None, None), Action::Install);
        // Installed, no remote data yet → Open (not Update).
        assert_eq!(card_action(Some("2024-01-01T00:00:00Z"), None), Action::Open);
        // Remote newer → Update.
        let remote = RemoteInfo {
            pushed_at: "2024-06-01T00:00:00Z".into(),
            err: None,
        };
        assert_eq!(
            card_action(Some("2024-01-01T00:00:00Z"), Some(&remote)),
            Action::Update
        );
        // Same stamp → Open.
        assert_eq!(
            card_action(Some("2024-06-01T00:00:00Z"), Some(&remote)),
            Action::Open
        );
        // Check failed — installed stays Openable; uninstalled still
        // offers Install (a dead button hides the real error; the
        // download worker surfaces it instead).
        let bad = RemoteInfo {
            pushed_at: String::new(),
            err: Some("403".into()),
        };
        assert_eq!(
            card_action(Some("2024-01-01T00:00:00Z"), Some(&bad)),
            Action::Open
        );
        assert_eq!(card_action(None, Some(&bad)), Action::Install);
    }

    /// Keyboard nav clamps at the edges — `nav_geo` only exists
    /// after a painted frame, so dy steps +/-1 like dx here.
    #[test]
    fn move_selection_walks_grid() {
        let mut v = DecompView::new();
        let sel = |v: &DecompView| {
            v.cards
                .primary_path()
                .map(|p| p.to_string_lossy().into_owned())
        };
        v.move_selection(1, 0);
        assert_eq!(sel(&v).as_deref(), Some("oot"));
        v.move_selection(0, 1);
        assert_eq!(sel(&v).as_deref(), Some("mm"));
        v.move_selection(-1, 0);
        assert_eq!(sel(&v).as_deref(), Some("oot"));
        v.move_selection(0, -10);
        assert_eq!(sel(&v).as_deref(), Some("sm64"));
        v.move_selection(0, 99);
        assert_eq!(sel(&v).as_deref(), PROJECTS.last().map(|p| p.slug));
    }

    /// The kind tab narrows the shown set (the list's subset);
    /// search text narrows it further -- name/repo/console/tags all
    /// match via the item keywords.
    #[test]
    fn filters_narrow_visible() {
        let mut v = DecompView::new();
        let shown = |v: &DecompView| v.cards.visible(&v.search.text).len();
        assert_eq!(shown(&v), PROJECTS.len());
        v.set_kind_filter(Some(Kind::Decomp));
        let decomps = shown(&v);
        assert!(decomps < PROJECTS.len() && decomps > 0);
        v.set_kind_filter(None);
        v.search.text = "zelda".into();
        // Repo names match too -- the three zeldaret decomps plus
        // Zelda64Recomp.
        assert_eq!(shown(&v), 4);
        v.search.text = "ocarina".into();
        assert_eq!(shown(&v), 1);
        v.search.text.clear();
        v.search.text = "n64decomp".into();
        assert_eq!(shown(&v), 3);
    }

    /// Two slugs colliding would alias install dirs AND cover cache
    /// files; "covers" as a slug would trash the art cache dir. All
    /// entries also need an art query — an empty one silently never
    /// fetches.
    #[test]
    fn slugs_are_unique_and_reserved() {
        let mut seen = std::collections::HashSet::new();
        for p in PROJECTS {
            assert!(seen.insert(p.slug), "duplicate slug {}", p.slug);
            assert_ne!(p.slug, "covers");
            assert!(!p.cover_q.is_empty(), "{} has no art query", p.slug);
            assert!(p.repo.contains('/'));
        }
    }

    /// Box art must letterbox inside the slot — a landscape scan
    /// shrinks to the slot's width, a tall box to its height, and
    /// the fit is always centered and contained.
    #[test]
    fn fit_rect_letterboxes_inside_slot() {
        let slot = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(96.0, 134.0));
        // Landscape image: fits to width, centered vertically.
        let r = fit_rect(slot, 200.0, 100.0);
        assert_eq!(r.width(), 96.0);
        assert_eq!(r.height(), 48.0);
        assert_eq!(r.center(), slot.center());
        assert!(slot.contains_rect(r));
        // Portrait box: fits to height.
        let r = fit_rect(slot, 100.0, 200.0);
        assert_eq!(r.height(), 134.0);
        assert_eq!(r.center(), slot.center());
        assert!(slot.contains_rect(r));
        // Degenerate dims keep the slot rather than NaN-ing.
        assert_eq!(fit_rect(slot, 0.0, 100.0), slot);
    }

    /// Release asset picking: the windows build must win over the
    /// linux/macos siblings and the junk (checksums, symbols, source
    /// dumps); an arm64 windows build must NOT beat the x64 one.
    #[test]
    fn pick_os_asset_prefers_windows_x64() {
        let a = |name: &str| {
            serde_json::json!({
                "name": name,
                "browser_download_url": format!("https://x/{name}"),
            })
        };
        let assets = vec![
            a("port-linux-x64.tar.gz"),
            a("port-macos-universal.dmg"),
            a("port-windows-arm64.zip"),
            a("port-windows-x64.zip"),
            a("port-windows-x64.zip.sha256"),
            a("port-windows-x64-symbols.zip"),
        ];
        assert_eq!(
            pick_os_asset(&assets).as_deref(),
            Some("https://x/port-windows-x64.zip")
        );
        // No windows asset at all → the caller falls back to source.
        let none = vec![a("port-linux-x64.tar.gz"), a("port-windows.dmg")];
        assert_eq!(pick_os_asset(&none), None);
        // A single unmarked-arch windows build still wins.
        let single = vec![a("port-linux.tar.gz"), a("port-windows.zip")];
        assert_eq!(
            pick_os_asset(&single).as_deref(),
            Some("https://x/port-windows.zip")
        );
    }

    /// Play routing mirrors the emulation view: no session launches
    /// straight away, the running game's own card only un-dismisses
    /// the capture viewport, a different card queues behind a close
    /// request (the caller's confirm prompt).
    #[test]
    fn play_route_launch_undismiss_queue() {
        let mut v = DecompView::default();
        let dir_a = Path::new("C:/Decomps/a");
        let dir_b = Path::new("C:/Decomps/b");
        assert_eq!(v.play_route(dir_a), DecompPlayRoute::Launch);
        // A launch already in flight (rx parked) counts as live —
        // same dir un-dismisses, a different dir queues.
        let (_tx, rx) = mpsc::channel();
        v.launch_rx = Some(rx);
        v.session_dir = Some(dir_a.to_path_buf());
        v.viewport_dismissed = true;
        assert_eq!(v.play_route(dir_a), DecompPlayRoute::UnDismiss);
        assert_eq!(v.play_route(dir_b), DecompPlayRoute::QueuePending);
        v.play_dir(dir_a, "Same".into());
        assert!(!v.viewport_dismissed);
        v.play_dir(dir_b, "Other".into());
        assert_eq!(
            v.pending_launch.as_ref().map(|(d, _)| d.as_path()),
            Some(dir_b)
        );
        // The close request is parked for the caller's confirm —
        // consumed exactly once.
        assert!(v.take_close_request());
        assert!(!v.take_close_request());
    }

    /// The ⇄ switch: dismissing the capture drops game-fullscreen
    /// so the app window comes back; toggling again un-dismisses.
    #[test]
    fn viewport_toggle_round_trip() {
        let mut v = DecompView::default();
        v.game_fs = true;
        v.toggle_viewport();
        assert!(v.viewport_dismissed);
        assert!(!v.game_fs);
        v.toggle_viewport();
        assert!(!v.viewport_dismissed);
    }

    /// Dismissing the capture also releases the input tunnel —
    /// neither fullscreen nor the tunnel has anything to drive once
    /// the list is back (same contract as the emulation view).
    #[test]
    fn dismiss_viewport_drops_fullscreen_and_tunnel() {
        let mut v = DecompView::default();
        v.game_fs = true;
        v.input_tun = true;
        v.toggle_viewport();
        assert!(v.viewport_dismissed);
        assert!(!v.game_fs);
        assert!(!v.input_tun);
    }

    /// The input-tunnel switch round-trips — the band's pointer
    /// icon and Ctrl+M share it.
    #[test]
    fn input_tunnel_toggles() {
        let mut v = DecompView::default();
        v.toggle_input_tunnel();
        assert!(v.input_tun);
        v.toggle_input_tunnel();
        assert!(!v.input_tun);
    }

    /// Entering game-fullscreen un-dismisses the viewport — the
    /// request alone can't mean fullscreen without the capture
    /// layer up.
    #[test]
    fn fullscreen_undismisses_viewport() {
        let mut v = DecompView::default();
        v.viewport_dismissed = true;
        v.toggle_fullscreen();
        assert!(v.game_fs);
        assert!(!v.viewport_dismissed);
        v.toggle_fullscreen();
        assert!(!v.game_fs);
    }

    /// find_exe picks the game binary — uninstallers are skipped
    /// and the shallowest executable wins (a launcher in a subdir
    /// loses to the game exe at the root).
    #[test]
    fn find_exe_skips_uninstaller_prefers_shallow() {
        let dir = std::env::temp_dir().join(format!("decomp_findexe_{}", std::process::id()));
        let sub = dir.join("bin");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join("uninstall.exe"), b"x").unwrap();
        std::fs::write(sub.join("game.exe"), b"yy").unwrap();
        assert_eq!(
            find_exe(&dir).as_deref(),
            Some(sub.join("game.exe").as_path())
        );
        std::fs::write(dir.join("game.exe"), b"zzz").unwrap();
        assert_eq!(
            find_exe(&dir).as_deref(),
            Some(dir.join("game.exe").as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Uninstall routing: an idle game surfaces a direct request
    /// (one-shot); the running game parks behind the close prompt
    /// and re-arms as a request only once its dir frees.
    #[test]
    fn uninstall_request_routes_and_settles() {
        let mut v = DecompView::default();
        let e = v.entries[0].clone();
        // Idle → direct request, consumed exactly once.
        v.request_uninstall(&e);
        assert_eq!(
            v.take_uninstall_request().map(|r| r.0),
            Some(e.slug.clone())
        );
        assert!(v.take_uninstall_request().is_none());
        // Running (or in-flight launch — session_dir is set at
        // launch start) → parked + close prompt instead.
        v.session_dir = install_dir(&e.slug);
        v.request_uninstall(&e);
        assert!(v.take_uninstall_request().is_none());
        assert!(v.pending_uninstall.is_some());
        assert!(v.take_close_request());
        // Dir still owned → no re-arm yet.
        v.settle_uninstall();
        assert!(v.take_uninstall_request().is_none());
        // Dir freed → the request surfaces for the delete prompt.
        v.session_dir = None;
        v.settle_uninstall();
        assert_eq!(
            v.take_uninstall_request().map(|r| r.0),
            Some(e.slug.clone())
        );
    }

    /// A confirmed uninstall drops a queued Play for the same dir —
    /// launching a deleted tree would just fail into the folder.
    #[test]
    fn uninstall_drops_queued_launch() {
        let mut v = DecompView::default();
        let e = v.entries[0].clone();
        let dir = install_dir(&e.slug).unwrap_or_else(|| PathBuf::from("decomp-noop"));
        v.pending_launch = Some((dir.clone(), e.name.clone()));
        v.uninstall(&e.slug);
        assert!(v.pending_launch.is_none());
    }
}
