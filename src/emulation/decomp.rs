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
use std::sync::mpsc;
use std::time::Instant;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, Pos2, Rect, Sense, Stroke,
    StrokeKind, Vec2,
};

use crate::album_row::AlbumRow;
use crate::carousel::{self, Mode};
use crate::emulation::catalog::{self, Entry, Kind};
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
            search: SearchPill::default(),
            cover_requested: HashSet::new(),
            cover_req_tx: req_tx,
            cover_req_rx: Some(req_rx),
            cover_res_rx: res_rx,
            cover_res_tx: res_tx,
            settings_dirty: false,
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

    /// Push `entries` into the shared list — each item's `path` is
    /// the slug (identity for selection, covers, actions), `keywords`
    /// carries the search haystack so `visible()`'s text filter keeps
    /// matching repo/dev/toolchain/tags, `artist`/`album` stay clean
    /// marquee lines.
    fn sync_items(&mut self) {
        let items = self
            .entries
            .iter()
            .map(|e| carousel::Item {
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
            })
            .collect();
        self.cards.open_items(items, "decomp".to_string());
    }

    /// The kind tab's `Carousel::set_subset` — slug paths of the
    /// chosen kind, `None` for All.
    fn apply_subset(&mut self) {
        self.cards.set_subset(self.filter.map(|k| {
            self.entries
                .iter()
                .filter(|e| e.kind == k)
                .map(|e| PathBuf::from(&e.slug))
                .collect()
        }));
    }

    /// Tab switch: restrict the subset and re-anchor the focus on the
    /// selection (a filtered-out selection lands on the first shown).
    fn set_kind_filter(&mut self, k: Option<Kind>) {
        self.filter = k;
        self.apply_subset();
        self.cards.focus(&self.search.text);
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
                    if let Some(exe) = find_exe(&dir) {
                        let _ = std::process::Command::new(&exe)
                            .current_dir(&dir)
                            .spawn();
                    } else {
                        open_in_file_manager(&dir);
                    }
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
        let resp = if interactive {
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
    let action = view.cards.ui(
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
        interactive,
        false,
        s,
    );
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
    let sel_idx = focused
        .as_deref()
        .and_then(|p| view.slug_idx.get(&*p.to_string_lossy()).copied());
    info_pane(ui, pane, view, sel_idx, alpha, interactive, s);

    // The item search pill — docked in the top-right corner with a
    // small top/right margin so the cards never slide under it.
    view.search.ui(
        ui,
        content.max.x - 18.0 * s,
        upper.min.y + 10.0 * s,
        "Search",
        true,
        false,
        alpha,
        interactive,
        s,
    );

    // --- Bottom band -----------------------------------------------
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

    // Right cluster: the card-layout toggle — the same 60px icon
    // slot and glyph the music band uses for its view modes. It
    // persists Emulation.DecompLayoutMode — independent of the
    // emulation view's own CoverLayoutMode.
    let trect = Rect::from_center_size(Pos2::new(band.max.x - 50.0 * s, cy), Vec2::splat(60.0 * s));
    let tr = if interactive {
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
        bx = btn.max.x + gap;
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
    // CC BY-SA attribution for the upstream dataset trails the row.
    painter.text(
        Pos2::new(bx + 6.0 * s, row_top + bh * 0.5),
        Align2::LEFT_CENTER,
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
                match find_exe(d) {
                    Some(exe) => {
                        let _ = std::process::Command::new(&exe).current_dir(d).spawn();
                    }
                    None => {
                        view.notice =
                            Some("No executable found — opening the folder".into());
                        open_in_file_manager(d);
                    }
                }
            }
        }
        PaneOp::OpenFolder => {
            if let Some(d) = dir {
                open_in_file_manager(d);
            }
        }
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
}
