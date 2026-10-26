//! The re/decomp catalog — fetched primarily from decomps.net's
//! project listing (server-rendered cards carrying console, type,
//! repo, stars, activity and box-art thumbs) and rebuilt into a
//! normalized `catalog.json` under `<data>/Decomps/`.
//!
//! The fallback source is the Recompendium data repo
//! (`Nio03/unricopie`, the dataset behind recompendium.com, CC BY-SA
//! 4.0): one zipball carrying per-project YAML files under
//! `src/data/{recomps,ports,decomps,mods,tools}/` plus `stats.json`
//! with fresh GitHub stats (`pushedAt`, latest release `version`,
//! stars, license, language) for every repo — so the per-repo
//! `pushed_at` update check costs zero extra API calls.
//!
//! The cached JSON keeps the app functional offline; the built-in
//! `decomp::PROJECTS` manifest is the last-resort fallback.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Upstream dataset — fetched as a default-branch zipball.
const UPSTREAM_REPO: &str = "Nio03/unricopie";
/// Normalized cache file inside `decomps_root()`.
const CATALOG_FILE: &str = "catalog.json";
/// The cache is refreshed on view-open when older than this.
pub const STALE_AFTER_SECS: i64 = 24 * 3600;

/// Catalog category — the upstream data folder the entry came from.
/// `Disasm` exists only for the built-in fallback manifest (pret
/// disassemblies aren't tracked upstream).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Decomp,
    Recomp,
    Disasm,
    Port,
    Reimpl,
    Mod,
    Tool,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Decomp => "Decomp",
            Kind::Recomp => "Recomp",
            Kind::Disasm => "Disassembly",
            Kind::Port => "Port",
            Kind::Reimpl => "Reimplementation",
            Kind::Mod => "Mod",
            Kind::Tool => "Tool",
        }
    }

    /// Filter-chip order — Ports first, they're the playable ones.
    pub const CHIPS: &'static [Kind] = &[
        Kind::Port,
        Kind::Reimpl,
        Kind::Recomp,
        Kind::Decomp,
        Kind::Mod,
        Kind::Tool,
    ];
}

/// Maturity label from upstream (`status:`), drives the badge color.
pub fn status_label(status: &str) -> Option<(&'static str, eframe::egui::Color32)> {
    use eframe::egui::Color32;
    Some(match status {
        "experimental" => ("Experimental", Color32::from_rgb(255, 120, 110)),
        "beta" => ("Beta", Color32::from_rgb(255, 175, 90)),
        "usable" => ("Usable", Color32::from_rgb(235, 210, 90)),
        "playable" => ("Playable", Color32::from_rgb(140, 210, 120)),
        "fully" | "stable" => ("Complete", Color32::from_rgb(90, 220, 130)),
        _ => return None,
    })
}

/// A normalized catalog entry — one YAML file plus its `stats.json`
/// facts merged by `repo`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub slug: String,
    pub name: String,
    pub kind: Kind,
    /// Upstream console id (n64, ps2, …) — `console_label` for display.
    pub console: String,
    #[serde(default)]
    pub status: String,
    /// `owner/repo` on GitHub — empty for listed-but-repoless plans.
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub developer: String,
    #[serde(default)]
    pub toolchain: String,
    #[serde(default)]
    pub fork_label: String,
    #[serde(default)]
    pub fork_url: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub requirements: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub featured: bool,
    /// Box-art search phrase.
    #[serde(default)]
    pub cover_q: String,
    // --- merged from stats.json ---
    #[serde(default)]
    pub stars: Option<u64>,
    /// Remote `pushedAt` — drives the update badge like the old
    /// per-repo API check did.
    #[serde(default)]
    pub pushed_at: String,
    /// Latest release tag upstream knows about.
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub language: String,
    /// Original game's release year.
    #[serde(default)]
    pub year: Option<u32>,
    /// Recomp completion 0–100, when upstream tracks it.
    #[serde(default)]
    pub progress: Option<u8>,
    /// Upstream flags — a dead or legally-removed project still
    /// lists, but flagged on the card and in the detail.
    #[serde(default)]
    pub abandoned: bool,
    #[serde(default)]
    pub takedown: bool,
    #[serde(default)]
    pub author: String,
    /// The game a mod applies to (mods only).
    #[serde(default)]
    pub game: String,
    /// Extra curated links (label, url).
    #[serde(default)]
    pub links: Vec<(String, String)>,
    /// Direct cover image URL — decomps.net thumbs skip the box-art
    /// search entirely; empty means the worker falls back to
    /// `cover_q`.
    #[serde(default)]
    pub cover_url: String,
    /// Marked AI-assisted upstream.
    #[serde(default)]
    pub ai: bool,
    /// Lowercased search haystack — filled by the view when entries
    /// load; never serialized into the cache.
    #[serde(skip)]
    pub search_blob: String,
}


/// Display name for an upstream console id.
pub fn console_label(id: &str) -> String {
    match id {
        "gc" | "gamecube" => "GameCube",
        "wiiu" => "Wii U",
        "switch" => "Switch",
        "switch2" => "Switch 2",
        "nds" | "ds" => "DS",
        "wii" => "Wii",
        "amiga" => "Amiga",
        "megadrive" | "genesis" => "Genesis",
        "segacd" => "Sega CD",
        "sms" => "Master System",
        "xbox" => "Xbox",
        "x360" => "Xbox 360",
        "xboxone" => "Xbox One",
        "xsx" => "Xbox S/X",
        "psp" => "PSP",
        "vita" => "PS Vita",
        "pc" => "PC",
        "dos" => "DOS",
        "mac" => "Mac",
        "mobile" => "Mobile",
        "arcade" => "Arcade",
        "neogeo" => "Neo Geo",
        "dreamcast" => "Dreamcast",
        "saturn" => "Saturn",
        "megacd" => "Sega CD",
        "gamegear" => "Game Gear",
        "32x" => "32X",
        "virtualboy" => "Virtual Boy",
        "pc98" => "PC-98",
        "other" => "Other",
        other => return other.to_uppercase(),
    }
    .to_string()
}

/// The rebuilt database — normalized entries + fetch stamp.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Catalog {
    pub fetched_at: i64,
    pub source: String,
    pub entries: Vec<Entry>,
}

/// `catalog.json` under the Decomps root.
fn catalog_path() -> Option<PathBuf> {
    super::decomp::decomps_root().map(|d| d.join(CATALOG_FILE))
}

/// Read the normalized cache, if present and parseable.
pub fn load_cached() -> Option<Catalog> {
    let path = catalog_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Cache age check for the auto-refresh policy.
pub fn is_stale(c: &Catalog) -> bool {
    now_secs() - c.fetched_at > STALE_AFTER_SECS
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Tolerant YAML shapes — every field optional; unknown keys ignored.
/// `desc`/`requirements` are `{en, es}` maps or plain strings.
#[derive(Deserialize)]
struct YamlEntry {
    name: Option<String>,
    console: Option<String>,
    status: Option<String>,
    repo: Option<String>,
    homepage: Option<String>,
    #[serde(rename = "originalDeveloper")]
    original_developer: Option<String>,
    toolchain: Option<String>,
    #[serde(rename = "forkOf")]
    fork_of: Option<YamlFork>,
    desc: Option<serde_norway::Value>,
    requirements: Option<serde_norway::Value>,
    tags: Option<Vec<String>>,
    featured: Option<bool>,
    year: Option<u32>,
    progress: Option<u8>,
    abandoned: Option<bool>,
    takedown: Option<bool>,
    author: Option<String>,
    game: Option<serde_norway::Value>,
    links: Option<Vec<YamlLink>>,
}

#[derive(Deserialize)]
struct YamlLink {
    label: Option<serde_norway::Value>,
    url: Option<String>,
}

#[derive(Deserialize)]
struct YamlFork {
    label: Option<String>,
    url: Option<String>,
}

#[derive(Deserialize, Default)]
struct StatEntry {
    stars: Option<u64>,
    #[serde(rename = "pushedAt")]
    pushed_at: Option<String>,
    language: Option<String>,
    version: Option<String>,
    license: Option<String>,
}

/// `en` text out of a localized YAML value — falls back to `es`
/// (upstream allows `{es}` alone) or a plain scalar.
fn en_text(v: Option<serde_norway::Value>) -> String {
    match v {
        Some(serde_norway::Value::Mapping(m)) => {
            for lang in ["en", "es"] {
                if let Some(s) = m
                    .get(serde_norway::Value::String(lang.into()))
                    .and_then(|v| v.as_str())
                {
                    return s.trim().to_string();
                }
            }
            String::new()
        }
        Some(serde_norway::Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    }
}

/// Install slugs of the previous hardcoded manifest, keyed by repo.
/// Kept so existing `<data>/Decomps/<old-slug>` installs (markers,
/// covers, baseroms) map onto the new catalog entries — applied only
/// when the repo appears exactly once upstream (Zelda64Recomp is
/// shared by two games, so its old slug isn't reused).
const LEGACY_SLUGS: &[(&str, &str)] = &[
    ("n64decomp/sm64", "sm64"),
    ("zeldaret/oot", "oot"),
    ("zeldaret/mm", "mm"),
    ("zeldaret/tp", "tp"),
    ("n64decomp/banjo-kazooie", "banjo"),
    ("n64decomp/perfect_dark", "perfect_dark"),
    ("pmret/papermario", "papermario"),
    ("open-goal/jak-project", "jak"),
    ("pret/pokered", "pokered"),
    ("pret/pokecrystal", "pokecrystal"),
    ("Rubberduckycooly/Sonic-CD-11-Decompilation", "sonic-cd"),
    ("Rubberduckycooly/Sonic-1-2-2013-Decompilation", "sonic-1-2"),
    ("Rubberduckycooly/Sonic-Mania-Decompilation", "sonic-mania"),
    ("diasurgical/devilution", "devilution"),
    ("diasurgical/devilutionX", "devilutionx"),
    ("alexbatalov/fallout1-ce", "fallout1-ce"),
    ("alexbatalov/fallout2-ce", "fallout2-ce"),
    ("dethrace-labs/dethrace", "dethrace"),
    ("Zelda64Recomp/Zelda64Recomp", "z64recomp"),
];

/// The data folders upstream and the kind they map to.
const CATEGORIES: &[(&str, Kind)] = &[
    ("ports", Kind::Port),
    ("recomps", Kind::Recomp),
    ("decomps", Kind::Decomp),
    ("mods", Kind::Mod),
    ("tools", Kind::Tool),
];

/// Fetch + rebuild the catalog: decomps.net first, the Recompendium
/// zipball when the site can't be reached or no longer parses.
/// The winning catalog is persisted to `catalog.json` — the disk
/// cache keeps the app functional offline either way.
/// Runs on a worker.
pub fn fetch() -> Result<Catalog, String> {
    match fetch_decomps().or_else(|de| {
        fetch_unricopie().map_err(|ue| format!("decomps.net: {de}; recompendium: {ue}"))
    }) {
        Ok(c) => {
            persist(&c);
            Ok(c)
        }
        Err(e) => Err(e),
    }
}

/// Write `catalog.json` next to the Decomps installs.
fn persist(c: &Catalog) {
    if let Some(path) = catalog_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(
            &path,
            serde_json::to_string(c).unwrap_or_default(),
        );
    }
}

/// Slug uniqueness + legacy remap + `cover_q` + sort — the
/// normalization both sources share once their raw entries exist.
fn finalize(entries: &mut Vec<Entry>) {
    // Legacy slug remap — only when the repo maps to exactly one
    // upstream entry.
    let mut repo_count: HashMap<String, usize> = HashMap::new();
    for e in entries.iter() {
        if !e.repo.is_empty() {
            *repo_count.entry(e.repo.clone()).or_default() += 1;
        }
    }
    let mut used: HashMap<String, usize> = HashMap::new();
    for e in entries.iter_mut() {
        if repo_count.get(&e.repo) == Some(&1) {
            if let Some(old) = LEGACY_SLUGS
                .iter()
                .find(|(repo, _)| *repo == e.repo)
                .map(|(_, slug)| *slug)
            {
                e.slug = old.to_string();
            }
        }
        // Filename collisions across category dirs get a kind suffix.
        let n = used.entry(e.slug.clone()).or_insert(0);
        *n += 1;
        if *n > 1 {
            e.slug = format!("{}-{}", e.slug, e.kind.label().to_lowercase());
        }
        if e.cover_q.is_empty() {
            e.cover_q = format!("{} box art", e.name);
        }
    }
    entries.sort_by(|a, b| {
        b.featured
            .cmp(&a.featured)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// Fallback source: upstream zipball → parse every YAML under
/// `src/data/` → merge `stats.json` → normalize.
fn fetch_unricopie() -> Result<Catalog, String> {
    let root = super::decomp::decomps_root().ok_or("Could not resolve the data directory")?;
    let stage = root.join("dl_tmp").join("catalog");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).map_err(|e| format!("Catalog dir not writable: {e}"))?;

    let zip = stage.join("catalog.zip");
    let url = format!("https://api.github.com/repos/{UPSTREAM_REPO}/zipball");
    let dl: super::emu_dl::Shared = std::sync::Arc::new(std::sync::Mutex::new(
        super::emu_dl::DlProgress::default(),
    ));
    super::emu_dl::download(&dl, "Fetching catalog…", &url, &zip)?;

    let extracted = stage.join("extracted");
    std::fs::create_dir_all(&extracted).map_err(|e| format!("Extract failed: {e}"))?;
    super::emu_dl::extract_archive(&zip, &extracted)?;
    let data_dir = super::emu_dl::normalize_root(&extracted)
        .join("src")
        .join("data");
    if !data_dir.is_dir() {
        return Err("Upstream layout changed — src/data not found".to_string());
    }

    let stats: HashMap<String, StatEntry> = std::fs::read_to_string(data_dir.join("stats.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();

    let mut entries = Vec::new();
    for (dir, kind) in CATEGORIES {
        let dir_path = data_dir.join(dir);
        let Ok(rd) = std::fs::read_dir(&dir_path) else {
            continue;
        };
        for f in rd.flatten() {
            let path = f.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let slug = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(y) = serde_norway::from_str::<YamlEntry>(&text) else {
                continue;
            };
            let repo = y.repo.unwrap_or_default().trim().trim_matches('"').to_string();
            let name = y.name.unwrap_or_else(|| slug.clone());
            let stat = stats.get(&repo);
            let (fork_label, fork_url) = y
                .fork_of
                .map(|f| (f.label.unwrap_or_default(), f.url.unwrap_or_default()))
                .unwrap_or_default();
            entries.push(Entry {
                slug,
                name: name.trim().to_string(),
                kind: *kind,
                console: y.console.unwrap_or_default().trim().to_lowercase(),
                status: y.status.unwrap_or_default().trim().to_lowercase(),
                repo,
                homepage: y.homepage.unwrap_or_default().trim().to_string(),
                developer: y.original_developer.unwrap_or_default().trim().to_string(),
                toolchain: y.toolchain.unwrap_or_default().trim().to_string(),
                fork_label,
                fork_url,
                description: en_text(y.desc),
                requirements: en_text(y.requirements),
                tags: y.tags.unwrap_or_default(),
                featured: y.featured.unwrap_or(false),
                cover_q: String::new(),
                stars: stat.and_then(|s| s.stars),
                pushed_at: stat.and_then(|s| s.pushed_at.clone()).unwrap_or_default(),
                version: stat.and_then(|s| s.version.clone()).unwrap_or_default(),
                license: stat.and_then(|s| s.license.clone()).unwrap_or_default(),
                language: stat.and_then(|s| s.language.clone()).unwrap_or_default(),
                year: y.year,
                progress: y.progress,
                abandoned: y.abandoned.unwrap_or(false),
                takedown: y.takedown.unwrap_or(false),
                author: y.author.unwrap_or_default().trim().to_string(),
                game: en_text(y.game),
                links: y
                    .links
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|l| {
                        let url = l.url?.trim().to_string();
                        (!url.is_empty()).then(|| (en_text(l.label), url))
                    })
                    .collect(),
                cover_url: String::new(),
                ai: false,
                search_blob: String::new(),
            });
        }
    }

    finalize(&mut entries);
    let _ = std::fs::remove_dir_all(&stage);
    Ok(Catalog {
        fetched_at: now_secs(),
        source: format!("{UPSTREAM_REPO}@main"),
        entries,
    })
}

// ------------------------------------------------------------------
// Primary source: decomps.net
// ------------------------------------------------------------------

/// The listing root — `?p=N` pages of 100 cards.
const DECOMPS: &str = "https://decomps.net";
/// Bail far past the real page count if the empty-page terminator
/// ever stops working.
const DECOMPS_MAX_PAGE: u32 = 40;

/// One listing card's scraped fields.
struct Card {
    /// The platform badge text — "Xbox 360", "GameCube".
    platform: String,
    kind: Kind,
    name: String,
    year: Option<u32>,
    /// `owner/repo` — empty for listed-but-repoless projects.
    repo: String,
    language: String,
    ai: bool,
    desc: String,
    stars: Option<u64>,
    /// Raw relative-activity text — "1 week ago", "yesterday".
    active: String,
    /// `/media/thumbs/…` path — box art without a search round-trip.
    thumb: String,
    /// `/game/…` detail page.
    page: String,
}

/// Scrape the listing pages until one comes back cardless (the
/// site's own end-of-pagination signal — page N+1 renders zero
/// `.pj-l` blocks rather than a 404).
fn fetch_decomps() -> Result<Catalog, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into();
    let mut cards = Vec::new();
    for page in 1..=DECOMPS_MAX_PAGE {
        let mut resp = agent
            .get(&format!("{DECOMPS}/?p={page}&f=1"))
            .call()
            .map_err(|e| format!("page {page}: {e}"))?;
        let bytes = resp
            .body_mut()
            .read_to_vec()
            .map_err(|e| format!("page {page}: {e}"))?;
        if parse_listing(&String::from_utf8_lossy(&bytes), &mut cards) == 0 {
            break;
        }
    }
    if cards.is_empty() {
        return Err("no project cards parsed".to_string());
    }

    let now = now_secs();
    let mut entries: Vec<Entry> = cards
        .into_iter()
        .map(|c| {
            let repo_slug = c.repo.rsplit('/').next().unwrap_or(&c.repo);
            let slug = if repo_slug.is_empty() {
                slugify(&format!("{}-{}", c.name, c.kind.label()))
            } else {
                slugify(repo_slug)
            };
            let mut links = Vec::new();
            if !c.page.is_empty() {
                links.push(("decomps.net".to_string(), format!("{DECOMPS}{c_p}", c_p = c.page)));
            }
            Entry {
                slug,
                name: c.name,
                kind: c.kind,
                console: decomps_console(&slugify(&c.platform)),
                status: String::new(),
                repo: c.repo.clone(),
                homepage: String::new(),
                developer: String::new(),
                toolchain: String::new(),
                fork_label: String::new(),
                fork_url: String::new(),
                description: c.desc,
                requirements: String::new(),
                tags: Vec::new(),
                featured: false,
                cover_q: String::new(),
                stars: c.stars,
                // Unparseable activity stays empty — stamping `now`
                // would light a false update badge on every install.
                pushed_at: if c.active.is_empty() {
                    String::new()
                } else {
                    iso_of(now - active_secs(&c.active) as i64)
                },
                version: String::new(),
                license: String::new(),
                language: c.language,
                year: c.year,
                progress: None,
                abandoned: false,
                takedown: false,
                author: c.repo.split('/').next().unwrap_or_default().to_string(),
                game: String::new(),
                links,
                cover_url: c.thumb,
                ai: c.ai,
                search_blob: String::new(),
            }
        })
        .collect();

    // Repo basename is the natural slug; same-named repos across
    // owners fall back to owner-repo, then a counter.
    let mut used_slugs: HashMap<String, usize> = HashMap::new();
    for e in &mut entries {
        if used_slugs.contains_key(&e.slug) {
            let alt = slugify(&e.repo.replace('/', "-"));
            let n = used_slugs.get(&alt).copied().unwrap_or(0);
            e.slug = if n == 0 { alt } else { format!("{alt}-{n}") };
        }
        *used_slugs.entry(e.slug.clone()).or_default() += 1;
    }

    finalize(&mut entries);
    Ok(Catalog {
        fetched_at: now,
        source: "decomps.net".to_string(),
        entries,
    })
}

/// Cards parsed from one listing page — returns how many this page
/// appended (0 = end of pagination).
fn parse_listing(html: &str, out: &mut Vec<Card>) -> usize {
    let n0 = out.len();
    for chunk in html.split("<div class=\"pj-l\">").skip(1) {
        if let Some(c) = parse_card(chunk) {
            out.push(c);
        }
    }
    out.len() - n0
}

/// `s` between the first `a` and the next `b` after it.
fn between<'a>(s: &'a str, a: &str, b: &str) -> Option<&'a str> {
    let i = s.find(a)? + a.len();
    Some(&s[i..i + s[i..].find(b)?])
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut tag = false;
    for c in s.chars() {
        match c {
            '<' => tag = true,
            '>' => tag = false,
            _ if !tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// The handful of entities the listing actually emits (`&#039;`
/// apostrophes, `&amp;` in hrefs). `&amp;` first so double-escaped
/// sequences still decode.
fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
}

fn parse_card(chunk: &str) -> Option<Card> {
    let h3 = between(chunk, "<h3", "</h3>")?;
    // The kind spans inside <h3> are platform then type — scanned by
    // class, not position, so a missing year can't shift them. (The
    // placeholder thumb carries a `kind pcN` span too, but it sits
    // outside the h3.)
    let mut platform = String::new();
    let mut kind = None;
    let mut scan = h3;
    while let Some(i) = scan.find("class=\"kind ") {
        let cls = between(&scan[i..], "class=\"kind ", "\"").unwrap_or_default();
        let text = between(&scan[i..], ">", "</span>")
            .map(|t| unescape(strip_tags(t).trim()))
            .unwrap_or_default();
        if let Some(k) = cls.strip_prefix("k-") {
            kind = match k {
                "decompilation" => Some(Kind::Decomp),
                "disassembly" => Some(Kind::Disasm),
                "recompilation" => Some(Kind::Recomp),
                "port" => Some(Kind::Port),
                "reimplementation" => Some(Kind::Reimpl),
                _ => kind,
            };
        } else if platform.is_empty() {
            platform = text;
        }
        scan = &scan[i + 12..];
    }
    let name = between(h3, "class=\"lnk\"", "</a>")
        .and_then(|s| s.find('>').map(|i| s[i + 1..].to_string()))
        .map(|s| unescape(strip_tags(&s).trim()))?;
    if name.is_empty() {
        return None;
    }
    let year = between(h3, "class=\"hint\">", "</span>")
        .and_then(|s| s.trim().parse::<u32>().ok());

    let repo = between(chunk, "href=\"/progress/", "\"")
        .unwrap_or_default()
        .to_string();
    let hint = between(chunk, "<p class=\"hint\">", "</p>")
        .map(|h| unescape(strip_tags(h).trim()))
        .unwrap_or_default();
    let ai = hint.contains("AI-assisted");
    // The language is the last `·`-separated segment — a lone repo
    // slug or the AI marker isn't one.
    let last = hint.rsplit('·').next().unwrap_or_default().trim();
    let language = if last.is_empty() || last == repo || last == "AI-assisted" {
        String::new()
    } else {
        last.to_string()
    };
    let desc = between(chunk, "<p class=\"f14 muted lh15\">", "</p>")
        .map(|d| unescape(strip_tags(d).trim()))
        .unwrap_or_default();

    let meta = between(chunk, "thr-meta", "</div>").unwrap_or_default();
    let active = between(meta, ">active ", "<")
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    Some(Card {
        platform,
        kind: kind?,
        name,
        year,
        repo,
        language,
        ai,
        desc,
        stars: parse_stars(meta),
        active,
        thumb: between(chunk, "class=\"pj-th\" src=\"", "\"")
            .map(|t| format!("{DECOMPS}{t}"))
            .unwrap_or_default(),
        page: between(chunk, "href=\"/game/", "\"")
            .map(|p| format!("/game/{p}"))
            .unwrap_or_default(),
    })
}

/// `★ 9.9K` → 9900, `★ 1.2M` → 1200000, plain ints pass through.
fn parse_stars(meta: &str) -> Option<u64> {
    let t = meta[meta.find('★')? + '★'.len_utf8()..].trim_start();
    let end = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let v: f64 = t[..end].parse().ok()?;
    let mult = match t[end..].trim_start().chars().next() {
        Some('K') | Some('k') => 1_000.0,
        Some('M') | Some('m') => 1_000_000.0,
        _ => 1.0,
    };
    Some((v * mult) as u64)
}

/// "1 week ago" → seconds back. The site only emits rough relative
/// activity — an approximate `pushed_at` is still enough for the
/// update badge's lexical compare.
fn active_secs(s: &str) -> u64 {
    let s = s.strip_suffix(" ago").unwrap_or(s).trim();
    match s {
        "yesterday" => return 86_400,
        "today" => return 0,
        _ => {}
    }
    let mut it = s.split_whitespace();
    let n: u64 = it.next().and_then(|t| t.parse().ok()).unwrap_or(0);
    let unit = it.next().unwrap_or_default();
    n * match unit.chars().next().unwrap_or('h') {
        'd' => 86_400,
        'w' => 604_800,
        'm' => 2_592_000,
        'y' => 31_536_000,
        _ => 3_600,
    }
}

/// Unix seconds → the ISO-8601 stamp `pushed_at` compares lexically.
fn iso_of(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs.max(0), 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

/// Lowercase, alnum runs joined by single dashes — both the console
/// id basis ("Xbox 360" → "xbox-360") and the install-dir slug.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut dash = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// decomps.net platform slug → our console id (they differ where our
/// id space predates the site). Unknown slugs pass through —
/// `console_label`'s uppercase fallback keeps them displayable.
fn decomps_console(slug: &str) -> String {
    match slug {
        "wii-u" => "wiiu",
        "xbox-360" => "x360",
        "ds" => "nds",
        "sega-cd" => "segacd",
        "master-system" => "sms",
        "neo-geo" => "neogeo",
        s => s,
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// YAML with a localized desc and a stats row parses into a
    /// normalized entry; plain-string desc works too.
    #[test]
    fn en_text_handles_map_and_scalar() {
        let map: serde_norway::Value =
            serde_norway::from_str("en: \"Hello\"\nes: \"Hola\"").unwrap();
        assert_eq!(en_text(Some(map)), "Hello");
        let scalar: serde_norway::Value = serde_norway::from_str("\"Just text\"").unwrap();
        assert_eq!(en_text(Some(scalar)), "Just text");
        assert_eq!(en_text(None), "");
    }

    #[test]
    fn console_labels_resolve() {
        assert_eq!(console_label("n64"), "N64");
        assert_eq!(console_label("gc"), "GameCube");
        assert_eq!(console_label("x360"), "Xbox 360");
        assert_eq!(console_label("wii"), "Wii");
        assert_eq!(console_label("amiga"), "Amiga");
        assert_eq!(console_label("weirdthing"), "WEIRDTHING");
    }

    /// The decomps listing card's fields land in `Card` — a normal
    /// row, an AI-assisted row with a placeholder thumb and no year,
    /// and HTML entities in names/descriptions.
    #[test]
    fn parse_card_reads_listing_markup() {
        let chunk = r##"
<a href="/game/super-mario-64-fzBsCMucXe"><img class="pj-th" src="/media/thumbs/fzBsCMucXe.jpg?v=1" alt=""></a>
<div class="thr-main">
<h3 class="t17 pj-t"><span class="kind pc5">N64</span><span class="kind k-decompilation">Decompilation</span><a class="lnk" href="/game/super-mario-64-fzBsCMucXe">Super Mario 64</a><span class="hint">1996</span></h3>
<p class="hint"><a class="tl" href="/progress/n64decomp/sm64">n64decomp/sm64</a> · C</p>
<p class="f14 muted lh15">A Super Mario 64 decompilation, brought to you by a bunch of clever folks.</p></div>
</div>
<div class="thr-meta"><span title="Stars on github.com">★ 8.9K</span><span>active 2 years ago</span></div>
"##;
        let c = parse_card(chunk).unwrap();
        assert_eq!(c.platform, "N64");
        assert_eq!(c.kind, Kind::Decomp);
        assert_eq!(c.name, "Super Mario 64");
        assert_eq!(c.year, Some(1996));
        assert_eq!(c.repo, "n64decomp/sm64");
        assert_eq!(c.language, "C");
        assert!(!c.ai);
        assert_eq!(c.stars, Some(8900));
        assert_eq!(c.active, "2 years ago");
        assert_eq!(c.thumb, "https://decomps.net/media/thumbs/fzBsCMucXe.jpg?v=1");
        assert_eq!(c.page, "/game/super-mario-64-fzBsCMucXe");

        // Placeholder thumb (a `kind pcN` span OUTSIDE the h3 must
        // not shadow the platform), AI-assisted, apostrophe entity,
        // no year.
        let chunk2 = r##"
<a href="/game/x-hg5xLkDkoY"><span class="pj-th pj-ph kind pc3">Switch</span></a>
<div class="thr-main">
<h3 class="t17 pj-t"><span class="kind pc3">Switch</span><span class="kind k-port">Port</span><a class="lnk" href="/game/x-hg5xLkDkoY">Zelda&#039;s Game</a></h3>
<p class="hint"><a class="tl" href="/progress/a/b">a/b</a> · AI-assisted · C++</p>
<p class="f14 muted lh15">Runs on Apple silicon &amp; more</p></div>
</div>
<div class="thr-meta"><span title="Stars on github.com">★ 5.6K</span><span>active 10 h ago</span></div>
"##;
        let c2 = parse_card(chunk2).unwrap();
        assert_eq!(c2.platform, "Switch");
        assert_eq!(c2.kind, Kind::Port);
        assert_eq!(c2.name, "Zelda's Game");
        assert_eq!(c2.year, None);
        assert!(c2.ai);
        assert_eq!(c2.language, "C++");
        assert_eq!(c2.desc, "Runs on Apple silicon & more");
        assert_eq!(c2.thumb, "");

        // Reimplementation maps to its own kind; repo-less cards
        // carry no repo/language; "yesterday" activity.
        let chunk3 = r##"
<a href="/game/tomb-raider-z"><img class="pj-th" src="/media/thumbs/z.jpg"></a>
<div class="thr-main">
<h3 class="t17 pj-t"><span class="kind pc1">PC</span><span class="kind k-reimplementation">Reimplementation</span><a class="lnk" href="/game/tomb-raider-z">Tomb Raider</a><span class="hint">1996</span></h3>
<p class="hint"><a class="tl" href="/progress/XProger/OpenLara">XProger/OpenLara</a> · C</p>
<p class="f14 muted lh15">Classic Tomb Raider open-source engine</p></div>
</div>
<div class="thr-meta"><span>active yesterday</span></div>
"##;
        let c3 = parse_card(chunk3).unwrap();
        assert_eq!(c3.kind, Kind::Reimpl);
        assert_eq!(c3.stars, None);
        assert_eq!(c3.active, "yesterday");

        // parse_listing counts appended cards and skips junk.
        let mut v = Vec::new();
        let html = format!("<div class=\"pj-l\">{chunk}<div class=\"pj-l\">{chunk2}");
        assert_eq!(parse_listing(&html, &mut v), 2);
        assert_eq!(parse_listing("<html></html>", &mut v), 0);
    }

    /// decomps platform labels slugify then translate to our console
    /// ids — unknown labels pass through so `console_label`'s
    /// fallback still renders them.
    #[test]
    fn decomps_console_maps_site_slugs() {
        assert_eq!(decomps_console(&slugify("Xbox 360")), "x360");
        assert_eq!(decomps_console(&slugify("Wii U")), "wiiu");
        assert_eq!(decomps_console(&slugify("GameCube")), "gamecube");
        assert_eq!(decomps_console(&slugify("N64")), "n64");
        assert_eq!(decomps_console(&slugify("Neo Geo")), "neogeo");
        assert_eq!(decomps_console(&slugify("Sega CD")), "segacd");
        assert_eq!(decomps_console(&slugify("Amiga")), "amiga");
    }

    #[test]
    fn parse_stars_handles_suffixes() {
        assert_eq!(parse_stars("★ 9.9K"), Some(9900));
        assert_eq!(parse_stars("★ 1.2M"), Some(1_200_000));
        assert_eq!(parse_stars("★ 1"), Some(1));
        assert_eq!(parse_stars("no stars"), None);
    }

    /// Relative activity → seconds — only rough units exist upstream;
    /// the pushed_at ISO just needs to sort right.
    #[test]
    fn active_secs_covers_site_units() {
        assert_eq!(active_secs("10 h ago"), 36_000);
        assert_eq!(active_secs("3 days ago"), 259_200);
        assert_eq!(active_secs("1 week ago"), 604_800);
        assert_eq!(active_secs("2 months ago"), 5_184_000);
        assert_eq!(active_secs("1 year ago"), 31_536_000);
        assert_eq!(active_secs("yesterday"), 86_400);
        assert_eq!(iso_of(1_700_000_000).len(), 20);
    }

    #[test]
    fn slugify_normalizes() {
        assert_eq!(slugify("Super Mario 64"), "super-mario-64");
        assert_eq!(slugify("Pokémon Emerald"), "pokémon-emerald");
        assert_eq!(slugify("Xbox 360"), "xbox-360");
        assert_eq!(slugify(" --weird-- "), "weird");
    }

    /// Live check against the real upstream dataset — hit network,
    /// rebuild `catalog.json`, and sanity-check the result. Run with
    /// `cargo test fetch_upstream -- --ignored`.
    #[test]
    #[ignore]
    fn fetch_upstream_end_to_end() {
        let c = fetch().expect("fetch failed");
        assert!(c.entries.len() > 500, "got {}", c.entries.len());
        assert!(c.entries.iter().any(|e| e.kind == Kind::Port));
        assert!(c.entries.iter().any(|e| e.kind == Kind::Recomp));
        assert!(c.entries.iter().any(|e| e.kind == Kind::Decomp));
        // Stats merged: SoH's repo carries pushed_at + stars.
        let soh = c
            .entries
            .iter()
            .find(|e| e.repo == "HarbourMasters/Shipwright")
            .expect("Shipwright missing");
        assert!(!soh.pushed_at.is_empty());
        // The legacy remap kept old install slugs where the repo is
        // unique upstream.
        assert!(c.entries.iter().any(|e| e.slug == "sm64"));
        // Every entry got unique slugs.
        let mut seen = std::collections::HashSet::new();
        for e in &c.entries {
            assert!(seen.insert(e.slug.clone()), "dup slug {}", e.slug);
        }
    }
}
