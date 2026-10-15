//! Emulation ROM metadata + online cover resolution — the Rust port
//! of Lacrima's `Xbox360MetadataService` (XEX header + XDVDFS walk +
//! path-derived title ids), `EmulationCoverCacheHelper` (per-ROM
//! sidecar files under the cache dir), and
//! `LibRetroThumbnailCoverService` (Named_Boxarts URL ladder).
//!
//! Resolution runs off-thread (`spawn_resolver`): each ROM yields a
//! title (database lookup via title id, else the file stem) and, when
//! missing, a downloaded cover image written to a content-keyed
//! sidecar so future loads never re-fetch.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::settings;

// ------------------------------------------------------------------
// Per-ROM sidecar cache (EmulationCoverCacheHelper)
// ------------------------------------------------------------------
fn cache_dir() -> PathBuf {
    settings::data_root()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Cache")
        .join("Emulation")
}

/// Filesystem key for a ROM path (normalized lowercase — drive-letter
/// and spelling variants share an entry).
fn key_for(rom: &Path) -> String {
    let norm = rom.to_string_lossy().to_lowercase();
    format!("{:016x}", crate::audio::stream_cache::hash_key(&norm))
}

/// Where a ROM's downloaded box art lives (`<key>.img`, Lacrima's
/// `{cacheId}.cover`). Raw downloaded bytes — the cover decode pool
/// handles png/jpg/webp.
pub fn cover_path(rom: &Path) -> PathBuf {
    cache_dir().join(format!("{}.img", key_for(rom)))
}

/// Resolved-title sidecar (`{cacheId}.meta` analog) — caches the
/// database lookup so an ISO's XDVDFS walk doesn't re-run every load.
fn meta_path(rom: &Path) -> PathBuf {
    cache_dir().join(format!("{}.title", key_for(rom)))
}

pub fn cached_title(rom: &Path) -> Option<String> {
    std::fs::read_to_string(meta_path(rom))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

fn save_cached_title(rom: &Path, title: &str) {
    let p = meta_path(rom);
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, title).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

fn save_cover(rom: &Path, bytes: &[u8]) -> Option<PathBuf> {
    let p = cover_path(rom);
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_err() {
        return None;
    }
    std::fs::rename(&tmp, &p).ok()?;
    Some(p)
}

// ------------------------------------------------------------------
// Database: Lacrima's Database/x360.json (titleid -> title)
// ------------------------------------------------------------------
#[derive(Deserialize)]
struct TitleEntry {
    titleid: String,
    title: String,
}

fn x360_titles() -> &'static HashMap<String, String> {
    static DB: OnceLock<HashMap<String, String>> = OnceLock::new();
    DB.get_or_init(|| {
        let json = include_str!("../assets/database/x360.json");
        serde_json::from_str::<Vec<TitleEntry>>(json)
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.titleid.len() == 8 && !e.title.is_empty())
            .map(|e| (e.titleid.to_uppercase(), e.title))
            .collect()
    })
}

// ------------------------------------------------------------------
// Xbox 360 metadata (Xbox360MetadataService)
// ------------------------------------------------------------------
const XEX2_MAGIC: u32 = 0x5845_5832; // "XEX2"
const EXEC_INFO_ID: u32 = 0x0004_0006;
const MAX_XEX_DIR_ENTRIES: u32 = 4096;
const MAX_XEX_READ: usize = 64 * 1024 * 1024;
const SECTOR: u64 = 0x800;
const MAX_DIR_BYTES: u32 = 32 * 1024 * 1024;
const MAX_ISO_SCAN: u64 = 512 * 1024 * 1024;
const XGD_MAGIC: &[u8] = b"MICROSOFT*XBOX*MEDIA";
const XGD_SECTORS: [u64; 4] = [0x20, 0x4120, 0x1FB40, 0x30620];

/// One resolved ROM: the database title (when the binary yielded a
/// known title id) — the cover candidate ladder also uses the stem.
pub struct RomInfo {
    pub path: PathBuf,
    /// Database-resolved title, when one was found.
    pub title: Option<String>,
    /// Freshly downloaded cover sidecar.
    pub cover: Option<PathBuf>,
}

fn be32(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_be_bytes(s.try_into().unwrap()))
}
fn le16(b: &[u8], off: usize) -> Option<u16> {
    b.get(off..off + 2)
        .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
}
fn le32(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
}

/// `title_id`/`media_id` out of an XEX2 header (`TryReadExecutionInfo`).
fn read_execution_info(data: &[u8], base: usize) -> Option<(u32, u32)> {
    if data.len() < base + 0x18 || be32(data, base)? != XEX2_MAGIC {
        return None;
    }
    let count = be32(data, base + 20)?;
    if count == 0 || count > MAX_XEX_DIR_ENTRIES {
        return None;
    }
    let dir = base + 0x18;
    let max_by_buf = (data.len().saturating_sub(dir)) / 8;
    for i in 0..(count as usize).min(max_by_buf) {
        let e = dir + i * 8;
        if be32(data, e)? != EXEC_INFO_ID {
            continue;
        }
        let exec = base + be32(data, e + 4)? as usize;
        let media = be32(data, exec)?;
        let title = be32(data, exec + 12)?;
        if looks_like_title_id(title) {
            return Some((title, media));
        }
    }
    None
}

fn looks_like_title_id(v: u32) -> bool {
    v != 0 && v != u32::MAX && v != 0x524F4D20 // "ROM "
}

fn read_xex_file(path: &Path) -> Option<(u32, u32)> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len().min(MAX_XEX_READ as u64) as usize;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf).ok()?;
    read_execution_info(&buf, 0)
}

fn read_sector(f: &mut std::fs::File, sector: u64) -> Option<[u8; SECTOR as usize]> {
    let mut buf = [0u8; SECTOR as usize];
    f.seek(SeekFrom::Start(sector.checked_mul(SECTOR)?)).ok()?;
    f.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn read_sector_region(f: &mut std::fs::File, start: u64, len: u32) -> Option<Vec<u8>> {
    if len == 0 || len as u64 > MAX_XEX_READ as u64 {
        return None;
    }
    let mut out = vec![0u8; len as usize];
    let mut copied = 0usize;
    while (copied as u32) < len {
        let sec = read_sector(f, start + (copied as u64 / SECTOR))?;
        let take = ((len as usize) - copied).min(SECTOR as usize);
        out[copied..copied + take].copy_from_slice(&sec[..take]);
        copied += take;
    }
    Some(out)
}

struct DirEntry {
    left: u16,
    right: u16,
    sector: u32,
    size: u32,
    is_dir: bool,
    name: String,
}

fn dir_entry(data: &[u8], offset: u32) -> Option<DirEntry> {
    let o = (offset as usize).checked_mul(4)?;
    let name_len = *data.get(o + 13)? as usize;
    if name_len == 0 {
        return None;
    }
    let name = data
        .get(o + 14..o + 14 + name_len)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .filter(|n| !n.trim().is_empty())?;
    Some(DirEntry {
        left: le16(data, o)?,
        right: le16(data, o + 2)?,
        sector: le32(data, o + 4)?,
        size: le32(data, o + 8)?,
        is_dir: data[o + 12] & 0x10 != 0,
        name,
    })
}

/// XDVDFS walk for `default.xex` (`TryExtractFileFromXdvdfs`).
fn extract_default_xex(f: &mut std::fs::File) -> Option<Vec<u8>> {
    let mut base_sector = 0u64;
    let mut root_sector = 0u32;
    let mut root_size = 0u32;
    for &magic_sector in &XGD_SECTORS {
        let Some(sec) = read_sector(f, magic_sector) else {
            continue;
        };
        if sec[..XGD_MAGIC.len()] != *XGD_MAGIC
            || sec[SECTOR as usize - XGD_MAGIC.len()..] != *XGD_MAGIC
        {
            continue;
        }
        let rs = le32(&sec, 20)?;
        let sz = le32(&sec, 24)?;
        if sz == 0 || sz > MAX_DIR_BYTES || magic_sector < 0x20 {
            continue;
        }
        base_sector = magic_sector - 0x20;
        root_sector = rs;
        root_size = sz;
        break;
    }
    if root_size == 0 {
        return None;
    }
    let root = read_sector_region(f, base_sector + root_sector as u64, root_size)?;
    let mut stack: Vec<(Vec<u8>, u32)> = vec![(root, 0)];
    while let Some((data, off)) = stack.pop() {
        let Some(e) = dir_entry(&data, off) else {
            continue;
        };
        if !e.is_dir && e.name.eq_ignore_ascii_case("default.xex") {
            if e.size == 0 || e.size > MAX_XEX_READ as u32 {
                return None;
            }
            return read_sector_region(f, base_sector + e.sector as u64, e.size);
        }
        for child in [e.right, e.left] {
            if child != 0 && child != u16::MAX && (child as usize) * 4 < data.len() {
                stack.push((data.clone(), child as u32));
            }
        }
        if e.is_dir && e.size > 0 && e.size <= MAX_DIR_BYTES {
            if let Some(sub) =
                read_sector_region(f, base_sector + e.sector as u64, e.size)
            {
                if !sub.is_empty() {
                    stack.push((sub, 0));
                }
            }
        }
    }
    None
}

/// `TryScanIsoForXexExecutionInfo` — when the XDVDFS walk fails, scan
/// the first 512MB for an XEX2 blob and parse its header in place.
fn scan_iso_for_xex(f: &mut std::fs::File, file_len: u64) -> Option<(u32, u32)> {
    const CHUNK: usize = 1024 * 1024;
    const WINDOW: u64 = 512 * 1024;
    let limit = file_len.min(MAX_ISO_SCAN);
    let mut buf = vec![0u8; CHUNK + 4];
    let mut carry = 0usize;
    let mut scanned = 0u64;
    f.seek(SeekFrom::Start(0)).ok()?;
    while scanned < limit {
        let want = (limit - scanned).min(CHUNK as u64) as usize;
        let read = f.read(&mut buf[carry..carry + want]).ok()?;
        if read == 0 {
            break;
        }
        let chunk_start = scanned; // absolute offset of buf[carry]
        let total = carry + read;
        for i in 0..total.saturating_sub(3) {
            if &buf[i..i + 4] != b"XEX2" {
                continue;
            }
            let abs = chunk_start + i as u64;
            let n = (WINDOW.min(file_len - abs as u64)) as usize;
            if n < 0x20 {
                continue;
            }
            f.seek(SeekFrom::Start(abs as u64)).ok()?;
            let mut win = vec![0u8; n];
            if f.read_exact(&mut win).is_err() {
                continue;
            }
            if let Some(info) = read_execution_info(&win, 0) {
                return Some(info);
            }
        }
        scanned += read as u64;
        carry = 3.min(total);
        buf.copy_within(total - carry..total, 0);
    }
    None
}

/// Path-derived title id (`TryGetTitleIdFromPath`) — a segment that is
/// exactly 8 hex chars (the `…/0000000000000000/<id>/` content layout
/// falls out of the same rule).
fn title_id_from_path(path: &Path) -> Option<String> {
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    for seg in path.to_string_lossy().split(['\\', '/']) {
        let b = seg.as_bytes();
        let mut i = 0;
        while i + 8 <= b.len() {
            let run = b[i..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
            // `\b[0-9A-Fa-f]{8}\b` — exactly 8 hex chars bounded by
            // non-word edges.
            if run == 8
                && (i == 0 || !word(b[i - 1]))
                && (i + 8 == b.len() || !word(b[i + 8]))
            {
                let id = seg[i..i + 8].to_uppercase();
                if x360_titles().contains_key(&id) {
                    return Some(id);
                }
            }
            i += run.max(1);
        }
    }
    None
}

/// `TryReadGameMetadata` — title id out of the binary (xex / iso /
/// xiso), the path, then the database title for it.
fn x360_title(path: &Path) -> Option<String> {
    let title_id = if path.is_dir() {
        walkdir_find_xex(path).and_then(|x| read_xex_file(&x))
    } else {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .as_deref()
        {
            Some("xex") => read_xex_file(path),
            Some("iso") | Some("xiso") => {
                let mut f = std::fs::File::open(path).ok()?;
                let len = f.metadata().ok()?.len();
                extract_default_xex(&mut f)
                    .and_then(|d| read_execution_info(&d, 0))
                    .or_else(|| scan_iso_for_xex(&mut f, len))
            }
            _ => None,
        }
    }
    .map(|(t, _)| format!("{t:08X}"))
    .or_else(|| title_id_from_path(path))?;
    x360_titles().get(&title_id).cloned()
}

fn walkdir_find_xex(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n == "default.xex") {
                return Some(p);
            }
        }
    }
    None
}

// ------------------------------------------------------------------
// LibRetro thumbnails (LibRetroThumbnailCoverService)
// ------------------------------------------------------------------
const LIBRETRO_BASE: &str = "https://thumbnails.libretro.com/";
const REGION_SUFFIXES: &[&str] = &[
    "(USA)",
    "(Europe)",
    "(World)",
    "(Japan)",
    "(Australia)",
    "(Canada)",
    "(France)",
    "(Germany)",
    "(Spain)",
    "(Italy)",
    "(Korea)",
    "(Brazil)",
    "(Netherlands)",
    "(Sweden)",
    "(Asia)",
];

/// `LibRetroPlatformCatalog.ConsoleKeyToFolder`.
fn libretro_folder(section_key: &str) -> Option<&'static str> {
    Some(match section_key {
        "SNES" => "Nintendo - Super Nintendo Entertainment System",
        "NES" => "Nintendo - Nintendo Entertainment System",
        "N64" => "Nintendo - Nintendo 64",
        "GBA" => "Nintendo - Game Boy Advance",
        "GCN" => "Nintendo - GameCube",
        "WII" => "Nintendo - Wii",
        "WIIU" => "Nintendo - Wii U",
        "3DS" => "Nintendo - Nintendo 3DS",
        "SWITCH" => "Nintendo - Nintendo Switch",
        "PSX" => "Sony - PlayStation",
        "PS2" => "Sony - PlayStation 2",
        "PS3" => "Sony - PlayStation 3",
        "PSP" => "Sony - PlayStation Portable",
        "DC" => "Sega - Dreamcast",
        "GENESIS" => "Sega - Mega Drive - Genesis",
        "SATURN" => "Sega - Saturn",
        "XBOX" => "Microsoft - Xbox",
        "X360" => "Microsoft - Xbox 360",
        "ARCADE" => "MAME",
        "FBN" => "FBNeo - Arcade Games",
        _ => return None,
    })
}

/// `Uri.EscapeDataString` — unreserved chars pass, else UTF-8 %XX.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn strip_region(title: &str) -> &str {
    for suf in REGION_SUFFIXES {
        if title.len() > suf.len()
            && title[title.len() - suf.len()..].eq_ignore_ascii_case(suf)
        {
            return title[..title.len() - suf.len()].trim_end();
        }
    }
    title
}

/// `ToLibRetroDisplayTitle` — all-caps stems (dumps like
/// "GEARS OF WAR 3") become title-case, the libretro listing's casing.
pub(crate) fn display_title(title: &str) -> String {
    let (base, region) = REGION_SUFFIXES
        .iter()
        .find(|s| {
            title.len() > s.len()
                && title[title.len() - s.len()..].eq_ignore_ascii_case(s)
        })
        .map(|s| (title[..title.len() - s.len()].trim_end(), *s))
        .unwrap_or((title, ""));
    let cased = if base.chars().any(|c| c.is_lowercase()) || base.is_empty() {
        base.to_string()
    } else {
        let mut out = String::with_capacity(base.len());
        let mut cap = true;
        for c in base.chars() {
            if c.is_ascii_alphabetic() {
                out.push(if cap { c } else { c.to_ascii_lowercase() });
                cap = false;
            } else {
                out.push(c);
                cap = !c.is_ascii();
            }
        }
        out
    };
    if region.is_empty() {
        cased
    } else {
        format!("{cased} {region}")
    }
}

/// `ExpandTitleVariants` — base + title-case variant, each also
/// region-stripped and region-appended, in order.
fn title_variants(title: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push = |v: String, out: &mut Vec<String>| {
        if !v.is_empty() && !out.iter().any(|e| e.eq_ignore_ascii_case(&v)) {
            out.push(v);
        }
    };
    for base in [title.trim().to_string(), display_title(title.trim())] {
        if base.is_empty() {
            continue;
        }
        push(base.clone(), &mut out);
        let stripped = strip_region(&base).to_string();
        if stripped != base {
            push(stripped.clone(), &mut out);
        }
        if strip_region(&base) == base {
            for suf in REGION_SUFFIXES {
                push(format!("{stripped} {suf}"), &mut out);
            }
        }
    }
    out
}

// ------------------------------------------------------------------
// LibRetro per-platform index — one fetch per section replaces the
// ~100-URL guess ladder with local matching. The GitHub tree API
// serves the repo's Named_Boxarts listing; cached on disk 7 days so
// a section's index costs two requests a week, not per run.
// ------------------------------------------------------------------
fn libretro_index(agent: &ureq::Agent, section_key: &str) -> Option<std::sync::Arc<HashSet<String>>> {
    use std::sync::{Arc, Mutex, OnceLock};
    static IDX: OnceLock<Mutex<HashMap<String, Option<Arc<HashSet<String>>>>>> = OnceLock::new();
    let map = IDX.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(v) = map.get(section_key) {
        return v.clone();
    }
    let loaded = load_libretro_index(agent, section_key);
    map.insert(section_key.to_string(), loaded.clone());
    loaded
}

fn index_cache_path(section_key: &str) -> PathBuf {
    cache_dir().join(format!("libretro-{section_key}.idx"))
}

fn load_libretro_index(
    agent: &ureq::Agent,
    section_key: &str,
) -> Option<std::sync::Arc<HashSet<String>>> {
    // Disk cache — fresh <7d wins; a stale cache is still better than
    // none when the API is down/rate-limited.
    let cache = index_cache_path(section_key);
    let mut stale: Option<Vec<String>> = None;
    if let Ok(bytes) = std::fs::read(&cache) {
        let names: Vec<String> = String::from_utf8_lossy(&bytes)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        if !names.is_empty() {
            let fresh = std::fs::metadata(&cache)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|e| e.as_secs() < 7 * 24 * 3600);
            if fresh {
                return Some(std::sync::Arc::new(
                    names.iter().map(|n| n.to_lowercase()).collect(),
                ));
            }
            stale = Some(names);
        }
    }
    let names = fetch_libretro_index(agent, section_key).or(stale);
    names.map(|names| {
        // Refresh the disk copy when it came from the network —
        // `stale` means it was already there.
        if let Some(d) = cache.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&cache, names.join("\n"));
        std::sync::Arc::new(names.iter().map(|n| n.to_lowercase()).collect())
    })
}

/// GitHub tree API: root tree → the `Named_Boxarts` subtree listing
/// (two requests total — non-recursive).
fn fetch_libretro_index(agent: &ureq::Agent, section_key: &str) -> Option<Vec<String>> {
    let folder = libretro_folder(section_key)?;
    let repo = folder.replace(' ', "_");
    let root = github_tree(agent, &repo, "master")?;
    let sha = root
        .iter()
        .find(|(p, _)| p == "Named_Boxarts")?
        .1
        .clone();
    let sub = github_tree(agent, &repo, &sha)?;
    let names: Vec<String> = sub
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| p.ends_with(".png") || p.ends_with(".jpg") || p.ends_with(".webp"))
        .collect();
    (!names.is_empty()).then_some(names)
}

/// `git/trees/<sha>` → (path, sha) pairs for its direct children.
fn github_tree(
    agent: &ureq::Agent,
    repo: &str,
    sha: &str,
) -> Option<Vec<(String, String)>> {
    #[derive(Deserialize)]
    struct Entry {
        path: String,
        sha: String,
    }
    #[derive(Deserialize)]
    struct Tree {
        tree: Vec<Entry>,
    }
    let url = format!(
        "https://api.github.com/repos/libretro-thumbnails/{repo}/git/trees/{sha}"
    );
    let mut resp = agent
        .get(&url)
        .header("User-Agent", "AES_Rustified")
        .header("Accept", "application/vnd.github+json")
        .call()
        .ok()?;
    let tree: Tree = serde_json::from_str(&resp.body_mut().read_to_string().ok()?).ok()?;
    Some(tree.tree.into_iter().map(|e| (e.path, e.sha)).collect())
}

/// Match a ROM's title candidates against the fetched index — returns
/// the exact filename to download (no 404 guessing).
fn libretro_match<'a>(
    index: &HashSet<String>,
    section_key: &str,
    candidates: &[String],
) -> Option<String> {
    let folder = libretro_folder(section_key)?;
    for c in candidates {
        for t in title_variants(c) {
            for ext in ["png", "webp", "jpg"] {
                let name = format!("{t}.{ext}");
                if index.contains(&name.to_lowercase()) {
                    return Some(format!(
                        "{LIBRETRO_BASE}{}/Named_Boxarts/{}",
                        escape(folder),
                        escape(&name)
                    ));
                }
            }
        }
    }
    None
}

// ------------------------------------------------------------------
// Bing image search — Lacrima's final fallback (the source that
// actually covers platforms like X360 where libretro's catalog is
// near-empty). One search page per query, then the first validating
// image download.
// ------------------------------------------------------------------
const BING_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36";

/// `<title> <platform> cover`-style queries — a couple of variants,
/// deduped.
fn cover_queries(title: &str, platform: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for q in [
        format!("{title} {platform} cover"),
        format!("{title} {platform} box art"),
        format!("{title} cover"),
    ] {
        let q = q.split_whitespace().collect::<Vec<_>>().join(" ");
        if !q.is_empty() && !out.iter().any(|e| e.eq_ignore_ascii_case(&q)) {
            out.push(q);
        }
    }
    out
}

/// Pull image URLs out of the Bing results HTML — the `m="{...murl...}"`
/// JSON attributes plus bare direct image links, mirroring Lacrima's
/// BingJsonImageUrl/BingHtmlEncoded/DirectImage regexes.
fn bing_image_urls(html: &str) -> Vec<String> {
    let decoded = html
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("\\u003d", "=")
        .replace("\\u0026", "&")
        .replace("\\/", "/");
    let mut out: Vec<String> = Vec::new();
    let push = |u: String, out: &mut Vec<String>| {
        if u.starts_with("http")
            && !u.contains("bing.com")
            && !u.contains("microsoft.com")
            && !out.contains(&u)
        {
            out.push(u);
        }
    };
    // "murl":"<url>" / "turl":"..." / "thumbnailUrl":"..."
    for key in ["\"murl\":\"", "\"turl\":\"", "\"imgurl\":\"", "\"thumbnailUrl\":\""] {
        let mut pos = 0;
        while let Some(i) = decoded[pos..].to_lowercase().find(&key.to_lowercase()) {
            let start = pos + i + key.len();
            if let Some(end) = decoded[start..].find('"') {
                push(decoded[start..start + end].to_string(), &mut out);
            }
            pos = start;
        }
    }
    // Bare <scheme>://host/....(jpg|png|webp) links.
    let mut pos = 0;
    let lower = decoded.to_lowercase();
    while let Some(i) = lower[pos..].find("http") {
        let start = pos + i;
        let rest = &decoded[start..];
        if !rest.starts_with("http://") && !rest.starts_with("https://") {
            pos = start + 4;
            continue;
        }
        let end = rest
            .find(|c: char| c == '"' || c == '\'' || c == '<' || c == '>' || c.is_whitespace())
            .unwrap_or(rest.len());
        let url = &rest[..end];
        let l = url.to_lowercase();
        if [".jpg", ".jpeg", ".png", ".webp"]
            .iter()
            .any(|e| l.contains(e))
        {
            push(url.to_string(), &mut out);
        }
        pos = start + end;
    }
    out.truncate(12);
    out
}

/// DuckDuckGo image API — fetch the search page once to grab the VQD
/// token, then the `i.js` JSON feed (`results[].image`).
fn ddg_image_urls(agent: &ureq::Agent, q: &str) -> Vec<String> {
    let main = format!(
        "https://duckduckgo.com/?q={}&iax=images&ia=images",
        escape(q)
    );
    let Ok(mut resp) = agent
        .get(&main)
        .header("User-Agent", BING_UA)
        .call()
    else {
        return Vec::new();
    };
    let Ok(html) = resp.body_mut().read_to_string() else {
        return Vec::new();
    };
    // vqd="..." or vqd=... (bare) — same regex Lacrima uses.
    let vqd = extract_vqd(&html);
    let Some(vqd) = vqd else { return Vec::new() };
    let api = format!(
        "https://duckduckgo.com/i.js?l=us-en&o=json&q={}&vqd={vqd}&f=,,,",
        escape(q)
    );
    let Ok(mut r) = agent
        .get(&api)
        .header("User-Agent", BING_UA)
        .header("Referer", &main)
        .call()
    else {
        return Vec::new();
    };
    #[derive(Deserialize)]
    struct Feed {
        results: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        image: String,
    }
    let Ok(feed) = serde_json::from_str::<Feed>(&r.body_mut().read_to_string().unwrap_or_default())
    else {
        return Vec::new();
    };
    feed.results
        .into_iter()
        .map(|e| e.image)
        .filter(|u| u.starts_with("http"))
        .take(12)
        .collect()
}

fn extract_vqd(html: &str) -> Option<String> {
    for pat in ["vqd=\"", "vqd='", "vqd="] {
        let mut pos = 0;
        while let Some(i) = html[pos..].find(pat) {
            let start = pos + i + pat.len();
            let rest = &html[start..];
            let end = rest
                .find(|c: char| c == '"' || c == '\'' || c == '&' || c.is_whitespace())
                .unwrap_or(rest.len());
            let tok = rest[..end].trim();
            if !tok.is_empty() {
                return Some(tok.to_string());
            }
            pos = start + end;
        }
    }
    None
}

/// `FetchImageResults` → first validating download. Returns the saved
/// sidecar path. Engine order mirrors Lacrima: DuckDuckGo, then Bing.
fn web_image_fetch(
    agent: &ureq::Agent,
    title: &str,
    platform: &str,
    rom: &Path,
) -> Option<PathBuf> {
    for q in cover_queries(title, platform) {
        // DuckDuckGo first (clean JSON feed)…
        let mut candidates = ddg_image_urls(agent, &q);
        // …then Bing's HTML page when DDG yields nothing.
        if candidates.is_empty() {
            if let Some(v) = bing_candidates(agent, &q) {
                candidates = v;
            }
        }
        for img in candidates {
            if let Some(p) = download_validated(agent, &img, rom) {
                return Some(p);
            }
        }
    }
    None
}

fn bing_candidates(agent: &ureq::Agent, q: &str) -> Option<Vec<String>> {
    let url = format!(
        "https://www.bing.com/images/search?q={}&form=HDRSC3&first=1",
        escape(q)
    );
    let mut resp = agent
        .get(&url)
        .header("User-Agent", BING_UA)
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Referer", "https://www.bing.com/")
        .call()
        .ok()?;
    let html = resp.body_mut().read_to_string().ok()?;
    let urls = bing_image_urls(&html);
    (!urls.is_empty()).then_some(urls)
}

/// GET + decode/size gate (Lacrima's `TryValidateAutoCoverImageBytes`)
/// + sidecar write.
fn download_validated(agent: &ureq::Agent, url: &str, rom: &Path) -> Option<PathBuf> {
    let mut r = agent
        .get(url)
        .header("User-Agent", BING_UA)
        .call()
        .ok()?;
    let bytes = r.body_mut().read_to_vec().ok()?;
    if bytes.len() < 2 * 1024 {
        return None;
    }
    match image::load_from_memory(&bytes) {
        Ok(d) if d.width() >= 120 && d.height() >= 120 => {}
        _ => return None,
    }
    save_cover(rom, &bytes)
}

// ------------------------------------------------------------------
// The resolver pool — N workers walk the item list over a shared
// cursor so a section's covers stream in parallel.
// ------------------------------------------------------------------
/// Spawn resolvers for `items`: per ROM the title comes from the
/// sidecar → database/binary → stem; the cover from the sidecar →
/// libretro index → Bing image search. Results stream back as they
/// land so cards populate progressively.
pub fn spawn_resolver(
    section_key: String,
    platform_name: String,
    items: Vec<PathBuf>,
) -> mpsc::Receiver<RomInfo> {
    let (tx, rx) = mpsc::channel();
    if items.is_empty() {
        return rx;
    }
    let items = std::sync::Arc::new(items);
    let cursor = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let workers = 6usize.min(items.len());
    for _ in 0..workers {
        let (tx, items, cursor, key, platform) = (
            tx.clone(),
            items.clone(),
            cursor.clone(),
            section_key.clone(),
            platform_name.clone(),
        );
        std::thread::Builder::new()
            .name("aes-emu-meta".into())
            .spawn(move || {
                let agent: ureq::Agent = ureq::Agent::config_builder()
                    .timeout_global(Some(std::time::Duration::from_secs(6)))
                    .http_status_as_error(true)
                    .build()
                    .into();
                // One fetch per section — all threads share the cached set.
                let index = libretro_index(&agent, &key);
                loop {
                    let i = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(path) = items.get(i) else { break };
                    let path = path.clone();
                    let info = resolve_one(&agent, &key, &platform, index.as_deref(), &path);
                    if info.title.is_some() || info.cover.is_some() {
                        if tx.send(info).is_err() {
                            break;
                        }
                    }
                }
            })
            .expect("emu-meta resolver thread");
    }
    rx
}

fn resolve_one(
    agent: &ureq::Agent,
    section_key: &str,
    platform: &str,
    index: Option<&HashSet<String>>,
    path: &Path,
) -> RomInfo {
    let cached = cover_path(path);
    // Title: sidecar → database/binary → stem.
    let title = cached_title(path).or_else(|| {
        let t = if section_key == "X360" {
            x360_title(path)
        } else {
            None
        };
        if let Some(t) = &t {
            save_cached_title(path, t);
        }
        t
    });
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cover = if cached.exists() {
        Some(cached)
    } else {
        // Title candidates in Lacrima's order: resolved title first,
        // then the stem and its region-stripped form.
        let mut cands: Vec<String> = Vec::new();
        if let Some(t) = &title {
            cands.push(t.clone());
        }
        let clean = strip_region(&stem).to_string();
        if !clean.is_empty() && !cands.iter().any(|c| c.eq_ignore_ascii_case(&clean)) {
            cands.push(clean.clone());
        }
        // 1. LibRetro via the platform index — a local match, not a
        //    URL ladder.
        let mut hit = index.and_then(|idx| libretro_match(idx, section_key, &cands));
        if hit.is_none() && index.is_none() {
            // No index at all — fall back to a *trimmed* ladder (the
            // top region suffixes only) rather than the full 96-URL
            // crawl.
            hit = ladder_fallback(agent, section_key, &cands);
        }
        let dl = hit.and_then(|url| download_cover(agent, url.as_str(), path));
        dl.or_else(|| {
            // 2. Search-engine image lookup (Lacrima's web-search
            //    fallback) — the real X360 coverage source.
            let t = title.as_deref().unwrap_or(&clean);
            web_image_fetch(agent, t, platform, path)
        })
    };
    RomInfo { path: path.to_path_buf(), title, cover }
}

/// Network-less guess ladder used only when the libretro index can't
/// be fetched — base + top region suffixes, 3 extensions.
fn ladder_fallback(
    agent: &ureq::Agent,
    section_key: &str,
    candidates: &[String],
) -> Option<String> {
    let folder = libretro_folder(section_key)?;
    for c in candidates {
        for t in title_variants_lite(c) {
            for ext in ["webp", "jpg", "png"] {
                let url = format!(
                    "{LIBRETRO_BASE}{}/Named_Boxarts/{}.{ext}",
                    escape(folder),
                    escape(&t)
                );
                let Ok(mut resp) = agent.get(&url).call() else {
                    continue;
                };
                let Ok(bytes) = resp.body_mut().read_to_vec() else {
                    continue;
                };
                if bytes.len() >= 1024 && image::load_from_memory(&bytes).is_ok() {
                    return Some(url);
                }
            }
        }
    }
    None
}

/// Trimmed `ExpandTitleVariants` — base + display + stripped + the
/// four most likely regions (keeps the no-index miss cheap).
fn title_variants_lite(title: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |v: String| {
        if !v.is_empty() && !out.iter().any(|e| e.eq_ignore_ascii_case(&v)) {
            out.push(v);
        }
    };
    for base in [title.trim().to_string(), display_title(title.trim())] {
        if base.is_empty() {
            continue;
        }
        push(base.clone());
        let stripped = strip_region(&base).to_string();
        if stripped != base {
            push(stripped.clone());
        } else {
            for suf in &["(USA)", "(Europe)", "(World)", "(Japan)"] {
                push(format!("{stripped} {suf}"));
            }
        }
    }
    out
}

fn download_cover(agent: &ureq::Agent, url: &str, rom: &Path) -> Option<PathBuf> {
    let mut resp = agent.get(url).call().ok()?;
    let bytes = resp.body_mut().read_to_vec().ok()?;
    if bytes.len() < 64 || image::load_from_memory(&bytes).is_err() {
        return None;
    }
    save_cover(rom, &bytes)
}

// ------------------------------------------------------------------
// The ROM metadata editor (EmulationMetadataOverlay)
// ------------------------------------------------------------------
/// The `.meta` document sidecar — Lacrima's BinaryMetadataHelper
/// CustomMetadata, minus the fields we don't model. Extra images
/// live next to it as `<key>.k<n>` payload files (Lacrima embeds
/// them base64'd in the JSON; separate files keep the doc small).
#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub struct RomMetaDoc {
    pub title: String,
    pub developer: String,
    pub platform: String,
    pub year: u32,
    pub genre: String,
    pub comment: String,
    pub video_url: String,
    pub x360_title_id: String,
    pub x360_media_id: String,
    pub user_edited: bool,
    /// ArcadeLockedPillarboxCrop{Left,Right,FrameWidth} — a
    /// user-locked pillarbox crop measured at `crop_fw` pixels wide;
    /// re-scaled to the live frame width when applied.
    pub crop_left: u32,
    pub crop_right: u32,
    pub crop_fw: u32,
    /// Non-cover images — `file` is the `<key>.k<n>` payload name.
    pub images: Vec<MetaImageRef>,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub struct MetaImageRef {
    pub kind: String,
    pub file: String,
}

pub fn meta_doc_path(rom: &Path) -> PathBuf {
    cache_dir().join(format!("{}.meta", key_for(rom)))
}

/// Payload path for a stored extra image (`<key>.k<n>`).
pub fn meta_image_path(rom: &Path, n: usize) -> PathBuf {
    cache_dir().join(format!("{}.k{}", key_for(rom), n))
}

pub fn load_meta(rom: &Path) -> Option<RomMetaDoc> {
    let s = std::fs::read_to_string(meta_doc_path(rom)).ok()?;
    serde_json::from_str(&s).ok()
}

pub fn save_meta(rom: &Path, doc: &RomMetaDoc) -> std::io::Result<()> {
    let p = meta_doc_path(rom);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string(doc)?)?;
    std::fs::rename(&tmp, &p)
}

/// Lacrima's ArcadePillarboxCropMetadataHelper — a user-locked
/// pillarbox crop for this ROM: `(left, right, frame_width)`, the
/// insets measured when the capture was `frame_width` px wide.
pub fn locked_crop(rom: &Path) -> Option<(u32, u32, u32)> {
    let m = load_meta(rom)?;
    (m.crop_fw > 0 && (m.crop_left > 0 || m.crop_right > 0))
        .then_some((m.crop_left, m.crop_right, m.crop_fw))
}

pub fn save_locked_crop(rom: &Path, left: u32, right: u32, frame_width: u32) {
    if frame_width == 0 || (left == 0 && right == 0) {
        return;
    }
    let mut doc = load_meta(rom).unwrap_or_default();
    doc.crop_left = left;
    doc.crop_right = right;
    doc.crop_fw = frame_width;
    let _ = save_meta(rom, &doc);
}

pub fn clear_locked_crop(rom: &Path) {
    let Some(mut doc) = load_meta(rom) else {
        return;
    };
    if doc.crop_left == 0 && doc.crop_right == 0 && doc.crop_fw == 0 {
        return;
    }
    doc.crop_left = 0;
    doc.crop_right = 0;
    doc.crop_fw = 0;
    let _ = save_meta(rom, &doc);
}

/// Drop every `<key>.k*` payload not in `keep` — image removals.
pub fn prune_meta_images(rom: &Path, keep: &HashSet<String>) {
    let key = key_for(rom);
    if let Ok(rd) = std::fs::read_dir(cache_dir()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(&format!("{key}.k")) && !keep.contains(&name) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Write the active cover sidecar (`WriteCoverFromBytes`) or delete
/// it when the editor cleared every cover image.
pub fn write_cover(rom: &Path, bytes: &[u8]) -> bool {
    save_cover(rom, bytes).is_some()
}

pub fn delete_cover(rom: &Path) {
    let _ = std::fs::remove_file(cover_path(rom));
}

/// The title sidecar is also the editor's persisted title — the
/// resolver reads it first, so a user edit always wins.
pub fn write_title(rom: &Path, title: &str) {
    let t = title.trim();
    if t.is_empty() {
        let _ = std::fs::remove_file(meta_path(rom));
    } else {
        save_cached_title(rom, t);
    }
}

/// X360 title + media id, both 8-hex — the editor's read-only
/// "Title ID" / "Media ID" rows.
pub fn x360_ids(path: &Path) -> Option<(String, String)> {
    let (title, media) = if path.is_dir() {
        walkdir_find_xex(path).and_then(|x| read_xex_file(&x))
    } else {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .as_deref()
        {
            Some("xex") => read_xex_file(path),
            Some("iso") | Some("xiso") => {
                let mut f = std::fs::File::open(path).ok()?;
                let len = f.metadata().ok()?.len();
                extract_default_xex(&mut f)
                    .and_then(|d| read_execution_info(&d, 0))
                    .or_else(|| scan_iso_for_xex(&mut f, len))
            }
            _ => None,
        }
    }?;
    Some((format!("{title:08X}"), format!("{media:08X}")))
}

// ------------------------------------------------------------------
// The editor's web image search (SearchImagesAsync) — returns up to
// 24 direct image URLs, DDG first then Bing, matching the app's
// engine order.
// ------------------------------------------------------------------
pub fn gather_image_candidates(query: &str) -> Vec<String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(8)))
        .http_status_as_error(true)
        .build()
        .into();
    let mut out = ddg_image_urls(&agent, query);
    if out.is_empty() {
        if let Some(v) = bing_candidates(&agent, query) {
            out = v;
        }
    }
    out.truncate(24);
    out
}

/// Download + decode-validate an image URL for the editor (`Add
/// URL`, picking a search result). Returns raw bytes.
pub fn fetch_image(url: &str) -> Option<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .http_status_as_error(true)
        .build()
        .into();
    let mut r = agent
        .get(url)
        .header("User-Agent", BING_UA)
        .call()
        .ok()?;
    let bytes = r.body_mut().read_to_vec().ok()?;
    if bytes.len() < 512 || image::load_from_memory(&bytes).is_err() {
        return None;
    }
    Some(bytes)
}

// ------------------------------------------------------------------
// Gameplay video search (AddGameplayAsync → YouTube results scrape)
// ------------------------------------------------------------------
pub struct YtHit {
    pub id: String,
    pub title: String,
    /// `i.ytimg.com` thumbnail — the result card's preview.
    pub thumb: String,
    /// The watch URL `VideoUrl` stores.
    pub url: String,
}

pub fn youtube_search(query: &str) -> Vec<YtHit> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .http_status_as_error(true)
        .build()
        .into();
    let url = format!(
        "https://www.youtube.com/results?search_query={}",
        escape(query)
    );
    let Ok(mut resp) = agent
        .get(&url)
        .header("User-Agent", BING_UA)
        .header("Accept-Language", "en-US,en;q=0.9")
        .call()
    else {
        return Vec::new();
    };
    let Ok(html) = resp.body_mut().read_to_string() else {
        return Vec::new();
    };
    // `"videoId":"<id>"` … `"title":{"runs":[{"text":"<t>"` —
    // Lacrima's YouTubeVideoIdRegex/YouTubeSearchTitleRegex pair.
    let mut out: Vec<YtHit> = Vec::new();
    let mut pos = 0;
    while let Some(i) = html[pos..].find("\"videoId\":\"") {
        let vs = pos + i + "\"videoId\":\"".len();
        let Some(ve) = html[vs..].find('"') else { break };
        let id = html[vs..vs + ve].to_string();
        // The title text follows shortly after the id.
        let window_end = (vs + ve + 4000).min(html.len());
        let title = html[vs + ve..window_end]
            .find("\"text\":\"")
            .map(|j| {
                let ts = vs + ve + j + "\"text\":\"".len();
                html[ts..]
                    .find('"')
                    .map(|e| unescape_json(&html[ts..ts + e]))
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        if !id.is_empty() && !out.iter().any(|h| h.id == id) {
            out.push(YtHit {
                thumb: format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"),
                url: format!("https://www.youtube.com/watch?v={id}"),
                id,
                title,
            });
        }
        pos = vs + ve;
        if out.len() >= 24 {
            break;
        }
    }
    out
}

/// Minimal JSON-string unescape for scraped titles (`\"` `\` `\/`
/// `\uXXXX` `\n` `\t`).
fn unescape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('u') => {
                let hex: String = it.by_ref().take(4).collect();
                if let Ok(v) = u32::from_str_radix(&hex, 16) {
                    out.push(char::from_u32(v).unwrap_or('\u{FFFD}'));
                }
            }
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}
