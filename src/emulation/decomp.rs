//! Re/Decompilation view — a curated catalog of decomp, source-port
//! and disassembly projects, rendered in the AES card-grid language.
//!
//! There is no central registry for decomp projects, so the catalog
//! is a fixed manifest. Tracking is cheap: one `repos/{o}/{r}` call
//! per project per session reads `pushed_at` + `default_branch`, and
//! the installed copy carries a `.oxide_pushed` marker written at
//! download time — a newer remote `pushed_at` is the update badge.
//! Downloads pull the repo's zipball (default branch) into
//! `<data>/Decomps/<slug>` via the emu_dl extract helpers. Source is
//! copied OVER the existing tree — never a wipe — so a baserom or
//! build outputs parked inside the project dir survive an update.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, Pos2, Rect, Sense, Stroke,
    StrokeKind, Vec2,
};

/// The install marker written next to the extracted source — holds
/// the remote `pushed_at` seen at download time.
const MARKER: &str = ".oxide_pushed";
const USER_AGENT: &str = "AES_Oxide-DecompCatalog/1.0";

/// What kind of project this is — drives the small badge on the card.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Byte-matching decompilation (zeldaret, n64decomp, pmret, …).
    Decomp,
    /// Static recompilation to a native port (Zelda64Recomp).
    Recomp,
    /// Maintained disassembly (pret's Pokémon repos).
    Disasm,
    /// Decomp-derived playable port that ships builds (devilutionX).
    SourcePort,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Decomp => "Decomp",
            Kind::Recomp => "Recomp",
            Kind::Disasm => "Disassembly",
            Kind::SourcePort => "Port",
        }
    }
}

/// One catalog entry — `repo` is `owner/repo` on GitHub.
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

/// The curated catalog — big-name, actively-maintained projects.
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
        kind: Kind::SourcePort,
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
    /// Not installed — download the source.
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

impl Action {
    fn label(self) -> &'static str {
        match self {
            Action::Install => "Install",
            Action::Update => "Update",
            Action::Open => "Open",
        }
    }
}

/// View state — selection, scroll spring, remote check results and
/// in-flight downloads keyed by slug.
pub struct DecompView {
    pub selected: i32,
    scroll_cur: f32,
    scroll_tgt: f32,
    scroll_vel: f32,
    /// Last computed grid column count — the vertical arrow step.
    columns: usize,
    /// slug → remote info; filled progressively by the check worker
    /// (a refresh button can run it again).
    remote: HashMap<String, RemoteInfo>,
    check_rx: Option<mpsc::Receiver<(String, RemoteInfo)>>,
    check_running: bool,
    downloads: HashMap<String, crate::emulation::emu_dl::Shared>,
    notice: Option<String>,
    /// One-shot session check — armed on first view.
    checked: bool,
    /// slug → decoded box art, fetched once + disk-cached under
    /// `Decomps/covers/`. `None` in the channel means "failed —
    /// don't retry this session".
    covers: HashMap<String, egui::TextureHandle>,
    cover_rx: Option<mpsc::Receiver<(String, Option<egui::ColorImage>)>>,
    cover_started: bool,
}

impl Default for DecompView {
    fn default() -> Self {
        Self {
            selected: 0,
            scroll_cur: 0.0,
            scroll_tgt: 0.0,
            scroll_vel: 0.0,
            columns: 1,
            remote: HashMap::new(),
            check_rx: None,
            check_running: false,
            downloads: HashMap::new(),
            notice: None,
            checked: false,
            covers: HashMap::new(),
            cover_rx: None,
            cover_started: false,
        }
    }
}

impl DecompView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fire the once-per-session remote check (and on manual refresh).
    /// One `repos/{o}/{r}` GET per project on a worker thread — paced
    /// and streamed: ~19 instant API calls trip GitHub's secondary
    /// rate limit, and per-item sends let cards resolve as answers
    /// land instead of waiting for the slowest.
    pub fn check_updates(&mut self) {
        if self.check_running {
            return;
        }
        self.check_running = true;
        let (tx, rx) = mpsc::channel();
        self.check_rx = Some(rx);
        std::thread::spawn(move || {
            let agent = crate::emulation::emu_dl::agent();
            for (i, p) in PROJECTS.iter().enumerate() {
                if i > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(350));
                }
                let url = format!("https://api.github.com/repos/{}", p.repo);
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
                if tx.send((p.slug.to_string(), info)).is_err() {
                    return;
                }
            }
        });
    }

    /// Arrow-key navigation: dx ±1 walks the row, dy ±1 walks a
    /// column. `columns` is stamped by the layout each frame.
    pub fn move_selection(&mut self, dx: i32, dy: i32) {
        let n = PROJECTS.len() as i32;
        if n == 0 {
            return;
        }
        let cur = self.selected.max(0);
        let step = dx + dy * self.columns.max(1) as i32;
        self.selected = (cur + step).clamp(0, n - 1);
    }

    /// Enter / the card button — install, update, or open.
    pub fn activate_selected(&mut self) {
        if self.selected < 0 || self.selected as usize >= PROJECTS.len() {
            return;
        }
        self.activate(&PROJECTS[self.selected as usize]);
    }

    /// The card's resolved action, run for `project`.
    fn activate(&mut self, p: &Project) {
        match card_action(
            install_dir(p.slug)
                .as_deref()
                .and_then(|d| installed_stamp(d))
                .as_deref(),
            self.remote.get(p.slug),
        ) {
            Action::Install | Action::Update => self.download(p),
            Action::Open => {
                if let Some(dir) = install_dir(p.slug) {
                    open_in_file_manager(&dir);
                }
            }
        }
    }

    /// Zipball → extract → copy into `<data>/Decomps/<slug>` on a
    /// worker; `DlProgress` carries stage/frac for the card.
    fn download(&mut self, p: &Project) {
        if self.downloads.contains_key(p.slug) {
            return;
        }
        let dl: crate::emulation::emu_dl::Shared = std::sync::Arc::new(std::sync::Mutex::new(
            crate::emulation::emu_dl::DlProgress {
                stage: format!("Fetching {}…", p.name),
                ..Default::default()
            },
        ));
        let shared = dl.clone();
        let repo = p.repo.to_string();
        let slug = p.slug.to_string();
        std::thread::spawn(move || {
            let res = download_work(&shared, &repo, &slug);
            let mut g = shared.lock().unwrap_or_else(|e| e.into_inner());
            g.done = Some(res);
        });
        self.downloads.insert(p.slug.to_string(), dl);
    }

    /// Per-frame bookkeeping: drain the check channel, retire finished
    /// downloads into `notice`, and keep repaints flowing while
    /// anything moves (scroll spring, live downloads, pending check).
    fn pump(&mut self, ctx: &egui::Context) {
        if !self.checked {
            self.checked = true;
            self.check_updates();
        }
        if !self.cover_started {
            self.cover_started = true;
            let (tx, rx) = mpsc::channel();
            self.cover_rx = Some(rx);
            std::thread::spawn(move || cover_work(tx));
        }
        if let Some(rx) = &self.cover_rx {
            let mut dead = false;
            loop {
                match rx.try_recv() {
                    Ok((slug, Some(img))) => {
                        self.covers.insert(
                            slug.clone(),
                            ctx.load_texture(
                                format!("decomp-cover-{slug}"),
                                img,
                                egui::TextureOptions::LINEAR,
                            ),
                        );
                    }
                    Ok((_, None)) => {}
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        dead = true;
                        break;
                    }
                }
            }
            if dead {
                self.cover_rx = None;
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
                Ok(path) => self.notice = Some(format!("Installed to {path}")),
                Err(e) => self.notice = Some(e),
            }
        }
        if self.check_running || !self.downloads.is_empty() || self.cover_rx.is_some() {
            ctx.request_repaint();
        }
    }

    /// The scroll spring's rest test — same keepalive contract as the
    /// album strip's (event-driven frames must be asked for).
    fn scroll_settled(&self, max_scroll: f32) -> bool {
        self.scroll_cur == self.scroll_tgt
            && self.scroll_vel == 0.0
            && self.scroll_tgt >= 0.0
            && self.scroll_tgt <= max_scroll
    }

    /// Spring `target` so card `index` is fully inside the view.
    fn reveal(&mut self, index: i32, rect: Rect, card_h: f32, spacing: f32, s: f32) {
        let cols = self.columns.max(1) as i32;
        let row = index.div_euclid(cols);
        let top = 24.0 * s + row as f32 * (card_h + spacing);
        let bot = top + card_h;
        let view_h = rect.height();
        if top < self.scroll_tgt {
            self.scroll_tgt = top;
        } else if bot > self.scroll_tgt + view_h {
            self.scroll_tgt = bot - view_h;
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
/// The `covers` dir shares the Decomps root but no project slug
/// collides with it (guarded by `slugs_are_unique_and_reserved`).
fn cover_cache(slug: &str) -> Option<PathBuf> {
    decomps_root().map(|d| d.join("covers").join(format!("{slug}.png")))
}

/// The box-art worker: for each project, read the disk cache or run
/// the image pipeline once and save the result. Cache hits and
/// freshly fetched bytes both decode here so the UI thread only
/// ever sees ready `ColorImage`s; a `None` payload just means "no
/// art this session".
fn cover_work(tx: mpsc::Sender<(String, Option<egui::ColorImage>)>) {
    for p in PROJECTS {
        let cache = cover_cache(p.slug);
        let bytes = cache
            .as_ref()
            .filter(|f| f.is_file())
            .and_then(|f| std::fs::read(f).ok())
            .or_else(|| {
                let b = crate::emulation::emu_meta::boxart_bytes(p.cover_q)?;
                if let Some(f) = &cache {
                    if let Some(d) = f.parent() {
                        let _ = std::fs::create_dir_all(d);
                    }
                    let _ = std::fs::write(f, &b);
                }
                Some(b)
            });
        let _ = tx.send((
            p.slug.to_string(),
            bytes.and_then(|b| decode_color_image(&b)),
        ));
    }
}

fn decode_color_image(bytes: &[u8]) -> Option<egui::ColorImage> {
    let d = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = (d.width() as usize, d.height() as usize);
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [w, h],
        &d.into_raw(),
    ))
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

/// The Re/Decompilation view: a card grid over black with the AES
/// bottom band — same geometry as the emulation view's chrome.
pub fn view(
    ui: &mut egui::Ui,
    content: Rect,
    alpha: f32,
    interactive: bool,
    view: &mut DecompView,
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

    // --- Card grid -------------------------------------------------
    let pad_x = 40.0 * s;
    let spacing = 20.0 * s;
    let card_w = 360.0 * s;
    let card_h = 150.0 * s;
    let avail_w = (upper.width() - pad_x * 2.0).max(card_w);
    let cols = ((avail_w + spacing) / (card_w + spacing)).max(1.0) as usize;
    view.columns = cols.max(1);
    let grid_w = cols as f32 * card_w + (cols.saturating_sub(1)) as f32 * spacing;
    let origin_x = upper.min.x + (upper.width() - grid_w).max(0.0) * 0.5;
    let rows = PROJECTS.len().div_ceil(cols);
    let content_h = 24.0 * s + rows as f32 * card_h + rows.saturating_sub(1) as f32 * spacing + 24.0 * s;
    let max_scroll = (content_h - upper.height()).max(0.0);

    // Wheel scroll + spring (same model as the strip's — eased
    // target, velocity-carrying current).
    let pointer = ui.ctx().input(|i| i.pointer.latest_pos());
    if interactive && pointer.is_some_and(|p| upper.contains(p)) {
        let d = ui.ctx().input(|i| i.smooth_scroll_delta);
        if d != Vec2::ZERO {
            view.scroll_tgt = (view.scroll_tgt - (d.x + d.y)).clamp(0.0, max_scroll);
        }
    }
    let dt = ui.ctx().input(|i| i.stable_dt).min(0.1);
    if view.scroll_tgt < 0.0 {
        view.scroll_tgt += -view.scroll_tgt * (12.0 * dt).min(1.0);
    } else if view.scroll_tgt > max_scroll {
        view.scroll_tgt -= (view.scroll_tgt - max_scroll) * (12.0 * dt).min(1.0);
    }
    let dist = view.scroll_tgt - view.scroll_cur;
    let stiffness = 680.0f32;
    let damping = 2.0 * stiffness.sqrt() * 0.9;
    view.scroll_vel += (dist * stiffness - view.scroll_vel * damping) * dt;
    view.scroll_cur += view.scroll_vel * dt;
    if dist.abs() < 0.01 && view.scroll_vel.abs() < 0.01 {
        view.scroll_cur = view.scroll_tgt;
        view.scroll_vel = 0.0;
    }
    if !view.scroll_settled(max_scroll) {
        ui.ctx().request_repaint();
    }

    let mut activate: Option<&'static Project> = None;
    for (i, p) in PROJECTS.iter().enumerate() {
        let col = i % cols;
        let row = i / cols;
        let card = Rect::from_min_size(
            Pos2::new(
                origin_x + col as f32 * (card_w + spacing),
                upper.min.y + 24.0 * s + row as f32 * (card_h + spacing) - view.scroll_cur,
            ),
            Vec2::new(card_w, card_h),
        );
        if card.max.y < upper.min.y - 4.0 || card.min.y > upper.max.y + 4.0 {
            continue;
        }
        let cp = painter.with_clip_rect(upper);
        let hovered = interactive && pointer.is_some_and(|pt| card.contains(pt));
        let selected = view.selected == i as i32;

        // Tile: white-alpha fill like the settings buttons; the
        // selection ring matches the carousel's picked-card cue.
        let fill = if hovered || selected { 14.0 } else { 8.0 };
        cp.rect_filled(
            card,
            CornerRadius::same((12.0 * s).round() as u8),
            Color32::from_white_alpha((fill * alpha) as u8),
        );
        let ring = if selected {
            150.0
        } else if hovered {
            90.0
        } else {
            40.0
        };
        cp.rect_stroke(
            card,
            CornerRadius::same((12.0 * s).round() as u8),
            Stroke::new(
                if selected { 2.0 * s } else { s },
                Color32::from_white_alpha((ring * alpha) as u8),
            ),
            StrokeKind::Inside,
        );

        // Box art — a dark inset slot on the card's left; the image
        // is contain-fit (letterboxed) so odd aspects never crop.
        let cover_h = card_h - 20.0 * s;
        let slot = Rect::from_min_size(
            card.min + Vec2::new(10.0 * s, 10.0 * s),
            Vec2::new(cover_h * 0.72, cover_h),
        );
        cp.rect_filled(
            slot,
            CornerRadius::same((8.0 * s).round() as u8),
            Color32::from_white_alpha((6.0 * alpha) as u8),
        );
        if let Some(tex) = view.covers.get(p.slug) {
            let dest = fit_rect(
                slot.shrink(2.0 * s),
                tex.size()[0] as f32,
                tex.size()[1] as f32,
            );
            cp.image(
                tex.id(),
                dest,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE.gamma_multiply(alpha),
            );
            cp.rect_stroke(
                slot,
                CornerRadius::same((8.0 * s).round() as u8),
                Stroke::new(
                    s.max(1.0),
                    Color32::from_white_alpha((30.0 * alpha) as u8),
                ),
                StrokeKind::Inside,
            );
        }

        // Name + console/kind badges — the text column starts right
        // of the cover slot.
        let tx = slot.max.x + 14.0 * s;
        let text_w = card.max.x - 14.0 * s - tx;
        cp.text(
            Pos2::new(tx, card.min.y + 24.0 * s),
            Align2::LEFT_CENTER,
            crate::ellipsize(&cp, p.name, &FontId::proportional(19.0 * s), text_w - 86.0 * s),
            FontId::proportional(19.0 * s),
            Color32::WHITE.gamma_multiply(alpha),
        );
        let badge = format!("{} · {}", p.console, p.kind.label());
        cp.text(
            Pos2::new(card.max.x - 14.0 * s, card.min.y + 26.0 * s),
            Align2::RIGHT_CENTER,
            &badge,
            FontId::proportional(11.0 * s),
            Color32::from_gray(140).gamma_multiply(alpha),
        );
        cp.text(
            Pos2::new(tx, card.min.y + 48.0 * s),
            Align2::LEFT_CENTER,
            &p.repo,
            FontId::monospace(11.0 * s),
            Color32::from_gray(110).gamma_multiply(alpha),
        );
        cp.text(
            Pos2::new(tx, card.min.y + 74.0 * s),
            Align2::LEFT_CENTER,
            crate::ellipsize(&cp, p.blurb, &FontId::proportional(13.0 * s), text_w),
            FontId::proportional(13.0 * s),
            Color32::from_gray(180).gamma_multiply(alpha),
        );

        // Status line — installed stamp, update state, or the live
        // download stage.
        let dir = install_dir(p.slug);
        let stamp = dir.as_deref().and_then(installed_stamp);
        // Release installs display their tag; source installs the
        // pushed_at stamp they always carried.
        let disp = dir
            .as_deref()
            .and_then(installed_release)
            .or_else(|| stamp.clone());
        let remote = view.remote.get(p.slug);
        let act = card_action(stamp.as_deref(), remote);
        let dl = view.downloads.get(p.slug);
        let (status, status_c) = if let Some(dl) = dl {
            let g = dl.lock().unwrap_or_else(|e| e.into_inner());
            (g.stage.clone(), Color32::from_rgb(120, 180, 255))
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
                (None, _) if remote.is_some_and(|r| r.err.is_some()) => (
                    "Update check failed".to_string(),
                    Color32::from_rgb(255, 190, 90),
                ),
                (None, _) => ("Not installed".to_string(), Color32::from_gray(150)),
            }
        };
        cp.text(
            Pos2::new(tx, card.max.y - 20.0 * s),
            Align2::LEFT_CENTER,
            crate::ellipsize(&cp, &status, &FontId::proportional(11.5 * s), text_w - 108.0 * s),
            FontId::proportional(11.5 * s),
            status_c.gamma_multiply(alpha),
        );

        // Action button — small pill at the card's bottom-right,
        // painted like the settings' "Open AES Directory" button.
        // While a download runs it becomes a progress bar.
        let bw = 92.0 * s;
        let bh = 30.0 * s;
        let btn = Rect::from_min_size(
            Pos2::new(card.max.x - bw - 14.0 * s, card.max.y - bh - 12.0 * s),
            Vec2::new(bw, bh),
        );
        if let Some(dl) = dl {
            let g = dl.lock().unwrap_or_else(|e| e.into_inner());
            cp.rect_filled(
                btn,
                CornerRadius::same((bh * 0.5).round() as u8),
                Color32::from_white_alpha((10.0 * alpha) as u8),
            );
            let fill_w = (btn.width() * g.frac.clamp(0.0, 1.0)).max(0.0);
            if fill_w > 1.0 {
                let mut fr = btn;
                fr.max.x = fr.min.x + fill_w;
                cp.rect_filled(
                    fr,
                    CornerRadius::same((bh * 0.5).round() as u8),
                    Color32::from_rgb(70, 130, 200).gamma_multiply(0.8 * alpha),
                );
            }
        } else {
            let r = if interactive {
                ui.interact(btn, ui.id().with(("decomp-act", p.slug)), Sense::click())
            } else {
                ui.interact(btn, ui.id().with(("decomp-act", p.slug)), Sense::hover())
            }
            .on_hover_text(match act {
                Action::Install => "Download the latest release (or source)",
                Action::Update => "Re-download — a newer commit is up",
                Action::Open => "Open the install folder",
            });
            // idle / hover / pressed — a real button reads all three.
            let hot = r.hovered();
            let down = r.is_pointer_button_down_on();
            let (fill, edge, txt) = if down {
                (30.0, 160.0, 255)
            } else if hot {
                (20.0, 120.0, 235)
            } else {
                (10.0, 70.0, 200)
            };
            cp.rect_filled(
                btn,
                CornerRadius::same((bh * 0.5).round() as u8),
                Color32::from_white_alpha((fill * alpha) as u8),
            );
            cp.rect_stroke(
                btn,
                CornerRadius::same((bh * 0.5).round() as u8),
                Stroke::new(
                    s,
                    Color32::from_white_alpha((edge * alpha) as u8),
                ),
                StrokeKind::Inside,
            );
            cp.text(
                btn.center(),
                Align2::CENTER_CENTER,
                act.label(),
                FontId::proportional(13.0 * s),
                Color32::from_gray(txt).gamma_multiply(alpha),
            );
            if hot {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            if r.clicked() {
                activate = Some(p);
            }
        }

        // Whole-card click selects (double-click runs the action).
        let cr = if interactive {
            ui.interact(card, ui.id().with(("decomp-card", p.slug)), Sense::click())
        } else {
            ui.interact(card, ui.id().with(("decomp-card", p.slug)), Sense::hover())
        };
        if cr.clicked() {
            view.selected = i as i32;
        }
        if cr.double_clicked() {
            view.selected = i as i32;
            activate = Some(p);
        }
        if hovered {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
    }
    if let Some(p) = activate {
        view.activate(p);
    }
    view.reveal(view.selected, upper, card_h, spacing, s);

    // Thin right-edge scrollbar when the grid overflows — same look
    // as the grids'.
    if max_scroll > 1.0 {
        let track = Rect::from_min_max(
            Pos2::new(upper.max.x - 6.0 * s, upper.min.y + 12.0 * s),
            Pos2::new(upper.max.x - 2.0 * s, upper.max.y - 12.0 * s),
        );
        let frac = (view.scroll_cur / max_scroll).clamp(0.0, 1.0);
        let view_frac = (upper.height() / (upper.height() + max_scroll)).min(1.0);
        let th = (track.height() * view_frac).max(24.0 * s);
        let thumb = Rect::from_min_size(
            Pos2::new(track.min.x, track.min.y + (track.height() - th) * frac),
            Vec2::new(track.width(), th),
        );
        painter.rect_filled(
            thumb,
            CornerRadius::same(2),
            Color32::from_white_alpha((80.0 * alpha) as u8),
        );
    }

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
    let installed = PROJECTS
        .iter()
        .filter(|p| {
            install_dir(p.slug)
                .as_deref()
                .and_then(installed_stamp)
                .is_some()
        })
        .count();
    let updates = PROJECTS
        .iter()
        .filter(|p| {
            card_action(
                install_dir(p.slug)
                    .as_deref()
                    .and_then(installed_stamp)
                    .as_deref(),
                view.remote.get(p.slug),
            ) == Action::Update
        })
        .count();
    let sub = view
        .notice
        .clone()
        .unwrap_or_else(|| {
            format!(
                "{} projects · {} installed{}{}",
                PROJECTS.len(),
                installed,
                if updates > 0 {
                    format!(" · {updates} update{}", if updates == 1 { "" } else { "s" })
                } else {
                    String::new()
                },
                if view.check_running { " · checking…" } else { "" },
            )
        });
    painter.text(
        Pos2::new(band.min.x + 40.0 * s, cy + 14.0 * s),
        Align2::LEFT_CENTER,
        crate::ellipsize(&painter, &sub, &FontId::proportional(12.0 * s), band.width() * 0.6),
        FontId::proportional(12.0 * s),
        Color32::from_gray(150).gamma_multiply(alpha),
    );

    // Right cluster: refresh check + open the Decomps folder. Same
    // 60px icon-button slots as the emulation band's.
    let mut right_x = band.max.x - 50.0 * s;
    let rsz = Vec2::splat(60.0 * s);
    let rrect = Rect::from_center_size(Pos2::new(right_x, cy), rsz);
    let rr = if interactive {
        ui.interact(rrect, ui.id().with("decomp-refresh"), Sense::click())
    } else {
        ui.interact(rrect, ui.id().with("decomp-refresh"), Sense::hover())
    }
    .on_hover_text(if view.check_running {
        "Checking…"
    } else {
        "Check for updates"
    });
    if rr.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    crate::transport::icon_update(&painter, rrect, alpha);
    if rr.clicked() {
        view.notice = None;
        view.check_updates();
    }
    right_x -= 64.0 * s;
    let orect = Rect::from_center_size(Pos2::new(right_x, cy), rsz);
    let orr = if interactive {
        ui.interact(orect, ui.id().with("decomp-open"), Sense::click())
    } else {
        ui.interact(orect, ui.id().with("decomp-open"), Sense::hover())
    }
    .on_hover_text("Open Decomps folder");
    if orr.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    icon_folder(&painter, orect, alpha);
    if orr.clicked() {
        if let Some(d) = decomps_root() {
            let _ = std::fs::create_dir_all(&d);
            open_in_file_manager(&d);
        }
    }
}

/// A plain folder glyph for the band's open button — sized and lit
/// to match the update cog's presence next to it.
fn icon_folder(painter: &egui::Painter, rect: Rect, alpha: f32) {
    let c = Color32::from_gray(235).gamma_multiply(alpha);
    let k = rect.height() / 100.0;
    let cx = rect.center();
    let w = 56.0 * k;
    let h = 42.0 * k;
    let y = cx.y - h * 0.5 + 5.0 * k;
    // Back of the folder with the tab raised on the left.
    let tab = Rect::from_min_size(
        Pos2::new(cx.x - w * 0.5, y),
        Vec2::new(w * 0.38, 8.0 * k),
    );
    let body = Rect::from_min_size(
        Pos2::new(cx.x - w * 0.5, y + 6.0 * k),
        Vec2::new(w, h - 6.0 * k),
    );
    painter.rect_filled(
        tab,
        CornerRadius::same((2.0 * k).round() as u8),
        c,
    );
    painter.rect_filled(
        body,
        CornerRadius::same((3.0 * k).round() as u8),
        c,
    );
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

    /// Keyboard nav clamps at the edges and vertical steps use the
    /// computed column count.
    #[test]
    fn move_selection_walks_grid() {
        let mut v = DecompView::new();
        v.columns = 4;
        v.move_selection(1, 0);
        assert_eq!(v.selected, 1);
        v.move_selection(0, 1);
        assert_eq!(v.selected, 5);
        v.move_selection(-1, 0);
        assert_eq!(v.selected, 4);
        v.move_selection(0, -10);
        assert_eq!(v.selected, 0);
        v.move_selection(0, 99);
        assert_eq!(v.selected, PROJECTS.len() as i32 - 1);
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
        let none = vec![a("port-linux-x64.tar.gz"), a("port-macos.dmg")];
        assert_eq!(pick_os_asset(&none), None);
        // A single unmarked-arch windows build still wins.
        let single = vec![a("port-linux.tar.gz"), a("port-windows.zip")];
        assert_eq!(
            pick_os_asset(&single).as_deref(),
            Some("https://x/port-windows.zip")
        );
    }
}
