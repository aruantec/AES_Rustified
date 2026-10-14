//! Xenia patches + custom-config service — the Rust port of Lacrima's
//! `XeniaPathsService` / `XeniaCustomConfigService` / `XeniaConfigSchema`
//! and the `EmulationViewModel.XeniaPatches` helpers.
//!
//! Layout mirrors xenia-canary's storage root:
//!   <root>/patches/<TITLEID> - <name>.patch.toml   — per-game patches
//!   <root>/xenia-canary.config.toml                — the active config
//!   <root>/xenia-canary.config.toml.default        — the baseline template
//!   <root>/custom_configs/<TITLEID>.json           — our per-game overrides
//!
//! Patches are edited by flipping `is_enabled` inside `[[patch]]`
//! blocks in place (formatting preserved). Config overrides are stored
//! as JSON and merged onto the template at launch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{emu_dl, emu_meta};

pub const ACTIVE_CONFIG: &str = "xenia-canary.config.toml";
pub const TEMPLATE_CONFIG: &str = "xenia-canary.config.toml.default";
pub const CUSTOM_CONFIGS: &str = "custom_configs";
pub const PATCHES_DIR: &str = "patches";

const TEMPLATE_URLS: [&str; 2] = [
    "https://raw.githubusercontent.com/xenia-canary/xenia-canary/canary/xenia-canary.config.toml",
    "https://raw.githubusercontent.com/xenia-canary/xenia-canary/master/xenia-canary.config.toml",
];

// ------------------------------------------------------------------
// Storage root (XeniaPathsService.ResolveStorageRoot, Windows order):
// the launcher's directory first, then the managed install dir; the
// first candidate carrying xenia storage markers wins.
// ------------------------------------------------------------------
pub fn storage_root(launcher: Option<&Path>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(l) = launcher.and_then(|p| p.parent()) {
        candidates.push(l.to_path_buf());
    }
    if let Some(d) = emu_dl::xenia_dir() {
        candidates.push(d);
    }
    candidates.dedup();
    candidates
        .iter()
        .find(|c| has_markers(c))
        .cloned()
        .or_else(|| candidates.into_iter().next())
}

fn has_markers(root: &Path) -> bool {
    let patches = root.join(PATCHES_DIR);
    if patches.is_dir() && patches.read_dir().is_ok_and(|mut r| r.next().is_some()) {
        return true;
    }
    if root.join(ACTIVE_CONFIG).is_file() || root.join(TEMPLATE_CONFIG).is_file() {
        return true;
    }
    let cc = root.join(CUSTOM_CONFIGS);
    if cc.is_dir() && cc.read_dir().is_ok_and(|mut r| r.next().is_some()) {
        return true;
    }
    root.join("portable.txt").is_file()
        || root.join("content").is_dir()
        || root.join("cache").is_dir()
}

// ------------------------------------------------------------------
// Per-title id: reuse the X360 metadata reader.
// ------------------------------------------------------------------
pub fn title_id(rom: &Path) -> Option<String> {
    emu_meta::x360_ids(rom).map(|(t, _)| t)
}

pub fn ids(rom: &Path) -> Option<(String, String)> {
    emu_meta::x360_ids(rom)
}

// ------------------------------------------------------------------
// Patches — FindXeniaPatchFiles / ParseXeniaPatchEntries /
// BuildUpdatedPatchDocument. Line-based (no regex): `[[patch]]`
// blocks in file order pair with the UI's toggle list in order.
// ------------------------------------------------------------------
pub struct PatchEntry {
    pub name: String,
    pub desc: String,
    pub enabled: bool,
}

pub fn find_patch_files(root: &Path, tid: &str) -> Vec<PathBuf> {
    let dir = root.join(PATCHES_DIR);
    let mut out = Vec::new();
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p
                .file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| {
                    f.len() >= tid.len() + ".patch.toml".len()
                        && f[..tid.len()].eq_ignore_ascii_case(tid)
                        && f.to_lowercase().ends_with(".patch.toml")
                })
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// `[[patch]]` blocks → entries. `[[patch.patch]]` sub-blocks are
/// skipped — they carry data, not toggles.
pub fn parse_patch_entries(text: &str) -> Vec<PatchEntry> {
    let mut out = Vec::new();
    let mut cur: Option<(PatchEntry, bool)> = None;
    for line in text.lines() {
        let t = line.trim();
        if t.eq_ignore_ascii_case("[[patch]]") {
            if let Some((mut e, _)) = cur.take() {
                if e.name.is_empty() {
                    e.name = "Unnamed patch".into();
                }
                out.push(e);
            }
            cur = Some((
                PatchEntry {
                    name: String::new(),
                    desc: String::new(),
                    enabled: false,
                },
                false,
            ));
            continue;
        }
        let Some((e, seen)) = cur.as_mut() else { continue };
        if let Some(v) = str_val(t, "name") {
            e.name = v;
        } else if let Some(v) = str_val(t, "desc") {
            e.desc = v;
        } else if !*seen {
            if let Some(rest) = t.strip_prefix("is_enabled") {
                *seen = true;
                e.enabled = rest
                    .trim_start_matches(|c: char| c == '=' || c.is_whitespace())
                    .eq_ignore_ascii_case("true");
            }
        }
    }
    if let Some((mut e, _)) = cur.take() {
        if e.name.is_empty() {
            e.name = "Unnamed patch".into();
        }
        out.push(e);
    }
    out
}

/// `key = "…"` string extraction for the patch header fields.
fn str_val(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Flip the FIRST `is_enabled` line of each `[[patch]]` block to the
/// matching entry's state — everything else (indent, comments,
/// addresses) untouched.
pub fn rewrite_patch_doc(text: &str, entries: &[PatchEntry]) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    let mut block = 0usize;
    let mut done = false;
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        if t.eq_ignore_ascii_case("[[patch]]") {
            block += 1;
            done = false;
            out.push_str(line);
            continue;
        }
        if block >= 1 && !done && t.len() >= 10 && t[..10].eq_ignore_ascii_case("is_enabled") {
            let Some(e) = entries.get(block - 1) else {
                out.push_str(line);
                continue;
            };
            let indent = &line[..line.len() - line.trim_start().len()];
            let nl = if line.ends_with('\n') { "\n" } else { "" };
            out.push_str(&format!("{indent}is_enabled = {}{nl}", e.enabled));
            done = true;
            continue;
        }
        out.push_str(line);
    }
    out
}

// ------------------------------------------------------------------
// TOML-lite — an ordered, format-preserving model good enough for
// xenia's flat `[Section]\nkey = value` config files. Values are read
// normalized (display strings) and written as raw TOML.
// ------------------------------------------------------------------
pub struct TomlDoc {
    lines: Vec<String>,
}

impl TomlDoc {
    pub fn parse(text: &str) -> Self {
        Self {
            lines: text.lines().map(str::to_string).collect(),
        }
    }

    /// Every `key = value` in the doc as `"Section.key"` → display
    /// value. Later duplicates win.
    pub fn values(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        let mut sec = String::new();
        for line in &self.lines {
            let t = line.trim();
            if let Some(s) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                if !s.starts_with('[') {
                    sec = s.trim().to_string();
                }
                continue;
            }
            if let Some((k, v)) = split_kv(t) {
                out.insert(format!("{sec}.{k}"), display_val(v));
            }
        }
        out
    }

    /// Set `section.key` to a raw TOML value — replaces the entry in
    /// place when present, else appends it to the section (creating
    /// the section at EOF when needed).
    pub fn set(&mut self, section: &str, key: &str, raw: &str) {
        let mut sec = String::new();
        let mut sec_last: Option<usize> = None;
        let mut found: Option<usize> = None;
        for (i, line) in self.lines.iter().enumerate() {
            let t = line.trim();
            if let Some(s) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                if !s.starts_with('[') {
                    sec = s.trim().to_string();
                }
                continue;
            }
            if sec.eq_ignore_ascii_case(section) {
                if let Some((k, _)) = split_kv(t) {
                    if k.eq_ignore_ascii_case(key) {
                        found = Some(i);
                        break;
                    }
                    sec_last = Some(i);
                }
            }
        }
        if let Some(i) = found {
            let indent = &self.lines[i][..self.lines[i].len() - self.lines[i].trim_start().len()];
            self.lines[i] = format!("{indent}{key} = {raw}");
        } else if let Some(i) = sec_last {
            self.lines.insert(i + 1, format!("{key} = {raw}"));
        } else {
            if !self.lines.last().is_some_and(|l| l.trim().is_empty()) {
                self.lines.push(String::new());
            }
            self.lines.push(format!("[{section}]"));
            self.lines.push(format!("{key} = {raw}"));
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n") + "\n"
    }
}

/// `key = value` split — value keeps quoting/comments for `get` to
/// normalize; keys are bare identifiers.
fn split_kv(t: &str) -> Option<(&str, &str)> {
    if t.is_empty() || t.starts_with('#') || t.starts_with('[') {
        return None;
    }
    let eq = t.find('=')?;
    let k = t[..eq].trim();
    if k.is_empty() || !k.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some((k, t[eq + 1..].trim()))
}

/// Raw TOML value → display string: strings unquoted, bare values cut
/// at a trailing comment.
fn display_val(raw: &str) -> String {
    let v = raw.trim();
    if let Some(rest) = v.strip_prefix('"') {
        let end = rest.find('"').unwrap_or(rest.len());
        return rest[..end].replace("\\\"", "\"").to_string();
    }
    v.split('#').next().unwrap_or("").trim().to_string()
}

/// Display string → raw TOML for a schema kind.
fn raw_val(kind: &Kind, v: &str) -> String {
    let v = v.trim();
    match kind {
        Kind::Bool => (v.eq_ignore_ascii_case("true")).to_string(),
        Kind::Int(..) | Kind::Gpu => v.parse::<i64>().unwrap_or(0).to_string(),
        Kind::Float(..) => v.parse::<f64>().map(|f| format!("{f}")).unwrap_or_else(|_| "0".into()),
        _ => format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"")),
    }
}

// ------------------------------------------------------------------
// Per-game override documents — custom_configs/<TITLEID>.json,
// `{"Overrides": {"Section": {"key": "value"}}}`.
// ------------------------------------------------------------------
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct OverridesDoc {
    #[serde(rename = "Overrides", default)]
    pub map: BTreeMap<String, BTreeMap<String, Option<String>>>,
}

pub fn json_config_path(root: &Path, tid: &str) -> PathBuf {
    root.join(CUSTOM_CONFIGS)
        .join(format!("{}.json", tid.to_uppercase()))
}

pub fn load_overrides(root: &Path, tid: &str) -> OverridesDoc {
    std::fs::read_to_string(json_config_path(root, tid))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_overrides(root: &Path, tid: &str, doc: &OverridesDoc) -> std::io::Result<()> {
    let path = json_config_path(root, tid);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(doc)?)
}

// ------------------------------------------------------------------
// Template / active config lifecycle.
// ------------------------------------------------------------------
pub fn template_path(root: &Path) -> PathBuf {
    root.join(TEMPLATE_CONFIG)
}
pub fn active_path(root: &Path) -> PathBuf {
    root.join(ACTIVE_CONFIG)
}

/// EnsureDefaultTemplate — template from the active config if present,
/// else downloaded from the xenia-canary repo, else a minimal builtin.
pub fn ensure_template(root: &Path) {
    let tpl = template_path(root);
    if tpl.is_file() {
        return;
    }
    let _ = std::fs::create_dir_all(root);
    let active = active_path(root);
    if active.is_file() && std::fs::copy(&active, &tpl).is_ok() {
        return;
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .build()
        .into();
    for url in TEMPLATE_URLS {
        if let Ok(mut resp) = agent.get(url).call() {
            if let Ok(text) = resp.body_mut().read_to_string() {
                if !text.trim().is_empty() && std::fs::write(&tpl, text).is_ok() {
                    return;
                }
            }
        }
    }
    let _ = std::fs::write(&tpl, MINIMAL_TOML);
}

const MINIMAL_TOML: &str = "[GPU]\ngpu = \"any\"\nvsync = true\nframerate_limit = 0\n\n\
     [Display]\nfullscreen = false\n\n[General]\napply_patches = true\n\n\
     [UI]\nwindow_size_x = 1280\nwindow_size_y = 720\n";

/// ReadMergedValues — schema keys → override or template value.
pub fn merged_values(root: &Path, doc: &OverridesDoc) -> BTreeMap<String, String> {
    let src = [template_path(root), active_path(root)]
        .into_iter()
        .find(|p| p.is_file())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| TomlDoc::parse(&t).values())
        .unwrap_or_default();
    let mut out = BTreeMap::new();
    for f in FIELDS {
        let comp = format!("{}.{}", f.section, f.key);
        if let Some(v) = doc
            .map
            .get(f.section)
            .and_then(|s| s.get(f.key))
            .and_then(|v| v.clone())
        {
            out.insert(comp, v);
        } else if let Some(v) = src.get(&comp) {
            out.insert(comp, v.clone());
        }
    }
    // draw_resolution_scale lives outside the schema fields.
    for key in [SCALE_X, SCALE_Y] {
        let comp = format!("GPU.{key}");
        if let Some(v) = doc
            .map
            .get("GPU")
            .and_then(|s| s.get(key))
            .and_then(|v| v.clone())
        {
            out.insert(comp, v);
        } else if let Some(v) = src.get(&comp) {
            out.insert(comp, v.clone());
        }
    }
    out
}

/// BuildOverridesFromValues — keep only fields whose current display
/// value differs from the template's.
pub fn build_overrides(
    current: &BTreeMap<String, String>,
    template: &BTreeMap<String, String>,
) -> OverridesDoc {
    let mut doc = OverridesDoc::default();
    let mut put = |comp: &str, v: &str, cur: &BTreeMap<String, String>| {
        let mut it = comp.splitn(2, '.');
        let (sec, key) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        doc.map
            .entry(sec.to_string())
            .or_default()
            .insert(key.to_string(), cur.get(comp).cloned().or(Some(v.to_string())));
    };
    for f in FIELDS {
        let comp = format!("{}.{}", f.section, f.key);
        let a = current.get(&comp).map(|v| norm(v)).unwrap_or_default();
        let b = template.get(&comp).map(|v| norm(v)).unwrap_or_default();
        if a != b {
            put(&comp, &a, current);
        }
    }
    // Resolution scale writes X and Y together when X differs.
    let cx = format!("GPU.{SCALE_X}");
    if current.get(&cx).map(|v| norm(v)) != template.get(&cx).map(|v| norm(v)) {
        put(&cx, "", current);
        put(&format!("GPU.{SCALE_Y}"), "", current);
    }
    doc
}

fn norm(v: &str) -> String {
    v.trim().to_string()
}

/// PrepareConfigForLaunch — copy the template over the active config,
/// then merge the game's overrides on top. Always runs before a boot
/// so Xenia starts from a known baseline.
pub fn prepare_for_launch(root: &Path, tid: Option<&str>) {
    ensure_template(root);
    let doc = tid.map(|t| load_overrides(root, t)).unwrap_or_default();
    apply_overrides(root, &doc);
}

pub fn apply_overrides(root: &Path, doc: &OverridesDoc) {
    ensure_template(root);
    let (tpl, active) = (template_path(root), active_path(root));
    if !tpl.is_file() {
        return;
    }
    if active.is_file() {
        let _ = std::fs::copy(&active, active.with_extension("toml.bak"));
    }
    if std::fs::copy(&tpl, &active).is_err() {
        return;
    }
    if doc.map.is_empty() {
        return;
    }
    let Ok(text) = std::fs::read_to_string(&active) else { return };
    let mut t = TomlDoc::parse(&text);
    for (sec, kv) in &doc.map {
        for (key, val) in kv {
            let raw = field(sec, key)
                .map(|f| raw_val(&f.kind, val.as_deref().unwrap_or("")))
                .unwrap_or_else(|| {
                    let v = val.as_deref().unwrap_or("");
                    if SCALE_KEYS.contains(&key.as_str()) {
                        v.parse::<i64>().unwrap_or(1).clamp(1, 7).to_string()
                    } else {
                        format!("\"{v}\"")
                    }
                });
            t.set(sec, key, &raw);
        }
    }
    let _ = std::fs::write(&active, t.text());
}

fn field(sec: &str, key: &str) -> Option<&'static Field> {
    FIELDS
        .iter()
        .find(|f| f.section.eq_ignore_ascii_case(sec) && f.key.eq_ignore_ascii_case(key))
}

// ------------------------------------------------------------------
// GPU adapter list (ShadPs4HardwareEnumeration — DXGI path): index 0
// is "Auto Select" (-1), then one entry per non-virtual adapter.
// ------------------------------------------------------------------
#[cfg(windows)]
pub fn gpu_adapters() -> Vec<(i64, String)> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    let mut out = vec![(-1i64, "Auto Select".to_string())];
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return out;
    };
    let mut idx = 0i64;
    let mut seen = std::collections::HashSet::new();
    for i in 0u32..16 {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else { continue };
        let name = String::from_utf16_lossy(&desc.Description)
            .trim_end_matches('\0')
            .trim()
            .to_string();
        if name.is_empty()
            || name.to_lowercase().contains("microsoft basic render")
            || name.to_lowercase().contains("remote display")
            || !seen.insert(name.to_lowercase())
        {
            continue;
        }
        out.push((idx, name));
        idx += 1;
    }
    out
}

#[cfg(not(windows))]
pub fn gpu_adapters() -> Vec<(i64, String)> {
    vec![(-1, "Auto Select".into())]
}

// ------------------------------------------------------------------
// The schema (XeniaConfigSchema.AllFields) — drives the overlay's
// section list and field widgets.
// ------------------------------------------------------------------
pub const SCALE_X: &str = "draw_resolution_scale_x";
pub const SCALE_Y: &str = "draw_resolution_scale_y";
const SCALE_KEYS: [&str; 2] = [SCALE_X, SCALE_Y];

#[derive(Clone)]
pub enum Kind {
    Bool,
    Int(i64, i64),
    Float(f64, f64),
    /// (labels, stored values) — identical arrays when values==labels.
    Choice(&'static [&'static str], &'static [&'static str]),
    Gpu,
}

pub struct Field {
    pub section: &'static str,
    pub key: &'static str,
    pub label: &'static str,
    pub desc: &'static str,
    pub kind: Kind,
}

const fn b(sec: &'static str, key: &'static str, label: &'static str) -> Field {
    Field {
        section: sec,
        key,
        label,
        desc: "",
        kind: Kind::Bool,
    }
}
const fn i(sec: &'static str, key: &'static str, label: &'static str, lo: i64, hi: i64) -> Field {
    Field {
        section: sec,
        key,
        label,
        desc: "",
        kind: Kind::Int(lo, hi),
    }
}
const fn f(
    sec: &'static str,
    key: &'static str,
    label: &'static str,
    lo: f64,
    hi: f64,
) -> Field {
    Field {
        section: sec,
        key,
        label,
        desc: "",
        kind: Kind::Float(lo, hi),
    }
}
const fn c(
    sec: &'static str,
    key: &'static str,
    label: &'static str,
    opts: &'static [&'static str],
) -> Field {
    Field {
        section: sec,
        key,
        label,
        desc: "",
        kind: Kind::Choice(opts, opts),
    }
}

const POST_SCALE_L: &[&str] = &["(default)", "bilinear", "cas", "fsr"];
const POST_SCALE_V: &[&str] = &["", "bilinear", "cas", "fsr"];
const POST_AA_L: &[&str] = &["(none)", "fxaa", "fxaa_extreme"];
const POST_AA_V: &[&str] = &["", "fxaa", "fxaa_extreme"];
const ANISO_L: &[&str] = &[
    "-1 (No override)",
    "0 (Disabled)",
    "1 (1x)",
    "2 (2x)",
    "3 (4x)",
    "4 (8x)",
    "5 (16x)",
];
const ANISO_V: &[&str] = &["-1", "0", "1", "2", "3", "4", "5"];

pub static FIELDS: &[Field] = &[
    c("APU", "apu", "Audio backend", &["sdl", "alsa", "any", "nop", "xaudio2"]),
    i("APU", "apu_max_queued_frames", "Max queued audio frames", 4, 64),
    b("APU", "mute", "Mute audio"),
    b("APU", "enable_xmp", "Enable XMP playback"),
    b("APU", "use_dedicated_xma_thread", "Dedicated XMA thread"),
    c("APU", "xma_decoder", "XMA decoder", &["fake", "master", "old", "new"]),
    b("APU", "ffmpeg_verbose", "Verbose FFmpeg"),
    b("CPU", "disable_prefetch_and_cachecontrol", "Disable prefetch/cache flush translation"),
    b("CPU", "disable_context_promotion", "Disable context promotion"),
    b("CPU", "ignore_trap_instructions", "Ignore trap instructions"),
    b("CPU", "clock_no_scaling", "Disable clock scaling"),
    b("CPU", "inline_mmio_access", "Inline MMIO access"),
    Field { section: "D3D12", key: "d3d12_adapter", label: "D3D12 adapter", desc: "Index: -1 any, -2 WARP.", kind: Kind::Gpu },
    b("D3D12", "d3d12_bindless", "Bindless resources"),
    b("D3D12", "d3d12_debug", "D3D12 debug layer"),
    i("D3D12", "d3d12_queue_priority", "Command queue priority", 0, 2),
    i("D3D12", "d3d12_pipeline_creation_threads", "Pipeline creation threads", -1, 128),
    b("D3D12", "d3d12_allow_variable_refresh_rate_and_tearing", "VRR and tearing"),
    b("D3D12", "d3d12_submit_on_primary_buffer_end", "Submit on primary buffer end"),
    b("Display", "fullscreen", "Start fullscreen"),
    Field { section: "Display", key: "postprocess_scaling_and_sharpening", label: "Scaling / sharpening", desc: "", kind: Kind::Choice(POST_SCALE_L, POST_SCALE_V) },
    Field { section: "Display", key: "postprocess_antialiasing", label: "Post-process AA", desc: "", kind: Kind::Choice(POST_AA_L, POST_AA_V) },
    f("Display", "postprocess_ffx_cas_additional_sharpness", "CAS additional sharpness", 0.0, 1.0),
    f("Display", "postprocess_ffx_fsr_sharpness_reduction", "FSR sharpness reduction", 0.0, 2.0),
    i("Display", "postprocess_ffx_fsr_max_upsampling_passes", "FSR max upsampling passes", 1, 8),
    b("Display", "postprocess_dither", "Dither to 8-bit"),
    b("Display", "present_letterbox", "Letterbox aspect ratio"),
    i("Display", "present_safe_area_x", "Safe area width %", 0, 100),
    i("Display", "present_safe_area_y", "Safe area height %", 0, 100),
    c("GPU", "gpu", "Graphics backend", &["any", "d3d12", "vulkan", "null"]),
    b("GPU", "vsync", "VSync"),
    i("GPU", "framerate_limit", "Framerate limit", 0, 240),
    Field { section: "GPU", key: "anisotropic_override", label: "Anisotropic filtering", desc: "", kind: Kind::Choice(ANISO_L, ANISO_V) },
    b("GPU", "async_shader_compilation", "Async shader compilation"),
    b("GPU", "clear_memory_page_state", "Clear memory page state"),
    b("GPU", "depth_float24_convert_in_pixel_shader", "Depth float24 in PS"),
    b("GPU", "depth_float24_round", "Depth float24 round"),
    c("GPU", "readback_resolve", "Readback resolve", &["none", "fast", "full"]),
    c("GPU", "render_target_path_d3d12", "D3D12 render target path", &["any", "rtv", "rov"]),
    c("GPU", "render_target_path_vulkan", "Vulkan render target path", &["any", "fbo", "fsi"]),
    b("GPU", "store_shaders", "Persist shaders"),
    b("GPU", "half_pixel_offset", "Half-pixel offset"),
    i("GPU", "texture_cache_memory_limit_soft", "Texture cache soft (MB)", 64, 4096),
    i("GPU", "texture_cache_memory_limit_hard", "Texture cache hard (MB)", 128, 8192),
    i("GPU", "query_occlusion_querybatch_range", "Occlusion querybatch range", 0, 1000),
    i("GPU", "query_occlusion_sample_lower_threshold", "Occlusion lower threshold", -1, 1000),
    i("GPU", "query_occlusion_sample_upper_threshold", "Occlusion upper threshold", 0, 1000),
    b("General", "discord", "Discord rich presence"),
    b("General", "debug", "Debug mode"),
    b("General", "apply_patches", "Apply patches"),
    f("General", "time_scalar", "Time scalar", 0.1, 10.0),
    i("General", "priority_class", "Process priority", 0, 2),
    b("General", "controller_hotkeys", "Controller hotkeys"),
    b("General", "disable_doubleclick_fullscreen", "Disable dbl-click fullscreen"),
    i("General", "recent_titles_entry_amount", "Recent titles count", 0, 50),
    c("HID", "hid", "Input backend", &["any", "nop", "sdl", "winkey", "xinput"]),
    b("HID", "vibration", "Controller vibration"),
    b("HID", "guide_button", "Forward guide button"),
    i("HID", "keyboard_mode", "Keyboard mode", 0, 2),
    f("HID", "left_stick_deadzone_percentage", "Left stick deadzone", 0.0, 1.0),
    f("HID", "right_stick_deadzone_percentage", "Right stick deadzone", 0.0, 1.0),
    b("Kernel", "apply_title_update", "Apply title updates"),
    b("Kernel", "allow_incompatible_title_update", "Allow incompatible updates"),
    i("Kernel", "console_type", "Console type", 0, 1),
    b("Kernel", "ignore_thread_affinities", "Ignore thread affinities"),
    b("Kernel", "ignore_thread_priorities", "Ignore thread priorities"),
    i("Kernel", "kernel_display_gamma_type", "Display gamma type", 0, 3),
    f("Kernel", "kernel_display_gamma_power", "Display gamma power", 1.0, 3.0),
    i("Logging", "log_level", "Log level", 0, 3),
    b("Logging", "enable_console", "Console window"),
    b("Logging", "log_to_stdout", "Log to stdout"),
    b("Logging", "flush_log", "Flush log"),
    b("Memory", "protect_zero", "Protect zero page"),
    b("Memory", "writable_executable_memory", "Writable executable memory"),
    b("Storage", "mount_cache", "Mount cache"),
    b("Storage", "mount_scratch", "Mount scratch"),
    b("Storage", "mount_memory_unit", "Mount memory unit"),
    b("Storage", "force_mount_devkit", "Force devkit mount"),
    i("UI", "window_size_x", "Window width", 320, 7680),
    i("UI", "window_size_y", "Window height", 240, 4320),
    i("UI", "font_size", "UI font size", 8, 32),
    b("UI", "headless", "Headless"),
    b("UI", "show_profiler", "Show profiler"),
    b("UI", "show_achievement_notification", "Achievement notifications"),
    b("UI", "storage_selection_dialog", "Storage selection dialog"),
    i("Video", "avpack", "AV pack / video mode", 0, 8),
    b("Video", "enable_3d_mode", "Stereoscopic 3D"),
    b("Video", "interlaced", "Interlaced"),
    i("Video", "custom_internal_display_resolution_x", "Custom internal width", 0, 1920),
    i("Video", "custom_internal_display_resolution_y", "Custom internal height", 0, 1080),
    Field { section: "Vulkan", key: "vulkan_device", label: "Vulkan device", desc: "", kind: Kind::Gpu },
    b("Vulkan", "vulkan_validation", "Vulkan validation"),
    b("Vulkan", "vulkan_sparse_shared_memory", "Sparse shared memory"),
    i("Vulkan", "vulkan_pipeline_creation_threads", "Pipeline threads", -1, 128),
    b("Vulkan", "vulkan_allow_present_mode_immediate", "Immediate present"),
    b("Vulkan", "vulkan_allow_present_mode_mailbox", "Mailbox present"),
    b("Vulkan", "vulkan_allow_present_mode_fifo_relaxed", "FIFO relaxed present"),
    b("Vulkan", "vulkan_semaphore_reuse_workaround", "Semaphore reuse workaround"),
    b("Win32", "win32_high_resolution_timer", "High-resolution timer"),
    b("Win32", "win32_mmcss", "MMCSS scheduling"),
    b("x64", "enable_host_guest_stack_synchronization", "Host/guest stack sync"),
    i("x64", "x64_extension_mask", "CPU extension mask", -1, 8191),
    b("HACKS", "ac6_ground_fix", "AC6 ground fix"),
];

/// Distinct section headers in schema order.
pub fn section_headers() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for f in FIELDS {
        if !out.contains(&f.section) {
            out.push(f.section);
        }
    }
    out
}
