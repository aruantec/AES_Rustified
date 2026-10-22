//! Cemu graphic packs — port of Lacrima's CemuPathsService,
//! CemuTitleIdHelper, CemuRulesTxtParser, CemuGraphicPacksService and
//! CemuGraphicPacksDownloadService.
//!
//! Packs are `rules.txt` trees under `<user-data>/graphicPacks`
//! (downloaded packs land in `graphicPacks/downloadedGraphicPacks`
//! with a `version.txt` marker). Enable/preset state lives in Cemu's
//! `settings.xml` under `content/GraphicPack/Entry` — entry present
//! means enabled unless `disabled="true"`, and `<Preset>` children
//! carry the active preset per category. User data is the exe dir's
//! `portable/` folder when present (Cemu 2.x portable mode) else the
//! exe dir itself — the same chain the launcher already uses for
//! logs and the `-g` launch.
//!
//! settings.xml edits are surgical text edits inside the
//! `<GraphicPack>` element so the rest of Cemu's config is preserved
//! byte-for-byte (XDocument round-trip equivalent without a dep).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::emulation::emu_dl::{DlProgress, Shared, USER_AGENT_PUBLIC};

// ------------------------------------------------------------------
// CemuPathsService
// ------------------------------------------------------------------
pub const GRAPHIC_PACKS_DIR: &str = "graphicPacks";
pub const DOWNLOADED_DIR: &str = "downloadedGraphicPacks";
pub const VERSION_FILE: &str = "version.txt";

/// `TryResolveUserDataDirectory`: `<exe dir>/portable` wins when it
/// exists (Cemu 2.x portable mode), else the exe dir itself.
pub fn user_data_dir(exe_dir: &Path) -> PathBuf {
    let portable = exe_dir.join("portable");
    if portable.is_dir() || portable.join("settings.xml").is_file() {
        portable
    } else {
        exe_dir.to_path_buf()
    }
}

pub fn settings_path(user_data: &Path) -> PathBuf {
    user_data.join("settings.xml")
}

pub fn packs_root(user_data: &Path) -> PathBuf {
    user_data.join(GRAPHIC_PACKS_DIR)
}

pub fn downloaded_root(user_data: &Path) -> PathBuf {
    packs_root(user_data).join(DOWNLOADED_DIR)
}

pub fn version_path(user_data: &Path) -> PathBuf {
    downloaded_root(user_data).join(VERSION_FILE)
}

/// `MakeRelativeRulesPath` — rules path relative to the user-data
/// root, '/'-separated (the `filename` attr form settings.xml uses).
fn rel_rules_path(user_data: &Path, rules: &Path) -> Option<String> {
    let rel = rules.strip_prefix(user_data).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

// ------------------------------------------------------------------
// CemuTitleIdHelper
// ------------------------------------------------------------------
/// `NormalizeDisplayTitleId` — hex-only uppercase, `XXXXXXXX-XXXXXXXX`
/// when the full 16 digits are present.
pub fn normalize_display_title_id(title_id: &str) -> Option<String> {
    let hex: String = title_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_uppercase();
    if hex.is_empty() {
        return None;
    }
    if hex.len() == 16 {
        Some(format!("{}-{}", &hex[..8], &hex[8..]))
    } else {
        Some(hex)
    }
}

/// `NormalizeMatchKey` — hex-only uppercase, no separator.
fn match_key(title_id: &str) -> String {
    title_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_uppercase()
}

/// `MatchesTitleId` — `*` is universal, else exact normalized match.
pub fn matches_title_id(pack_title_ids: &[String], game_title_id: &str) -> bool {
    if pack_title_ids.is_empty() {
        return false;
    }
    if pack_title_ids.iter().any(|t| t == "*") {
        return true;
    }
    let game = match_key(game_title_id);
    !game.is_empty()
        && pack_title_ids
            .iter()
            .any(|t| match_key(t) == game)
}

// ------------------------------------------------------------------
// CemuRulesTxtParser
// ------------------------------------------------------------------
#[derive(Default)]
struct RulesDef {
    name: String,
    ui_path: Option<String>,
    desc: Option<String>,
    title_ids: Vec<String>,
    universal: bool,
    enabled_by_default: bool,
    /// (name, category, is_default)
    presets: Vec<(String, String, bool)>,
}

fn unquote(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let t = if t.len() >= 2
        && ((t.starts_with('"') && t.ends_with('"'))
            || (t.starts_with('\'') && t.ends_with('\'')))
    {
        &t[1..t.len() - 1]
    } else {
        t
    };
    Some(t.replace("\\n", "\n").trim().to_string())
}

fn parse_bool(raw: Option<&String>) -> bool {
    matches!(raw.map(|s| s.trim()), Some("1" | "true" | "True" | "TRUE"))
}

/// rules.txt `titleIds` tokens — 16 hex digits or `*`.
fn parse_title_ids(raw: Option<&String>) -> Vec<String> {
    let Some(raw) = raw else { return Vec::new() };
    let mut out = Vec::new();
    let mut cur = String::new();
    let push = |cur: &mut String, out: &mut Vec<String>| {
        let t = cur.trim();
        if (t.len() == 16 && t.chars().all(|c| c.is_ascii_hexdigit())) || t == "*" {
            if !out.iter().any(|v: &String| v.eq_ignore_ascii_case(t)) {
                out.push(t.to_string());
            }
        }
        cur.clear();
    };
    for c in raw.chars() {
        if c.is_ascii_hexdigit() || c == '*' {
            cur.push(c);
        } else {
            push(&mut cur, &mut out);
        }
    }
    push(&mut cur, &mut out);
    out
}

/// `TryParseGraphicPack` — `[Definition]` keys + `[Preset]` sections.
fn parse_rules(path: &Path) -> Option<RulesDef> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut def = RulesDef::default();
    let mut definition: HashMap<String, String> = HashMap::new();
    let mut section = String::new();
    let mut preset_vals: HashMap<String, String> = HashMap::new();

    let flush_preset = |section: &str,
                            vals: &HashMap<String, String>,
                            def: &mut RulesDef| {
        if !section.eq_ignore_ascii_case("Preset") {
            return;
        }
        let name = vals.get("name").and_then(|v| unquote(v));
        let Some(name) = name.filter(|n| !n.trim().is_empty()) else {
            return;
        };
        let category = vals
            .get("category")
            .and_then(|v| unquote(v))
            .unwrap_or_default();
        let is_default = parse_bool(vals.get("default"))
            || name.to_lowercase().contains("(default)");
        def.presets
            .push((name.trim().to_string(), category.trim().to_string(), is_default));
    };

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            flush_preset(&section, &preset_vals, &mut def);
            section = line[1..line.len() - 1].trim().to_string();
            preset_vals.clear();
            continue;
        }
        if section.is_empty() {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        if eq == 0 {
            continue;
        }
        let key = line[..eq].trim().to_lowercase();
        if key.is_empty() {
            continue;
        }
        let value = line[eq + 1..].trim().to_string();
        if section.eq_ignore_ascii_case("Definition") {
            definition.insert(key, value);
        } else if section.eq_ignore_ascii_case("Preset") {
            preset_vals.insert(key, value);
        }
    }
    flush_preset(&section, &preset_vals, &mut def);
    if definition.is_empty() {
        return None;
    }

    def.name = definition
        .get("name")
        .and_then(|v| unquote(v))
        .unwrap_or_else(|| {
            path.parent()
                .and_then(|p| p.file_name())
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .trim()
        .to_string();
    def.ui_path = definition.get("path").and_then(|v| unquote(v));
    def.desc = definition.get("description").and_then(|v| unquote(v));
    def.title_ids = parse_title_ids(definition.get("titleids"));
    def.universal = def.title_ids.iter().any(|t| t == "*");
    def.enabled_by_default = parse_bool(definition.get("default"));
    Some(def)
}

// ------------------------------------------------------------------
// settings.xml — GraphicPack config state
// ------------------------------------------------------------------
/// Per-entry persisted state: enabled + active preset per category.
struct CfgState {
    enabled: bool,
    presets: HashMap<String, String>,
}

fn xml_attr(tag: &str, name: &str) -> Option<String> {
    // name="..." or name='...'
    let mut rest = tag;
    let needle = format!("{name}=");
    while let Some(i) = rest.find(&needle) {
        let after = &rest[i + needle.len()..];
        // Ensure the match is on an attribute boundary, not a suffix
        // of a longer attr name (e.g. `xfilename=`).
        let before_ok = i == 0
            || rest[..i]
                .chars()
                .last()
                .is_some_and(|c| c.is_whitespace());
        if !before_ok {
            rest = after;
            continue;
        }
        let mut ch = after.chars();
        let q = ch.next()?;
        if q != '"' && q != '\'' {
            rest = after;
            continue;
        }
        let body = &after[1..];
        let end = body.find(q)?;
        return Some(xml_unescape(&body[..end]));
    }
    None
}

fn xml_escape(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_unescape(v: &str) -> String {
    v.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// `<child>value</child>` pull inside an element body.
fn xml_child(body: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    let v = xml_unescape(body[start..end].trim());
    (!v.is_empty()).then_some(v)
}

/// The `<GraphicPack>` element's interior byte range inside `xml`
/// (self-closing tags report an empty span). `Err` → absent.
fn graphic_pack_span(xml: &str) -> Option<(usize, usize, usize)> {
    // Returns (inner_start, inner_end, close_tag_end).
    let mut search = 0;
    loop {
        let i = xml[search..].find("<GraphicPack")? + search;
        let after = &xml[i + "<GraphicPack".len()..];
        let boundary = after
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == '/' || c == '>');
        if !boundary {
            search = i + 1;
            continue;
        }
        let open_end = xml[i..].find('>')? + i;
        if xml[..open_end].ends_with('/') {
            return Some((open_end + 1, open_end + 1, open_end + 1));
        }
        let inner_start = open_end + 1;
        let close = xml[inner_start..].find("</GraphicPack>")? + inner_start;
        return Some((inner_start, close, close + "</GraphicPack>".len()));
    }
}

/// One `<Entry ...>` or `<Entry ...>...</Entry>` block inside the
/// GraphicPack interior: (block_start, block_end, open_tag, inner).
fn entry_blocks(region: &str) -> Vec<(usize, usize, String, String)> {
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(rel) = region[search..].find("<Entry") {
        let i = rel + search;
        let after = &region[i + 6..];
        let boundary = after
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == '/' || c == '>');
        if !boundary {
            search = i + 1;
            continue;
        }
        let Some(open_end) = region[i..].find('>').map(|e| e + i) else {
            break;
        };
        if region[..open_end].ends_with('/') {
            out.push((i, open_end + 1, region[i..=open_end].to_string(), String::new()));
            search = open_end + 1;
            continue;
        }
        let inner_start = open_end + 1;
        let Some(close) = region[inner_start..].find("</Entry>").map(|c| c + inner_start)
        else {
            break;
        };
        out.push((
            i,
            close + "</Entry>".len(),
            region[i..=open_end].to_string(),
            region[inner_start..close].to_string(),
        ));
        search = close + "</Entry>".len();
    }
    out
}

/// `<Preset>` blocks inside an `<Entry>` body — same one-level shape.
fn preset_blocks(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(rel) = body[search..].find("<Preset") {
        let i = rel + search;
        let after = &body[i + 7..];
        let boundary = after
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == '/' || c == '>');
        if !boundary {
            search = i + 1;
            continue;
        }
        let Some(open_end) = body[i..].find('>').map(|e| e + i) else {
            break;
        };
        let (tag, inner, next) = if body[..open_end].ends_with('/') {
            (body[i..=open_end].to_string(), "", open_end + 1)
        } else {
            let inner_start = open_end + 1;
            let Some(close) = body[inner_start..].find("</Preset>").map(|c| c + inner_start)
            else {
                break;
            };
            (
                body[i..=open_end].to_string(),
                &body[inner_start..close],
                close + "</Preset>".len(),
            )
        };
        // category: attr first, then <category> child — the element
        // form is what Cemu itself writes (Lacrima reads only the
        // attr; reading both is a strict superset that keeps packs
        // Cemu configured from silently resetting to defaults).
        let category = xml_attr(&tag, "category")
            .or_else(|| xml_child(inner, "category"))
            .unwrap_or_default();
        let Some(preset) = xml_attr(&tag, "preset").or_else(|| xml_child(inner, "preset"))
        else {
            search = next;
            continue;
        };
        out.push((category, preset));
        search = next;
    }
    out
}

/// `LoadConfigLookup` — `filename`-keyed enabled/preset state.
fn load_config_lookup(settings: &Path) -> HashMap<String, CfgState> {
    let mut lookup = HashMap::new();
    let Ok(xml) = std::fs::read_to_string(settings) else {
        return lookup;
    };
    let Some((start, end, _)) = graphic_pack_span(&xml) else {
        return lookup;
    };
    let region = &xml[start..end];
    for (_, _, tag, inner) in entry_blocks(region) {
        let filename = xml_attr(&tag, "filename")
            .or_else(|| xml_child(&inner, "filename"))
            .map(|f| f.trim().replace('\\', "/"));
        let Some(filename) = filename.filter(|f| !f.is_empty()) else {
            continue;
        };
        let disabled = xml_attr(&tag, "disabled")
            .is_some_and(|d| d.eq_ignore_ascii_case("true"));
        let presets = preset_blocks(&inner)
            .into_iter()
            .collect::<HashMap<_, _>>();
        lookup.insert(filename.to_lowercase(), CfgState {
            enabled: !disabled,
            presets,
        });
    }
    lookup
}

// ------------------------------------------------------------------
// CemuGraphicPacksService
// ------------------------------------------------------------------
pub struct PresetGroup {
    pub category: String,
    pub label: String,
    pub names: Vec<String>,
    pub selected: usize,
}

pub struct PackEntry {
    /// `BuildEntryKey` — the '/'-separated relative rules path.
    pub key: String,
    pub rel_rules: String,
    pub name: String,
    /// uiPath — description — relPath (`BuildCemuGraphicPackSubtitle`).
    pub subtitle: String,
    pub enabled: bool,
    pub groups: Vec<PresetGroup>,
}

fn rules_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            rules_files(&p, out);
        } else if p
            .file_name()
            .and_then(|f| f.to_str())
            .is_some_and(|f| f.eq_ignore_ascii_case("rules.txt"))
        {
            out.push(p);
        }
    }
}

/// `TryGetGraphicPacksForTitleId` — packs matching the title (or
/// universal), carrying config state and preset groups.
pub fn packs_for_title(exe_dir: &Path, title_id: &str) -> Result<Vec<PackEntry>, String> {
    let user_data = user_data_dir(exe_dir);
    if !user_data.is_dir() {
        return Err("Cemu user data directory was not found.".into());
    }
    let settings = settings_path(&user_data);
    let Some(tid) = normalize_display_title_id(title_id) else {
        return Err("Title ID is invalid.".into());
    };
    let config = load_config_lookup(&settings);

    let mut files = Vec::new();
    rules_files(&packs_root(&user_data), &mut files);
    let mut packs: Vec<(String, PackEntry)> = Vec::new();
    for rules in files {
        let Some(rel) = rel_rules_path(&user_data, &rules) else {
            continue;
        };
        let Some(def) = parse_rules(&rules) else { continue };
        if !def.universal && !matches_title_id(&def.title_ids, &tid) {
            continue;
        }
        let cfg = config.get(&rel.to_lowercase());
        let enabled = cfg.map(|c| c.enabled).unwrap_or(def.enabled_by_default);

        // BuildPresetGroups — presets grouped by category (ci);
        // selection order: configured preset (when still valid) →
        // IsDefault-marked → first.
        let mut groups: Vec<PresetGroup> = Vec::new();
        for (name, cat, is_def) in &def.presets {
            let cat_key = cat.to_lowercase();
            let gi = match groups
                .iter()
                .position(|g: &PresetGroup| g.category.to_lowercase() == cat_key)
            {
                Some(i) => i,
                None => {
                    groups.push(PresetGroup {
                        category: cat.clone(),
                        label: if cat.trim().is_empty() {
                            "Active preset".into()
                        } else {
                            cat.clone()
                        },
                        names: Vec::new(),
                        selected: 0,
                    });
                    groups.len() - 1
                }
            };
            groups[gi].names.push(name.clone());
            if *is_def {
                groups[gi].selected = groups[gi].names.len() - 1;
            }
        }
        if let Some(c) = cfg {
            for g in &mut groups {
                if let Some(want) = c
                    .presets
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(&g.category))
                    .map(|(_, v)| v)
                {
                    if let Some(pos) =
                        g.names.iter().position(|n| n.eq_ignore_ascii_case(want))
                    {
                        g.selected = pos;
                    }
                }
            }
        }
        groups.sort_by(|a, b| a.category.to_lowercase().cmp(&b.category.to_lowercase()));

        let parts: Vec<String> = [
            def.ui_path.as_deref().map(str::trim),
            def.desc.as_deref().map(str::trim),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
        let subtitle = if parts.is_empty() {
            rel.clone()
        } else {
            parts.join(" — ")
        };
        // Lacrima orders by UiPath ?? Name then Name (ci).
        let sort = def
            .ui_path
            .clone()
            .unwrap_or_else(|| def.name.clone())
            .to_lowercase();
        packs.push((
            sort,
            PackEntry {
                key: rel.clone(),
                rel_rules: rel,
                name: def.name,
                subtitle,
                enabled,
                groups,
            },
        ));
    }

    packs.sort_by(|a, b| {
        a.0
            .cmp(&b.0)
            .then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase()))
    });
    Ok(packs.into_iter().map(|(_, e)| e).collect())
}

/// One `CemuGraphicPackToggle` worth of state.
pub struct Toggle {
    pub rel_rules: String,
    pub enabled: bool,
    /// category → preset name.
    pub presets: Vec<(String, String)>,
}

/// `SaveEnabledStates` — upsert `Entry` elements inside
/// `content/GraphicPack`, preserving the rest of settings.xml
/// byte-for-byte. Creates the file when absent.
pub fn save_enabled_states(exe_dir: &Path, toggles: &[Toggle]) -> Result<(), String> {
    let user_data = user_data_dir(exe_dir);
    let settings = settings_path(&user_data);
    let mut xml = std::fs::read_to_string(&settings).unwrap_or_else(|_| {
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<content>\n</content>\n".to_string()
    });

    // A self-closing <GraphicPack/> reports an empty span — expand it
    // to a paired element first, else entries would be inserted as
    // inert SIBLINGS outside it.
    if let Some((s, e, _)) = graphic_pack_span(&xml) {
        if s == e && s >= 2 && &xml[s - 2..s] == "/>" {
            xml.replace_range(s - 2..s, "></GraphicPack>");
        }
    }
    // Ensure <GraphicPack> exists under <content>.
    if graphic_pack_span(&xml).is_none() {
        if let Some(close) = xml.rfind("</content>") {
            xml.insert_str(close, "    <GraphicPack>\n    </GraphicPack>\n");
        } else {
            xml.push_str("<content>\n    <GraphicPack>\n    </GraphicPack>\n</content>\n");
        }
    }

    // Orphaned filename-attr'd Entry blocks outside the GraphicPack
    // element are inert leftovers — sweep them all, not just the ones
    // this save touches.
    loop {
        let Some((gs, ge, _)) = graphic_pack_span(&xml) else {
            break;
        };
        let stray = entry_blocks(&xml).into_iter().find(|(bs, be, tag, _)| {
            (*be <= gs || *bs >= ge)
                && xml_attr(tag, "filename").is_some()
        });
        let Some((bs, be, _, _)) = stray else { break };
        xml.replace_range(bs..be, "");
    }

    for t in toggles {
        let normalized = t.rel_rules.replace('\\', "/");
        // Remove every Entry carrying this filename — inside
        // GraphicPack or a stray the sweep just handled.
        // (RecentLaunchFiles' <Entry>path</Entry> elements have no
        // filename attr, so they pass through untouched.)
        loop {
            let mut removed = false;
            for (bs, be, tag, _) in entry_blocks(&xml) {
                if xml_attr(&tag, "filename")
                    .map(|f| f.trim().replace('\\', "/"))
                    .is_some_and(|f| f.eq_ignore_ascii_case(&normalized))
                {
                    xml.replace_range(bs..be, "");
                    removed = true;
                    break;
                }
            }
            if !removed {
                break;
            }
        }
        // Insert the fresh entry before </GraphicPack>.
        let Some((_, e, _)) = graphic_pack_span(&xml) else {
            break;
        };
        // Cemu's own serialization (CemuConfig.cpp): `filename` +
        // `disabled="true"` ONLY when disabled (enabled entries carry
        // no attr), and Preset values as CHILD ELEMENTS — attribute
        // form is silently ignored by Cemu's parser.
        let mut entry = format!("        <Entry filename=\"{}\"", xml_escape(&normalized));
        let presets: Vec<&(String, String)> = t
            .presets
            .iter()
            .filter(|(_, v)| !v.trim().is_empty())
            .collect();
        if !t.enabled {
            entry.push_str(" disabled=\"true\"/>\n");
        } else if presets.is_empty() {
            entry.push_str("/>\n");
        } else {
            entry.push_str(">\n");
            for (cat, preset) in presets {
                entry.push_str("            <Preset>\n");
                if !cat.trim().is_empty() {
                    entry.push_str(&format!(
                        "                <category>{}</category>\n",
                        xml_escape(cat)
                    ));
                }
                entry.push_str(&format!(
                    "                <preset>{}</preset>\n",
                    xml_escape(preset)
                ));
                entry.push_str("            </Preset>\n");
            }
            entry.push_str("        </Entry>\n");
        }
        xml.insert_str(e, &entry);
    }

    if let Some(d) = settings.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("Settings dir not writable: {e}"))?;
    }
    std::fs::write(&settings, xml).map_err(|e| format!("Failed to write settings.xml: {e}"))?;
    Ok(())
}

// ------------------------------------------------------------------
// CemuGraphicPacksDownloadService
// ------------------------------------------------------------------
const QUERY_URL_TEMPLATE: &str =
    "https://cemu.info/api2/query_graphicpack_url.php?version=2.6.0&t=";
const GITHUB_LATEST_API: &str =
    "https://api.github.com/repos/cemu-project/cemu_graphic_packs/releases/latest";

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(600)))
        .build()
        .into()
}

fn set(dl: &Shared, stage: &str, frac: f32) {
    let mut p = dl.lock().unwrap_or_else(|e| e.into_inner());
    p.stage = stage.into();
    p.frac = frac;
}

fn http_text(url: &str) -> Result<String, String> {
    let mut resp = agent()
        .get(url)
        .header("User-Agent", USER_AGENT_PUBLIC)
        .call()
        .map_err(|e| format!("Request failed: {e}"))?;
    resp.body_mut()
        .read_to_string()
        .map_err(|e| format!("Read failed: {e}"))
}

fn http_bytes(dl: &Shared, url: &str) -> Result<Vec<u8>, String> {
    let mut resp = agent()
        .get(url)
        .header("User-Agent", USER_AGENT_PUBLIC)
        .call()
        .map_err(|e| format!("Download failed: {e}"))?;
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let mut reader = resp.body_mut().as_reader();
    let mut buf = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut chunk = [0u8; 16384];
    loop {
        let n = reader
            .read(&mut chunk)
            .map_err(|e| format!("Download failed: {e}"))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(t) = total {
            set(dl, "Downloading graphic packs…", (buf.len() as f32 / t as f32).clamp(0.0, 1.0));
        }
    }
    Ok(buf)
}

/// `ExtractZipToDirectory` — in-memory zip, guards intact: skip
/// entries escaping the root, empty names, >128MB members.
fn extract_zip_bytes(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("Invalid zip: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("Extract failed: {e}"))?;
        if entry.name().map(|n| n.is_empty()).unwrap_or(true) {
            continue;
        }
        let Some(name) = entry.enclosed_name().map(|n| n.to_path_buf()) else {
            continue;
        };
        if entry.size() > 128 * 1024 * 1024 {
            continue;
        }
        let out = dest.join(name);
        if entry.is_dir() {
            let _ = std::fs::create_dir_all(&out);
            continue;
        }
        if let Some(d) = out.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let mut w =
            std::fs::File::create(&out).map_err(|e| format!("Extract failed: {e}"))?;
        std::io::copy(&mut entry, &mut w).map_err(|e| format!("Extract failed: {e}"))?;
    }
    Ok(())
}

/// `ClearDownloadedGraphicPacks`.
fn clear_dir(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let _ = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
    }
}

fn download_latest(user_data: &Path, dl: &Shared) -> Result<String, String> {
    let downloaded = downloaded_root(user_data);
    let version_file = version_path(user_data);
    std::fs::create_dir_all(&downloaded)
        .map_err(|e| format!("Graphic packs dir not writable: {e}"))?;

    // Lacrima asks cemu.info's API for the current release URL first
    // (it returns a GitHub API URL as plain text), falling back to
    // the known latest-release endpoint.
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let api_url = http_text(&format!("{QUERY_URL_TEMPLATE}{unix}"))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| t.to_lowercase().starts_with("http"))
        .unwrap_or_else(|| GITHUB_LATEST_API.to_string());

    let json: serde_json::Value = serde_json::from_str(&http_text(&api_url)?)
        .map_err(|e| format!("Bad release response: {e}"))?;
    let release_name = json
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or("Graphic pack release response did not include a name.")?
        .to_string();

    if let Ok(installed) = std::fs::read_to_string(&version_file) {
        if installed.trim().eq_ignore_ascii_case(release_name.trim()) {
            return Ok("Graphic packs are already up to date.".into());
        }
    }

    let url = json
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|a| a.get("browser_download_url"))
        .and_then(|u| u.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("Graphic pack release did not include a download asset.")?
        .to_string();

    let bytes = http_bytes(dl, &url)?;
    clear_dir(&downloaded);
    extract_zip_bytes(&bytes, &downloaded)?;
    std::fs::write(&version_file, &release_name)
        .map_err(|e| format!("Failed to write version marker: {e}"))?;
    Ok(format!("Downloaded graphic packs ({release_name})."))
}

/// Async wrapper matching `rpcs3_patch::download` — the overlay polls
/// the shared progress.
pub fn download(user_data: PathBuf) -> Shared {
    let dl: Shared = Arc::new(Mutex::new(DlProgress {
        stage: "Downloading latest Cemu graphic packs…".into(),
        ..Default::default()
    }));
    let shared = dl.clone();
    std::thread::spawn(move || {
        let res = download_latest(&user_data, &shared);
        shared.lock().unwrap_or_else(|e| e.into_inner()).done = Some(res);
    });
    dl
}

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &str = r#"[Definition]
titleIds = 0005000010172600
name = "Test Game - Enhancements"
path = "Test Game/Enhancements"
description = "Test description"
default = 0

[Preset]
name = "1920x1080 (Default)"
category = TV Resolution
default = 1

[Preset]
name = 3840x2160
category = TV Resolution

[Preset]
name = "Ultra"
category = Shadow Quality
"#;

    const RULES_UNIVERSAL: &str = r#"[Definition]
titleIds = *
name = Universal Pack
"#;

    fn rules_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aes_cemu_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn title_id_normalization_and_match() {
        assert_eq!(
            normalize_display_title_id("00050000-10172600").as_deref(),
            Some("00050000-10172600")
        );
        assert_eq!(
            normalize_display_title_id("0005000010172600").as_deref(),
            Some("00050000-10172600")
        );
        assert!(normalize_display_title_id("---").is_none());
        let ids = vec!["00050000-10172600".to_string()];
        assert!(matches_title_id(&ids, "0005000010172600"));
        assert!(!matches_title_id(&ids, "00050000DEADBEEF"));
        assert!(matches_title_id(&["*".to_string()], "anything"));
        assert!(!matches_title_id(&[], "0005000010172600"));
    }

    #[test]
    fn rules_txt_parses_definition_and_presets() {
        let dir = rules_dir("rules");
        let path = dir.join("rules.txt");
        std::fs::write(&path, RULES).unwrap();
        let def = parse_rules(&path).unwrap();
        assert_eq!(def.name, "Test Game - Enhancements");
        assert_eq!(def.ui_path.as_deref(), Some("Test Game/Enhancements"));
        assert_eq!(def.desc.as_deref(), Some("Test description"));
        assert_eq!(def.title_ids, vec!["0005000010172600".to_string()]);
        assert!(!def.universal);
        assert!(!def.enabled_by_default);
        assert_eq!(def.presets.len(), 3);
        // "(Default)" in the name marks it even without default=1.
        assert_eq!(def.presets[0].1, "TV Resolution");
        assert!(def.presets[0].2);
        assert!(!def.presets[1].2);
        assert_eq!(def.presets[2].1, "Shadow Quality");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_roundtrip_preserves_state_and_doc() {
        let dir = rules_dir("settings");
        let exe = dir.join("emu");
        let portable = exe.join("portable");
        std::fs::create_dir_all(&portable).unwrap();
        let settings = portable.join("settings.xml");
        std::fs::write(
            &settings,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<content>\n    <logflag>0</logflag>\n    <RecentLaunchFiles>\n        <Entry>F:/games/x</Entry>\n    </RecentLaunchFiles>\n</content>\n",
        )
        .unwrap();

        let rel_a = "graphicPacks/downloadedGraphicPacks/GameA/rules.txt".to_string();
        let rel_b = "graphicPacks/downloadedGraphicPacks/GameB/rules.txt".to_string();
        save_enabled_states(
            &exe,
            &[
                Toggle {
                    rel_rules: rel_a.clone(),
                    enabled: true,
                    presets: vec![(
                        "TV Resolution".into(),
                        "3840x2160".into(),
                    )],
                },
                Toggle {
                    rel_rules: rel_b.clone(),
                    enabled: false,
                    presets: vec![],
                },
            ],
        )
        .unwrap();

        let text = std::fs::read_to_string(&settings).unwrap();
        // Unrelated content survives the upsert byte-for-byte.
        assert!(text.contains("<logflag>0</logflag>"));
        assert!(text.contains("<Entry>F:/games/x</Entry>"));
        // Cemu's native schema — Preset values as child ELEMENTS
        // (attribute form is silently ignored by Cemu's parser) and
        // no `disabled` attr on enabled entries.
        assert!(text.contains("<category>TV Resolution</category>"));
        assert!(text.contains("<preset>3840x2160</preset>"));
        assert!(text.contains(&format!(
            "<Entry filename=\"{rel_b}\" disabled=\"true\"/>"
        )));

        let lookup = load_config_lookup(&settings);
        let a = lookup.get(&rel_a.to_lowercase()).unwrap();
        assert!(a.enabled);
        assert_eq!(
            a.presets.get("TV Resolution").map(|s| s.as_str()),
            Some("3840x2160")
        );
        let b = lookup.get(&rel_b.to_lowercase()).unwrap();
        assert!(!b.enabled);

        // Re-save toggling A off — the old entry is replaced, not duped.
        save_enabled_states(
            &exe,
            &[Toggle {
                rel_rules: rel_a.clone(),
                enabled: false,
                presets: vec![],
            }],
        )
        .unwrap();
        let text = std::fs::read_to_string(&settings).unwrap();
        assert_eq!(text.matches(&rel_a).count(), 1);
        let lookup = load_config_lookup(&settings);
        assert!(!lookup.get(&rel_a.to_lowercase()).unwrap().enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cemu writes `<GraphicPack/>` self-closing when empty — entries
    /// must land INSIDE the element (siblings are inert to both our
    /// reader and Cemu).
    #[test]
    fn save_into_self_closing_graphic_pack_nests_entries() {
        let dir = rules_dir("selfclosing");
        let exe = dir.join("emu");
        let portable = exe.join("portable");
        std::fs::create_dir_all(&portable).unwrap();
        let settings = portable.join("settings.xml");
        std::fs::write(
            &settings,
            "<content>\n    <GraphicPack/>\n    <RecentLaunchFiles>\n        <Entry>F:/games/x</Entry>\n    </RecentLaunchFiles>\n</content>\n",
        )
        .unwrap();

        let rel = "graphicPacks/downloadedGraphicPacks/GameA/rules.txt".to_string();
        save_enabled_states(
            &exe,
            &[Toggle {
                rel_rules: rel.clone(),
                enabled: true,
                presets: vec![("Quality".into(), "Ultra".into())],
            }],
        )
        .unwrap();

        let text = std::fs::read_to_string(&settings).unwrap();
        // The entry must precede </GraphicPack> and the element must
        // no longer be self-closing.
        let gp_open = text.find("<GraphicPack").unwrap();
        let gp_close = text.find("</GraphicPack>").unwrap();
        let entry_pos = text.find(&format!("filename=\"{rel}\"")).unwrap();
        assert!(gp_open < entry_pos && entry_pos < gp_close);
        assert!(text[gp_open..].starts_with("<GraphicPack>"));
        let lookup = load_config_lookup(&settings);
        let a = lookup.get(&rel.to_lowercase()).unwrap();
        assert!(a.enabled);
        assert_eq!(a.presets.get("Quality").map(|s| s.as_str()), Some("Ultra"));
        // RecentLaunchFiles <Entry> survived.
        assert!(text.contains("<Entry>F:/games/x</Entry>"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn packs_for_title_filters_and_carries_state() {
        let dir = rules_dir("packs");
        let exe = dir.join("emu");
        let user = exe.join("portable");
        let pack_dir = user
            .join("graphicPacks")
            .join("downloadedGraphicPacks")
            .join("TestGame")
            .join("Enhancements");
        std::fs::create_dir_all(&pack_dir).unwrap();
        std::fs::write(pack_dir.join("rules.txt"), RULES).unwrap();
        let uni = user
            .join("graphicPacks")
            .join("downloadedGraphicPacks")
            .join("UniPack");
        std::fs::create_dir_all(&uni).unwrap();
        std::fs::write(uni.join("rules.txt"), RULES_UNIVERSAL).unwrap();
        // A pack for a different title must be filtered out.
        let other = user
            .join("graphicPacks")
            .join("downloadedGraphicPacks")
            .join("OtherGame");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join("rules.txt"),
            RULES.replace("0005000010172600", "00050000DEADBEEF"),
        )
        .unwrap();

        let packs = packs_for_title(&exe, "00050000-10172600").unwrap();
        assert_eq!(packs.len(), 2);
        let p = packs
            .iter()
            .find(|p| p.name == "Test Game - Enhancements")
            .unwrap();
        assert!(!p.enabled); // default = 0
        assert_eq!(p.groups.len(), 2);
        let tv = p
            .groups
            .iter()
            .find(|g| g.category == "TV Resolution")
            .unwrap();
        assert_eq!(tv.names.len(), 2);
        assert_eq!(tv.selected, 0); // the (Default)-marked preset
        assert!(packs.iter().any(|p| p.name == "Universal Pack"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
