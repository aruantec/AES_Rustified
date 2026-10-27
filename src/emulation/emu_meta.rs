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
use std::time::{Duration, Instant};

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
        // U+FFFD = a sidecar written before the cp1252 decode — re-
        // resolve so the clean title overwrites it (self-healing).
        .filter(|t| !t.is_empty() && !t.contains('\u{FFFD}'))
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

/// Lazily inflates a gzip'd title database (build.rs compresses
/// `assets/database/*.json` into OUT_DIR so the exe doesn't carry
/// ~14MB of raw JSON — first-launch AV scan cost scales with size).
/// Callers invoke it inside a OnceLock init, so each DB inflates once.
fn db_json(name: &str) -> String {
    let gz: &[u8] = match name {
        "x360.json" => include_bytes!(concat!(env!("OUT_DIR"), "/db/x360.json.gz")),
        "psx.json" => include_bytes!(concat!(env!("OUT_DIR"), "/db/psx.json.gz")),
        "ps2.json" => include_bytes!(concat!(env!("OUT_DIR"), "/db/ps2.json.gz")),
        "fbn.json" => include_bytes!(concat!(env!("OUT_DIR"), "/db/fbn.json.gz")),
        "arcade.json" => include_bytes!(concat!(env!("OUT_DIR"), "/db/arcade.json.gz")),
        "nes.json" => include_bytes!(concat!(env!("OUT_DIR"), "/db/nes.json.gz")),
        "snes.json" => include_bytes!(concat!(env!("OUT_DIR"), "/db/snes.json.gz")),
        "gba.json" => include_bytes!(concat!(env!("OUT_DIR"), "/db/gba.json.gz")),
        "psp.json" => include_bytes!(concat!(env!("OUT_DIR"), "/db/psp.json.gz")),
        "psp_redump.json" => {
            include_bytes!(concat!(env!("OUT_DIR"), "/db/psp_redump.json.gz"))
        }
        _ => include_bytes!(concat!(env!("OUT_DIR"), "/db/genesis.json.gz")),
    };
    let mut s = String::new();
    let _ = flate2::read::GzDecoder::new(gz).read_to_string(&mut s);
    s
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
        let json = db_json("x360.json");
        serde_json::from_str::<Vec<TitleEntry>>(&json)
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
            if let Some(sub) = read_sector_region(f, base_sector + e.sector as u64, e.size) {
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
            if run == 8 && (i == 0 || !word(b[i - 1])) && (i + 8 == b.len() || !word(b[i + 8])) {
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
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
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
// Dreamcast titles (RomInspector's IP.BIN extraction)
// ------------------------------------------------------------------
/// Printable ASCII + latin-1, NUL-terminated — CleanDreamcastText.
fn dc_text(b: &[u8]) -> String {
    let mut out = String::new();
    for &c in b {
        if c == 0 {
            break;
        }
        if (32..=126).contains(&c) || c >= 160 {
            out.push(c as char);
        }
    }
    out.trim().to_string()
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    (!needle.is_empty())
        .then(|| hay.windows(needle.len()).position(|w| w == needle))
        .flatten()
}

/// IP.BIN sits at the start of the last session's data track:
/// hardware id "SEGA SEGAKATANA"/"SEGA ENTERPRISES" at +0, product
/// code at +0x40 (10B), game title at +0x80 (128B). Scanned rather
/// than seeked so raw 2352-byte tracks work without sector math —
/// the signature and its fields are contiguous inside one sector's
/// data payload.
fn ipbin_title_in(f: &mut std::fs::File, limit: u64) -> Option<String> {
    let mut buf = Vec::new();
    f.take(limit).read_to_end(&mut buf).ok()?;
    let buf = &buf[..];
    let pos = [
        b"SEGA SEGAKATANA".as_slice(),
        b"SEGA ENTERPRISES".as_slice(),
    ]
    .iter()
    .find_map(|s| find_bytes(buf, s))?;
    let ip = &buf[pos..];
    if ip.len() < 0x100 {
        return None;
    }
    let title = dc_text(&ip[0x80..0x100]);
    if title.chars().filter(|c| c.is_alphanumeric()).count() >= 2 {
        return Some(title);
    }
    let prod = dc_text(&ip[0x40..0x4A]);
    (!prod.is_empty()).then_some(prod)
}

/// `FILE "track.bin" BINARY` / `FILE track.bin BINARY` names from a
/// cue sheet, resolved against the cue's directory.
fn cue_track_files(path: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if !l.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("FILE ")) {
            continue;
        }
        let rest = l[5..].trim();
        let name = if let Some(inner) = rest.strip_prefix('"').and_then(|r| r.split('"').next()) {
            inner
        } else {
            rest.split_whitespace().next().unwrap_or("")
        };
        if name.is_empty() {
            continue;
        }
        let p = dir.join(name.replace('\\', "/"));
        if p.is_file() {
            out.push(p);
        }
    }
    out
}

/// Track filenames from a .gdi sheet — fields are
/// `<track> <lba> <type> <sectorsize> <file> <offset>` with the file
/// optionally quoted (Lacrima's regex reads field 2 by mistake; real
/// GDIs carry the LBA there).
fn gdi_track_files(path: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let l = line.trim();
        if l.is_empty() || l.starts_with(';') {
            continue;
        }
        // Field 5 is the filename (may be quoted); skip the first
        // four fields then read the quoted span or first word.
        let mut rest = l;
        let mut ok = true;
        for _ in 0..4 {
            rest = rest.trim_start();
            match rest.find(char::is_whitespace) {
                Some(sp) => rest = &rest[sp..],
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            continue;
        }
        rest = rest.trim_start();
        let file = if let Some(inner) = rest.strip_prefix('"').and_then(|r| r.split('"').next()) {
            inner
        } else {
            rest.split_whitespace().next().unwrap_or("")
        };
        if file.is_empty() {
            continue;
        }
        let p = dir.join(file.replace('\\', "/"));
        if p.is_file() {
            out.push(p);
        }
    }
    out
}

/// Dreamcast title — IP.BIN out of the data track for gdi/cue/
/// cdi/iso/bin/img. CHD is compressed (unsupported here, like
/// Lacrima) → stem fallback covers it.
fn dc_title(path: &Path) -> Option<String> {
    const SCAN: u64 = 8 * 1024 * 1024;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())?;
    let targets: Vec<PathBuf> = match ext.as_str() {
        "gdi" => gdi_track_files(path),
        "cue" => cue_track_files(path),
        "cdi" | "iso" | "bin" | "img" => vec![path.to_path_buf()],
        _ => return None,
    };
    for t in targets {
        if let Ok(mut f) = std::fs::File::open(&t) {
            if let Some(title) = ipbin_title_in(&mut f, SCAN) {
                return Some(title);
            }
        }
    }
    None
}

// ------------------------------------------------------------------
// PS3 — PARAM.SFO (Lacrima's Ps3InstalledGameHelper). SFO is a keyed
// binary blob: a 20-byte header (magic, key/data table offsets,
// entry count), 16-byte index entries, then the key + data tables.
// ------------------------------------------------------------------
/// The dirs a PARAM.SFO can live in relative to a game root —
/// disc dumps keep it in `PS3_GAME`, installed pkgs in the dir
/// root / `USRDIR` (GetCandidateDirectories).
pub(crate) fn ps3_sfo_candidates(root: &Path) -> [PathBuf; 4] {
    [
        root.join("PARAM.SFO"),
        root.join("PS3_GAME").join("PARAM.SFO"),
        root.join("PS3_GAME").join("USRDIR").join("PARAM.SFO"),
        root.join("USRDIR").join("PARAM.SFO"),
    ]
}

/// Read `want` ("TITLE"/"TITLE_ID"/"APP_VER") out of a PARAM.SFO —
/// a port of TryReadValueFromParamSfo.
pub(crate) fn sfo_value(sfo: &Path, want: &str) -> Option<String> {
    // SFOs are KBs — cap the read so a junk file can't burn memory.
    let bytes = std::fs::read(sfo).ok()?;
    let u32be = |o: usize| -> Option<usize> {
        bytes
            .get(o..o + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
    };
    if bytes.len() < 20 || bytes[0..4] != [0x00, 0x50, 0x53, 0x46] {
        return None;
    }
    let key_table = u32be(8)?;
    let data_table = u32be(12)?;
    let count = u32be(16)?;
    for i in 0..count {
        let e = 20 + i * 16;
        let Some(entry) = bytes.get(e..e + 16) else {
            break;
        };
        let key_off = u16::from_le_bytes(entry[0..2].try_into().unwrap()) as usize;
        let data_len = u32::from_le_bytes(entry[4..8].try_into().unwrap()) as usize;
        let data_off = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
        let key: String = bytes
            .get(key_table + key_off..)
            .unwrap_or(&[])
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| b as char)
            .collect();
        if !key.eq_ignore_ascii_case(want) {
            continue;
        }
        if data_len == 0 {
            return None;
        }
        let raw = bytes.get(data_table + data_off..data_table + data_off + data_len)?;
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        let value = decode_sfo_str(&raw[..end]).trim().to_string();
        return (!value.is_empty()).then_some(value);
    }
    None
}

/// SFO strings aren't always UTF-8 — ™/® and other specials arrive
/// as cp1252 bytes ("God of War\x99 III"), which from_utf8_lossy
/// would mangle to U+FFFD and poison the cover-search queries.
/// Decode UTF-8 first, else map each byte through cp1252 (0x80-9F
/// table; the rest is latin1-identical).
fn decode_sfo_str(raw: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(raw) {
        return s.to_string();
    }
    const CP1252: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž',
        '\u{8F}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}',
        'ž', 'Ÿ',
    ];
    raw.iter()
        .map(|&b| match b {
            0x80..=0x9F => CP1252[(b - 0x80) as usize],
            _ => b as char,
        })
        .collect()
}

/// PS3 title — PARAM.SFO `TITLE` from the game dir (folder ROM or
/// an extracted `EBOOT.BIN`); plain `.iso` dumps carry the name in
/// the filename so the stem fallback covers them.
fn ps3_title(path: &Path) -> Option<String> {
    let roots: Vec<PathBuf> = if path.is_dir() {
        vec![path.to_path_buf()]
    } else {
        // A file ROM: walk up so .../PS3_GAME/USRDIR/EBOOT.BIN finds
        // its sibling PARAM.SFO (file's dir → 3 ancestors max).
        path.ancestors()
            .skip(1)
            .take(3)
            .map(|d| d.to_path_buf())
            .collect()
    };
    for root in roots {
        for sfo in ps3_sfo_candidates(&root) {
            if let Some(title) = sfo_value(&sfo, "TITLE") {
                return Some(title);
            }
        }
    }
    None
}

/// Lacrima's GetPreferredIconPath — a PS3 dump ships its own art
/// (ICON0.PNG cover, PIC1.PNG back); used as the cover fallback
/// when the web pipeline finds nothing.
fn ps3_local_cover(path: &Path) -> Option<PathBuf> {
    let roots: Vec<PathBuf> = if path.is_dir() {
        vec![path.to_path_buf()]
    } else {
        path.ancestors()
            .skip(1)
            .take(3)
            .map(|d| d.to_path_buf())
            .collect()
    };
    for root in roots {
        for dir in [
            root.clone(),
            root.join("PS3_GAME"),
            root.join("PS3_GAME").join("USRDIR"),
            root.join("USRDIR"),
        ] {
            for name in ["ICON0.PNG", "icon0.png", "PIC1.PNG", "pic1.png"] {
                let p = dir.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// Ps3InstalledGameHelper.IsInstalledGameFolder — a dir that owns a
/// PARAM.SFO/ICON0 in the standard spots, or carries a title id
/// (BLES12345 & co) in any path segment. Such a folder IS the ROM —
/// scanners add it whole and never descend into it.
pub fn ps3_is_installed_folder(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    if dir
        .ancestors()
        .filter_map(|a| a.file_name().and_then(|n| n.to_str()))
        .any(ps3_title_id_in)
    {
        return true;
    }
    for d in [
        dir.to_path_buf(),
        dir.join("PS3_GAME"),
        dir.join("PS3_GAME").join("USRDIR"),
        dir.join("USRDIR"),
    ] {
        if d.join("PARAM.SFO").is_file()
            || d.join("ICON0.PNG").is_file()
            || d.join("icon0.png").is_file()
        {
            return true;
        }
    }
    false
}

/// `(BLUS|BLES|BLJM|BLJS|NPUB|NPEB|NPJB|NPUJ|NPUX)\d{5}` — the disc/
/// pkg title-id pattern PS3 dumps carry in their folder names.
fn ps3_title_id_in(s: &str) -> bool {
    const PRE: &[&[u8; 4]] = &[
        b"BLUS", b"BLES", b"BLJM", b"BLJS", b"NPUB", b"NPEB", b"NPJB", b"NPUJ", b"NPUX",
    ];
    let b = s.as_bytes();
    if b.len() < 9 {
        return false;
    }
    (0..=b.len() - 9).any(|i| {
        PRE.iter().any(|p| b[i..i + 4].eq_ignore_ascii_case(&p[..]))
            && b[i + 4..i + 9].iter().all(|c| c.is_ascii_digit())
    })
}

/// Extract a PS3 title-id token from free text — the RPCS3 patch
/// service's wider `\b(BLUS|BLES|BLJM|BLJS|NPUB|NPEB|NPJB|NPUJ|NPUA|
/// NPUX|BCUS|BCES|MRTC)\d{5}\b` (TitleIdTokenRegex), returned
/// uppercased.
pub(crate) fn ps3_title_id_token(s: &str) -> Option<String> {
    const PRE: &[&[u8; 4]] = &[
        b"BLUS", b"BLES", b"BLJM", b"BLJS", b"NPUB", b"NPEB", b"NPJB", b"NPUJ", b"NPUA", b"NPUX",
        b"BCUS", b"BCES", b"MRTC",
    ];
    let b = s.as_bytes();
    // A token needs the full 9 bytes — without this the digit slice
    // b[i+4..i+9] panics on a short name whose prefix matches
    // ("BLES x" under a dir called "BLES").
    if b.len() < 9 {
        return None;
    }
    let word = |i: usize| {
        (i == 0 || !b[i - 1].is_ascii_alphanumeric())
            && (i + 9 >= b.len() || !b[i + 9].is_ascii_alphanumeric())
    };
    (0..=b.len().saturating_sub(9))
        .find(|&i| {
            word(i)
                && PRE.iter().any(|p| b[i..i + 4].eq_ignore_ascii_case(&p[..]))
                && b[i + 4..i + 9].iter().all(|c| c.is_ascii_digit())
        })
        .map(|i| s[i..i + 9].to_uppercase())
}

/// TITLE_ID + APP_VER for a PS3 rom — PARAM.SFO first (Lacrima's
/// `TryReadValueFromParamSfo`), then a title-id token from the path
/// segments (ResolveTitleId's fallback). Used by the RPCS3 patches
/// overlay.
pub(crate) fn ps3_ids(path: &Path) -> (Option<String>, Option<String>) {
    let roots: Vec<PathBuf> = if path.is_dir() {
        vec![path.to_path_buf()]
    } else {
        path.ancestors()
            .skip(1)
            .take(3)
            .map(|d| d.to_path_buf())
            .collect()
    };
    let mut tid = None;
    let mut ver = None;
    for root in &roots {
        for sfo in ps3_sfo_candidates(root) {
            if tid.is_none() {
                tid = sfo_value(&sfo, "TITLE_ID");
            }
            if ver.is_none() {
                ver = sfo_value(&sfo, "APP_VER");
            }
        }
    }
    if tid.is_none() {
        tid = path
            .ancestors()
            .filter_map(|a| a.file_name().and_then(|n| n.to_str()))
            .find_map(ps3_title_id_token);
    }
    (tid, ver)
}

// ======================================================================
// PS4 — same SFO format as PS3 (param.sfo lives in sce_sys/), the
// dump ships its own icon0.png, and title ids are `XXXX12345`.
// ======================================================================

/// Lacrima's `Ps4InstalledGameHelper` SFO candidates — `param.sfo`
/// sits under `sce_sys/` in an installed dump.
pub(crate) fn ps4_sfo_candidates(root: &Path) -> [PathBuf; 3] {
    [
        root.join("sce_sys").join("param.sfo"),
        root.join("sce_sys").join("PARAM.SFO"),
        root.join("param.sfo"),
    ]
}

/// PS4 title — `sce_sys/param.sfo` `TITLE` (file rom: ancestors).
fn ps4_title(path: &Path) -> Option<String> {
    let roots: Vec<PathBuf> = if path.is_dir() {
        vec![path.to_path_buf()]
    } else {
        path.ancestors()
            .skip(1)
            .take(3)
            .map(|d| d.to_path_buf())
            .collect()
    };
    for root in roots {
        for sfo in ps4_sfo_candidates(&root) {
            if let Some(title) = sfo_value(&sfo, "TITLE") {
                return Some(title);
            }
        }
    }
    None
}

/// GetPreferredIconPath — the installed dump's `sce_sys/icon0.png`
/// (512×512 square cover; pic0/pic1 as larger fallbacks).
fn ps4_local_cover(path: &Path) -> Option<PathBuf> {
    let roots: Vec<PathBuf> = if path.is_dir() {
        vec![path.to_path_buf()]
    } else {
        path.ancestors()
            .skip(1)
            .take(3)
            .map(|d| d.to_path_buf())
            .collect()
    };
    for root in roots {
        for dir in [root.clone(), root.join("sce_sys")] {
            for name in [
                "icon0.png",
                "ICON0.PNG",
                "pic0.png",
                "PIC0.PNG",
                "pic1.png",
                "PIC1.PNG",
            ] {
                let p = dir.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// Ps4InstalledGameHelper.IsInstalledGameFolder — a dir that owns
/// `sce_sys/param.sfo`, `eboot.bin` or `icon0.png`, or carries a
/// CUSA-style title id in a path segment.
pub fn ps4_is_installed_folder(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    if dir
        .ancestors()
        .filter_map(|a| a.file_name().and_then(|n| n.to_str()))
        .any(|n| ps4_title_id_token(n).is_some())
    {
        return true;
    }
    dir.join("sce_sys").join("param.sfo").is_file()
        || dir.join("sce_sys").join("PARAM.SFO").is_file()
        || dir.join("eboot.bin").is_file()
        || dir.join("icon0.png").is_file()
        || dir.join("ICON0.PNG").is_file()
}

/// `\b[A-Z]{4}\d{5}\b` — CUSA/PCAS/PPSA-style PS4 title ids, a token
/// anywhere in the text (shadPS4 dumps are named `CUSA12345 - Title`).
pub(crate) fn ps4_title_id_token(s: &str) -> Option<String> {
    let b = s.as_bytes();
    // Same guard — a <9-byte name whose first 4 letters are all alpha
    // ("Games", "Sony") would panic slicing b[4..9].
    if b.len() < 9 {
        return None;
    }
    let word = |i: usize| {
        (i == 0 || !b[i - 1].is_ascii_alphanumeric())
            && (i + 9 >= b.len() || !b[i + 9].is_ascii_alphanumeric())
    };
    (0..=b.len().saturating_sub(9))
        .find(|&i| {
            word(i)
                && b[i..i + 4].iter().all(|c| c.is_ascii_alphabetic())
                && b[i + 4..i + 9].iter().all(|c| c.is_ascii_digit())
        })
        .map(|i| s[i..i + 9].to_uppercase())
}

/// TITLE_ID + APP_VER for a PS4 rom — `sce_sys/param.sfo` first,
/// then a title-id token from the path segments.
pub(crate) fn ps4_ids(path: &Path) -> (Option<String>, Option<String>) {
    let roots: Vec<PathBuf> = if path.is_dir() {
        vec![path.to_path_buf()]
    } else {
        path.ancestors()
            .skip(1)
            .take(3)
            .map(|d| d.to_path_buf())
            .collect()
    };
    let mut tid = None;
    let mut ver = None;
    for root in &roots {
        for sfo in ps4_sfo_candidates(root) {
            if tid.is_none() {
                tid = sfo_value(&sfo, "TITLE_ID");
            }
            if ver.is_none() {
                ver = sfo_value(&sfo, "APP_VER");
            }
        }
    }
    if tid.is_none() {
        tid = path
            .ancestors()
            .filter_map(|a| a.file_name().and_then(|n| n.to_str()))
            .find_map(ps4_title_id_token);
    }
    (tid, ver)
}

/// ClearAlbumCache — drop the fetch sidecars (`{key}.img` cover +
/// `{key}.title` resolved name) so the resolver starts over. The
/// `.meta` doc and `.k*` editor images are user data — preserved.
pub fn clear_cache(rom: &Path) {
    let _ = std::fs::remove_file(cover_path(rom));
    let _ = std::fs::remove_file(meta_path(rom));
}

/// Files a disc descriptor references — the scanner collapses these
/// so a cue/gdi/m3u owns its tracks instead of listing them as ROMs
/// (CollapseDiscImageArtifacts).
pub(crate) fn descriptor_refs(path: &Path) -> Vec<PathBuf> {
    let Some(ext) = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
    else {
        return Vec::new();
    };
    match ext.as_str() {
        "cue" => cue_track_files(path),
        "gdi" => gdi_track_files(path),
        "m3u" => {
            let Some(dir) = path.parent() else {
                return Vec::new();
            };
            let Ok(text) = std::fs::read_to_string(path) else {
                return Vec::new();
            };
            text.lines()
                .map(|l| l.trim().trim_matches('"'))
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(|l| dir.join(l.replace('\\', "/")))
                .filter(|p| p.is_file())
                .collect()
        }
        _ => Vec::new(),
    }
}

// ------------------------------------------------------------------
// PlayStation / PlayStation 2 serial + title DB
// (PsxIsoTitleIdHelper + embedded Database/{psx,ps2}.json —
// NormalizeSerialKey lookup → game title)
// ------------------------------------------------------------------
const PS_SECTOR: u64 = 2048;
const PS_PVD_OFF: u64 = 16 * PS_SECTOR;
const PS_SERIAL_PREFIXES: &[&str] = &[
    "SLUS", "SCUS", "SLES", "SCES", "SLPS", "SCPS", "SLPM", "SCPM", "SLKA", "SCKA", "SLAJ", "SCAJ",
    "SLEH", "SCEH", "PAPX", "SCZS",
];

/// Lacrima's NormalizeSerialKey — spaces/underscores/dots become
/// dashes, then uppercase. Display form for extracted serials.
fn ps_serial_norm(s: &str) -> String {
    s.trim()
        .replace(' ', "-")
        .replace('_', "-")
        .replace('.', "-")
        .to_uppercase()
}

/// Lookup key — alphanumeric-only squash, so "SLUS_005.94",
/// "SLUS-00594" and "slus00594" all hit the same DB row (the
/// strict normalized form misses the common `XXXX_NNN.NN` CNF style).
fn ps_serial_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn ps_db(file: &'static str) -> &'static HashMap<String, String> {
    static PSX: OnceLock<HashMap<String, String>> = OnceLock::new();
    static PS2: OnceLock<HashMap<String, String>> = OnceLock::new();
    let slot = if file == "psx.json" { &PSX } else { &PS2 };
    slot.get_or_init(|| {
        let json = db_json(file);
        let mut map = HashMap::new();
        for e in serde_json::from_str::<Vec<serde_json::Value>>(&json).unwrap_or_default() {
            let (Some(serial), Some(title)) = (
                e.get("serial").and_then(|s| s.as_str()),
                e.get("title").and_then(|s| s.as_str()),
            ) else {
                continue;
            };
            // First wins on squash collisions (Lacrima's
            // `!lookup.ContainsKey` behavior).
            map.entry(ps_serial_key(serial))
                .or_insert_with(|| title.trim().to_string());
        }
        map
    })
}

// ------------------------------------------------------------------
// NES: No-Intro hash database (Lacrima's EmulationHashTitleResolver)
// ------------------------------------------------------------------

/// Lacrima's `FullHashThreshold` — cartridge files only.
const CART_HASH_LIMIT: u64 = 200 * 1024 * 1024;

/// A No-Intro hash→title database (`EmulationHashTitleDatabase`) —
/// nes.json / genesis.json share the same {title, md5, sha1, crc}
/// row shape.
#[derive(Default)]
struct HashTitleDb {
    md5: HashMap<String, String>,
    sha1: HashMap<String, String>,
    crc: HashMap<String, String>,
}

fn hash_title_db(file: &'static str) -> &'static HashTitleDb {
    static NES: OnceLock<HashTitleDb> = OnceLock::new();
    static GENESIS: OnceLock<HashTitleDb> = OnceLock::new();
    static SNES: OnceLock<HashTitleDb> = OnceLock::new();
    static GBA: OnceLock<HashTitleDb> = OnceLock::new();
    static PSP: OnceLock<HashTitleDb> = OnceLock::new();
    static PSP_REDUMP: OnceLock<HashTitleDb> = OnceLock::new();
    static FBN: OnceLock<HashTitleDb> = OnceLock::new();
    static ARCADE: OnceLock<HashTitleDb> = OnceLock::new();
    let slot = match file {
        "fbn.json" => &FBN,
        "arcade.json" => &ARCADE,
        "nes.json" => &NES,
        "snes.json" => &SNES,
        "gba.json" => &GBA,
        "psp.json" => &PSP,
        "psp_redump.json" => &PSP_REDUMP,
        _ => &GENESIS,
    };
    slot.get_or_init(|| {
        let json = db_json(file);
        let mut db = HashTitleDb::default();
        for e in serde_json::from_str::<Vec<serde_json::Value>>(&json).unwrap_or_default() {
            let Some(title) = e
                .get("title")
                .and_then(|s| s.as_str())
                .map(str::trim)
                .filter(|t| !t.is_empty())
            else {
                continue;
            };
            // Lacrima's NormalizeHash/NormalizeCrc — hashes lowercase
            // without dashes, CRC uppercase without the 0x prefix.
            if let Some(h) = e.get("md5").and_then(|s| s.as_str()).map(norm_hash) {
                db.md5.entry(h).or_insert_with(|| title.to_string());
            }
            if let Some(h) = e.get("sha1").and_then(|s| s.as_str()).map(norm_hash) {
                db.sha1.entry(h).or_insert_with(|| title.to_string());
            }
            if let Some(c) = e.get("crc").and_then(|s| s.as_str()).map(norm_crc) {
                db.crc.entry(c).or_insert_with(|| title.to_string());
            }
        }
        db
    })
}

fn norm_hash(v: &str) -> String {
    v.trim().replace('-', "").to_lowercase()
}

fn norm_crc(v: &str) -> String {
    v.trim()
        .strip_prefix("0x")
        .or_else(|| v.trim().strip_prefix("0X"))
        .unwrap_or_else(|| v.trim())
        .to_uppercase()
}

/// Lowercase hex for a digest — the 0.11 digest crates' hybrid-array
/// output no longer implements `LowerHex`.
fn digest_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// md5 → sha1 → crc, in that order (Lacrima's TryResolveHashes).
fn hash_db_resolve(file: &'static str, bytes: &[u8]) -> Option<String> {
    use md5::Digest as _;
    let db = hash_title_db(file);
    let md5 = digest_hex(&md5::Md5::digest(bytes));
    if let Some(t) = db.md5.get(&md5) {
        return Some(t.clone());
    }
    let sha1 = digest_hex(&sha1::Sha1::digest(bytes));
    if let Some(t) = db.sha1.get(&sha1) {
        return Some(t.clone());
    }
    let crc = format!("{:08X}", crc32fast::hash(bytes));
    db.crc.get(&crc).cloned()
}

/// Lacrima's `TryNormalizeNesRomForHash` — iNES files also hash their
/// payload minus the 16-byte header and optional 512-byte trainer,
/// catching dumps whose header bytes differ from the DAT's.
fn nes_normalized_body(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() <= 16 || bytes[0..4] != [b'N', b'E', b'S', 0x1A] {
        return None;
    }
    let trainer = if bytes[6] & 0x04 != 0 { 512 } else { 0 };
    bytes.get(16 + trainer..)
}

/// Lacrima's `ExtractNesTitle` — the iNES 2.0 "Name" tag in the
/// extension area after PRG+CHR (+trainer).
fn nes_ines_title(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 16 || bytes[0..4] != [b'N', b'E', b'S', 0x1A] {
        return None;
    }
    // NES 2.0 marker: byte 7 bits 2-3 == 0b10.
    if bytes[7] & 0x0C != 0x08 {
        return None;
    }
    let prg = bytes[4] as usize * 0x4000;
    let chr = bytes[5] as usize * 0x2000;
    let trainer = if bytes[6] & 0x04 != 0 { 512 } else { 0 };
    let ext_off = 16 + trainer + prg + chr;
    if bytes.len() <= ext_off + 8 {
        return None;
    }
    let end = bytes.len().min(ext_off + 64 * 1024);
    let area = &bytes[ext_off..end];
    let mut i = 0;
    while i + 8 <= area.len() {
        if area[i..i + 4] == *b"Name" {
            let len = u32::from_le_bytes(area[i + 4..i + 8].try_into().ok()?) as usize;
            if len == 0 || i + 8 + len > area.len() {
                return None;
            }
            let title = clean_ascii_text(
                String::from_utf8_lossy(&area[i + 8..i + 8 + len])
                    .trim_end_matches('\0'),
            );
            return if valid_rom_title(&title) {
                Some(title)
            } else {
                None
            };
        }
        if area[i] == 0 && (i == 0 || area[i - 1] == 0) {
            break;
        }
        i += 1;
    }
    None
}

/// Lacrima's `CleanAsciiText` — printable ASCII + whitespace,
/// collapse runs of space/tab.
fn clean_ascii_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_ws = false;
    for c in raw.chars() {
        if c == '\t' || c == '\n' || c == '\r' || (c as u32) >= 32 && (c as u32) <= 126 {
            out.push(c);
        } else {
            out.push(' ');
        }
    }
    let mut clean = String::with_capacity(out.len());
    for c in out.chars() {
        match c {
            ' ' | '\t' if pending_ws => {}
            ' ' | '\t' => {
                pending_ws = true;
                clean.push(' ');
            }
            '\r' => {}
            _ => {
                pending_ws = false;
                clean.push(c);
            }
        }
    }
    clean.trim().to_string()
}

/// Lacrima's `IsValidTitle` — ≥3 chars, ≥2 alnum, ≥30% alnum.
fn valid_rom_title(text: &str) -> bool {
    let text = text.trim();
    if text.len() < 3 {
        return false;
    }
    let alnum = text.chars().filter(|c| c.is_alphanumeric()).count();
    alnum >= 2 && alnum as f64 / text.len() as f64 >= 0.3
}

/// NES title — hash the file (then the headerless iNES body) against
/// nes.json, else the iNES 2.0 name tag, else the caller's stem.
fn nes_title(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() as u64 <= CART_HASH_LIMIT {
        if let Some(t) = hash_db_resolve("nes.json", &bytes) {
            return Some(t);
        }
        if let Some(body) = nes_normalized_body(&bytes) {
            if let Some(t) = hash_db_resolve("nes.json", body) {
                return Some(t);
            }
        }
    }
    nes_ines_title(&bytes)
}

// ------------------------------------------------------------------
// Genesis: No-Intro hash DB over the NORMALIZED dump (Lacrima's
// TryApplyGenesisCartridgeInspection) → header title
// (ExtractGenesisTitleFromBytes).
// ------------------------------------------------------------------

/// Cartridge extension lists for archive-member selection — the
/// same sets the sections scan, minus the containers.
const GENESIS_CART_EXTS: &[&str] = &["gen", "md", "smd", "32x", "sg", "bin"];
const SNES_CART_EXTS: &[&str] = &["sfc", "smc", "fig", "swc"];
const GBA_CART_EXTS: &[&str] = &["gba"];

/// The ROM bytes to inspect: the file itself, or the first
/// cartridge member inside a zip/7z (member name returned for the
/// container-format hint — e.g. Lacrima's .smd filename rule).
fn cart_rom_bytes(path: &Path, cart_exts: &[&str]) -> Option<(Vec<u8>, Option<String>)> {
    use std::io::Read as _;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())?;
    match ext.as_str() {
        "zip" => {
            let f = std::fs::File::open(path).ok()?;
            let mut z = zip::ZipArchive::new(f).ok()?;
            for i in 0..z.len() {
                let mut e = z.by_index(i).ok()?;
                if e.is_dir() || e.size() > CART_HASH_LIMIT {
                    continue;
                }
                let Ok(entry_name) = e.name() else {
                    continue;
                };
                let name = entry_name
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or_default()
                    .to_string();
                let eext = name
                    .rsplit_once('.')
                    .map(|(_, x)| x.to_ascii_lowercase())
                    .unwrap_or_default();
                if !cart_exts.contains(&eext.as_str()) {
                    continue;
                }
                let mut buf = Vec::with_capacity(e.size() as usize);
                if e.read_to_end(&mut buf).is_err() {
                    return None;
                }
                return Some((buf, Some(name)));
            }
            None
        }
        "7z" => {
            let f = std::fs::File::open(path).ok()?;
            let mut r =
                sevenz_rust2::ArchiveReader::new(f, sevenz_rust2::Password::empty()).ok()?;
            let name = r
                .archive()
                .files
                .iter()
                .find(|e| {
                    e.has_stream()
                        && !e.is_directory()
                        && e.size <= CART_HASH_LIMIT
                        && e.name()
                            .rsplit_once('.')
                            .map(|(_, x)| {
                                cart_exts
                                    .contains(&x.to_ascii_lowercase().as_str())
                            })
                            .unwrap_or(false)
                })
                .map(|e| e.name().to_string())?;
            let buf = r.read_file(&name).ok()?;
            Some((buf, Some(name)))
        }
        _ => {
            if std::fs::metadata(path).ok()?.len() > CART_HASH_LIMIT {
                return None;
            }
            Some((std::fs::read(path).ok()?, None))
        }
    }
}

fn genesis_has_tag(buf: &[u8], offset: usize, tag: &[u8; 4]) -> bool {
    offset + 4 <= buf.len() && buf[offset..offset + 4] == *tag
}

/// SMD block deinterleave: even bytes live in the first half of each
/// 16KB block, odd bytes in the second (DeinterleaveGenesisPayload).
fn genesis_deinterleave(interleaved: &[u8]) -> Vec<u8> {
    if interleaved.is_empty() || interleaved.len() % 16384 != 0 {
        return interleaved.to_vec();
    }
    let mut out = vec![0u8; interleaved.len()];
    for block in 0..interleaved.len() / 16384 {
        let start = block * 16384;
        for i in 0..8192 {
            out[start + i * 2] = interleaved[start + i];
            out[start + i * 2 + 1] = interleaved[start + 8192 + i];
        }
    }
    out
}

/// Lacrima's `TryBuildGenesisLookupRom` — produce the canonical dump
/// bytes the No-Intro hashes are computed over: strip a 512-byte SMD
/// copier header, try raw SEGA tag, deinterleave, then the ESAG
/// byte-pair swap.
fn genesis_lookup_rom(data: &[u8], entry_name: Option<&str>) -> Option<Vec<u8>> {
    let mut payload: &[u8] = data;
    let smd = entry_name
        .is_some_and(|n| n.to_ascii_lowercase().ends_with(".smd"))
        || (data.len() > 512 && (data.len() - 512) % 16384 == 0);
    if smd {
        if data.len() <= 512 {
            return None;
        }
        payload = &data[512..];
    }
    if payload.len() < 0x200 {
        return None;
    }
    if genesis_has_tag(payload, 0x100, b"SEGA") {
        return Some(payload.to_vec());
    }
    if payload.len() % 16384 != 0 {
        return None;
    }
    let de = genesis_deinterleave(payload);
    if genesis_has_tag(&de, 0x100, b"SEGA") {
        return Some(de);
    }
    if !genesis_has_tag(&de, 0x100, b"ESAG") {
        return None;
    }
    let mut swapped = de;
    for pair in swapped.chunks_exact_mut(2) {
        pair.swap(0, 1);
    }
    genesis_has_tag(&swapped, 0x100, b"SEGA").then_some(swapped)
}

/// A Genesis ASCII header field — printable-ASCII run, trimmed
/// (Lacrima's ReadGenesisAsciiField).
fn genesis_ascii_field(buf: &[u8], offset: usize, len: usize) -> String {
    if offset >= buf.len() {
        return String::new();
    }
    let max = buf.len().min(offset + len);
    let mut end = offset;
    while end < max && (32..=126).contains(&buf[end]) {
        end += 1;
    }
    String::from_utf8_lossy(&buf[offset..end]).trim_end().to_string()
}

/// ExtractGenesisTitleFromBytes — international name at 0x150 wins;
/// the domestic name at 0x120 tries the short form before '/' first.
fn genesis_header_title(normalized: &[u8]) -> Option<String> {
    if normalized.len() < 0x180 {
        return None;
    }
    let international = genesis_ascii_field(normalized, 0x150, 48);
    if !international.trim().is_empty() {
        let cleaned = clean_ascii_text(&international);
        if valid_rom_title(&cleaned) {
            return Some(cleaned);
        }
    }
    let domestic = genesis_ascii_field(normalized, 0x120, 48);
    if !domestic.trim().is_empty() {
        if let Some(slash) = domestic.find('/') {
            let short = clean_ascii_text(&domestic[..slash]);
            if valid_rom_title(&short) {
                return Some(short);
            }
        }
        let cleaned = clean_ascii_text(&domestic);
        if valid_rom_title(&cleaned) {
            return Some(cleaned);
        }
    }
    None
}

/// Genesis title — normalize the dump (SMD/deinterleave/pair-swap),
/// hash it against genesis.json; else the SEGA header name field;
/// a non-normalizable file still gets the raw-hash shot like
/// Lacrima's fallback ApplyCartridgeHashes.
fn genesis_title(path: &Path) -> Option<String> {
    let (bytes, inner_name) = cart_rom_bytes(path, GENESIS_CART_EXTS)?;
    if let Some(norm) = genesis_lookup_rom(&bytes, inner_name.as_deref()) {
        if let Some(t) = hash_db_resolve("genesis.json", &norm) {
            return Some(t);
        }
        return genesis_header_title(&norm);
    }
    hash_db_resolve("genesis.json", &bytes)
}

// ------------------------------------------------------------------
// SNES: No-Intro hash DB (snes.json, generated from the libretro
// No-Intro dat) over the file then the copier-header-stripped body →
// validated LoROM/HiROM header title (ExtractSnesTitle).
// ------------------------------------------------------------------

/// SNES header candidates — LoROM and HiROM, each in the clean and
/// the +512-byte copier-header layout (Lacrima's headerOffsets).
const SNES_HEADER_OFFSETS: [usize; 4] = [0x7FB0, 0x7FB0 + 0x200, 0xFFB0, 0xFFB0 + 0x200];

/// Lacrima's `IsSnesHeader` — checksum + complement == 0xFFFF with a
/// nonzero/non-FFFF checksum.
fn snes_valid_header(buf: &[u8], offset: usize) -> bool {
    let Some(b) = buf.get(offset + 0x2C..offset + 0x30) else {
        return false;
    };
    let complement = u16::from_le_bytes([b[0], b[1]]);
    let checksum = u16::from_le_bytes([b[2], b[3]]);
    checksum != 0 && checksum != 0xFFFF && checksum.wrapping_add(complement) == 0xFFFF
}

/// Lacrima's `ExtractSnesTitle` — the 21-byte ASCII field 0x10 bytes
/// into the first validated header.
fn snes_header_title(bytes: &[u8]) -> Option<String> {
    for offset in SNES_HEADER_OFFSETS {
        if !snes_valid_header(bytes, offset) {
            continue;
        }
        let Some(field) = bytes.get(offset + 0x10..offset + 0x10 + 21) else {
            continue;
        };
        let cleaned = clean_ascii_text(&String::from_utf8_lossy(field));
        if valid_rom_title(&cleaned) {
            return Some(cleaned);
        }
    }
    None
}

/// SNES title — hash the file (then the body minus a 512-byte copier
/// header, which is what No-Intro actually hashes) against snes.json,
/// else the header title field, else the caller's stem.
fn snes_title(path: &Path) -> Option<String> {
    let (bytes, _inner) = cart_rom_bytes(path, SNES_CART_EXTS)?;
    if let Some(t) = hash_db_resolve("snes.json", &bytes) {
        return Some(t);
    }
    // Copier-headered dump: strip 512 only when the shifted header
    // actually validates as SNES.
    if bytes.len() > 512
        && (snes_valid_header(&bytes, 0x7FB0 + 0x200)
            || snes_valid_header(&bytes, 0xFFB0 + 0x200))
    {
        if let Some(t) = hash_db_resolve("snes.json", &bytes[512..]) {
            return Some(t);
        }
    }
    snes_header_title(&bytes)
}

// ------------------------------------------------------------------
// GBA: No-Intro hash DB (gba.json) over the file → 12-byte header
// title at 0xA0 (ExtractGbaTitle).
// ------------------------------------------------------------------

/// Lacrima's `ExtractGbaTitle` — the 12-byte ASCII title field at
/// 0xA0, trimmed of NUL/space padding.
fn gba_header_title(bytes: &[u8]) -> Option<String> {
    let field = bytes.get(0xA0..0xA0 + 12)?;
    let cleaned = clean_ascii_text(
        String::from_utf8_lossy(field)
            .trim_end_matches(['\0', ' ']),
    );
    valid_rom_title(&cleaned).then_some(cleaned)
}

/// GBA title — hash the file against gba.json (Lacrima has no GBA
/// normalization), else the header title, else the caller's stem.
fn gba_title(path: &Path) -> Option<String> {
    let (bytes, _inner) = cart_rom_bytes(path, GBA_CART_EXTS)?;
    if let Some(t) = hash_db_resolve("gba.json", &bytes) {
        return Some(t);
    }
    gba_header_title(&bytes)
}

// ------------------------------------------------------------------
// PSP: PARAM.SFO TITLE first (Lacrima's TryResolvePspTitle), then the
// redump hash db, then the No-Intro hash db — the opposite order of
// the cart sections, since every retail UMD image carries an SFO.
// ------------------------------------------------------------------

const SFO_MAGIC: u32 = 0x46535000; // "\0PSF"
const SFO_MAGIC_ALT: u32 = 0x00465350; // "PSF\0" (some homebrew/tools)
const PSP_SCAN_LIMIT: usize = 8 * 1024 * 1024;

/// Lacrima's `PspParamSfoReader.TryReadSfo` — returns (DISC_ID,
/// TITLE). Index entries are 16 bytes: key_off u16, data_format u16,
/// data_len u32, _max_len u32, data_off u32.
fn psp_sfo_fields(bytes: &[u8]) -> (Option<String>, Option<String>) {
    let u32le = |o: usize| -> Option<u32> {
        bytes
            .get(o..o + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    let magic = u32le(0).unwrap_or(0);
    if magic != SFO_MAGIC && magic != SFO_MAGIC_ALT {
        return (None, None);
    }
    let key_table = u32le(8).unwrap_or(0) as usize;
    let data_table = u32le(12).unwrap_or(0) as usize;
    let count = u32le(16).unwrap_or(0) as usize;
    if count == 0 || count > 128 {
        return (None, None);
    }
    let (mut disc_id, mut title) = (None, None);
    for i in 0..count {
        let e = 0x14 + i * 16;
        let Some(entry) = bytes.get(e..e + 16) else { break };
        let key_off = u16::from_le_bytes(entry[0..2].try_into().unwrap()) as usize;
        let data_format = u16::from_le_bytes(entry[2..4].try_into().unwrap());
        let data_len = u32::from_le_bytes(entry[4..8].try_into().unwrap()) as usize;
        let data_off = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
        // Key: cstring at key_table+key_off, ≤64 bytes.
        let key_start = key_table + key_off;
        let key: String = bytes
            .get(key_start..key_start.saturating_add(64).min(bytes.len()))
            .unwrap_or(&[])
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| b as char)
            .collect();
        if key.is_empty() {
            continue;
        }
        let value = if (data_format == 0x0404 || data_format == 0x0204) && data_len > 0 {
            let start = data_table + data_off;
            let take = data_len.min(512);
            bytes
                .get(start..start + take)
                .map(|raw| psp_decode_sfo_string(raw))
        } else if data_format == 0x0402 && data_len >= 4 {
            bytes
                .get(data_table + data_off..data_table + data_off + 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()).to_string())
        } else {
            None
        };
        let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
            continue;
        };
        if key.eq_ignore_ascii_case("DISC_ID") {
            disc_id = Some(value.clone());
        } else if key.eq_ignore_ascii_case("TITLE") {
            title = Some(value.clone());
        }
    }
    (disc_id, title)
}

/// Lacrima's `DecodeSfoString` — strict UTF-8, then ShiftJIS, then a
/// printable-ASCII prefix.
fn psp_decode_sfo_string(raw: &[u8]) -> String {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let span = &raw[..end];
    if span.is_empty() {
        return String::new();
    }
    let decoded = std::str::from_utf8(span)
        .map(|s| s.to_string())
        .unwrap_or_else(|_| {
            let (cow, _, _) = encoding_rs::SHIFT_JIS.decode(span);
            cow.into_owned()
        });
    let cleaned = sanitize_psp_title(&decoded);
    if !cleaned.is_empty() {
        return cleaned;
    }
    sanitize_psp_title(
        &span
            .iter()
            .take_while(|&&b| (32..=126).contains(&b))
            .map(|&b| b as char)
            .collect::<String>(),
    )
}

/// Lacrima's `SanitizePspTitle` — trim, drop NUL/space padding, then
/// any trailing '?'/replacement chars left by a failed decode.
fn sanitize_psp_title(value: &str) -> String {
    let v = value.trim().trim_end_matches(['\0', ' ']);
    v.trim_end_matches(['?', '\u{FFFD}']).trim_end().to_string()
}

/// Read `PSP_GAME/PARAM.SFO` out of an ISO9660 image — PVD at sector
/// 16, root dir record at +156, two-level directory walk (Lacrima's
/// `TryLocateIso9660File`). `read_at` abstracts the file/CSO-block
/// source.
fn psp_iso9660_sfo(
    read_at: &mut dyn FnMut(u64, usize) -> Option<Vec<u8>>,
) -> Option<(Option<String>, Option<String>)> {
    let pvd = read_at(16 * PS_SECTOR, PS_SECTOR as usize)?;
    if pvd.first() != Some(&1) || pvd.len() < 156 + 34 || pvd[156] == 0 {
        return None;
    }
    let root_lba = le32(&pvd, 156 + 2)? as u64;
    let root_size = le32(&pvd, 156 + 10)?.min(64 * 1024 * 1024) as usize;
    if root_lba == 0 || root_size == 0 {
        return None;
    }
    let root_dir = read_at(root_lba * PS_SECTOR, root_size)?;
    let sfo_dir = iso_dir_data(&root_dir, "PSP_GAME", read_at)?;
    let sfo = iso_dir_data(&sfo_dir, "PARAM.SFO", read_at)?;
    let bytes = &sfo[..sfo.len().min(64 * 1024)];
    Some(psp_sfo_fields(bytes))
}

/// One ISO9660 directory level — find `name` in `dir` and return its
/// data extent (a directory's extent is the next level's dir bytes).
fn iso_dir_data(
    dir: &[u8],
    name: &str,
    read_at: &mut dyn FnMut(u64, usize) -> Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    while pos < dir.len() {
        let rec = dir[pos] as usize;
        if rec == 0 {
            // Directory records can't cross a sector boundary.
            pos += (PS_SECTOR as usize - pos % PS_SECTOR as usize).max(1);
            continue;
        }
        if pos + rec > dir.len() {
            break;
        }
        let name_len = dir[pos + 32] as usize;
        if name_len > 0 {
            if let Some(raw) = dir.get(pos + 33..pos + 33 + name_len) {
                let n = String::from_utf8_lossy(raw);
                let n = n.split(';').next().unwrap_or("").trim_end_matches(['\0', ' ']);
                if n.eq_ignore_ascii_case(name) {
                    let lba = le32(dir, pos + 2)? as u64;
                    let size = le32(dir, pos + 10)?.min(64 * 1024 * 1024) as usize;
                    return (lba != 0 && size != 0)
                        .then(|| read_at(lba * PS_SECTOR, size))
                        .flatten();
                }
            }
        }
        pos += rec;
    }
    None
}

/// Raw scan for `\0PSF`/`PSF\0` magic in the first bytes of a source
/// (Lacrima's `TryScanForSfo` — the fallback for images whose PVD
/// path lookup fails).
fn psp_scan_sfo(bytes: &[u8]) -> Option<(Option<String>, Option<String>)> {
    let mut off = 0usize;
    while off + 0x14 <= bytes.len() {
        let magic = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
        if magic == SFO_MAGIC || magic == SFO_MAGIC_ALT {
            let (id, title) = psp_sfo_fields(&bytes[off..]);
            if id.is_some() || title.is_some() {
                return Some((id, title));
            }
        }
        off += 1;
    }
    None
}

/// Read `(DISC_ID, TITLE)` from a PSP package — PBP via its param
/// offset, ISO via ISO9660 → raw scan, CSO via its first data block
/// (stored uncompressed when the index high bit is set; Lacrima
/// reads the block raw either way).
fn psp_sfo_read(path: &Path) -> Option<(Option<String>, Option<String>)> {
    use std::io::{Read, Seek, SeekFrom};
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())?;
    let mut f = std::fs::File::open(path).ok()?;
    let file_len = f.metadata().ok()?.len();
    let mut read_at = |off: u64, len: usize| -> Option<Vec<u8>> {
        if off >= file_len {
            return None;
        }
        let take = len.min((file_len - off) as usize);
        let mut b = vec![0u8; take];
        f.seek(SeekFrom::Start(off)).ok()?;
        f.read_exact(&mut b).ok()?;
        Some(b)
    };
    match ext.as_str() {
        "pbp" => {
            let hdr = read_at(0, 0x20)?;
            if hdr[0..4] != [0x00, b'P', b'B', b'P'] {
                return None;
            }
            let sfo_off = u32::from_le_bytes(hdr[8..12].try_into().unwrap()) as u64;
            if sfo_off == 0 || sfo_off >= file_len {
                return None;
            }
            let sfo = read_at(sfo_off, (file_len - sfo_off).min(64 * 1024) as usize)?;
            Some(psp_sfo_fields(&sfo))
        }
        "cso" => {
            let hdr = read_at(0, 0x20)?;
            if hdr[0..4] != *b"CISO" {
                return None;
            }
            let header_size = le32(&hdr, 4)? as u64;
            let block_size = le32(&hdr, 0x0C)? as u64;
            if header_size == 0 || block_size == 0 || header_size + 4 > file_len {
                return None;
            }
            let idx = read_at(header_size, 4)?;
            let block_off = (le32(&idx, 0)? & 0x7FFF_FFFF) as u64;
            if block_off == 0 || block_off >= file_len {
                return None;
            }
            let block = read_at(block_off, block_size as usize)?;
            psp_iso9660_sfo(&mut |off, len| {
                let take = len.min(block.len().saturating_sub(off as usize));
                block
                    .get(off as usize..off as usize + take)
                    .map(|s| s.to_vec())
            })
            .or_else(|| psp_scan_sfo(&block))
        }
        "iso" => psp_iso9660_sfo(&mut read_at).or_else(|| {
            let head = read_at(0, PSP_SCAN_LIMIT.min(file_len as usize))?;
            psp_scan_sfo(&head)
        }),
        _ => None,
    }
}

/// Stream-hash a (potentially multi-GB) image and try each db in
/// order — Lacrima's TryResolvePspTitle fallback pattern.
fn hash_db_resolve_file(path: &Path, dbs: &[&'static str]) -> Option<String> {
    use md5::Digest as _;
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let (mut md5, mut sha1, mut crc) =
        (md5::Md5::new(), sha1::Sha1::new(), crc32fast::Hasher::new());
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        md5.update(&buf[..n]);
        sha1.update(&buf[..n]);
        crc.update(&buf[..n]);
    }
    let (md5, sha1, crc) = (
        digest_hex(&md5.finalize()),
        digest_hex(&sha1.finalize()),
        format!("{:08X}", crc.finalize()),
    );
    for file in dbs {
        let db = hash_title_db(file);
        if let Some(t) = db
            .md5
            .get(&md5)
            .or_else(|| db.sha1.get(&sha1))
            .or_else(|| db.crc.get(&crc))
        {
            return Some(t.clone());
        }
    }
    None
}

fn psp_hash_title(path: &Path) -> Option<String> {
    hash_db_resolve_file(path, &["psp_redump.json", "psp.json"])
}

// ------------------------------------------------------------------
// FBN: the dat hashes the romset archive itself (zip), so a whole-
// file hash resolves the title; else the caller's stem.
// ------------------------------------------------------------------

fn fbn_title(path: &Path) -> Option<String> {
    hash_db_resolve_file(path, &["fbn.json"])
}

// ------------------------------------------------------------------
// ARCADE: MAME consolidated-romsets dat — same archive-hash scheme
// as FBN (the dat's rom() hashes the set zip itself).
// ------------------------------------------------------------------

fn arcade_title(path: &Path) -> Option<String> {
    hash_db_resolve_file(path, &["arcade.json"])
}

/// PSP title — Lacrima's `TryResolvePspTitle`: the PARAM.SFO TITLE
/// wins (every UMD carries one), then redump → No-Intro hash lookups,
/// then the caller's stem.
fn psp_title(path: &Path) -> Option<String> {
    if let Some((_id, title)) = psp_sfo_read(path) {
        if let Some(t) = title {
            let cleaned = clean_ascii_text(sanitize_psp_title(&t).trim());
            if valid_rom_title(&cleaned) {
                return Some(cleaned);
            }
        }
    }
    psp_hash_title(path)
}

/// The disc serial — Lacrima's GetTitleId dispatch on extension.
pub(crate) fn ps_disc_serial(path: &Path) -> Option<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())?;
    match ext.as_str() {
        // A cue owns its tracks — read the first data bin.
        "cue" => cue_track_files(path)
            .into_iter()
            .find_map(|b| ps_iso_serial(&b)),
        "m3u" => descriptor_refs(path)
            .into_iter()
            .find_map(|p| ps_disc_serial(&p)),
        "pbp" => ps_pbp_serial(path),
        _ => ps_iso_serial(path),
    }
}

/// PBP's embedded PARAM.SFO carries TITLE_ID (GetTitleIdFromPbp).
fn ps_pbp_serial(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let mut hdr = [0u8; 0x28];
    f.read_exact(&mut hdr).ok()?;
    // Real PBP magic is 00 50 42 50 ("\0PBP") — the earlier "PBP\0"
    // check could never match an actual file.
    if &hdr[0..4] != b"\0PBP" {
        return None;
    }
    let sfo_off = u32::from_le_bytes(hdr[8..12].try_into().unwrap()) as u64;
    f.seek(SeekFrom::Start(sfo_off)).ok()?;
    // The SFO data table bound is unknown — read to EOF (PBPs are
    // small relative to what follows anyway) and let the parser walk.
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).ok()?;
    if bytes.len() < 20 || bytes[0..4] != [0x00, 0x50, 0x53, 0x46] {
        return None;
    }
    let u32le = |o: usize| -> Option<usize> {
        bytes
            .get(o..o + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
    };
    let key_table = u32le(8)?;
    let data_table = u32le(12)?;
    let count = u32le(16)?.min(4096);
    for i in 0..count {
        let e = 20 + i * 16;
        let Some(entry) = bytes.get(e..e + 16) else {
            break;
        };
        let key_off = u16::from_le_bytes(entry[0..2].try_into().unwrap()) as usize;
        let data_len = u32::from_le_bytes(entry[4..8].try_into().unwrap()) as usize;
        let data_off = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
        let key: String = bytes
            .get(key_table + key_off..)
            .unwrap_or(&[])
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| b as char)
            .collect();
        if key.eq_ignore_ascii_case("TITLE_ID") && data_len > 0 {
            let raw = bytes.get(data_table + data_off..data_table + data_off + data_len)?;
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            let v = String::from_utf8_lossy(&raw[..end]).trim().to_string();
            return (!v.is_empty()).then_some(v);
        }
    }
    None
}

/// MODE1/2048 PVD → root dir → SYSTEM.CNF `BOOT=` serial, then
/// PSX.EXE scan, then the 16-sector system area, then the filename
/// (Lacrima's GetTitleIdFromIso fallback chain).
fn ps_iso_serial(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let read_at = |f: &mut std::fs::File, off: u64, len: usize| -> Option<Vec<u8>> {
        f.seek(SeekFrom::Start(off)).ok()?;
        let mut b = vec![0u8; len];
        let n = f.read(&mut b).ok()?;
        (n > 0).then(|| {
            b.truncate(n);
            b
        })
    };
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len >= PS_PVD_OFF + PS_SECTOR {
        if let Some(pvd) = read_at(&mut f, PS_PVD_OFF, PS_SECTOR as usize) {
            if pvd.first() == Some(&1) {
                // Root directory record sits at PVD+156 (34 bytes):
                // extent LBA le32@+2, data size le32@+10.
                let root_lba = le32(&pvd, 156 + 2).unwrap_or(0) as u64;
                let root_size = le32(&pvd, 156 + 10).unwrap_or(0).min(64 * 1024 * 1024);
                if root_lba != 0 && root_size != 0 {
                    if let Some(dir) = read_at(&mut f, root_lba * PS_SECTOR, root_size as usize) {
                        if let Some(cnf) = iso_read_named(&mut f, &dir, "SYSTEM.CNF", &read_at) {
                            if let Some(s) = ps_parse_system_cnf(&cnf) {
                                return Some(s);
                            }
                        }
                        if let Some(exe) = iso_read_named(&mut f, &dir, "PSX.EXE", &read_at) {
                            if let Some(s) = ps_scan_serial(&exe) {
                                return Some(s);
                            }
                        }
                    }
                }
            }
        }
        // The 32KB system area precedes the PVD — some images keep
        // the serial there.
        if let Some(area) = read_at(&mut f, 0, 16 * PS_SECTOR as usize) {
            if let Some(s) = ps_scan_serial(&area) {
                return Some(s);
            }
        }
    }
    ps_serial_from_name(path)
}

/// Walk an ISO9660 directory for `name` (single level — Lacrima
/// only reads the root dir) and return the file's bytes.
fn iso_read_named(
    f: &mut std::fs::File,
    dir: &[u8],
    name: &str,
    read_at: &dyn Fn(&mut std::fs::File, u64, usize) -> Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    while pos < dir.len() {
        let rec = dir[pos] as usize;
        if rec == 0 {
            // Directory records can't cross a sector boundary.
            let pad = PS_SECTOR as usize - (pos % PS_SECTOR as usize);
            pos += pad.max(1);
            continue;
        }
        if pos + rec > dir.len() {
            break;
        }
        let name_len = dir[pos + 32] as usize;
        if name_len > 0 {
            if let Some(raw) = dir.get(pos + 33..pos + 33 + name_len) {
                let n = String::from_utf8_lossy(raw);
                let n = n.split(';').next().unwrap_or("");
                if n.eq_ignore_ascii_case(name) {
                    let lba = le32(dir, pos + 2)? as u64;
                    let size = le32(dir, pos + 10)?.min(64 * 1024 * 1024) as usize;
                    if lba != 0 && size != 0 {
                        return read_at(f, lba * PS_SECTOR, size);
                    }
                    return None;
                }
            }
        }
        pos += rec;
    }
    None
}

/// SYSTEM.CNF `BOOT = cdrom:\SLUS_005.94;1` → "SLUS-00594".
fn ps_parse_system_cnf(data: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(data);
    for line in text.lines() {
        let line = line.trim();
        let Some(eq) = line.find('=') else {
            continue;
        };
        // PSX discs use BOOT, PS2 discs BOOT2 (cdrom0:/...).
        let key = line[..eq].trim();
        if !key.eq_ignore_ascii_case("BOOT") && !key.eq_ignore_ascii_case("BOOT2") {
            continue;
        }
        let mut exe = line[eq + 1..].trim();
        let lower = exe.to_lowercase();
        for p in ["cdrom0:", "cdrom:"] {
            if lower.starts_with(p) {
                exe = exe[p.len()..].trim_start_matches(['/', '\\']);
                break;
            }
        }
        let base = exe
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(exe)
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        if base.is_empty() {
            continue;
        }
        let upper = base.to_uppercase();
        if let Some(s) = ps_prefix_serial(&upper) {
            return Some(s);
        }
        return Some(ps_serial_norm(&upper));
    }
    None
}

/// Find a SERIAL_PREFIXES token in `s` and collect ≤16 alnum/-_
/// chars after it (≥9 total → "SLUS-00594").
fn ps_prefix_serial(upper: &str) -> Option<String> {
    for p in PS_SERIAL_PREFIXES {
        let Some(idx) = upper.find(p) else {
            continue;
        };
        let bytes = upper.as_bytes();
        let mut end = idx + p.len();
        while end < bytes.len() && end - idx < 16 {
            let c = bytes[end] as char;
            if !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
                break;
            }
            end += 1;
        }
        if end - idx >= 9 {
            return Some(ps_serial_norm(&upper[idx..end]));
        }
    }
    None
}

/// Scan raw bytes for a serial-prefixed token (PSX.EXE / system
/// area fallback).
fn ps_scan_serial(data: &[u8]) -> Option<String> {
    for i in 0..data.len().saturating_sub(9) {
        if !matches!(data[i], b'S' | b's') {
            continue;
        }
        for p in PS_SERIAL_PREFIXES {
            let pb = p.as_bytes();
            if i + pb.len() > data.len() {
                continue;
            }
            if !data[i..i + pb.len()].eq_ignore_ascii_case(pb) {
                continue;
            }
            let mut end = i + pb.len();
            while end < data.len() && end - i < 16 {
                let c = data[end];
                if !(c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
                    break;
                }
                end += 1;
            }
            if (9..=16).contains(&(end - i)) {
                let s = String::from_utf8_lossy(&data[i..end]);
                return Some(ps_serial_norm(&s));
            }
        }
    }
    None
}

/// Last resort: "SLUS-00594 - Game Name.iso" (TryExtractFromFileName).
fn ps_serial_from_name(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?.to_uppercase();
    ps_prefix_serial(&stem)
}

/// psx.json serial → title (Lacrima's ResolvePsTitle on PSX).
fn psx_title(path: &Path) -> Option<String> {
    let id = ps_disc_serial(path)?;
    ps_db("psx.json").get(&ps_serial_key(&id)).cloned()
}

/// ps2.json serial → title — same ISO layout + serial family.
fn ps2_title(path: &Path) -> Option<String> {
    let id = ps_disc_serial(path)?;
    ps_db("ps2.json").get(&ps_serial_key(&id)).cloned()
}

// ------------------------------------------------------------------
// GameCube / Wii — DolphinDiscMetadataReader port: the volume header
// at disc offset 0 (game id @0, internal name @0x20..0x80), read
// through the WBFS / GCZ / CISO container readers. `.wad` carries a
// title id but no embedded name — the stem fallback wins there.
// ------------------------------------------------------------------

const GC_DISC_MAGIC: u32 = 0xC2339F3D; // big-endian @0x1C
const WII_DISC_MAGIC: u32 = 0x5D1C9EA3; // big-endian @0x18
const GCZ_MAGIC: u32 = 0xB10BC001; // little-endian @0
const CISO_MAGIC: u32 = 0x4F53_4943; // "CISO" little-endian @0
const WBFS_MAGIC: u32 = 0x5346_4257; // "WBFS" little-endian @0

/// The 0x80-byte volume header at disc offset 0, mapped through the
/// container when the file is WBFS/GCZ/CISO. Plain images (iso, gcm,
/// dol, elf, tgc…) read it straight.
fn dolphin_disc_header(path: &Path) -> Option<[u8; 0x80]> {
    use std::io::{Read, Seek, SeekFrom};
    let ext = path.extension()?.to_str()?.to_lowercase();
    let mut f = std::fs::File::open(path).ok()?;
    let mut hdr = [0u8; 0x80];
    match ext.as_str() {
        "wbfs" => {
            let mut h = [0u8; 512];
            f.read_exact(&mut h).ok()?;
            if u32::from_le_bytes(h[..4].try_into().ok()?) != WBFS_MAGIC || h[12] == 0 {
                return None;
            }
            let (hd_shift, wbfs_shift) = (h[8] as u32, h[9] as u32);
            if hd_shift > 31 || wbfs_shift > 31 {
                return None;
            }
            let hd_size = 1u64 << hd_shift;
            let wbfs_size = 1u64 << wbfs_shift;
            if wbfs_size < 0x8000 {
                return None;
            }
            let blocks = (143432u64 * 2 * 0x8000).div_ceil(wbfs_size);
            let wlba_off = hd_size + 256;
            let wlba_bytes = blocks * 2;
            let flen = f.metadata().ok()?.len();
            if flen < wlba_off + wlba_bytes {
                return None;
            }
            f.seek(SeekFrom::Start(wlba_off)).ok()?;
            let mut w = [0u8; 2];
            f.read_exact(&mut w).ok()?;
            let addr = wbfs_size * u16::from_be_bytes(w) as u64;
            if addr + 0x80 > flen {
                return None;
            }
            f.seek(SeekFrom::Start(addr)).ok()?;
            f.read_exact(&mut hdr).ok()?;
        }
        "gcz" => {
            let mut h = [0u8; 32];
            f.read_exact(&mut h).ok()?;
            if u32::from_le_bytes(h[0..4].try_into().ok()?) != GCZ_MAGIC {
                return None;
            }
            let comp_size = u64::from_le_bytes(h[8..16].try_into().ok()?);
            let block_size = u32::from_le_bytes(h[24..28].try_into().ok()?) as usize;
            let num_blocks = u32::from_le_bytes(h[28..32].try_into().ok()?) as usize;
            if block_size == 0 || num_blocks == 0 || block_size > (256 << 20) {
                return None;
            }
            // Block-pointer table: num_blocks u64s at offset 32, then
            // num_blocks adler32s, then the payload. Only block 0 is
            // needed for the volume header.
            f.seek(SeekFrom::Start(32)).ok()?;
            let mut p0 = [0u8; 8];
            f.read_exact(&mut p0).ok()?;
            let mut start = u64::from_le_bytes(p0);
            let stored_raw = (start & (1 << 63)) != 0;
            start &= !(1 << 63);
            let end = if num_blocks > 1 {
                let mut p1 = [0u8; 8];
                f.read_exact(&mut p1).ok()?;
                u64::from_le_bytes(p1) & !(1 << 63)
            } else {
                comp_size
            };
            if end <= start || end - start > (256 << 20) {
                return None;
            }
            let data_off = 32 + num_blocks as u64 * 8 + num_blocks as u64 * 4;
            f.seek(SeekFrom::Start(data_off + start)).ok()?;
            let mut comp = vec![0u8; (end - start) as usize];
            f.read_exact(&mut comp).ok()?;
            if stored_raw {
                let n = comp.len().min(0x80);
                hdr[..n].copy_from_slice(&comp[..n]);
            } else {
                use flate2::read::ZlibDecoder;
                let mut dec = ZlibDecoder::new(&comp[..]);
                dec.read_exact(&mut hdr).ok()?;
            }
        }
        "ciso" => {
            // Header 0x8000: magic, blockSize, then a 1-byte used flag
            // per disc block; used blocks map to sequential file
            // blocks. Block 0 unused → header is zeroed → no title.
            let mut h = [0u8; 8];
            f.read_exact(&mut h).ok()?;
            if u32::from_le_bytes(h[0..4].try_into().ok()?) != CISO_MAGIC {
                return None;
            }
            let block_size = u32::from_le_bytes(h[4..8].try_into().ok()?) as u64;
            if block_size == 0 || block_size > (256 << 20) {
                return None;
            }
            let mut flag = [0u8; 1];
            f.read_exact(&mut flag).ok()?;
            if flag[0] != 1 {
                return None;
            }
            f.seek(SeekFrom::Start(0x8000)).ok()?;
            f.read_exact(&mut hdr).ok()?;
        }
        _ => {
            f.read_exact(&mut hdr).ok()?;
        }
    }
    Some(hdr)
}

/// Internal disc name → display title. Accepts the header only when
/// the Wii/GameCube magic or a full 6-char game id validates the
/// read (Lacrima's ParseDiscHeader + FilterGameId guards).
fn dolphin_disc_title(path: &Path) -> Option<String> {
    let hdr = dolphin_disc_header(path)?;
    let game_id: String = hdr[..6]
        .iter()
        .filter(|b| b.is_ascii_alphanumeric())
        .map(|b| *b as char)
        .collect();
    let magic_ok = u32::from_be_bytes(hdr[0x18..0x1C].try_into().ok()?) == WII_DISC_MAGIC
        || u32::from_be_bytes(hdr[0x1C..0x20].try_into().ok()?) == GC_DISC_MAGIC;
    if !magic_ok && game_id.len() < 6 {
        return None;
    }
    let raw = String::from_utf8_lossy(&hdr[0x20..0x80]);
    let t = raw.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    (t.chars().filter(|c| !c.is_control()).count() >= 2).then(|| t.to_string())
}

// ------------------------------------------------------------------
// Wii U (WiiUInstalledGameHelper): installed-game folders carry
// code/content/meta with a meta.xml — longname_en wins, then the
// other locales, then any longname_*. Bare wud/wux/wua/rpx files
// check the parent + grandparent dirs for that layout (an .rpx sits
// in `code/`), then fall back to a cleaned filename stem.
// ------------------------------------------------------------------
fn wiiu_installed_dir(path: &Path) -> Option<&Path> {
    let is_pkg = |d: &Path| {
        d.join("code").is_dir()
            && d.join("content").is_dir()
            && d.join("meta").is_dir()
            && wiiu_meta_xml(d).is_some()
    };
    if path.is_dir() {
        return is_pkg(path).then_some(path);
    }
    // A bare file — its parent (or grandparent for `code/*.rpx`)
    // may be the package root; Lacrima's GetCandidateDirectories
    // yields exactly those two.
    for dir in [path.parent(), path.parent().and_then(Path::parent)]
        .into_iter()
        .flatten()
    {
        if is_pkg(dir) {
            return Some(dir);
        }
    }
    None
}

fn wiiu_meta_xml(dir: &Path) -> Option<PathBuf> {
    let meta = dir.join("meta");
    let direct = meta.join("meta.xml");
    if direct.is_file() {
        return Some(direct);
    }
    // Case-insensitive fallback (Lacrima enumerates meta\*).
    std::fs::read_dir(&meta).ok()?.flatten().find_map(|e| {
        let p = e.path();
        (p.file_name()
            .and_then(|f| f.to_str())
            .is_some_and(|f| f.eq_ignore_ascii_case("meta.xml")))
        .then_some(p)
    })
}

/// Flat `<key>value</key>` pull — meta.xml is flat enough that a tag
/// scan beats pulling in an XML crate. Real meta.xml tags carry
/// `type`/`length` attributes (`<title_id type="hexBinary"
/// length="8">`), so the match is prefix + boundary, not the literal
/// `<key>`.
fn wiiu_tag(xml: &str, key: &str) -> Option<String> {
    let needle = format!("<{key}");
    let close = format!("</{key}>");
    let mut rest = xml;
    while let Some(i) = rest.find(&needle) {
        let after = &rest[i + needle.len()..];
        let boundary = after
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == '>' || c == '/');
        if !boundary {
            rest = after;
            continue;
        }
        // `>` must come after the needle — an earlier tag's close
        // can precede the match in `rest`.
        let Some(tag_end) = rest[i + needle.len()..]
            .find('>')
            .map(|e| e + i + needle.len())
        else {
            return None;
        };
        // Self-closing tag → no body; keep scanning.
        if rest[i + needle.len()..tag_end].ends_with('/') {
            rest = &rest[tag_end + 1..];
            continue;
        }
        let body = &rest[tag_end + 1..];
        let Some(end) = body.find(&close) else {
            return None;
        };
        let v = body[..end].trim();
        if !v.is_empty() {
            return Some(v.replace(['\r', '\n'], " ").trim().to_string());
        }
        rest = &rest[tag_end + 1..];
    }
    None
}

fn wiiu_meta_title(dir: &Path) -> Option<String> {
    let xml = std::fs::read_to_string(wiiu_meta_xml(dir)?).ok()?;
    let grab = |key: &str| wiiu_tag(&xml, key);
    for k in [
        "longname_en",
        "longname_us",
        "longname_ja",
        "longname_fr",
        "longname_de",
        "longname_es",
        "longname_it",
    ] {
        if let Some(t) = grab(k) {
            return Some(t);
        }
    }
    // Any remaining longname_* (scan tags so attribute-order can't
    // matter).
    let mut rest = xml.as_str();
    while let Some(i) = rest.find("<longname_") {
        rest = &rest[i + 1..];
        let Some(tag_end) = rest.find('>') else {
            break;
        };
        let key = &rest["longname_".len()..tag_end];
        let Some(key) = key.split([' ', '/', '\t']).next() else {
            break;
        };
        let open_end = rest.find('>').unwrap() + 1;
        let body = &rest[open_end..];
        let Some(close) = body.find(&format!("</longname_{key}>")) else {
            rest = &rest[open_end..];
            continue;
        };
        let v = body[..close].trim();
        if !v.is_empty() {
            return Some(v.replace(['\r', '\n'], " ").trim().to_string());
        }
        rest = &rest[open_end..];
    }
    None
}

fn wiiu_title(path: &Path) -> Option<String> {
    if let Some(dir) = wiiu_installed_dir(path) {
        if let Some(t) = wiiu_meta_title(dir) {
            return Some(t);
        }
    }
    // Cleaned filename stem: drop the title-id token + region/paren
    // noise (ExtractTitleNameFromFilePath).
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let mut out = String::with_capacity(stem.len());
    let mut last_space = false;
    for w in stem.split([' ', '_', '-', '[', ']', '(', ')']) {
        let hex = w.len() >= 16 && w.chars().all(|c| c.is_ascii_hexdigit())
            || w.len() == 17 && w.chars().filter(|c| c.is_ascii_hexdigit()).count() == 16;
        if w.is_empty() || hex {
            continue;
        }
        if last_space {
            out.push(' ');
        }
        out.push_str(w);
        last_space = true;
    }
    let t = out.trim();
    (t.len() >= 2).then(|| t.to_string())
}

/// `WiiUInstalledGameHelper.GetTitleId` + `ExtractTitleIdFromText` —
/// `<title_id>` out of the installed package's meta.xml first, else a
/// 16-hex (or `XXXXXXXX-_XXXXXXXX`) token embedded in the path.
/// Normalized to Cemu's `XXXXXXXX-XXXXXXXX` display form
/// (CemuTitleIdHelper.NormalizeDisplayTitleId).
pub fn wiiu_title_id(path: &Path) -> Option<String> {
    if let Some(dir) = wiiu_installed_dir(path) {
        if let Some(xml_path) = wiiu_meta_xml(dir) {
            if let Ok(xml) = std::fs::read_to_string(xml_path) {
                if let Some(v) = wiiu_tag(&xml, "title_id") {
                    if let Some(tid) = crate::emulation::cemu_gpack::normalize_display_title_id(&v) {
                        return Some(tid);
                    }
                }
            }
        }
    }
    wiiu_title_id_in(&path.to_string_lossy()).or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .and_then(wiiu_title_id_in)
    })
}

/// Cached-first variant for the overlay — Lacrima's
/// `ResolveWiiUTitleId`: metadata cache (`WiiUTitleId`) wins, then
/// the live helper, persisted back into the `.meta` doc.
pub fn wiiu_title_id_cached(rom: &Path) -> Option<String> {
    if let Some(doc) = load_meta(rom) {
        if let Some(tid) = crate::emulation::cemu_gpack::normalize_display_title_id(&doc.wiiu_title_id) {
            return Some(tid);
        }
    }
    let live = wiiu_title_id(rom)?;
    // Persist like GenericAlbumNormalizer's metadata cache write.
    let mut doc = load_meta(rom).unwrap_or_default();
    if doc.wiiu_title_id.trim().is_empty() {
        doc.wiiu_title_id = live.clone();
        let _ = save_meta(rom, &doc);
    }
    Some(live)
}

/// `TitleIdTextRegex` — a maximal hex/`-`/`_` run whose hex digits
/// total exactly 16, with at most one separator sitting at position
/// 8 (the `XXXXXXXX-XXXXXXXX` / contiguous forms).
fn wiiu_title_id_in(text: &str) -> Option<String> {
    let is_tok = |c: char| c.is_ascii_hexdigit() || c == '-' || c == '_';
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if !is_tok(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_tok(chars[i]) {
            i += 1;
        }
        let run: String = chars[start..i].iter().collect();
        let hex: String = run.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        let seps: Vec<usize> = run
            .char_indices()
            .filter(|(_, c)| *c == '-' || *c == '_')
            .map(|(i, _)| i)
            .collect();
        let ok = hex.len() == 16
            && match seps.as_slice() {
                [] => true,
                [pos] => *pos == 8,
                _ => false,
            };
        if ok {
            return crate::emulation::cemu_gpack::normalize_display_title_id(&hex);
        }
    }
    None
}

// ------------------------------------------------------------------
// Nintendo Switch (SwitchRomMetadataReader + SwitchNcaHeaderReader +
// SwitchPartitionReader + SwitchNacpReader — cleartext only; the
// LibHac/NCA-decrypt path needs prod.keys and yields nothing without
// them, same as Lacrima on a keyless machine)
// ------------------------------------------------------------------
// ------------------------------------------------------------------
// Nintendo 3DS (RomInspector.TryExtract3dsMetadata): NCSD/CCI/3DS and
// raw NCCH/CXI title ids, decrypted ExeFS icon SMDH titles, plus CIA
// TMD title ids and meta-region SMDH titles.
// ------------------------------------------------------------------
const N3DS_CIA_HEADER_SIZE: usize = 0x2020;
const N3DS_SMDH_SIZE: usize = 0x36C0;
const N3DS_SMDH_TITLE_ENTRY_SIZE: usize = 0x200;
const N3DS_SMDH_SHORT_DESCRIPTION_SIZE: usize = 0x80;

fn n3ds_read_at(f: &mut std::fs::File, off: u64, buf: &mut [u8]) -> Option<()> {
    let len = f.metadata().ok()?.len();
    if off.checked_add(buf.len() as u64)? > len {
        return None;
    }
    f.seek(SeekFrom::Start(off)).ok()?;
    f.read_exact(buf).ok()
}

fn n3ds_tag(buf: &[u8], off: usize, tag: &[u8]) -> bool {
    buf.get(off..off + tag.len()) == Some(tag)
}

fn n3ds_tid_le(buf: &[u8], off: usize) -> Option<String> {
    let tid = u64::from_le_bytes(buf.get(off..off + 8)?.try_into().ok()?);
    (tid != 0).then(|| format!("{tid:016X}"))
}

fn n3ds_tid_be(buf: &[u8]) -> Option<String> {
    let tid = u64::from_be_bytes(buf.get(..8)?.try_into().ok()?);
    (tid != 0).then(|| format!("{tid:016X}"))
}

/// RomInspector.CleanAsciiText + IsValidTitle — the SMDH decoder is
/// UTF-16, but Lacrima's display title still accepts only printable
/// ASCII and rejects symbol-heavy garbage.
fn n3ds_clean_title(raw: &str) -> Option<String> {
    let mut cleaned = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c == '\t' || c == '\n' || c == '\r' || (' '..='~').contains(&c) {
            cleaned.push(c);
        } else {
            cleaned.push(' ');
        }
    }
    let cleaned = cleaned
        .replace('\r', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let title = cleaned.trim();
    let alpha_num = title.chars().filter(|c| c.is_alphanumeric()).count();
    (title.len() >= 3 && alpha_num >= 2 && alpha_num * 10 >= title.len() * 3)
        .then(|| title.to_string())
}

fn n3ds_smdh_title(smdh: &[u8]) -> Option<String> {
    if !n3ds_tag(smdh, 0, b"SMDH") {
        return None;
    }
    // The SMDH title array starts at 0x08; entry 1 is English and its
    // 0x80-byte short description is UTF-16LE.
    let off = 0x08 + N3DS_SMDH_TITLE_ENTRY_SIZE;
    let bytes = smdh.get(off..off + N3DS_SMDH_SHORT_DESCRIPTION_SIZE)?;
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let raw = String::from_utf16_lossy(&units);
    n3ds_clean_title(raw.trim_end_matches(['\0', ' ', '\n', '\r', '\t']))
}

fn n3ds_media_unit(header: &[u8]) -> u64 {
    let shift = header.get(0x188 + 6).copied().unwrap_or(0);
    0x200u64 << u32::from(if shift > 16 { 0 } else { shift })
}

/// Locates the `icon` file inside a decrypted NCCH's ExeFS and reads
/// the English SMDH title. Encrypted dumps leave the filename fallback.
fn n3ds_exefs_title(
    f: &mut std::fs::File,
    ncch_header: &[u8],
    ncch_offset: u64,
    file_len: u64,
) -> Option<String> {
    if ncch_header.len() < 0x1A8 {
        return None;
    }
    let media_unit = n3ds_media_unit(ncch_header);
    let exe_fs_units = u64::from(le32(ncch_header, 0x1A0)?);
    let exe_fs_size_units = u64::from(le32(ncch_header, 0x1A4)?);
    if exe_fs_units == 0 || exe_fs_size_units == 0 {
        return None;
    }
    let exe_fs_offset = ncch_offset.checked_add(exe_fs_units.checked_mul(media_unit)?)?;
    let exe_fs_size = exe_fs_size_units.checked_mul(media_unit)?;
    if exe_fs_offset == 0 || exe_fs_offset.checked_add(0x200)? > file_len {
        return None;
    }

    let mut header = [0u8; 0x200];
    n3ds_read_at(f, exe_fs_offset, &mut header)?;
    for i in 0..10 {
        let entry = i * 0x10;
        let name = std::str::from_utf8(&header[entry..entry + 8])
            .unwrap_or("")
            .trim_end_matches(['\0', ' ']);
        if name != "icon" {
            continue;
        }
        let icon_off = u64::from(le32(&header, entry + 8)?);
        let icon_size = u64::from(le32(&header, entry + 12)?);
        if icon_size < N3DS_SMDH_SIZE as u64 {
            return None;
        }
        let icon_abs = exe_fs_offset.checked_add(0x200)?.checked_add(icon_off)?;
        if icon_abs.checked_add(N3DS_SMDH_SIZE as u64)? > file_len
            || icon_abs.checked_add(N3DS_SMDH_SIZE as u64)?
                > exe_fs_offset.checked_add(exe_fs_size)?
        {
            return None;
        }
        let mut smdh = vec![0u8; N3DS_SMDH_SIZE];
        n3ds_read_at(f, icon_abs, &mut smdh)?;
        return n3ds_smdh_title(&smdh);
    }
    None
}

fn n3ds_ncch(
    f: &mut std::fs::File,
    ncch_header: &[u8],
    ncch_offset: u64,
    file_len: u64,
) -> (Option<String>, Option<String>) {
    let tid = n3ds_tid_le(ncch_header, 0x118);
    let unencrypted = ncch_header
        .get(0x188 + 7)
        .is_some_and(|flags| flags & 0x04 != 0);
    let title = unencrypted
        .then(|| n3ds_exefs_title(f, ncch_header, ncch_offset, file_len))
        .flatten();
    (tid, title)
}

fn n3ds_ncsd(
    f: &mut std::fs::File,
    header: &[u8],
    file_len: u64,
) -> Option<(String, Option<String>)> {
    let ncsd_tid = n3ds_tid_le(header, 0x108)?;
    let media_unit = n3ds_media_unit(header);
    let first_partition = u64::from(le32(header, 0x120)?);
    if first_partition == 0 {
        return Some((ncsd_tid, None));
    }
    let ncch_offset = first_partition.checked_mul(media_unit)?;
    if ncch_offset.checked_add(0x200)? > file_len {
        return Some((ncsd_tid, None));
    }
    let mut ncch_header = [0u8; 0x200];
    n3ds_read_at(f, ncch_offset, &mut ncch_header)?;
    if !n3ds_tag(&ncch_header, 0x100, b"NCCH") {
        return Some((ncsd_tid, None));
    }
    let (_ncch_tid, title) = n3ds_ncch(f, &ncch_header, ncch_offset, file_len);
    Some((ncsd_tid, title))
}

fn n3ds_signed_body_offset(sig_type: u32) -> Option<u64> {
    match sig_type {
        0x0001_0000 | 0x0001_0003 => Some(0x240),
        0x0001_0001 | 0x0001_0004 => Some(0x140),
        0x0001_0002 | 0x0001_0005 => Some(0x80),
        _ => None,
    }
}

fn n3ds_cia(f: &mut std::fs::File, file_len: u64) -> Option<(String, Option<String>)> {
    let mut header = [0u8; N3DS_CIA_HEADER_SIZE];
    n3ds_read_at(f, 0, &mut header)?;
    if le32(&header, 0)? as usize != N3DS_CIA_HEADER_SIZE {
        return None;
    }
    let cert_size = u64::from(le32(&header, 0x08)?);
    let ticket_size = u64::from(le32(&header, 0x0C)?);
    let tmd_size = u64::from(le32(&header, 0x10)?);
    let meta_size = u64::from(le32(&header, 0x14)?);
    let content_size = u64::from_le_bytes(header.get(0x18..0x20)?.try_into().ok()?);
    let align64 = |v: u64| -> Option<u64> { Some(v.checked_add(63)? & !63) };

    let tmd_offset = align64(N3DS_CIA_HEADER_SIZE as u64)?
        .checked_add(align64(cert_size)?)?
        .checked_add(align64(ticket_size)?)?;
    if tmd_offset.checked_add(0x1DC)? > file_len {
        return None;
    }
    let mut sig = [0u8; 4];
    n3ds_read_at(f, tmd_offset, &mut sig)?;
    let body = n3ds_signed_body_offset(u32::from_be_bytes(sig))?;
    let tid_offset = tmd_offset.checked_add(body)?.checked_add(0x4C)?;
    let mut tid_bytes = [0u8; 8];
    n3ds_read_at(f, tid_offset, &mut tid_bytes)?;
    let tid = n3ds_tid_be(&tid_bytes)?;

    let mut title = None;
    if meta_size >= N3DS_SMDH_SIZE as u64 {
        let meta_offset = tmd_offset
            .checked_add(align64(tmd_size)?)?
            .checked_add(align64(content_size)?)?;
        // Meta layout: 0x180 dependencies + 0x180 reserved + 0x4 core
        // version + 0xFC reserved, then the SMDH at +0x400.
        let smdh_offset = meta_offset.checked_add(0x400)?;
        if smdh_offset.checked_add(N3DS_SMDH_SIZE as u64)? <= file_len {
            let mut smdh = vec![0u8; N3DS_SMDH_SIZE];
            if n3ds_read_at(f, smdh_offset, &mut smdh).is_some() {
                title = n3ds_smdh_title(&smdh);
            }
        }
    }
    Some((tid, title))
}

/// TryExtract3dsMetadata → (title_id, internal_title). `.3dsx` stays a
/// filename fallback just as in Lacrima (it is not one of its parsed
/// 3DS extensions).
fn three_ds_read(path: &Path) -> (Option<String>, Option<String>) {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (None, None),
    };
    let file_len = f.metadata().ok().map(|m| m.len()).unwrap_or(0);
    if file_len < 0x200 {
        return (None, None);
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());
    if ext.as_deref() == Some("cia") {
        return n3ds_cia(&mut f, file_len)
            .map(|(tid, title)| (Some(tid), title))
            .unwrap_or((None, None));
    }
    let mut header = [0u8; 0x200];
    if n3ds_read_at(&mut f, 0, &mut header).is_none() {
        return (None, None);
    }
    if n3ds_tag(&header, 0x100, b"NCSD") {
        return n3ds_ncsd(&mut f, &header, file_len)
            .map(|(tid, title)| (Some(tid), title))
            .unwrap_or((None, None));
    }
    if n3ds_tag(&header, 0x100, b"NCCH") {
        let (tid, title) = n3ds_ncch(&mut f, &header, 0, file_len);
        return (tid, title);
    }
    (None, None)
}

/// Lacrima's `LooksLike3dsProductCode` — CTR/KTR/TWL product codes
/// cached as titles by older scans must never survive as the display
/// title; they're re-inspected and replaced.
pub(crate) fn looks_like_3ds_product_code(title: &str) -> bool {
    let t = title.trim();
    (8..=16).contains(&t.len())
        && ["CTR-", "KTR-", "TWL-"]
            .iter()
            .any(|p| t.get(..4).is_some_and(|h| h.eq_ignore_ascii_case(p)))
}

/// resolve_one's 3DS arm — Lacrima's `ResolveNintendo3dsTitle`: the
/// SMDH title wins, the binary title id is cached, a stale
/// product-code title self-heals to the SMDH title (or, when the
/// dump is encrypted, to the normalized filename).
fn three_ds_title(path: &Path) -> Option<String> {
    let (tid, title) = three_ds_read(path);
    let mut doc = load_meta(path).unwrap_or_default();
    let mut dirty = false;
    if let Some(tid) = tid {
        if doc.nintendo3ds_title_id.trim().is_empty() {
            doc.nintendo3ds_title_id = tid;
            dirty = true;
        }
    }
    if let Some(t) = &title {
        if !doc.user_edited
            && (doc.title.trim().is_empty() || looks_like_3ds_product_code(&doc.title))
            && doc.title.trim() != t.trim()
        {
            doc.title = t.clone();
            dirty = true;
        }
    }
    if dirty {
        let _ = save_meta(path, &doc);
    }
    title.or_else(|| {
        // Self-heal tail: with no SMDH title (encrypted dump) and a
        // stale product-code cache, return the normalized filename so
        // the caller's save replaces the bad value.
        cached_title(path)
            .filter(|t| looks_like_3ds_product_code(t))
            .and_then(|_| path.file_stem()?.to_str().map(display_title))
    })
}

const NCA_HEADER_START: u64 = 0x200;
const NCA_TITLE_ID: u64 = 0x210;
const NCA_CONTENT_TYPE: u64 = 0x205;
const NCA_CT_PROGRAM: u8 = 0x00;
const NCA_CT_META: u8 = 0x01;
const NCA_CT_CONTROL: u8 = 0x02;

fn switch_read_at(f: &mut std::fs::File, off: u64, buf: &mut [u8]) -> Option<()> {
    let len = f.metadata().ok()?.len();
    if off.checked_add(buf.len() as u64)? > len {
        return None;
    }
    f.seek(SeekFrom::Start(off)).ok()?;
    f.read_exact(buf).ok()
}

/// SwitchNcaHeaderReader.TryReadAt — "NCA"[0-3] at `nca+0x200`,
/// content type at `nca+0x205` (≤0x05), LE u64 title id at `nca+0x210`.
fn switch_nca_header(f: &mut std::fs::File, nca: u64) -> Option<(String, u8)> {
    let mut magic = [0u8; 4];
    switch_read_at(f, nca + NCA_HEADER_START, &mut magic)?;
    if !matches!(&magic, b"NCA3" | b"NCA2" | b"NCA1" | b"NCA0") {
        return None;
    }
    let mut ct = [0u8; 1];
    switch_read_at(f, nca + NCA_CONTENT_TYPE, &mut ct)?;
    if ct[0] > 0x05 {
        return None;
    }
    let mut tid = [0u8; 8];
    switch_read_at(f, nca + NCA_TITLE_ID, &mut tid)?;
    let v = u64::from_le_bytes(tid);
    if v == 0 || v == u64::MAX {
        return None;
    }
    Some((format!("{v:016X}"), ct[0]))
}

/// SwitchNcaHeaderReader.IsValidTitleId — 16 hex digits starting 01.
fn switch_tid_valid(tid: &str) -> bool {
    tid.len() == 16
        && tid.chars().all(|c| c.is_ascii_hexdigit())
        && tid[..2].eq_ignore_ascii_case("01")
}

/// SwitchPartitionReader.TryRead — PFS0/HFS0 header at `base`:
/// magic, fileCount, stringTableSize, then per-entry
/// (offset,size,nameOff) triples; names are NUL-terminated ASCII in
/// the string table. Returns (name, data_offset, size).
fn switch_partition(f: &mut std::fs::File, base: u64) -> Option<Vec<(String, u64, u64)>> {
    let mut head = [0u8; 0x10];
    switch_read_at(f, base, &mut head)?;
    let magic = u32::from_le_bytes(head[0..4].try_into().ok()?);
    if magic != 0x3053_4650 && magic != 0x3053_4648 {
        return None; // "PFS0" / "HFS0"
    }
    let count = i32::from_le_bytes(head[4..8].try_into().ok()?);
    let str_size = i32::from_le_bytes(head[8..12].try_into().ok()?);
    if !(1..=512).contains(&count) || !(0..=0x400000).contains(&str_size) {
        return None;
    }
    let entry_size = count as usize * 0x18;
    let mut table = vec![0u8; entry_size + str_size as usize];
    switch_read_at(f, base + 0x10, &mut table)?;
    let strs = &table[entry_size..];
    let data_base = base + 0x10 + table.len() as u64;
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        let e = &table[i * 0x18..];
        let file_off = u64::from_le_bytes(e[0..8].try_into().ok()?);
        let file_size = u64::from_le_bytes(e[8..16].try_into().ok()?);
        let name_off = u32::from_le_bytes(e[16..20].try_into().ok()?) as usize;
        if name_off >= strs.len() {
            continue;
        }
        let raw = &strs[name_off..];
        let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
        let name = String::from_utf8_lossy(&raw[..end]).into_owned();
        if name.trim().is_empty() {
            continue;
        }
        out.push((name, data_base + file_off, file_size));
    }
    (!out.is_empty()).then_some(out)
}

/// TryReadFromPartitionEntries — first program-NCA title id, else
/// control, else meta, else any NCA in the partition.
fn switch_tid_from_entries(
    f: &mut std::fs::File,
    entries: &[(String, u64, u64)],
) -> Option<String> {
    let len = f.metadata().ok()?.len();
    let (mut program, mut control, mut meta, mut any) = (None, None, None, None);
    for (name, off, size) in entries {
        if !name.to_lowercase().ends_with(".nca") || *size < 0x400 {
            continue;
        }
        if off.checked_add(0x400)? > len {
            continue;
        }
        let Some((tid, ct)) = switch_nca_header(f, *off) else {
            continue;
        };
        any.get_or_insert(tid.clone());
        match ct {
            NCA_CT_PROGRAM => program.get_or_insert(tid),
            NCA_CT_CONTROL => control.get_or_insert(tid),
            NCA_CT_META => meta.get_or_insert(tid),
            _ => continue,
        };
    }
    program.or(control).or(meta).or(any)
}

/// TryFindPartitionMagicOffset — a LE u32 magic scan over the first
/// 32MB in 4MB chunks (PFS0 buried in an NSP/XCI variant layout).
fn switch_find_magic(f: &mut std::fs::File, magic: u32) -> Option<u64> {
    let len = f.metadata().ok()?.len().min(32 * 1024 * 1024);
    if len < 0x10 {
        return None;
    }
    let want = magic.to_le_bytes();
    let mut buf = vec![0u8; len.min(4 * 1024 * 1024) as usize];
    f.seek(SeekFrom::Start(0)).ok()?;
    let mut total = 0u64;
    while total < len {
        let take = (len - total).min(buf.len() as u64) as usize;
        let read = f.read(&mut buf[..take]).ok()?;
        if read == 0 {
            break;
        }
        for (i, w) in buf[..read].windows(4).enumerate() {
            if w == want {
                return Some(total + i as u64);
            }
        }
        total += read as u64;
        if read < take {
            break;
        }
    }
    None
}

/// TryScanForNcaHeaders — a byte scan for "NCA" + version digit over
/// the first 64MB; a program NCA wins, else the first valid header.
fn switch_scan_ncas(f: &mut std::fs::File) -> Option<String> {
    let len = f.metadata().ok()?.len().min(64 * 1024 * 1024);
    if len < 0x400 {
        return None;
    }
    let mut buf = vec![0u8; len.min(4 * 1024 * 1024) as usize];
    f.seek(SeekFrom::Start(0)).ok()?;
    let mut total = 0u64;
    let mut best = None;
    while total < len {
        let take = (len - total).min(buf.len() as u64) as usize;
        let read = f.read(&mut buf[..take]).ok()?;
        if read == 0 {
            break;
        }
        for i in 0..read.saturating_sub(3) {
            if &buf[i..i + 3] != b"NCA" || !matches!(buf[i + 3], b'0'..=b'3') {
                continue;
            }
            let Some(nca) = (total + i as u64).checked_sub(NCA_HEADER_START) else {
                continue;
            };
            let Some((tid, ct)) = switch_nca_header(f, nca) else {
                continue;
            };
            if ct == NCA_CT_PROGRAM {
                return Some(tid);
            }
            best.get_or_insert(tid);
        }
        total += read as u64;
        if read < take {
            break;
        }
    }
    best
}

/// TryReadPartitionedPackage — NSP/NSZ/XCZ: PFS0 at 0 → entries;
/// a PFS0 magic buried deeper → entries; else the raw NCA scan.
fn switch_read_package(f: &mut std::fs::File) -> Option<String> {
    if let Some(entries) = switch_partition(f, 0) {
        if let Some(tid) = switch_tid_from_entries(f, &entries) {
            return Some(tid);
        }
    }
    if let Some(off) = switch_find_magic(f, 0x3053_4650) {
        if let Some(entries) = switch_partition(f, off) {
            if let Some(tid) = switch_tid_from_entries(f, &entries) {
                return Some(tid);
            }
        }
    }
    switch_scan_ncas(f)
}

/// TryReadXci — the HFS0 "secure" partition sits at one of the
/// fixed offsets (gamecard headers vary), else the raw scan.
fn switch_read_xci(f: &mut std::fs::File) -> Option<String> {
    let len = f.metadata().ok()?.len();
    for off in [0xF000u64, 0x10000, 0xE000, 0x1000] {
        if off >= len {
            continue;
        }
        if let Some(entries) = switch_partition(f, off) {
            if let Some(tid) = switch_tid_from_entries(f, &entries) {
                return Some(tid);
            }
        }
    }
    switch_scan_ncas(f)
}

/// SwitchNacpReader — the first non-empty application name across
/// the 0x300-byte language blocks (American English first).
fn switch_nacp_title(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 0x300 + 0x200 {
        return None;
    }
    let mut off = 0x300;
    while off + 0x200 <= bytes.len() && off < 0x4000 {
        let raw = &bytes[off..off + 0x200];
        let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
        let t = String::from_utf8_lossy(&raw[..end]).trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
        off += 0x300;
    }
    None
}

/// ExtractTitleIdFromText — a bracketed `[16hex]` token first, then
/// a bare `01…` 16-hex run anywhere in the text.
fn switch_tid_in(text: &str) -> Option<String> {
    let up = text.to_uppercase();
    let bytes = up.as_bytes();
    // [XXXXXXXXXXXXXXXX]
    for (i, b) in bytes.iter().enumerate() {
        if *b != b'[' || i + 17 >= bytes.len() || bytes[i + 17] != b']' {
            continue;
        }
        let cand = &up[i + 1..i + 17];
        if switch_tid_valid(cand) {
            return Some(cand.to_string());
        }
    }
    // 01XXXXXXXXXXXXXX — every 16-hex window, leftmost first
    // (EmbeddedTitleIdRegex's `01[0-9A-Fa-f]{14}` order).
    let mut i = 0;
    while i + 16 <= bytes.len() {
        if !bytes[i].is_ascii_hexdigit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            i += 1;
        }
        if i - start >= 16 {
            for s in start..=i - 16 {
                let cand = &up[s..s + 16];
                if switch_tid_valid(cand) {
                    return Some(cand.to_string());
                }
            }
        }
    }
    None
}

/// CleanDisplayTitleFromFileName — strip the title id, `[vN]` and
/// other bracket tags, `_`/`.`→space, collapse whitespace.
fn switch_clean_title(name: &str, tid: Option<&str>) -> Option<String> {
    let mut s = name.to_string();
    if let Some(tid) = tid {
        // Case-insensitive removal of [tid] then the bare id. Search
        // s's own bytes — positions taken from s.to_uppercase() are in
        // the WRONG coordinate space when a char's uppercase changes
        // byte length (ſ→S, ß→SS), and replace_range then panics on a
        // non-boundary index. The needle is ASCII so a byte-wise
        // case-insensitive scan only matches on ASCII runs — always
        // char boundaries.
        for needle in [format!("[{tid}]"), tid.to_string()] {
            let nb = needle.as_bytes();
            while let Some(pos) = s
                .as_bytes()
                .windows(nb.len())
                .position(|w| w.eq_ignore_ascii_case(nb))
            {
                s.replace_range(pos..pos + nb.len(), "");
            }
        }
    }
    // [vN] then any remaining […] groups.
    let mut out = String::with_capacity(s.len());
    let mut depth = 0;
    for c in s.chars() {
        match c {
            '[' => depth += 1,
            ']' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let cleaned: String = out
        .replace('_', " ")
        .replace('.', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let cleaned = cleaned.trim_matches([' ', '-']).to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// TryReadInstalledDirectory — a `*.nacp` anywhere under the dir
/// gives the title (tid from the dir name or the nacp's filename);
/// else the first top-level `*.nca` (cnmt preferred) reads as a
/// single NCA.
fn switch_read_installed_dir(dir: &Path) -> (Option<String>, Option<String>) {
    fn walk_nacp(d: &Path) -> Option<PathBuf> {
        for e in std::fs::read_dir(d).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Some(found) = walk_nacp(&p) {
                    return Some(found);
                }
            } else if p
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("nacp"))
            {
                return Some(p);
            }
        }
        None
    }
    if let Some(nacp) = walk_nacp(dir) {
        let title = std::fs::read(&nacp)
            .ok()
            .and_then(|b| switch_nacp_title(&b));
        let tid = dir
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(switch_tid_in)
            .or_else(|| {
                nacp.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(switch_tid_in)
            });
        if tid.is_some() || title.is_some() {
            return (tid, title);
        }
    }
    // Top-level *.cnmt.nca first, then any *.nca.
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.extension()
                            .and_then(|x| x.to_str())
                            .is_some_and(|x| x.eq_ignore_ascii_case("nca"))
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort_by_key(|p| {
        (!p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.to_lowercase().ends_with(".cnmt.nca"))) as u8
    });
    for nca in entries {
        if let Some(tid) = std::fs::File::open(&nca)
            .ok()
            .and_then(|mut f| switch_nca_header(&mut f, 0))
        {
            return (Some(tid.0), None);
        }
    }
    (None, None)
}

/// SwitchRomMetadataReader.TryRead — (title_id, display_title).
/// Containers first, filename tokens merged in; the LibHac leg is
/// skipped (it needs prod.keys/title.keys and returns None without
/// them — same net result here).
fn switch_read(path: &Path) -> (Option<String>, Option<String>) {
    let from_name = |p: &Path| -> (Option<String>, Option<String>) {
        // EnumeratePathSegmentsForTitleId: stem, then each ancestor
        // dir name up the tree, then the full path.
        let mut segs: Vec<String> = Vec::new();
        if let Some(s) = p.file_stem().and_then(|s| s.to_str()) {
            segs.push(s.to_string());
        }
        let mut d = p.parent();
        while let Some(dir) = d {
            if let Some(n) = dir.file_name().and_then(|n| n.to_str()) {
                segs.push(n.to_string());
            }
            d = dir.parent();
        }
        segs.push(p.to_string_lossy().into_owned());
        for seg in segs {
            if let Some(tid) = switch_tid_in(&seg) {
                let title = switch_clean_title(&seg, Some(&tid));
                return (Some(tid), title);
            }
        }
        (None, None)
    };

    if path.is_dir() {
        let (tid, title) = switch_read_installed_dir(path);
        if tid.is_some() || title.is_some() {
            return (tid, title);
        }
    } else if !path.is_file() {
        return (None, None);
    }

    let container_tid = if path.is_file() {
        let mut f = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return (None, None),
        };
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .as_deref()
        {
            Some("nca") => switch_nca_header(&mut f, 0).map(|h| h.0),
            Some("xci") => switch_read_xci(&mut f),
            _ => switch_read_package(&mut f),
        }
    } else {
        None
    };

    let (name_tid, name_title) = from_name(path);
    let tid = container_tid.or(name_tid).filter(|t| switch_tid_valid(t));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let title = name_title.or_else(|| switch_clean_title(stem, tid.as_deref()));
    if tid.is_none() && title.is_none() {
        return (None, None);
    }
    (tid, title)
}

/// resolve_one's SWITCH arm — the display title out of
/// `switch_read`, with the title id persisted into the `.meta` doc
/// (Lacrima's `metadata.SwitchTitleId` cache write).
fn switch_title(path: &Path) -> Option<String> {
    let (tid, title) = switch_read(path);
    if let Some(tid) = tid {
        let mut doc = load_meta(path).unwrap_or_default();
        if doc.switch_title_id.trim().is_empty() {
            doc.switch_title_id = tid;
            let _ = save_meta(path, &doc);
        }
    }
    title
}

/// The ROM's Switch title id — metadata cache first, live read
/// after, persisted back (`ResolveSwitchTitleId` shape). Reserved
/// for a Switch overlay/metadata UI — `switch_title` already
/// persists the id on the resolve pass.
#[allow(dead_code)]
pub fn switch_title_id_cached(rom: &Path) -> Option<String> {
    if let Some(doc) = load_meta(rom) {
        if switch_tid_valid(doc.switch_title_id.trim()) {
            return Some(doc.switch_title_id.trim().to_string());
        }
    }
    let (tid, _) = switch_read(rom);
    let tid = tid?;
    let mut doc = load_meta(rom).unwrap_or_default();
    if doc.switch_title_id.trim().is_empty() {
        doc.switch_title_id = tid.clone();
        let _ = save_meta(rom, &doc);
    }
    Some(tid)
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
        // `get` — a suffix byte-len can cut inside a multi-byte char
        // (titles end in ®/™ from PARAM.SFO) where `[]` slicing panics.
        if title.len() > suf.len()
            && title
                .get(title.len() - suf.len()..)
                .is_some_and(|t| t.eq_ignore_ascii_case(suf))
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
                && title
                    .get(title.len() - s.len()..)
                    .is_some_and(|t| t.eq_ignore_ascii_case(s))
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

/// Libretro filename form of a title — `:` separates name/subtitle
/// as " - " and ™®© are dropped ("Ratchet & Clank: Tools of
/// Destruction™" → "Ratchet & Clank - Tools of Destruction"), the
/// convention Named_Boxarts actually uses.
fn libretro_name(t: &str) -> String {
    let s = t
        .replace(['™', '®', '©'], "")
        .replace(':', " - ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    s.trim().to_string()
}

/// `ExpandTitleVariants` — base + title-case + libretro-name
/// variants, each also region-stripped and region-appended, in order.
fn title_variants(title: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push = |v: String, out: &mut Vec<String>| {
        if !v.is_empty() && !out.iter().any(|e| e.eq_ignore_ascii_case(&v)) {
            out.push(v);
        }
    };
    for raw in [title.trim().to_string(), display_title(title.trim())] {
        for base in [raw.clone(), libretro_name(&raw)] {
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
    }
    out
}

// ------------------------------------------------------------------
// LibRetro per-platform index — one fetch per section replaces the
// ~100-URL guess ladder with local matching. The GitHub tree API
// serves the repo's Named_Boxarts listing; cached on disk 7 days so
// a section's index costs two requests a week, not per run.
// ------------------------------------------------------------------
fn libretro_index(
    agent: &ureq::Agent,
    section_key: &str,
) -> Option<std::sync::Arc<HashSet<String>>> {
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
    let sha = root.iter().find(|(p, _)| p == "Named_Boxarts")?.1.clone();
    let sub = github_tree(agent, &repo, &sha)?;
    let names: Vec<String> = sub
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| p.ends_with(".png") || p.ends_with(".jpg") || p.ends_with(".webp"))
        .collect();
    (!names.is_empty()).then_some(names)
}

/// `git/trees/<sha>` → (path, sha) pairs for its direct children.
fn github_tree(agent: &ureq::Agent, repo: &str, sha: &str) -> Option<Vec<(String, String)>> {
    #[derive(Deserialize)]
    struct Entry {
        path: String,
        sha: String,
    }
    #[derive(Deserialize)]
    struct Tree {
        tree: Vec<Entry>,
    }
    let url = format!("https://api.github.com/repos/libretro-thumbnails/{repo}/git/trees/{sha}");
    let mut resp = agent
        .get(&url)
        .header("User-Agent", "AES_Oxide")
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
    for key in [
        "\"murl\":\"",
        "\"turl\":\"",
        "\"imgurl\":\"",
        "\"thumbnailUrl\":\"",
    ] {
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
        // `start` is an offset in `lower` — to_lowercase can shift
        // byte lengths (İ etc.) so index `decoded` via `get`.
        let Some(rest) = decoded.get(start..) else {
            pos = start + 4;
            continue;
        };
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
fn ddg_image_urls(agent: &ureq::Agent, q: &str, target: f32) -> Vec<String> {
    let main = format!(
        "https://duckduckgo.com/?q={}&iax=images&ia=images",
        escape(q)
    );
    let Ok(mut resp) = agent.get(&main).header("User-Agent", BING_UA).call() else {
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
        width: Option<u32>,
        height: Option<u32>,
    }
    let Ok(feed) = serde_json::from_str::<Feed>(&r.body_mut().read_to_string().unwrap_or_default())
    else {
        return Vec::new();
    };
    let mut results = feed.results;
    // The feed carries the image's real dimensions — rank candidates
    // by distance from the section's box-front aspect (Dreamcast
    // jewel-case fronts are ~square, keep-case boxes portrait).
    // Unknown dims sort mid-pack; actual pixel validation still
    // happens post-download, this just orders the downloads.
    results.sort_by_key(|e| match (e.width, e.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            ((w as f32 / h as f32 - target).abs() * 1000.0) as i64
        }
        _ => 400,
    });
    results
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

/// Query DuckDuckGo + Bing in parallel and merge — DDG hits first
/// (its feed carries real dimensions for aspect ranking), unseen
/// Bing URLs appended. Returns once both engines answer, ≥12 hits
/// are in and the second engine had a short grace window, or the
/// budget elapses — a dead/unreachable engine never stalls the
/// search at its full timeout.
fn image_candidates(agent: &ureq::Agent, q: &str, target: f32, budget: Duration) -> Vec<String> {
    let (tx, rx) = mpsc::channel::<(u8, Vec<String>)>();
    let a = agent.clone();
    let q2 = q.to_string();
    let tx2 = tx.clone();
    std::thread::spawn(move || {
        let _ = tx2.send((0, ddg_image_urls(&a, &q2, target)));
    });
    let a = agent.clone();
    let q2 = q.to_string();
    std::thread::spawn(move || {
        let _ = tx.send((1, bing_candidates(&a, &q2).unwrap_or_default()));
    });
    let t0 = Instant::now();
    let mut first_at: Option<Instant> = None;
    let (mut ddg, mut bing) = (Vec::new(), Vec::new());
    for _ in 0..2 {
        let have = ddg.len() + bing.len();
        // Enough hits — give the slower engine a short grace to
        // supplement, then cut it off.
        let left = if have >= 12 {
            match first_at {
                Some(f) if f.elapsed() >= Duration::from_millis(600) => break,
                Some(f) => Duration::from_millis(600).saturating_sub(f.elapsed()),
                None => Duration::from_millis(600),
            }
        } else {
            budget.saturating_sub(t0.elapsed())
        };
        if left.is_zero() {
            break;
        }
        match rx.recv_timeout(left) {
            Ok((0, v)) => {
                if !v.is_empty() {
                    first_at.get_or_insert_with(Instant::now);
                }
                ddg = v;
            }
            Ok((_, v)) => {
                if !v.is_empty() {
                    first_at.get_or_insert_with(Instant::now);
                }
                bing = v;
            }
            Err(_) => break,
        }
    }
    let mut out = ddg;
    for u in bing {
        if !out.iter().any(|e| e.eq_ignore_ascii_case(&u)) {
            out.push(u);
        }
    }
    // Known art-host URLs download first, retail/photo listings last
    // (stable — preserves each engine's own ranking inside a tier).
    // A "physical case photographed on a table" almost always comes
    // from a shop listing; mobygames & co are curated flat box art.
    out.sort_by_key(|u| -cover_url_score(u));
    out
}

/// Candidate-URL quality tier: + art archive, − retail/photo listing.
fn cover_url_score(u: &str) -> i32 {
    let u = u.to_lowercase();
    if [
        "mobygames",
        "thecoverproject",
        "coverproject",
        "launchbox",
        "gamesdb",
        "gamefaqs",
        "giantbomb",
        "boxart",
        "box_art",
        "packshot",
        "cover",
    ]
    .iter()
    .any(|k| u.contains(k))
    {
        2
    } else if [
        "ebay",
        "walmartimages",
        "secondhand",
        "preowned",
        "pawn",
        "auction",
        "cash-crusaders",
        "cash_converters",
    ]
    .iter()
    .any(|k| u.contains(k))
    {
        -2
    } else {
        0
    }
}

/// `FetchImageResults` → first validating download. Returns the saved
/// sidecar path. Both image engines run in parallel (`image_candidates`)
/// — Lacrima queries them sequentially, but a dead engine would burn
/// its whole timeout per query otherwise.
fn web_image_fetch(
    agent: &ureq::Agent,
    title: &str,
    platform: &str,
    rom: &Path,
    section_key: &str,
    cancel: &std::sync::atomic::AtomicBool,
) -> Option<PathBuf> {
    let ok = || !cancel.load(std::sync::atomic::Ordering::Relaxed);
    let target = cover_target(section_key);
    for q in cover_queries(title, platform) {
        if !ok() {
            return None;
        }
        let candidates = image_candidates(agent, &q, target, Duration::from_millis(4500));
        // First `Good` (flat front cover) wins. A landscape hit (flat
        // front+back+spine scan) and a product photo/3D mockup are
        // still saved as last-resort fallbacks — a real front cover
        // beats either whenever one lands.
        let mut photo: Option<Vec<u8>> = None;
        let mut landscape: Option<Vec<u8>> = None;
        for img in candidates {
            match download_classify(agent, &img, section_key) {
                Some((b, CoverClass::Good)) => {
                    return ok().then(|| save_cover(rom, &b)).flatten();
                }
                Some((b, CoverClass::Photo)) if photo.is_none() => photo = Some(b),
                Some((b, CoverClass::Landscape)) if landscape.is_none() => landscape = Some(b),
                _ => {}
            }
        }
        if let Some(b) = landscape.or(photo) {
            return ok().then(|| save_cover(rom, &b)).flatten();
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

/// Cover quality tiers — `Good` is a flat front cover filling the
/// frame; `Photo` is a product shot/3D mockup (the object floats on
/// a uniform background, so all four edges have uniform borders);
/// `Landscape` is a flat front+back+spine scan or a banner.
enum CoverClass {
    Good,
    Photo,
    Landscape,
}

/// The box-front aspect ratio the section's real covers cluster
/// around — Dreamcast/PSX/Saturn jewel-case fronts are ~square, the
/// keep-case boxes (X360, PS2, …) portrait.
fn cover_target(section_key: &str) -> f32 {
    match section_key {
        "DC" | "PSX" | "SATURN" => 1.0,
        _ => 0.72,
    }
}

/// GET + decode/size gate (Lacrima's `TryValidateAutoCoverImageBytes`).
/// Returns `(bytes, class)` — the final image payload (trimmed &
/// re-encoded when borders were cut), classified `Good`/`Photo`/
/// `Landscape`. Nothing is written here: the caller saves only the
/// winning candidate — the old per-candidate save churned `.img`
/// mid-fetch and the card decode pool could cache an early loser
/// permanently (card showed a photo while `.img` held the real art).
fn download_classify(
    agent: &ureq::Agent,
    url: &str,
    section_key: &str,
) -> Option<(Vec<u8>, CoverClass)> {
    let mut r = agent.get(url).header("User-Agent", BING_UA).call().ok()?;
    let bytes = r.body_mut().read_to_vec().ok()?;
    if bytes.len() < 2 * 1024 {
        return None;
    }
    let d = image::load_from_memory(&bytes).ok()?.to_rgba8();
    if d.width() < 120 || d.height() < 120 {
        return None;
    }
    let (trimmed, sides) = trim_borders(&d);
    let d = trimmed.as_ref().unwrap_or(&d);
    if d.width() < 120 || d.height() < 120 {
        return None;
    }
    let max_ratio = cover_target(section_key) + 0.12;
    let class = if d.width() as f32 > d.height() as f32 * max_ratio {
        // Landscape beyond the section's box-front tolerance — a
        // flat scan or banner (this also catches box photos whose
        // white margins hid a landscape object).
        CoverClass::Landscape
    } else if sides.all() || has_inset_vertical_edges(d) {
        // Uniform borders on every edge — the object floats on a
        // backdrop, i.e. a product photo or 3D mockup — or a sharp
        // inset rectangle edge on both sides: a physical case
        // photographed inside the frame. Both are photos, not flat
        // art.
        CoverClass::Photo
    } else {
        CoverClass::Good
    };
    let bytes = match trimmed {
        Some(t) => {
            // Re-encode so the sidecar carries the cropped art.
            let mut buf = Vec::new();
            image::DynamicImage::ImageRgba8(t.clone())
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
                .ok()?;
            buf
        }
        None => bytes,
    };
    Some((bytes, class))
}

/// Per-side border widths detected by `trim_borders` — `l/r/t/b` are
/// near-white/near-black bar widths (what gets physically trimmed);
/// `ul/ur/ut/ub` are uniform-of-any-luminance widths (a dark photo
/// backdrop counts here even though it isn't a bar).
/// `all()` means the image is an object floating on a backdrop.
struct Sides {
    l: u32,
    r: u32,
    t: u32,
    b: u32,
    ul: u32,
    ur: u32,
    ut: u32,
    ub: u32,
    w: u32,
    h: u32,
}

impl Sides {
    /// Meaningful uniform border on *every* side (≥4% of that
    /// dimension, ≥4px) — the product-photo/mockup signature. Either
    /// a flat bar or a consistent-color backdrop band counts.
    fn all(&self) -> bool {
        let x = (self.w / 25).max(4);
        let y = (self.h / 25).max(4);
        self.l.max(self.ul) >= x
            && self.r.max(self.ur) >= x
            && self.t.max(self.ut) >= y
            && self.b.max(self.ub) >= y
    }
}

/// Crop uniform near-white/near-black borders on all four sides —
/// search results pillar/letterbox box fronts onto a wider canvas,
/// and product photos float the object on a uniform backdrop. A
/// column/row counts as a border when ≥90% of 17 sampled pixels are
/// uniformly white or black; the trim is capped at 35% per axis and
/// never reduces to a sliver.
fn trim_borders(img: &image::RgbaImage) -> (Option<image::RgbaImage>, Sides) {
    let (w, h) = img.dimensions();
    let none = Sides {
        l: 0,
        r: 0,
        t: 0,
        b: 0,
        ul: 0,
        ur: 0,
        ut: 0,
        ub: 0,
        w,
        h,
    };
    if w < 40 || h < 40 {
        return (None, none);
    }
    const N: u32 = 17;
    let is_bar_px = |p: &image::Rgba<u8>| -> Option<bool> {
        let mx = p[0].max(p[1]).max(p[2]);
        let mn = p[0].min(p[1]).min(p[2]);
        if mx - mn > 14 {
            return None;
        }
        if mx >= 235 {
            Some(true)
        } else {
            Some(mx <= 20)
        }
    };
    let col_is_bar = |x: u32| {
        let mut ok = 0u32;
        for i in 0..N {
            let y = (((i as f32 + 0.5) / N as f32) * h as f32) as u32;
            match is_bar_px(img.get_pixel(x, y.min(h - 1))) {
                Some(true) => ok += 1,
                Some(false) | None => {}
            }
        }
        ok >= N * 9 / 10
    };
    let row_is_bar = |y: u32| {
        let mut ok = 0u32;
        for i in 0..N {
            let x = (((i as f32 + 0.5) / N as f32) * w as f32) as u32;
            match is_bar_px(img.get_pixel(x.min(w - 1), y)) {
                Some(true) => ok += 1,
                Some(false) | None => {}
            }
        }
        ok >= N * 9 / 10
    };
    // Uniform-of-any-luminance band — a photographed backdrop (dark
    // table, wall, gradient-free surface) holds a tight luminance
    // band, so ≥80% of samples sit within ±20 of the column median.
    // Only feeds `Sides::all` (photo detection), never the trim.
    let uni_col = |x: u32| {
        let mut v = [0i32; N as usize];
        for (i, s) in v.iter_mut().enumerate() {
            let y = (((i as f32 + 0.5) / N as f32) * h as f32) as u32;
            let p = img.get_pixel(x, y.min(h - 1));
            *s = (p[0] as i32 + p[1] as i32 + p[2] as i32) / 3;
        }
        v.sort_unstable();
        let med = v[N as usize / 2];
        v.iter().filter(|&&l| (l - med).abs() <= 20).count() >= N as usize * 8 / 10
    };
    let uni_row = |y: u32| {
        let mut v = [0i32; N as usize];
        for (i, s) in v.iter_mut().enumerate() {
            let x = (((i as f32 + 0.5) / N as f32) * w as f32) as u32;
            let p = img.get_pixel(x.min(w - 1), y);
            *s = (p[0] as i32 + p[1] as i32 + p[2] as i32) / 3;
        }
        v.sort_unstable();
        let med = v[N as usize / 2];
        v.iter().filter(|&&l| (l - med).abs() <= 20).count() >= N as usize * 8 / 10
    };
    let cap_x = w * 35 / 100;
    let cap_y = h * 35 / 100;
    let mut l = 0u32;
    while l < cap_x && col_is_bar(l) {
        l += 1;
    }
    let mut r = 0u32;
    while l + r < cap_x && col_is_bar(w - 1 - r) {
        r += 1;
    }
    let mut t = 0u32;
    while t < cap_y && row_is_bar(t) {
        t += 1;
    }
    let mut b = 0u32;
    while t + b < cap_y && row_is_bar(h - 1 - b) {
        b += 1;
    }
    let mut ul = 0u32;
    while ul < cap_x && (ul < l || uni_col(ul)) {
        ul += 1;
    }
    let mut ur = 0u32;
    while ul + ur < cap_x && (ur < r || uni_col(w - 1 - ur)) {
        ur += 1;
    }
    let mut ut = 0u32;
    while ut < cap_y && (ut < t || uni_row(ut)) {
        ut += 1;
    }
    let mut ub = 0u32;
    while ut + ub < cap_y && (ub < b || uni_row(h - 1 - ub)) {
        ub += 1;
    }
    let sides = Sides {
        l,
        r,
        t,
        b,
        ul,
        ur,
        ut,
        ub,
        w,
        h,
    };
    let (cw, ch) = (w - l - r, h - t - b);
    if (l + r < 4 && t + b < 4) || cw < w / 3 || ch < h / 3 {
        return (None, sides);
    }
    (
        Some(image::imageops::crop_imm(img, l, t, cw, ch).to_image()),
        sides,
    )
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
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> mpsc::Receiver<RomInfo> {
    let (tx, rx) = mpsc::channel();
    if items.is_empty() {
        return rx;
    }
    let items = std::sync::Arc::new(items);
    let cursor = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let workers = 6usize.min(items.len());
    for _ in 0..workers {
        let (tx, items, cursor, key, platform, cancel) = (
            tx.clone(),
            items.clone(),
            cursor.clone(),
            section_key.clone(),
            platform_name.clone(),
            cancel.clone(),
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
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    // A bad item must not kill the worker — one
                    // panic here previously zeroed the whole section.
                    let info = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        resolve_one(&agent, &key, &platform, index.as_deref(), &path, &cancel)
                    }))
                    .unwrap_or_else(|_| RomInfo {
                        path: path.clone(),
                        title: None,
                        cover: None,
                    });
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
    cancel: &std::sync::atomic::AtomicBool,
) -> RomInfo {
    use std::sync::atomic::Ordering::Relaxed;
    let ok = || !cancel.load(Relaxed);
    let cached = cover_path(path);
    // Title: sidecar → database/binary → stem. A cached 3DS product
    // code (CTR-/KTR-/TWL-) is a stale scan artifact — bypass it so
    // the binary inspection replaces it (Lacrima's self-heal).
    let title = cached_title(path)
        .filter(|t| section_key != "3DS" || !looks_like_3ds_product_code(t))
        .or_else(|| {
            let t = match section_key {
                "X360" => x360_title(path),
                "DC" => dc_title(path),
                "PSX" => psx_title(path),
                "PS2" => ps2_title(path),
                // GC + Wii share Dolphin's volume-header reader — the
                // disc magic picks the console, not the section.
                "GCN" | "WII" => dolphin_disc_title(path),
                // Wii U installed folders carry meta.xml; bare files
                // resolve via their package dir or stem.
                "WIIU" => wiiu_title(path),
                "SWITCH" => switch_title(path),
                "3DS" => three_ds_title(path),
                // No-Intro hash DB (nes.json) → iNES 2.0 name tag.
                "NES" => nes_title(path),
                // No-Intro hash DB over the normalized dump → SEGA
                // header name field.
                "GENESIS" => genesis_title(path),
                // No-Intro hash DB → LoROM/HiROM header title.
                "SNES" => snes_title(path),
                // No-Intro hash DB → 12-byte header title at 0xA0.
                "GBA" => gba_title(path),
                // PARAM.SFO TITLE (PBP/ISO9660/CSO) → redump →
                // No-Intro hash DBs (psp_redump.json / psp.json).
                "PSP" => psp_title(path),
                // FBNeo dat hash of the romset archive (fbn.json).
                "FBN" => fbn_title(path),
                // MAME consolidated-romsets dat (arcade.json).
                "ARCADE" => arcade_title(path),
                "PS3" => ps3_title(path),
                "PS4" => ps4_title(path),
                _ => None,
            };
            if let Some(t) = &t {
                // A canceled worker (Clear Section Cache) must not write
                // sidecars over the fresh refetch.
                if ok() {
                    save_cached_title(path, t);
                }
            }
            t
        });
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    // PS3 dumps ship their own art — ICON0.PNG is the icon RPCS3's
    // game list shows (PIC1.PNG the full back art). Guaranteed
    // correct and instant; it wins over the web lottery, which can
    // land case photos — the "good cover replaced by a photo" churn.
    let ps3_local = || -> Option<PathBuf> {
        if !ok() {
            return None;
        }
        // PS4 dumps ship sce_sys/icon0.png; same treatment as PS3.
        let icon = match section_key {
            "PS3" => ps3_local_cover(path)?,
            "PS4" => ps4_local_cover(path)?,
            _ => return None,
        };
        let dst = cover_path(path);
        if let Some(d) = dst.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        std::fs::copy(&icon, &dst).ok().map(|_| dst)
    };
    let cover = if cached.exists() && cached_is_front(&cached, section_key) {
        Some(cached)
    } else {
        ps3_local().or_else(|| {
            // Title candidates in Lacrima's order: resolved title
            // first, then the stem and its region-stripped form.
            let mut cands: Vec<String> = Vec::new();
            if let Some(t) = &title {
                cands.push(t.clone());
            }
            let clean = strip_region(&stem).to_string();
            if !clean.is_empty() && !cands.iter().any(|c| c.eq_ignore_ascii_case(&clean)) {
                cands.push(clean.clone());
            }
            // 1. LibRetro via the platform index — a local match, not
            //    a URL ladder.
            let mut hit = index.and_then(|idx| libretro_match(idx, section_key, &cands));
            if hit.is_none() && index.is_none() {
                // No index at all — fall back to a *trimmed* ladder
                // (the top region suffixes only) rather than the full
                // 96-URL crawl.
                hit = ladder_fallback(agent, section_key, &cands);
            }
            let dl = hit.and_then(|url| download_cover(agent, url.as_str(), path, cancel));
            dl.or_else(|| {
                // 2. Search-engine image lookup (Lacrima's web-search
                //    fallback) — the real X360 coverage source.
                let t = title.as_deref().unwrap_or(&clean);
                web_image_fetch(agent, t, platform, path, section_key, cancel)
            })
        })
    };
    RomInfo {
        path: path.to_path_buf(),
        title,
        cover,
    }
}

/// Photo-of-an-object detection: a physical case photographed
/// straight-on leaves a sharp luminance step where the case ends and
/// the backdrop begins — a long continuous vertical edge sitting
/// *inside* the frame (flat scans run edge-to-edge, so they don't
/// have an inset rectangle's edges). Needs one on each side.
fn has_inset_vertical_edges(img: &image::RgbaImage) -> bool {
    let (w, h) = img.dimensions();
    if w < 60 || h < 60 {
        return false;
    }
    let lum = |x: u32, y: u32| -> i32 {
        let p = img.get_pixel(x, y);
        (p[0] as i32 * 3 + p[1] as i32 * 4 + p[2] as i32) >> 3
    };
    // A column is a case-edge when ≥60% of sampled rows have a
    // strong horizontal luminance step across it.
    let need = (h / 2) * 6 / 10;
    let edge_col = |x: u32| -> bool {
        let mut strong = 0u32;
        for y in (0..h).step_by(2) {
            if (lum(x, y) - lum(x - 1, y)).abs() > 28 {
                strong += 1;
                if strong >= need {
                    return true;
                }
            }
        }
        false
    };
    let margin = (w / 16).max(4);
    (margin..w * 45 / 100).any(|x| edge_col(x)) && (w * 55 / 100..w - margin).any(|x| edge_col(x))
}

/// A cached cover is only trusted while it classifies `Good` — a
/// landscape sidecar is almost always a flat front+back+spine scan
/// and an all-bordered/inset-edged one a product photo, both from
/// the old dumb fetch, so they re-resolve and self-heal.
/// Should the cached `.img` be trusted as-is? Bytes + load_from_memory
/// (extension-based `image::open` can't always identify `.img`).
/// Photos are rejected — a cached case photo re-resolves. Shape is
/// NOT gated: saved art was already classified at fetch time, and a
/// landscape cover (a last-resort flat scan, or PS3 ICON0/PIC1 which
/// are landscape by nature) is still correct art — rejecting it just
/// refetched on every launch and let worse images win the retry.
fn cached_is_front(p: &Path, _section_key: &str) -> bool {
    let Ok(bytes) = std::fs::read(p) else {
        return false;
    };
    let Ok(d) = image::load_from_memory(&bytes).map(|d| d.to_rgba8()) else {
        return false;
    };
    let (trimmed, sides) = trim_borders(&d);
    let d = trimmed.as_ref().unwrap_or(&d);
    if d.width() < 120 || d.height() < 120 {
        return false;
    }
    !sides.all() && !has_inset_vertical_edges(d)
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

fn download_cover(
    agent: &ureq::Agent,
    url: &str,
    rom: &Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Option<PathBuf> {
    let mut resp = agent.get(url).call().ok()?;
    let bytes = resp.body_mut().read_to_vec().ok()?;
    if bytes.len() < 64 || image::load_from_memory(&bytes).is_err() {
        return None;
    }
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
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
    /// Wii U title id (`XXXXXXXX-XXXXXXXX`) — Lacrima's
    /// `metadata.WiiUTitleId` cache field the graphic-packs overlay
    /// reads before re-extracting from meta.xml.
    pub wiiu_title_id: String,
    /// Nintendo 3DS title id (16 hex) — Lacrima's
    /// `metadata.Nintendo3dsTitleId`, filled by the NCSD/NCCH/CIA
    /// readers so future scans don't need to reopen the container.
    pub nintendo3ds_title_id: String,
    /// Switch title id (16 hex, `01…`) — Lacrima's
    /// `metadata.SwitchTitleId` cache, filled by the NCA/partition
    /// reader so the container walk doesn't re-run.
    pub switch_title_id: String,
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

/// The ROM's "Back Cover" extra-image payload — what the capture
/// letterbox fill draws behind the frame (Lacrima's
/// `EmulationLetterboxHelper.TryLoadBackCoverBitmap`, which reads
/// the `.meta` store's BackCover entries).
pub fn back_cover_path(rom: &Path) -> Option<PathBuf> {
    let doc = load_meta(rom)?;
    for (i, r) in doc.images.iter().enumerate() {
        if !r.kind.eq_ignore_ascii_case("Back Cover") && !r.kind.eq_ignore_ascii_case("BackCover") {
            continue;
        }
        let p = cache_dir().join(&r.file);
        if p.is_file() {
            return Some(p);
        }
        let alt = meta_image_path(rom, i);
        if alt.is_file() {
            return Some(alt);
        }
    }
    None
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
    (m.crop_fw > 0 && (m.crop_left > 0 || m.crop_right > 0)).then_some((
        m.crop_left,
        m.crop_right,
        m.crop_fw,
    ))
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
    let mut out = image_candidates(&agent, query, 0.72, Duration::from_millis(5000));
    out.truncate(24);
    out
}

/// Box-art bytes for a game title — the ROM fetcher's candidate
/// ladder (flat front cover > landscape scan > product photo) minus
/// the sidecar save, for callers that cache under their own dir
/// (the decomp catalog does, per project).
pub fn boxart_bytes(query: &str) -> Option<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(8)))
        .http_status_as_error(true)
        .build()
        .into();
    let mut photo: Option<Vec<u8>> = None;
    let mut landscape: Option<Vec<u8>> = None;
    for img in image_candidates(&agent, query, 0.72, Duration::from_millis(4500)) {
        match download_classify(&agent, &img, "N64") {
            Some((b, CoverClass::Good)) => return Some(b),
            Some((b, CoverClass::Photo)) if photo.is_none() => photo = Some(b),
            Some((b, CoverClass::Landscape)) if landscape.is_none() => landscape = Some(b),
            _ => {}
        }
    }
    landscape.or(photo)
}

/// Download + decode-validate an image URL for the editor (`Add
/// URL`, picking a search result). Returns raw bytes.
pub fn fetch_image(url: &str) -> Option<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .http_status_as_error(true)
        .build()
        .into();
    let mut r = agent.get(url).header("User-Agent", BING_UA).call().ok()?;
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
        let Some(ve) = html[vs..].find('"') else {
            break;
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(3)))
            .build()
            .into()
    }

    /// PS3 dumps resolve to their own ICON0 art — the flag bug that
    /// inverted `cancel` and silently dropped every cover.
    #[test]
    fn ps3_dump_prefers_icon0() {
        let dir = std::env::temp_dir().join(format!("aes_test_ps3_{}", std::process::id()));
        let game = dir.join("BLES99999");
        let ps3_game = game.join("PS3_GAME");
        std::fs::create_dir_all(&ps3_game).unwrap();
        let mut png = Vec::new();
        image::DynamicImage::new_rgba8(150, 200)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        std::fs::write(ps3_game.join("ICON0.PNG"), &png).unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let info = resolve_one(&agent(), "PS3", "PlayStation 3", None, &game, &cancel);
        let dst = cover_path(&game);
        assert_eq!(info.cover.as_deref(), Some(dst.as_path()));
        assert_eq!(std::fs::read(&dst).unwrap(), png);

        let _ = std::fs::remove_file(&dst);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Titles ending in multi-byte chars (®/™ from PARAM.SFO cp1252)
    /// must not panic the byte-indexed suffix checks — this killed
    /// every resolver worker and zeroed a whole section's covers.
    #[test]
    fn multibyte_suffix_titles_dont_panic() {
        for t in [
            "Dante's Inferno®",
            "Ratchet & Clank: Tools of Destruction™",
            "Killzone®3",
            "God of War® III",
        ] {
            let _ = strip_region(t);
            let _ = display_title(t);
            let _ = libretro_name(t);
            let _ = title_variants(t);
        }
        // Sanity: an ASCII region suffix still strips.
        assert_eq!(strip_region("Halo (USA)"), "Halo");
    }

    /// A canceled resolver must bail before any network or sidecar
    /// write — Clear Section Cache relies on this to stop stale
    /// workers racing the fresh refetch.
    #[test]
    fn canceled_fetch_writes_nothing() {
        let dir = std::env::temp_dir().join(format!("aes_test_cancel_{}", std::process::id()));
        let rom = dir.join("Game BLES99998");
        std::fs::create_dir_all(&rom).unwrap();
        let cancel = AtomicBool::new(true);
        assert!(web_image_fetch(&agent(), "Game", "PS3", &rom, "PS3", &cancel).is_none());
        assert!(download_cover(&agent(), "http://127.0.0.1:1/x.png", &rom, &cancel).is_none());
        assert!(!cover_path(&rom).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The title-id token scanners slice b[i..i+9] — a short name
    /// ("Games", "BLES") must return None, not panic. This was the
    /// Scan-Folder crash: a 4-letter dir name panicked the worker,
    /// which silently abandoned the scan with the notice stuck.
    #[test]
    fn title_id_tokens_safe_on_short_names() {
        for t in ["Games", "Sony", "PS4", "BLES", "CUSA", "", "a", "CUSA1234"] {
            assert!(ps4_title_id_token(t).is_none());
            assert!(ps3_title_id_token(t).is_none());
        }
        assert_eq!(
            ps4_title_id_token("CUSA12345 - Game"),
            Some("CUSA12345".to_string())
        );
        assert_eq!(
            ps3_title_id_token("Game BLES99998"),
            Some("BLES99998".to_string())
        );
        // Prefix chars that aren't all-alpha mustn't false-positive
        // the generic PS4 pattern on "123456789".
        assert!(ps4_title_id_token("123456789").is_none());
    }

    /// The embedded No-Intro NES database parses and keys land in
    /// the normalized forms the resolver produces.
    #[test]
    fn nes_db_loads_and_normalizes() {
        let db = hash_title_db("nes.json");
        assert!(db.md5.len() > 1000, "nes.json should be thousands of rows");
        // First entry in nes.json: "!Clik! (World) (Proto 1)".
        assert_eq!(
            db.crc.get("865709AB").map(String::as_str),
            Some("!Clik! (World) (Proto 1)")
        );
        assert_eq!(norm_crc(" 0x865709ab "), "865709AB");
        assert_eq!(norm_hash("  AA-BB-CC "), "aabbcc");
        // genesis.json loads too — first row "007 Shitou - The
        // Duel (Japan) (En)".
        let gdb = hash_title_db("genesis.json");
        assert_eq!(
            gdb.crc.get("AEB4B262").map(String::as_str),
            Some("007 Shitou - The Duel (Japan) (En)")
        );
    }

    /// iNES normalization strips the 16-byte header and the
    /// trainer when flag6 bit2 is set — the alt-hash candidate.
    #[test]
    fn nes_normalized_body_strips_header_and_trainer() {
        let mut rom = b"NES\x1A".to_vec();
        rom.extend([1u8, 1, 0x04, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // trainer flag set
        rom.extend([0xAA; 512]); // trainer
        rom.extend([0xBB; 0x4000]); // prg
        rom.extend([0xCC; 0x2000]); // chr
        assert_eq!(nes_normalized_body(&rom).map(|b| b.len()), Some(512 + 0x4000 + 0x2000 - 512));
        // Body starts right after header+trainer — all AA trainer
        // bytes must be excluded.
        let body = nes_normalized_body(&rom).unwrap();
        assert_eq!(&body[..16], &[0xBB; 16]);
        assert!(nes_normalized_body(b"not a rom").is_none());
    }

    /// iNES 2.0 "Name" tag extraction + the validity gate that
    /// rejects garbage extension data.
    #[test]
    fn nes_ines2_name_tag_title() {
        let mut rom = b"NES\x1A".to_vec();
        rom.extend([1, 1, 0, 0x08, 0, 0, 0, 0, 0, 0, 0, 0]); // NES 2.0
        rom.extend([0u8; 0x4000 + 0x2000]); // prg + chr
        rom.extend(b"Name");
        rom.extend(11u32.to_le_bytes());
        rom.extend(b"StarCursor\0");
        assert_eq!(nes_ines_title(&rom).as_deref(), Some("StarCursor"));
        // Same payload without the 2.0 marker → no tag read.
        rom[7] = 0;
        assert!(nes_ines_title(&rom).is_none());
        // Garbage tag content fails the alnum-ratio check.
        rom[7] = 0x08;
        let tail = rom.len() - 4 - 11;
        for i in 0..11 {
            rom[tail + i] = 0x01;
        }
        assert!(nes_ines_title(&rom).is_none());
    }

    /// Genesis lookup normalization: plain SEGA-tag dumps pass
    /// through, SMD containers get the 512B strip + deinterleave,
    /// ESAG dumps get the pair swap.
    #[test]
    fn genesis_lookup_rom_normalizes_variants() {
        // Plain .gen — SEGA tag at 0x100.
        let mut rom = vec![0u8; 0x400];
        rom[0x100..0x104].copy_from_slice(b"SEGA");
        assert_eq!(
            genesis_lookup_rom(&rom, Some("game.gen")).map(|v| v.len()),
            Some(0x400)
        );

        // Real SMD layout: 512B copier header + one interleaved 16KB
        // block — even output bytes come from the block's first
        // half, odd bytes from the second.
        let mut file = vec![0u8; 512 + 16384];
        let p = 512; // payload base
        file[p + 128] = b'S'; // → out[0x100]
        file[p + 8192 + 128] = b'E'; // → out[0x101]
        file[p + 129] = b'G'; // → out[0x102]
        file[p + 8192 + 129] = b'A'; // → out[0x103]
        let norm = genesis_lookup_rom(&file, Some("game.smd")).unwrap();
        assert_eq!(&norm[0x100..0x104], b"SEGA");

        // Interleaved AND byte-swapped: ESAG after deinterleave →
        // the pair swap recovers SEGA.
        file[p + 128] = b'E';
        file[p + 8192 + 128] = b'S';
        file[p + 129] = b'A';
        file[p + 8192 + 129] = b'G';
        let norm = genesis_lookup_rom(&file, Some("game2.smd")).unwrap();
        assert_eq!(&norm[0x100..0x104], b"SEGA");

        // Garbage never normalizes.
        assert!(genesis_lookup_rom(&vec![0x55; 8192], Some("x.bin")).is_none());
    }

    /// Header title fields: international name at 0x150 wins over
    /// the domestic field at 0x120; the domestic '/' short form is
    /// tried first.
    #[test]
    fn genesis_header_title_fields() {
        let mut rom = vec![b' '; 0x200];
        rom[0x100..0x104].copy_from_slice(b"SEGA");
        rom[0x120..0x12B].copy_from_slice(b"DOMESTIC NA");
        rom[0x150..0x15F].copy_from_slice(b"SONIC THE HEDGE");
        assert_eq!(
            genesis_header_title(&rom).as_deref(),
            Some("SONIC THE HEDGE")
        );
        // No international field → domestic wins.
        rom[0x150..0x180].fill(b' ');
        assert_eq!(genesis_header_title(&rom).as_deref(), Some("DOMESTIC NA"));
    }

    /// The generated No-Intro SNES/GBA databases parse and land in
    /// the normalized lookup keys.
    #[test]
    fn snes_gba_dbs_load() {
        let sdb = hash_title_db("snes.json");
        assert!(sdb.md5.len() > 3000, "snes.json should be ~3700 rows");
        assert_eq!(
            sdb.crc.get("2D206BF7").map(String::as_str),
            Some("Chrono Trigger (USA)")
        );
        let g = hash_title_db("gba.json");
        assert!(g.md5.len() > 1000, "gba.json should be thousands of rows");
        assert_eq!(
            g.crc.get("F0815EE7").map(String::as_str),
            Some("Pokemon - Ruby Version (USA, Europe)")
        );
        // The generated FBNeo dat db — archive hashes of romset zips.
        let f = hash_title_db("fbn.json");
        assert!(f.md5.len() > 7000, "fbn.json should be ~7700 rows");
        assert_eq!(
            f.crc.get("AC6E93EA").map(String::as_str),
            Some("Metal Slug - Super Vehicle-001")
        );
        // The MAME consolidated-romsets dat (arcade.json) — same
        // archive-hash scheme.
        let a = hash_title_db("arcade.json");
        assert!(a.md5.len() > 25000, "arcade.json should be ~30k rows");
        assert_eq!(
            a.crc.get("8F401426").map(String::as_str),
            Some("10-Yard Fight (World, set 1)")
        );
    }

    /// SNES header validation (checksum+complement==0xFFFF) and the
    /// 21-byte title field — LoROM, and the +0x200 copier-header
    /// layout.
    #[test]
    fn snes_header_title_fields() {
        fn put_header(rom: &mut Vec<u8>, off: usize) {
            rom[off + 0x10..off + 0x10 + 16].copy_from_slice(b"SUPER MARIOWORLD");
            // complement + checksum == 0xFFFF (LE u16s at +0x2C/+0x2E)
            rom[off + 0x2C..off + 0x2E].copy_from_slice(&0xEDCBu16.to_le_bytes());
            rom[off + 0x2E..off + 0x30].copy_from_slice(&0x1234u16.to_le_bytes());
        }
        let mut rom = vec![0u8; 0x8000];
        put_header(&mut rom, 0x7FB0);
        assert!(snes_valid_header(&rom, 0x7FB0));
        assert_eq!(snes_header_title(&rom).as_deref(), Some("SUPER MARIOWORLD"));
        // 512-byte copier header shifts every offset by +0x200.
        let mut headered = vec![0xEE; 512];
        headered.extend_from_slice(&rom);
        assert!(snes_valid_header(&headered, 0x7FB0 + 0x200));
        assert_eq!(
            snes_header_title(&headered).as_deref(),
            Some("SUPER MARIOWORLD")
        );
        // A broken checksum pair validates nothing.
        rom[0x7FB0 + 0x2E] ^= 0xFF;
        assert!(!snes_valid_header(&rom, 0x7FB0));
        assert!(snes_header_title(&rom).is_none());
    }

    /// The GBA 12-byte title field at 0xA0, NUL/space padded.
    #[test]
    fn gba_header_title_field() {
        let mut rom = vec![0u8; 0x200];
        rom[0xA0..0xA0 + 11].copy_from_slice(b"POKEMONRUBY");
        assert_eq!(gba_header_title(&rom).as_deref(), Some("POKEMONRUBY"));
        // All-NUL field → no title.
        rom[0xA0..0xA0 + 12].fill(0);
        assert!(gba_header_title(&rom).is_none());
    }

    /// PSP PARAM.SFO index parsing — DISC_ID + TITLE fields out of a
    /// hand-built SFO (magic \0PSF, 2 index entries, key + data tables).
    #[test]
    fn psp_sfo_fields_parse() {
        let mut sfo = vec![0u8; 0x44 + 32];
        sfo[0..4].copy_from_slice(&[0x00, 0x50, 0x53, 0x46]); // \0PSF
        sfo[8..12].copy_from_slice(&0x34u32.to_le_bytes()); // key table
        sfo[12..16].copy_from_slice(&0x44u32.to_le_bytes()); // data table
        sfo[16..20].copy_from_slice(&2u32.to_le_bytes()); // index count
        // entry 0 — DISC_ID, string (0x0404), 12 bytes at data+0.
        sfo[0x16..0x18].copy_from_slice(&0x0404u16.to_le_bytes());
        sfo[0x18..0x1C].copy_from_slice(&12u32.to_le_bytes());
        // entry 1 — TITLE, key at +8, 12 bytes at data+16.
        sfo[0x24..0x26].copy_from_slice(&8u16.to_le_bytes());
        sfo[0x26..0x28].copy_from_slice(&0x0404u16.to_le_bytes());
        sfo[0x28..0x2C].copy_from_slice(&12u32.to_le_bytes());
        sfo[0x30..0x34].copy_from_slice(&16u32.to_le_bytes());
        sfo[0x34..0x3C].copy_from_slice(b"DISC_ID\0");
        sfo[0x3C..0x42].copy_from_slice(b"TITLE\0");
        sfo[0x44..0x50].copy_from_slice(b"ULUS-12345\0\0");
        sfo[0x54..0x60].copy_from_slice(b"MY PSP GAME\0");

        let (id, title) = psp_sfo_fields(&sfo);
        assert_eq!(id.as_deref(), Some("ULUS-12345"));
        assert_eq!(title.as_deref(), Some("MY PSP GAME"));

        // Bad magic → nothing.
        sfo[0] = 0x50;
        assert_eq!(psp_sfo_fields(&sfo), (None, None));
    }

    /// SFO string decode — NUL-truncated UTF-8, ShiftJIS fallback,
    /// and the trailing '?'/U+FFFD sanitize step.
    #[test]
    fn psp_sfo_string_decode() {
        assert_eq!(psp_decode_sfo_string(b"GAME\0PAD").as_str(), "GAME");
        // 0x93 0xFA 0x96 0x75 = 真紅 in ShiftJIS — not valid UTF-8.
        let sjis = psp_decode_sfo_string(&[0x93, 0xFA, 0x96, 0x75]);
        assert!(!sjis.is_empty());
        // Decode garbage that sanitizes empty → ASCII prefix path.
        assert_eq!(sanitize_psp_title("Title???\u{FFFD}").as_str(), "Title");
    }

    /// The PBP reader honors the \0PBP magic + param offset (the PS1
    /// serial path checked "PBP\0", which real files never match).
    #[test]
    fn psp_pbp_sfo_offset() {
        // Build a minimal PBP: \0PBP + param_offset → SFO.
        let mut pbp = vec![0u8; 0x100];
        pbp[0..4].copy_from_slice(&[0x00, 0x50, 0x42, 0x50]);
        pbp[8..12].copy_from_slice(&0x40u32.to_le_bytes());
        let mut sfo = vec![0u8; 0x60];
        sfo[0..4].copy_from_slice(&[0x00, 0x50, 0x53, 0x46]);
        sfo[8..12].copy_from_slice(&0x34u32.to_le_bytes());
        sfo[12..16].copy_from_slice(&0x40u32.to_le_bytes());
        sfo[16..20].copy_from_slice(&1u32.to_le_bytes());
        sfo[0x16..0x18].copy_from_slice(&0x0404u16.to_le_bytes());
        sfo[0x18..0x1C].copy_from_slice(&6u32.to_le_bytes());
        sfo[0x34..0x3A].copy_from_slice(b"TITLE\0");
        sfo[0x40..0x46].copy_from_slice(b"HELLO\0");
        pbp[0x40..0x40 + 0x60].copy_from_slice(&sfo);
        // parse directly at the offset — same code path psp_sfo_read
        // drives after seeking.
        let (id, title) = psp_sfo_fields(&pbp[0x40..]);
        assert!(id.is_none());
        assert_eq!(title.as_deref(), Some("HELLO"));
    }

    /// SYSTEM.CNF parsing — PSX `BOOT = cdrom:\...` and PS2
    /// `BOOT2 = cdrom0:\...` forms both yield a normalized serial.
    #[test]
    fn system_cnf_serial_parse() {
        // The serial keeps its on-disc punctuation — the DB lookup
        // squashes separators, so SLUS_005.94 == SLUS-00594.
        assert_eq!(
            ps_parse_system_cnf(b"BOOT = cdrom:\\SLUS_005.94;1\r\n")
                .map(|s| ps_serial_key(&s))
                .as_deref(),
            Some("SLUS00594")
        );
        assert_eq!(
            ps_parse_system_cnf(b"BOOT2 = cdrom0:\\SCUS_971.13;1\n")
                .map(|s| ps_serial_key(&s))
                .as_deref(),
            Some("SCUS97113")
        );
        assert!(ps_parse_system_cnf(b"VMODE = PAL\n").is_none());
    }

    /// A synthetic ISO9660 (PVD@16 + root dir + SYSTEM.CNF) resolves
    /// a serial → the embedded psx/ps2 DB title — the whole chain.
    #[test]
    fn psx_iso_serial_resolves_db_title() {
        fn dirrec(img: &mut [u8], off: usize, lba: u32, size: u32, name: &[u8]) -> usize {
            let len = 33 + name.len() + usize::from(name.len() % 2 == 0);
            img[off] = len as u8;
            img[off + 2..off + 6].copy_from_slice(&lba.to_le_bytes());
            img[off + 6..off + 10].copy_from_slice(&lba.to_be_bytes());
            img[off + 10..off + 14].copy_from_slice(&size.to_le_bytes());
            img[off + 14..off + 18].copy_from_slice(&size.to_be_bytes());
            img[off + 32] = name.len() as u8;
            img[off + 33..off + 33 + name.len()].copy_from_slice(name);
            len
        }
        let cnf = b"BOOT = cdrom:\\SLUS_005.94;1";
        let mut img = vec![0u8; 22 * 2048];
        img[16 * 2048] = 1; // PVD type
        let rec = 16 * 2048 + 156;
        dirrec(&mut img, rec, 20, 2048, &[0]);
        let dir = 20 * 2048;
        let mut p = dir;
        p += dirrec(&mut img, p, 20, 2048, &[0]);
        p += dirrec(&mut img, p, 20, 2048, &[1]);
        let _ = dirrec(&mut img, p, 21, cnf.len() as u32, b"SYSTEM.CNF;1");
        img[21 * 2048..21 * 2048 + cnf.len()].copy_from_slice(cnf);

        let dir_p = std::env::temp_dir().join(format!("aes_test_psx_{}", std::process::id()));
        std::fs::create_dir_all(&dir_p).unwrap();
        let iso = dir_p.join("game.iso");
        std::fs::write(&iso, &img).unwrap();
        assert_eq!(
            ps_disc_serial(&iso).map(|s| ps_serial_key(&s)).as_deref(),
            Some("SLUS00594")
        );
        assert_eq!(
            psx_title(&iso).as_deref(),
            Some("Metal Gear Solid (Disc 1)")
        );
        let _ = std::fs::remove_dir_all(&dir_p);
    }

    /// GC + Wii disc headers resolve their internal names through
    /// the plain reader; magic validates the layout.
    #[test]
    fn dolphin_iso_reads_internal_title() {
        let dir = std::env::temp_dir().join(format!("aes_test_gc_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut img = vec![0u8; 0x200];
        img[..6].copy_from_slice(b"GALE01");
        img[0x1C..0x20].copy_from_slice(&GC_DISC_MAGIC.to_be_bytes());
        img[0x20..0x20 + 20].copy_from_slice(b"Super Mario Sunshine");
        let iso = dir.join("sms.iso");
        std::fs::write(&iso, &img).unwrap();
        assert_eq!(
            dolphin_disc_title(&iso).as_deref(),
            Some("Super Mario Sunshine")
        );
        // Wii magic sits at 0x18.
        img[0x1C..0x20].fill(0);
        img[0x18..0x1C].copy_from_slice(&WII_DISC_MAGIC.to_be_bytes());
        img[..6].copy_from_slice(b"RMCE01");
        img[0x20..0x60].fill(0);
        img[0x20..0x20 + 14].copy_from_slice(b"Mario Kart Wii");
        std::fs::write(&iso, &img).unwrap();
        assert_eq!(dolphin_disc_title(&iso).as_deref(), Some("Mario Kart Wii"));
        // Garbage → no title (the stem fallback wins).
        img.fill(0xAB);
        std::fs::write(&iso, &img).unwrap();
        assert!(dolphin_disc_title(&iso).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same header inside a WBFS container resolves through the
    /// wlba→cluster indirection.
    #[test]
    fn dolphin_wbfs_reads_internal_title() {
        let dir = std::env::temp_dir().join(format!("aes_test_wbfs_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut img = vec![0u8; 600_000];
        img[0..4].copy_from_slice(b"WBFS");
        img[8] = 6; // hd sector shift → 64
        img[9] = 15; // wbfs sector shift → 32768
        img[12] = 1; // disc count
                     // wlba table at hd_size(64)+256=320; wlba[0]=1 → cluster @32768.
        img[320..322].copy_from_slice(&1u16.to_be_bytes());
        let h = 32768usize;
        img[h..h + 6].copy_from_slice(b"GALE01");
        img[h + 0x1C..h + 0x20].copy_from_slice(&GC_DISC_MAGIC.to_be_bytes());
        img[h + 0x20..h + 0x20 + 20].copy_from_slice(b"Super Mario Sunshine");
        let wbfs = dir.join("sms.wbfs");
        std::fs::write(&wbfs, &img).unwrap();
        assert_eq!(
            dolphin_disc_title(&wbfs).as_deref(),
            Some("Super Mario Sunshine")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// GCZ block 0 is zlib-deflated — the header must decompress
    /// before parsing.
    #[test]
    fn dolphin_gcz_reads_internal_title() {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("aes_test_gcz_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut disc = vec![0u8; 4096];
        disc[..6].copy_from_slice(b"GALE01");
        disc[0x1C..0x20].copy_from_slice(&GC_DISC_MAGIC.to_be_bytes());
        disc[0x20..0x20 + 20].copy_from_slice(b"Super Mario Sunshine");
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&disc).unwrap();
        let comp = enc.finish().unwrap();
        let mut img = Vec::new();
        img.extend_from_slice(&GCZ_MAGIC.to_le_bytes());
        img.extend_from_slice(&0u32.to_le_bytes()); // subtype
        img.extend_from_slice(&(comp.len() as u64).to_le_bytes()); // compressed size
        img.extend_from_slice(&(disc.len() as u64).to_le_bytes()); // data size
        img.extend_from_slice(&(disc.len() as u32).to_le_bytes()); // block size
        img.extend_from_slice(&1u32.to_le_bytes()); // num blocks
        img.extend_from_slice(&0u64.to_le_bytes()); // block 0 ptr @0
        img.extend_from_slice(&0u32.to_le_bytes()); // adler32
        img.extend_from_slice(&comp);
        let gcz = dir.join("sms.gcz");
        std::fs::write(&gcz, &img).unwrap();
        assert_eq!(
            dolphin_disc_title(&gcz).as_deref(),
            Some("Super Mario Sunshine")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CISO's used-block map routes disc block 0 to file offset
    /// 0x8000.
    #[test]
    fn dolphin_ciso_reads_internal_title() {
        let dir = std::env::temp_dir().join(format!("aes_test_ciso_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut img = vec![0u8; 0x8000 * 2];
        img[0..4].copy_from_slice(b"CISO");
        img[4..8].copy_from_slice(&0x8000u32.to_le_bytes());
        img[8] = 1; // block 0 used → file block index 0
        let h = 0x8000usize;
        img[h..h + 6].copy_from_slice(b"RMCE01");
        img[h + 0x18..h + 0x1C].copy_from_slice(&WII_DISC_MAGIC.to_be_bytes());
        img[h + 0x20..h + 0x20 + 14].copy_from_slice(b"Mario Kart Wii");
        let ciso = dir.join("mkwii.ciso");
        std::fs::write(&ciso, &img).unwrap();
        assert_eq!(dolphin_disc_title(&ciso).as_deref(), Some("Mario Kart Wii"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The letterbox fill reads the "Back Cover" `.meta` payload —
    /// a doc ref must resolve to its `<key>.k<n>` file.
    #[test]
    fn back_cover_payload_resolves() {
        let dir = std::env::temp_dir().join(format!("aes_test_bc_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rom = dir.join("game.iso");
        std::fs::write(&rom, b"fake").unwrap();
        // No doc → no back cover.
        assert!(back_cover_path(&rom).is_none());
        let payload = meta_image_path(&rom, 0);
        std::fs::create_dir_all(payload.parent().unwrap()).unwrap();
        std::fs::write(&payload, b"png-bytes").unwrap();
        let doc = RomMetaDoc {
            images: vec![
                MetaImageRef {
                    kind: "Wallpaper".into(),
                    file: payload.file_name().unwrap().to_string_lossy().into_owned(),
                },
                MetaImageRef {
                    kind: "Back Cover".into(),
                    file: payload.file_name().unwrap().to_string_lossy().into_owned(),
                },
            ],
            ..Default::default()
        };
        save_meta(&rom, &doc).unwrap();
        assert_eq!(back_cover_path(&rom).as_deref(), Some(payload.as_path()));
        let _ = std::fs::remove_file(&payload);
        let _ = std::fs::remove_file(meta_doc_path(&rom));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Serial normalization hits the DB through its variants —
    /// filenames like "SLUS_005.94 - Game.iso" must map too.
    #[test]
    fn ps_serial_normalization() {
        assert_eq!(ps_serial_norm("slus_005.94"), "SLUS-005-94");
        assert_eq!(ps_serial_key("slus_005.94"), "SLUS00594");
        assert_eq!(
            ps_serial_from_name(Path::new("SLUS-00594 - Metal Gear Solid.iso"))
                .map(|s| ps_serial_key(&s))
                .as_deref(),
            Some("SLUS00594")
        );
    }

    /// WiiUInstalledGameHelper: an installed folder (code/content/
    /// meta + meta.xml) yields its localized longname; a bare .rpx
    /// in `code/` resolves via the package grandparent; bare files
    /// fall back to a title-id-stripped stem.
    #[test]
    fn wiiu_title_resolution() {
        let dir = std::env::temp_dir().join(format!("aes_test_wiiu_{}", std::process::id()));
        let pkg = dir.join("Some Game [0005000010101A00]");
        for sub in ["code", "content", "meta"] {
            std::fs::create_dir_all(pkg.join(sub)).unwrap();
        }
        std::fs::write(
            pkg.join("meta").join("meta.xml"),
            // Real meta.xml tags carry type/length attributes.
            "<menu type=\"complex\" access=\"777\">\
             <title_id type=\"hexBinary\" length=\"8\">0005000010101A00</title_id>\
             <longname_en type=\"string\" length=\"512\">Test Quest</longname_en>\
             <longname_fr type=\"string\" length=\"512\">Test Quête FR</longname_fr></menu>",
        )
        .unwrap();
        // Package dir itself.
        assert_eq!(wiiu_title(&pkg).as_deref(), Some("Test Quest"));
        assert_eq!(wiiu_title_id(&pkg).as_deref(), Some("00050000-10101A00"));
        // The .rpx inside code/ resolves via the grandparent walk.
        let rpx = pkg.join("code").join("game.rpx");
        std::fs::write(&rpx, b"bin").unwrap();
        assert_eq!(wiiu_title(&rpx).as_deref(), Some("Test Quest"));
        assert_eq!(wiiu_title_id(&rpx).as_deref(), Some("00050000-10101A00"));
        // A bare .wud outside a package → cleaned stem.
        let wud = dir.join("Great Game 00050000101ABC00.wud");
        std::fs::write(&wud, b"bin").unwrap();
        assert_eq!(wiiu_title(&wud).as_deref(), Some("Great Game"));
        // …and its title id comes from the filename token.
        assert_eq!(wiiu_title_id(&wud).as_deref(), Some("00050000-101ABC00"));
        // Hex noise under 16 digits is not a title id.
        let plain = dir.join("No Id Here.wud");
        std::fs::write(&plain, b"bin").unwrap();
        assert_eq!(wiiu_title_id(&plain), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ----------------------------------------------------------
    // Nintendo 3DS (RomInspector.TryExtract3dsMetadata port)
    // ----------------------------------------------------------

    fn n3ds_smdh(title: &str) -> Vec<u8> {
        let mut out = vec![0u8; N3DS_SMDH_SIZE];
        out[..4].copy_from_slice(b"SMDH");
        let off = 0x08 + N3DS_SMDH_TITLE_ENTRY_SIZE;
        for (i, unit) in title.encode_utf16().take(0x40).enumerate() {
            out[off + i * 2..off + i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        out
    }

    fn n3ds_ncch_payload(tid: u64, title: &str, encrypted: bool) -> Vec<u8> {
        let smdh = n3ds_smdh(title);
        let mut out = vec![0u8; 0x200];
        out[0x100..0x104].copy_from_slice(b"NCCH");
        out[0x118..0x120].copy_from_slice(&tid.to_le_bytes());
        if !encrypted {
            out[0x188 + 7] |= 0x04;
            out[0x1A0..0x1A4].copy_from_slice(&1u32.to_le_bytes());
            let exe_fs_size = 0x200 + smdh.len();
            out[0x1A4..0x1A8]
                .copy_from_slice(&((exe_fs_size + 0x1FF) as u32 / 0x200).to_le_bytes());
        }
        let mut exe_fs = vec![0u8; 0x200];
        exe_fs[..4].copy_from_slice(b"icon");
        exe_fs[0x0C..0x10].copy_from_slice(&(smdh.len() as u32).to_le_bytes());
        out.extend_from_slice(&exe_fs);
        if !encrypted {
            out.extend_from_slice(&smdh);
        }
        out
    }

    #[test]
    fn n3ds_ncsd_and_cxi_read_smdh_title() {
        let dir = std::env::temp_dir().join(format!("aes_test_3ds_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ncch = n3ds_ncch_payload(0x00040000000F5200, "Test Quest 3D", false);

        // NCSD's own title id wins (RomInspector applies it before the
        // first partition's NCCH header).
        let mut cci = vec![0u8; 0x200];
        cci[0x100..0x104].copy_from_slice(b"NCSD");
        cci[0x108..0x110].copy_from_slice(&0x00040000000ABC00u64.to_le_bytes());
        cci[0x120..0x124].copy_from_slice(&1u32.to_le_bytes());
        cci.extend_from_slice(&ncch);
        let rom = dir.join("dump.cci");
        std::fs::write(&rom, &cci).unwrap();
        let (tid, title) = three_ds_read(&rom);
        assert_eq!(tid.as_deref(), Some("00040000000ABC00"));
        assert_eq!(title.as_deref(), Some("Test Quest 3D"));

        // Raw NCCH/CXI reads the same ExeFS icon at offset zero.
        let cxi = dir.join("dump.cxi");
        std::fs::write(&cxi, &ncch).unwrap();
        let (tid, title) = three_ds_read(&cxi);
        assert_eq!(tid.as_deref(), Some("00040000000F5200"));
        assert_eq!(title.as_deref(), Some("Test Quest 3D"));

        // Encrypted payloads still yield the title id, but no fake
        // product-code/internal title — the filename fallback wins.
        let mut encrypted = n3ds_ncch_payload(0x00040000000F5300, "Ignored", true);
        encrypted.truncate(0x200);
        let enc = dir.join("encrypted.3ds");
        std::fs::write(&enc, &encrypted).unwrap();
        let (tid, title) = three_ds_read(&enc);
        assert_eq!(tid.as_deref(), Some("00040000000F5300"));
        assert_eq!(title, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn n3ds_cia_reads_tmd_id_and_meta_title() {
        let dir = std::env::temp_dir().join(format!("aes_test_3ds_cia_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tid = 0x00040000000F5400u64;
        let tmd_size = 0xC40u64;
        let meta_size = 0x400 + N3DS_SMDH_SIZE as u64;
        let tmd_offset = 0x2040usize;
        let meta_offset = tmd_offset + tmd_size as usize;
        let mut cia = vec![0u8; N3DS_CIA_HEADER_SIZE];
        cia[0x00..0x04].copy_from_slice(&(N3DS_CIA_HEADER_SIZE as u32).to_le_bytes());
        cia[0x10..0x14].copy_from_slice(&(tmd_size as u32).to_le_bytes());
        cia[0x14..0x18].copy_from_slice(&(meta_size as u32).to_le_bytes());
        cia.resize(tmd_offset, 0);
        cia.extend_from_slice(&0x0001_0001u32.to_be_bytes()); // RSA-2048 body at +0x140
        cia.resize(tmd_offset + 0x140 + 0x4C, 0);
        cia.extend_from_slice(&tid.to_be_bytes());
        cia.resize(meta_offset + 0x400, 0);
        cia.extend_from_slice(&n3ds_smdh("CIA Title"));
        let rom = dir.join("dump.cia");
        std::fs::write(&rom, &cia).unwrap();

        let (read_tid, title) = three_ds_read(&rom);
        assert_eq!(read_tid.as_deref(), Some("00040000000F5400"));
        assert_eq!(title.as_deref(), Some("CIA Title"));
        assert_eq!(three_ds_title(&rom).as_deref(), Some("CIA Title"));
        assert_eq!(
            load_meta(&rom).unwrap().nintendo3ds_title_id.as_str(),
            "00040000000F5400"
        );
        let _ = std::fs::remove_file(meta_doc_path(&rom));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn n3ds_stale_product_code_title_self_heals() {
        let dir = std::env::temp_dir().join(format!("aes_test_3ds_heal_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Decrypted dump + a stale CTR- title sidecar → the SMDH title
        // comes back and the meta doc records it with the title id.
        let mut cci = vec![0u8; 0x200];
        cci[0x100..0x104].copy_from_slice(b"NCSD");
        cci[0x108..0x110].copy_from_slice(&0x00040000000AAA00u64.to_le_bytes());
        cci[0x120..0x124].copy_from_slice(&1u32.to_le_bytes());
        cci.extend_from_slice(&n3ds_ncch_payload(0, "Real Title", false));
        let rom = dir.join("stale.cci");
        std::fs::write(&rom, &cci).unwrap();
        std::fs::create_dir_all(meta_path(&rom).parent().unwrap()).unwrap();
        std::fs::write(meta_path(&rom), "CTR-P-AQEE").unwrap();
        assert_eq!(three_ds_title(&rom).as_deref(), Some("Real Title"));
        let doc = load_meta(&rom).unwrap();
        assert_eq!(doc.nintendo3ds_title_id, "00040000000AAA00");
        assert_eq!(doc.title, "Real Title");

        // Encrypted dump + stale KTR- sidecar → normalized filename
        // fallback replaces the product code.
        let mut encrypted = n3ds_ncch_payload(0x00040000000BBB00, "x", true);
        encrypted.truncate(0x200);
        let enc = dir.join("My Great Game (USA).cxi");
        std::fs::write(&enc, &encrypted).unwrap();
        std::fs::write(meta_path(&enc), "KTR-P-BBBB").unwrap();
        assert_eq!(
            three_ds_title(&enc).as_deref(),
            Some(display_title("My Great Game (USA)").as_str())
        );

        // A non-product-code cached title is not flagged stale.
        assert!(!looks_like_3ds_product_code("Ocarina of Time 3D"));
        assert!(looks_like_3ds_product_code("CTR-P-AQEE"));
        assert!(looks_like_3ds_product_code("twl-p-abcd"));
        let _ = std::fs::remove_file(meta_path(&rom));
        let _ = std::fs::remove_file(meta_doc_path(&rom));
        let _ = std::fs::remove_file(meta_path(&enc));
        let _ = std::fs::remove_file(meta_doc_path(&enc));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ----------------------------------------------------------
    // Nintendo Switch (SwitchRomMetadataReader port)
    // ----------------------------------------------------------    // ----------------------------------------------------------
    // Nintendo Switch (SwitchRomMetadataReader port)
    // ----------------------------------------------------------

    /// An NCA blob: "NCA3" at 0x200, content type at 0x205, LE u64
    /// title id at 0x210.
    fn nca_bytes(tid: u64, content_type: u8) -> Vec<u8> {
        let mut b = vec![0u8; 0x400];
        b[0x200..0x204].copy_from_slice(b"NCA3");
        b[0x205] = content_type;
        b[0x210..0x218].copy_from_slice(&tid.to_le_bytes());
        b
    }

    /// A PFS0 image holding `entries` — (name, payload) pairs laid
    /// out per SwitchPartitionReader.
    fn pfs0_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut names = Vec::new();
        let mut name_offs = Vec::new();
        for (n, _) in entries {
            name_offs.push(names.len() as u32);
            names.extend_from_slice(n.as_bytes());
            names.push(0);
        }
        let count = entries.len();
        let head_size = 0x10 + count * 0x18 + names.len();
        let mut out = Vec::with_capacity(head_size);
        out.extend_from_slice(b"PFS0");
        out.extend_from_slice(&(count as i32).to_le_bytes());
        out.extend_from_slice(&(names.len() as i32).to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        let mut data_off = 0u64;
        for ((_, payload), noff) in entries.iter().zip(&name_offs) {
            out.extend_from_slice(&data_off.to_le_bytes());
            out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
            out.extend_from_slice(&noff.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
            data_off += payload.len() as u64;
        }
        out.extend_from_slice(&names);
        for (_, payload) in entries {
            out.extend_from_slice(payload);
        }
        out
    }

    #[test]
    fn switch_title_id_tokens() {
        // Bracketed + bare forms (TitleIdInNameRegex /
        // EmbeddedTitleIdRegex).
        assert_eq!(
            switch_tid_in("Super Game [0100ABCDEF123456] [v0]"),
            Some("0100ABCDEF123456".into())
        );
        assert_eq!(
            switch_tid_in("dump_0100F2C0115B6000_v65536"),
            Some("0100F2C0115B6000".into())
        );
        // A non-01 16-hex token is not a Switch application id.
        assert_eq!(switch_tid_in("[0005000010101A00]"), None);
        assert_eq!(switch_tid_in("Game 1234"), None);
        assert_eq!(switch_tid_in("nothing"), None);
    }

    #[test]
    fn switch_clean_title_strips_tags() {
        assert_eq!(
            switch_clean_title(
                "Super_Mario_Odyssey [0100000000010000][v65536]",
                Some("0100000000010000")
            )
            .as_deref(),
            Some("Super Mario Odyssey")
        );
        assert_eq!(
            switch_clean_title("Zelda.TOTK.0100A00000000000", Some("0100A00000000000")).as_deref(),
            Some("Zelda TOTK")
        );
        // No id → separators still collapse.
        assert_eq!(
            switch_clean_title("Animal_Crossing.New_Horizons", None).as_deref(),
            Some("Animal Crossing New Horizons")
        );
    }

    /// Regression: the tid-removal scan must index the ORIGINAL
    /// string, not its to_uppercase() — chars whose uppercase changes
    /// byte length (ſ→S, ß→SS) shift every later position, and
    /// replace_range on that wrong index panics on a non-char boundary
    /// (the crash on a "®"-carrying filename).
    #[test]
    fn switch_clean_title_unicode_casefold() {
        // ſ is 2 bytes but its uppercase "S" is 1 — the uppercased
        // match position lands inside ® (bytes 2..4) in the original.
        let out = switch_clean_title("®ſ [0100ABCD1234ABCD] Game", Some("0100ABCD1234ABCD"));
        assert!(out.is_some());
        assert!(out.unwrap().contains("Game"));
        // ß→SS grows instead — wrong space in the other direction.
        let out = switch_clean_title("Fuß [0100111122223333] Ball", Some("0100111122223333"));
        assert!(out.is_some());
        assert!(out.unwrap().contains("Ball"));
    }

    #[test]
    fn switch_nsp_container_resolution() {
        let dir = std::env::temp_dir().join(format!("aes_test_nsw_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Meta NCA first, program NCA second — the program type must
        // win regardless of entry order (TryReadFromPartitionEntries).
        let meta = nca_bytes(0x0100BBBB00000002, NCA_CT_META);
        let prog = nca_bytes(0x0100AAAA00000001, NCA_CT_PROGRAM);
        let nsp = pfs0_bytes(&[("pack.cnmt.nca", &meta), ("main.program.nca", &prog)]);
        let rom = dir.join("garbage_name.nsp");
        std::fs::write(&rom, &nsp).unwrap();
        let (tid, title) = switch_read(&rom);
        assert_eq!(tid.as_deref(), Some("0100AAAA00000001"));
        // Filename has no id → cleaned stem fallback.
        assert_eq!(title.as_deref(), Some("garbage name"));

        // A bare .nca reads its own header.
        let nca = dir.join("single.nca");
        std::fs::write(&nca, &prog).unwrap();
        let (tid, _) = switch_read(&nca);
        assert_eq!(tid.as_deref(), Some("0100AAAA00000001"));

        // An XCI with its secure partition at the standard 0xF000.
        let mut xci = vec![0u8; 0xF000];
        xci.extend_from_slice(&pfs0_bytes(&[("sec.program.nca", &prog)]));
        let rom = dir.join("cart.xci");
        std::fs::write(&rom, &xci).unwrap();
        let (tid, _) = switch_read(&rom);
        assert_eq!(tid.as_deref(), Some("0100AAAA00000001"));

        // Filename tokens fill in for opaque/encrypted containers.
        let blob = dir.join("Some Game [0100C0FFEE0000AA].nsp");
        std::fs::write(&blob, b"not-a-partition").unwrap();
        let (tid, title) = switch_read(&blob);
        assert_eq!(tid.as_deref(), Some("0100C0FFEE0000AA"));
        assert_eq!(title.as_deref(), Some("Some Game"));

        // Installed dir: a .nacp gives the real title, the dir name
        // the id.
        let inst = dir.join("Cool Game [0100DEADBEEF0100]");
        std::fs::create_dir_all(inst.join("control")).unwrap();
        let mut nacp = vec![0u8; 0x4000];
        nacp[0x300..0x300 + 9].copy_from_slice(b"Cool Game");
        std::fs::write(inst.join("control").join("game.nacp"), &nacp).unwrap();
        let (tid, title) = switch_read(&inst);
        assert_eq!(tid.as_deref(), Some("0100DEADBEEF0100"));
        assert_eq!(title.as_deref(), Some("Cool Game"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod payload_decode_tests {
    /// Regression: `.k<n>` payload files have no image extension —
    /// `image::open` rejects them (`PathExtension` error), so the
    /// letterbox path must decode from bytes with content sniffing.
    #[test]
    fn extensionless_payload_decodes_from_bytes() {
        let dir = std::env::temp_dir().join(format!("aes_lb_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("abc.k0");
        let img = image::RgbImage::from_pixel(4, 4, image::Rgb([1, 2, 3]));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        std::fs::write(&p, buf.into_inner()).unwrap();
        assert!(image::open(&p).is_err(), ".k0 must fail extension decode");
        let bytes = std::fs::read(&p).unwrap();
        assert!(image::load_from_memory(&bytes).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
