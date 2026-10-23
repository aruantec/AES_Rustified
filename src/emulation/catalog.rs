//! The re/decomp catalog — fetched from the Recompendium data repo
//! (`Nio03/unricopie`, the dataset behind recompendium.com, CC BY-SA
//! 4.0) and rebuilt into a normalized `catalog.json` under
//! `<data>/Decomps/`.
//!
//! One zipball carries everything: per-project YAML files under
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
            Kind::Mod => "Mod",
            Kind::Tool => "Tool",
        }
    }

    /// Filter-chip order — Ports first, they're the playable ones.
    pub const CHIPS: &'static [Kind] = &[
        Kind::Port,
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
        "nds" => "DS",
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

/// Fetch + rebuild the catalog: upstream zipball → parse every YAML
/// under `src/data/` → merge `stats.json` → normalize (legacy slugs,
/// uniqueness) → sort → write `catalog.json`. Runs on a worker.
pub fn fetch() -> Result<Catalog, String> {
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
                search_blob: String::new(),
            });
        }
    }

    // Legacy slug remap — only when the repo maps to exactly one
    // upstream entry.
    let mut repo_count: HashMap<String, usize> = HashMap::new();
    for e in &entries {
        if !e.repo.is_empty() {
            *repo_count.entry(e.repo.clone()).or_default() += 1;
        }
    }
    let mut used: HashMap<String, usize> = HashMap::new();
    for e in &mut entries {
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
        e.cover_q = format!("{} box art", e.name);
    }
    entries.sort_by(|a, b| {
        b.featured
            .cmp(&a.featured)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    let catalog = Catalog {
        fetched_at: now_secs(),
        source: format!("{UPSTREAM_REPO}@main"),
        entries,
    };
    if let Some(path) = catalog_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(
            &path,
            serde_json::to_string(&catalog).unwrap_or_default(),
        );
    }
    let _ = std::fs::remove_dir_all(&stage);
    Ok(catalog)
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
        assert_eq!(console_label("weirdthing"), "WEIRDTHING");
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
