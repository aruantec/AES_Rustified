//! shadPS4 per-game content — ports of Lacrima's
//! `ShadPs4PatchesService`, `ShadPs4CheatsService` and
//! `ShadPs4ContentDownloadService` (plus the handler's
//! `EnsureBackgroundInputEnabled` + portable-user-dir redirection).
//!
//! Layout (Windows portable mode — the launch redirects APPDATA and
//! friends at `<exe_dir>/user`):
//!   user/patches/<repo>/*.xml      patch docs; `files.json` maps
//!                                  file -> title ids for lookup
//!   user/cheats/<TID>_<ver>*.json  cheat docs per game version
//!   user/cheats_enabled/<f>.json   {enabled_mods:{name:bool}} state
//!
//! Patch XML carries one `<Metadata Name Note AppVer isEnabled>`
//! element per patch; enabled state is the `isEnabled` attribute,
//! edited in place so the rest of the document is untouched.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::emu_dl::{self, DlProgress, Shared};

/// Repositories Lacrima lists (ShadPs4ContentRepository.All).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Repo {
    ShadPs4,
    GoldHen,
}

impl Repo {
    pub fn id(self) -> &'static str {
        match self {
            Repo::ShadPs4 => "shadPS4",
            Repo::GoldHen => "GoldHEN",
        }
    }
    pub fn label(self) -> &'static str {
        self.id()
    }
    pub fn next(self) -> Repo {
        match self {
            Repo::ShadPs4 => Repo::GoldHen,
            Repo::GoldHen => Repo::ShadPs4,
        }
    }
}

/// Portable user dir: `<launcher dir>/user` (ResolvePortableUser-
/// Directory — SHADPS4_USER_DIR + APPDATA/LOCALAPPDATA/XDG_* all
/// point here so every shadPS4 store is self-contained).
pub fn user_dir(emu_dir: &Path) -> PathBuf {
    emu_dir.join("user")
}

fn patches_root(emu_dir: &Path) -> PathBuf {
    user_dir(emu_dir).join("patches")
}

fn cheats_dir(emu_dir: &Path) -> PathBuf {
    user_dir(emu_dir).join("cheats")
}

fn cheats_enabled_dir(emu_dir: &Path) -> PathBuf {
    user_dir(emu_dir).join("cheats_enabled")
}

// ======================================================================
// Patches
// ======================================================================

/// One `<Metadata>` patch row — `ShadPs4PatchEntry`.
#[derive(Debug, Clone)]
pub struct PatchEntry {
    pub name: String,
    pub note: String,
    pub app_ver: String,
    pub enabled: bool,
}

/// Lacrima's FindPatchFile — `user/patches/<repo>` searched in the
/// preferred order (shadPS4, GoldHEN, then any other dir); within a
/// repo `files.json` (filename -> [title ids]) wins, else every
/// `*.xml` is opened and its `<ID>` elements scanned.
pub fn find_patch_file(emu_dir: &Path, title_id: &str) -> Option<PathBuf> {
    let root = patches_root(emu_dir);
    let tid = title_id.trim().to_uppercase();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for pref in ["shadPS4", "shadps4", "GoldHEN"] {
        let d = root.join(pref);
        if d.is_dir() {
            dirs.push(d);
        }
    }
    if let Ok(rd) = std::fs::read_dir(&root) {
        let mut rest: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && !dirs.iter().any(|d| d == p))
            .collect();
        dirs.append(&mut rest);
    }
    for dir in dirs {
        if let Some(p) = find_in_repo(&dir, &tid) {
            return Some(p);
        }
    }
    None
}

fn find_in_repo(dir: &Path, tid: &str) -> Option<PathBuf> {
    let index = dir.join("files.json");
    if let Ok(text) = std::fs::read_to_string(&index) {
        if let Ok(map) = serde_json::from_str::<HashMap<String, Vec<String>>>(&text) {
            for (file, ids) in &map {
                if ids.iter().any(|id| id.trim().eq_ignore_ascii_case(tid)) {
                    let p = dir.join(file);
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }
    }
    let rd = std::fs::read_dir(dir).ok()?;
    let mut xmls: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("xml"))
        })
        .collect();
    xmls.sort();
    for p in xmls {
        if xml_has_title_id(&p, tid) {
            return Some(p);
        }
    }
    None
}

fn xml_has_title_id(path: &Path, tid: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    for (i, _) in text.match_indices("<ID") {
        // <ID>CUSA12345</ID> — also tolerates <ID ...>attrs.
        let after = &text[i + 3..];
        let Some(gt) = after.find('>') else { continue };
        let body = &after[gt + 1..];
        let Some(end) = body.find('<') else { continue };
        if body[..end].trim().eq_ignore_ascii_case(tid) {
            return true;
        }
    }
    // Legacy: a bare <TitleID>CUSA...</TitleID>.
    for (i, _) in text.match_indices("<TitleID") {
        let after = &text[i + 8..];
        let Some(gt) = after.find('>') else { continue };
        let body = &after[gt + 1..];
        let Some(end) = body.find('<') else { continue };
        if body[..end].trim().eq_ignore_ascii_case(tid) {
            return true;
        }
    }
    false
}

/// Parse `key="value"` attributes out of a tag body — enough for the
/// self-closing `<Metadata>` elements patch files use.
fn attrs(tag: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let b = tag.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        let start = i;
        while i < b.len() && b[i] != b'=' && !(b[i] as char).is_whitespace() && b[i] != b'>' {
            i += 1;
        }
        if i == start {
            break;
        }
        let key = &tag[start..i];
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            continue;
        }
        i += 1;
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= b.len() || (b[i] != b'"' && b[i] != b'\'') {
            continue;
        }
        let q = b[i];
        i += 1;
        let vstart = i;
        while i < b.len() && b[i] != q {
            i += 1;
        }
        out.insert(key.to_string(), xml_unescape(&tag[vstart..i]));
        i += 1;
    }
    out
}

fn xml_unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// ParseShadPs4PatchEntries — every `<Metadata>` element in document
/// order, `isEnabled` defaulting to false.
pub fn parse_patch_entries(text: &str) -> Vec<PatchEntry> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = text[from..].find("<Metadata") {
        let start = from + rel;
        let after = &text[start + 9..];
        // Self-closing or paired — find the tag's closing '>'.
        let mut end = after.len();
        for (i, _) in after.match_indices('>') {
            end = i;
            break;
        }
        let tag_end = start + 9 + end;
        let a = attrs(&text[start + 9..tag_end]);
        out.push(PatchEntry {
            name: a
                .get("Name")
                .cloned()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "Unnamed patch".into()),
            note: a.get("Note").cloned().unwrap_or_default(),
            app_ver: a.get("AppVer").cloned().unwrap_or_default(),
            enabled: a
                .get("isEnabled")
                .is_some_and(|v| v.eq_ignore_ascii_case("true")),
        });
        from = tag_end + 1;
    }
    out
}

/// BuildUpdatedShadPs4PatchDocument — set/remove `isEnabled` on the
/// `<Metadata>` tags, paired with `entries` by document order.
pub fn write_patch_doc(original: &str, entries: &[PatchEntry]) -> String {
    // Re-locate the tags so edits use fresh offsets (entries may have
    // been parsed from this same text, but play it safe).
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = original[from..].find("<Metadata") {
        let start = from + rel;
        let after = &original[start + 9..];
        if let Some(end) = after.find('>') {
            spans.push((start + 9, start + 9 + end));
            from = start + 9 + end + 1;
        } else {
            break;
        }
    }
    let mut out = original.to_string();
    // Apply from the back so earlier byte offsets stay valid.
    for (i, (s, e)) in spans.iter().enumerate().rev() {
        let Some(entry) = entries.get(i) else { break };
        let tag = out[*s..*e].to_string();
        let new_tag = set_enabled_attr(&tag, entry.enabled);
        out.replace_range(*s..*e, &new_tag);
    }
    out
}

/// Add `isEnabled="true"` or strip the attribute — matching
/// Lacrima's SetAttributeValue/Remove on the Metadata element.
fn set_enabled_attr(tag: &str, enabled: bool) -> String {
    // Drop any existing isEnabled="..." (single or double quoted).
    let mut t = tag.to_string();
    loop {
        let bytes = t.as_bytes();
        let mut found = None;
        let mut i = 0;
        while i + 9 < bytes.len() {
            if t[i..].starts_with("isEnabled") && t[i + 9..].trim_start().starts_with('=') {
                // Walk back over whitespace, forward over ="..."
                let attr_start = i;
                let mut j = i + 9;
                while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'=' {
                    j += 1;
                    while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                        j += 1;
                    }
                    if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
                        let q = bytes[j];
                        j += 1;
                        while j < bytes.len() && bytes[j] != q {
                            j += 1;
                        }
                        j = (j + 1).min(bytes.len());
                    }
                    found = Some((attr_start, j));
                    break;
                }
            }
            i += 1;
        }
        match found {
            Some((s, e)) => {
                // Eat one leading whitespace run too.
                let mut s2 = s;
                while s2 > 0 && bytes[s2 - 1] == b' ' {
                    s2 -= 1;
                }
                t.replace_range(s2..e, "");
            }
            None => break,
        }
    }
    if enabled {
        let trimmed = t.trim_end();
        let suffix = &t[trimmed.len()..]; // keep trailing `/` handled by caller span (no '>')
        format!("{trimmed} isEnabled=\"true\"{suffix}")
    } else {
        t
    }
}

// ======================================================================
// Cheats — user/cheats/<TID>_<ver>*.json + cheats_enabled/<f>.json
// ======================================================================

#[derive(Debug, Clone, serde::Deserialize)]
pub struct CheatDoc {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub mods: Vec<CheatMod>,
    #[serde(default)]
    pub credits: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct CheatMod {
    #[serde(default)]
    pub name: String,
    pub hint: Option<String>,
    #[serde(default = "default_cheat_type")]
    pub r#type: String,
}

fn default_cheat_type() -> String {
    "checkbox".into()
}

/// FindCheatFiles — `{TID}_{version}*.json`, plus the version with
/// its leading char dropped ("01.00" -> "1.00"); without a version
/// any `{TID}_*.json`.
pub fn find_cheat_files(emu_dir: &Path, title_id: &str, version: &str) -> Vec<PathBuf> {
    let dir = cheats_dir(emu_dir);
    let tid = title_id.trim().to_uppercase();
    let ver = version.trim();
    let mut out: Vec<PathBuf> = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    let vers: Vec<String> = {
        let mut v = Vec::new();
        if !ver.is_empty() {
            v.push(ver.to_string());
            if ver.len() > 1 {
                v.push(ver[1..].to_string());
            }
        }
        v
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        if p.extension()
            .and_then(|x| x.to_str())
            .is_none_or(|x| !x.eq_ignore_ascii_case("json"))
        {
            continue;
        }
        let Some(stem) = p.file_stem().and_then(|f| f.to_str()) else {
            continue;
        };
        let su = stem.to_uppercase();
        let Some(rest) = su.strip_prefix(&format!("{tid}_")) else {
            continue;
        };
        if ver.is_empty() || vers.iter().any(|v| rest.starts_with(&v.to_uppercase())) {
            out.push(p);
        }
    }
    out.sort();
    out
}

pub fn load_cheat_doc(path: &Path) -> Option<CheatDoc> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// cheats_enabled/<file>.json — `{enabled_mods:{name:bool}}`.
pub fn load_cheat_state(emu_dir: &Path, cheat_file: &Path) -> HashMap<String, bool> {
    let stem = cheat_file
        .file_stem()
        .and_then(|f| f.to_str())
        .unwrap_or_default();
    let p = cheats_enabled_dir(emu_dir).join(format!("{stem}.json"));
    let Ok(text) = std::fs::read_to_string(p) else {
        return HashMap::new();
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("enabled_mods")?.as_object().map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
                    .collect()
            })
        })
        .unwrap_or_default()
}

pub fn save_cheat_state(
    emu_dir: &Path,
    cheat_file: &Path,
    state: &HashMap<String, bool>,
) -> Result<(), String> {
    let stem = cheat_file
        .file_stem()
        .and_then(|f| f.to_str())
        .unwrap_or_default();
    let dir = cheats_enabled_dir(emu_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cheats dir not writable: {e}"))?;
    let doc = serde_json::json!({ "enabled_mods": state });
    std::fs::write(dir.join(format!("{stem}.json")), doc.to_string())
        .map_err(|e| format!("Failed to save cheat state: {e}"))
}

// ======================================================================
// Downloads — ShadPs4ContentDownloadService
// ======================================================================

fn http_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(300)))
            .build(),
    )
}

fn http_get(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, String> {
    let mut resp = agent
        .get(url)
        .header("User-Agent", emu_dl::USER_AGENT_PUBLIC)
        .header("Accept", "application/vnd.github.v3+json")
        .call()
        .map_err(|e| format!("{e}"))?;
    resp.body_mut().read_to_vec().map_err(|e| format!("{e}"))
}

/// DownloadPatchesAsync — list the repo's PATCHES dir through the
/// GitHub contents API, pull every *.xml, rebuild files.json.
fn download_patches_work(emu_dir: &Path, repo: Repo) -> Result<String, String> {
    let api = match repo {
        Repo::ShadPs4 => "https://api.github.com/repos/shadps4-emu/ps4_cheats/contents/PATCHES",
        Repo::GoldHen => {
            "https://api.github.com/repos/illusion0001/PS4-PS5-Game-Patch/contents/patches/xml"
        }
    };
    let dir = patches_root(emu_dir).join(repo.id());
    std::fs::create_dir_all(&dir).map_err(|e| format!("Patches dir not writable: {e}"))?;
    let agent = http_agent();
    let body = http_get(&agent, api).map_err(|e| format!("Failed to list patches: {e}"))?;
    let json: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| format!("Bad patch index: {e}"))?;
    let entries = json.as_array().cloned().unwrap_or_default();
    if entries.is_empty() {
        return Err(format!("No patch files found in {}.", repo.label()));
    }
    let mut files_json: HashMap<String, Vec<String>> = HashMap::new();
    let mut downloaded = 0usize;
    for entry in entries {
        let Some(name) = entry.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if !name.to_lowercase().ends_with(".xml") {
            continue;
        }
        let Some(url) = entry.get("download_url").and_then(|v| v.as_str()) else {
            continue;
        };
        let bytes = http_get(&agent, url).map_err(|e| format!("Failed to download {name}: {e}"))?;
        std::fs::write(dir.join(name), &bytes)
            .map_err(|e| format!("Failed to write {name}: {e}"))?;
        downloaded += 1;
    }
    // files.json — filename -> title ids found in the xml.
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension()
                .and_then(|x| x.to_str())
                .is_none_or(|x| !x.eq_ignore_ascii_case("xml"))
            {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            let mut ids: Vec<String> = Vec::new();
            for (i, _) in text.match_indices("<ID") {
                let after = &text[i + 3..];
                let Some(gt) = after.find('>') else { continue };
                let body = &after[gt + 1..];
                let Some(end) = body.find('<') else { continue };
                let id = body[..end].trim();
                if !id.is_empty() && !ids.iter().any(|x| x == id) {
                    ids.push(id.to_string());
                }
            }
            if !ids.is_empty() {
                if let Some(f) = p.file_name().and_then(|f| f.to_str()) {
                    files_json.insert(f.to_string(), ids);
                }
            }
        }
    }
    let _ = std::fs::write(
        dir.join("files.json"),
        serde_json::to_string(&files_json).unwrap_or_default(),
    );
    Ok(format!(
        "Downloaded {downloaded} patch file(s) from {}.",
        repo.label()
    ))
}

/// DownloadCheatsAsync — the repo's text manifest is regex-scanned
/// for `<TID>_<ver>*.json` entries; each is fetched and saved with a
/// `_GoldHEN`/`_shadPS4` suffix before the extension.
fn download_cheats_work(
    emu_dir: &Path,
    title_id: &str,
    version: &str,
    repo: Repo,
) -> Result<String, String> {
    let tid = title_id.trim().to_uppercase();
    if tid.is_empty() {
        return Err("Title ID is required to download cheats.".into());
    }
    let (manifest_url, base_url, suffix) = match repo {
        Repo::GoldHen => (
            "https://raw.githubusercontent.com/GoldHEN/GoldHEN_Cheat_Repository/main/json.txt",
            "https://raw.githubusercontent.com/GoldHEN/GoldHEN_Cheat_Repository/main/json/",
            "_GoldHEN",
        ),
        Repo::ShadPs4 => (
            "https://raw.githubusercontent.com/shadps4-emu/ps4_cheats/main/CHEATS_JSON.txt",
            "https://raw.githubusercontent.com/shadps4-emu/ps4_cheats/main/CHEATS/",
            "_shadPS4",
        ),
    };
    let agent = http_agent();
    let manifest = http_get(&agent, manifest_url)
        .map_err(|e| format!("Failed to download cheat index: {e}"))?;
    let manifest = String::from_utf8_lossy(&manifest);
    // `TID_<ver>*.json` — or any `TID_*.json` when no version known.
    let ver = version.trim();
    let mut names: Vec<String> = Vec::new();
    for tok in manifest.split(|c: char| c.is_whitespace() || c == '=' || c == '"') {
        let tok = tok.trim_matches(|c| c == '\'' || c == ',');
        let Some(file) = tok
            .rsplit('/')
            .next()
            .filter(|f| f.to_lowercase().ends_with(".json"))
        else {
            continue;
        };
        let fu = file.to_uppercase();
        let Some(rest) = fu.strip_prefix(&format!("{tid}_")) else {
            continue;
        };
        if !ver.is_empty() {
            let vu = ver.to_uppercase();
            let alt = if ver.len() > 1 {
                ver[1..].to_uppercase()
            } else {
                vu.clone()
            };
            if !(rest.starts_with(&vu) || rest.starts_with(&alt)) {
                continue;
            }
        }
        if !names.iter().any(|n| n == file) {
            names.push(file.to_string());
        }
    }
    if names.is_empty() {
        return Err(format!(
            "No cheats found for {tid} {} in {}.",
            ver,
            repo.label()
        ));
    }
    let dir = cheats_dir(emu_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cheats dir not writable: {e}"))?;
    let mut downloaded = 0usize;
    for name in &names {
        let local = name
            .strip_suffix(".json")
            .or_else(|| name.strip_suffix(".JSON"))
            .map(|stem| format!("{stem}{suffix}.json"))
            .unwrap_or_else(|| format!("{name}{suffix}"));
        let bytes = http_get(&agent, &format!("{base_url}{name}"))
            .map_err(|e| format!("Failed to download {name}: {e}"))?;
        std::fs::write(dir.join(&local), &bytes)
            .map_err(|e| format!("Failed to write {local}: {e}"))?;
        downloaded += 1;
    }
    Ok(format!(
        "Downloaded {downloaded} cheat file(s) from {}.",
        repo.label()
    ))
}

/// Shared-progress wrapper — the overlays poll `DlProgress`.
pub fn download_patches(emu_dir: PathBuf, repo: Repo) -> Shared {
    spawn_dl("Downloading shadPS4 patches…", move || {
        download_patches_work(&emu_dir, repo)
    })
}

pub fn download_cheats(emu_dir: PathBuf, title_id: String, version: String, repo: Repo) -> Shared {
    spawn_dl("Downloading cheats…", move || {
        download_cheats_work(&emu_dir, &title_id, &version, repo)
    })
}

fn spawn_dl(stage: &str, work: impl FnOnce() -> Result<String, String> + Send + 'static) -> Shared {
    let dl: Shared = Arc::new(Mutex::new(DlProgress {
        stage: stage.to_string(),
        ..Default::default()
    }));
    let shared = dl.clone();
    std::thread::spawn(move || {
        let res = work();
        shared.lock().unwrap_or_else(|e| e.into_inner()).done = Some(res);
    });
    dl
}

// ======================================================================
// Launch helpers
// ======================================================================

/// EnsureBackgroundInputEnabled — the captured window is hidden and
/// unfocused, so `user/config.json`'s `Input.background_controller_input`
/// is forced to `true` (config.toml gets the same for the old schema —
/// config.json wins when both exist, like Lacrima).
pub fn ensure_background_input(emu_dir: &Path) {
    let user = user_dir(emu_dir);
    let json = user.join("config.json");
    if json.is_file() {
        if let Ok(text) = std::fs::read_to_string(&json) {
            if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) {
                if !v.is_object() {
                    return;
                }
                let input = v
                    .as_object_mut()
                    .unwrap()
                    .entry("Input")
                    .or_insert_with(|| serde_json::json!({}));
                if !input.is_object() {
                    *input = serde_json::json!({});
                }
                input["background_controller_input"] = serde_json::json!(true);
                if let Ok(out) = serde_json::to_string_pretty(&v) {
                    if out != text {
                        let _ = std::fs::write(&json, out);
                    }
                }
            }
        }
        return;
    }
    let toml = user.join("config.toml");
    if toml.is_file() {
        if let Ok(text) = std::fs::read_to_string(&toml) {
            let mut changed = false;
            let out: Vec<String> = text
                .lines()
                .map(|l| {
                    if l.trim_start()
                        .to_lowercase()
                        .starts_with("background_controller_input")
                        && l.contains('=')
                    {
                        if l.trim() != "background_controller_input = true" {
                            changed = true;
                            return "background_controller_input = true".to_string();
                        }
                    }
                    l.to_string()
                })
                .collect();
            if changed {
                let _ = std::fs::write(&toml, out.join("\n"));
            }
        }
    }
}

/// ResolveShadPs4ExecutablePath — a launcher/qtlauncher selection
/// upgrades to the sibling `shadPS4.exe` (the core is what accepts
/// `-g`; the launcher would ignore it).
pub fn resolve_exe_path(configured: &Path) -> PathBuf {
    let name = configured
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_lowercase();
    if name == "shadps4" {
        return configured.to_path_buf();
    }
    if let Some(dir) = configured.parent() {
        for cand in ["shadPS4.exe", "shadps4.exe"] {
            let p = dir.join(cand);
            if p.is_file() {
                return p;
            }
        }
    }
    configured.to_path_buf()
}

/// ResolveShadPs4GamePath — a file passes through; a folder boots if
/// it has `sce_sys/param.sfo`/`eboot.bin`, else we descend into the
/// first subdir that does (nested dumps like `Game/CUSA12345/`).
pub fn resolve_game_path(rom: &Path) -> PathBuf {
    if rom.is_file() {
        return rom.to_path_buf();
    }
    if !rom.is_dir() {
        return rom.to_path_buf();
    }
    let bootable =
        |d: &Path| d.join("sce_sys").join("param.sfo").is_file() || d.join("eboot.bin").is_file();
    if bootable(rom) {
        return rom.to_path_buf();
    }
    // One level down (most dumps nest the CUSA folder once), then two.
    for depth in [false, true] {
        if let Some(found) = find_bootable(rom, depth, 0) {
            return found;
        }
    }
    rom.to_path_buf()
}

fn find_bootable(dir: &Path, deep: bool, level: u32) -> Option<PathBuf> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut nested = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if p.join("sce_sys").join("param.sfo").is_file() || p.join("eboot.bin").is_file() {
            return Some(p);
        }
        nested.push(p);
    }
    if deep && level < 3 {
        for n in nested {
            if let Some(f) = find_bootable(&n, deep, level + 1) {
                return Some(f);
            }
        }
    }
    None
}

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<Patches>
  <TitleID>
    <ID>CUSA12345</ID>
    <ID>CUSA99999</ID>
  </TitleID>
  <Patch Name="Top">
    <Metadata Name="60 FPS" Note="unlock" AppVer="01.00" isEnabled="true">
    </Metadata>
    <Metadata Name="Skip Intro" Note="" AppVer="01.00">
    </Metadata>
  </Patch>
</Patches>
"#;

    #[test]
    fn patch_entries_parse_and_roundtrip() {
        let entries = parse_patch_entries(PATCH_XML);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "60 FPS");
        assert!(entries[0].enabled);
        assert_eq!(entries[0].app_ver, "01.00");
        assert!(!entries[1].enabled);

        let mut flipped = entries.clone();
        flipped[0].enabled = false;
        flipped[1].enabled = true;
        let out = write_patch_doc(PATCH_XML, &flipped);
        let reread = parse_patch_entries(&out);
        assert!(!reread[0].enabled);
        assert!(reread[1].enabled);
        // Nothing else disturbed.
        assert!(out.contains("<ID>CUSA12345</ID>"));
        assert!(out.contains("Name=\"60 FPS\""));
    }

    #[test]
    fn xml_title_id_lookup() {
        let dir = std::env::temp_dir().join(format!("aes_ps4patch_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = dir.join("user").join("patches").join("shadPS4");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("g.xml"), PATCH_XML).unwrap();
        assert_eq!(find_patch_file(&dir, "cusa99999"), Some(repo.join("g.xml")));
        assert!(find_patch_file(&dir, "CUSA00000").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cheat_files_match_version_variants() {
        let dir = std::env::temp_dir().join(format!("aes_ps4cheat_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cdir = dir.join("user").join("cheats");
        std::fs::create_dir_all(&cdir).unwrap();
        std::fs::write(cdir.join("CUSA12345_01.00_GoldHEN.json"), "{}").unwrap();
        std::fs::write(cdir.join("CUSA12345_1.00_shadPS4.json"), "{}").unwrap();
        std::fs::write(cdir.join("CUSA12345_02.00.json"), "{}").unwrap();
        std::fs::write(cdir.join("CUSA99999_01.00.json"), "{}").unwrap();

        let any = find_cheat_files(&dir, "CUSA12345", "");
        assert_eq!(any.len(), 3);
        let v = find_cheat_files(&dir, "CUSA12345", "01.00");
        // "01.00" and its leading-char-dropped variant "1.00" match.
        assert_eq!(v.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
