//! RPCS3 patches — a port of Lacrima's `Rpcs3PatchesService` +
//! `Rpcs3PatchesDownloadService` + `Rpcs3PatchYamlLoader`.
//!
//! The official patch DB is `patches/patch.yml` under the emulator
//! directory. Its layout is *not* plain-YAML friendly: the file
//! repeats top-level `Anchors:` blocks (~160 of them) and references
//! them through `*alias` / `&anchor` all over — yaml-cpp merges
//! duplicate keys, serde_yaml errors on them, so this module carries
//! a focused YAML-subset parser with the same merge semantics.
//!
//! Activation state lives in `config/patch_config.yml`:
//! `PPU-hash -> patch name -> game title -> serial -> app version ->
//! Enabled: true`. Saving adds/removes those leaves and prunes empty
//! parents, preserving entries for other games.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use crate::emulation::emu_dl::{self, DlProgress, Shared};

/// RPCS3's patch engine version (Lacrima's PatchEngineVersion).
pub const PATCH_ENGINE_VERSION: &str = "1.2";
/// Lacrima's PatchesDownloadEndpoint — serves the official patch DB.
const DOWNLOAD_URL: &str =
    "https://rpcs3.net/compatibility?patch&api=v1&v=1.2";
const OFFICIAL_FILE: &str = "patch.yml";
const IMPORTED_FILE: &str = "imported_patch.yml";
const ARTEMIS_MARKER: &str = "(Artemis)";
/// Entry-identity separator — same unit separator Lacrima's
/// `BuildEntryKey` uses (`Join('\u001F', ...)`).
const KEY_SEP: char = '\u{1f}';

// ======================================================================
// YAML subset — maps/seqs/scalars + anchors/aliases + dup-key merge
// ======================================================================

#[derive(Debug, Clone)]
enum YNode {
    Scalar(String),
    Seq(Vec<YNode>),
    Map(Vec<(String, YNode)>),
    Alias(String),
}

struct YLine {
    indent: usize,
    text: String,
}

fn yaml_lines(text: &str) -> Vec<YLine> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim_end();
            let t = l.trim_start_matches(' ');
            if t.is_empty() || t.starts_with('#') || t.starts_with('%') || t == "---" || t == "..."
            {
                return None;
            }
            Some(YLine {
                indent: l.len() - t.len(),
                text: t.to_string(),
            })
        })
        .collect()
}

fn is_seq_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

/// Cut a trailing ` # comment` — `#` only starts a comment when
/// preceded by whitespace and outside quotes.
fn strip_comment(s: &str) -> &str {
    let b = s.as_bytes();
    let (mut dq, mut sq) = (false, false);
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' if !sq => {
                dq = !dq;
                if dq && i > 0 && b[i - 1] == b'\\' {
                    dq = !dq;
                }
            }
            b'\'' if !dq => sq = !sq,
            b'#' if !dq && !sq && (i == 0 || b[i - 1] == b' ' || b[i - 1] == b'\t') => {
                return s[..i].trim_end();
            }
            _ => {}
        }
        i += 1;
    }
    s.trim_end()
}

/// Split `key: rest` honoring quoted keys. The `:` that ends a plain
/// key must be followed by whitespace or EOL.
fn split_key(text: &str) -> Option<(String, &str)> {
    let b = text.as_bytes();
    if b.first() == Some(&b'"') || b.first() == Some(&b'\'') {
        let dq = b[0] == b'"';
        let mut i = 1;
        while i < b.len() {
            if dq && b[i] == b'\\' {
                i += 2;
                continue;
            }
            if b[i] == b[0] {
                if !dq && b.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                let key = if dq {
                    unquote_dq(&text[1..i])
                } else {
                    text[1..i].replace("''", "'")
                };
                let rest = text[i + 1..].trim_start();
                let Some(after) = rest.strip_prefix(':') else {
                    return None;
                };
                return Some((key, after.trim_start()));
            }
            i += 1;
        }
        return None;
    }
    for i in 0..b.len() {
        if b[i] == b':' && (i + 1 == b.len() || b[i + 1] == b' ' || b[i + 1] == b'\t') {
            let key = text[..i].trim().to_string();
            return Some((key, text[i + 1..].trim_start()));
        }
    }
    None
}

fn unquote_dq(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Insert into a mapping with yaml-cpp merge semantics: duplicate
/// keys merge mappings recursively and concatenate sequences instead
/// of replacing — the official patch.yml depends on this (repeated
/// `Anchors:` blocks accumulate).
fn map_insert(map: &mut Vec<(String, YNode)>, key: String, val: YNode) {
    if let Some((_, old)) = map.iter_mut().find(|(k, _)| *k == key) {
        merge(old, val);
    } else {
        map.push((key, val));
    }
}

fn merge(old: &mut YNode, new: YNode) {
    match (old, new) {
        (YNode::Map(a), YNode::Map(b)) => {
            for (k, v) in b {
                map_insert(a, k, v);
            }
        }
        (YNode::Seq(a), YNode::Seq(b)) => a.extend(b),
        (o, n) => *o = n,
    }
}

struct YParser {
    lines: Vec<YLine>,
    pos: usize,
    anchors: HashMap<String, YNode>,
}

impl YParser {
    fn parse(text: &str) -> (YNode, HashMap<String, YNode>) {
        let mut p = YParser {
            lines: yaml_lines(text),
            pos: 0,
            anchors: HashMap::new(),
        };
        let node = if p.lines.is_empty() {
            YNode::Map(Vec::new())
        } else {
            p.parse_block(p.lines[0].indent)
        };
        (node, p.anchors)
    }

    fn parse_block(&mut self, indent: usize) -> YNode {
        if self.pos < self.lines.len() && is_seq_item(&self.lines[self.pos].text) {
            YNode::Seq(self.parse_seq(indent))
        } else {
            YNode::Map(self.parse_map(indent))
        }
    }

    /// A deeper-indented block following this line, or a same-indent
    /// sequence (YAML allows `key:` then `- item` at the key's level).
    fn nested_follows(&self, indent: usize) -> bool {
        self.lines.get(self.pos).is_some_and(|l| {
            l.indent > indent || (l.indent == indent && is_seq_item(&l.text))
        })
    }

    fn parse_map(&mut self, indent: usize) -> Vec<(String, YNode)> {
        let mut out = Vec::new();
        while self.pos < self.lines.len() {
            let l = &self.lines[self.pos];
            if l.indent < indent || (l.indent == indent && is_seq_item(&l.text)) {
                break;
            }
            if l.indent > indent {
                // Orphaned deeper line — tolerate and skip.
                self.pos += 1;
                continue;
            }
            let text = self.lines[self.pos].text.clone();
            self.pos += 1;
            let Some((key, rest)) = split_key(&text) else { continue };
            let val = self.parse_value(indent, rest);
            map_insert(&mut out, key, val);
        }
        out
    }

    fn parse_seq(&mut self, indent: usize) -> Vec<YNode> {
        let mut out = Vec::new();
        while self.pos < self.lines.len() {
            let l = &self.lines[self.pos];
            if l.indent != indent || !is_seq_item(&l.text) {
                break;
            }
            let rest = l.text[1..].trim_start().to_string();
            self.pos += 1;
            let val = self.parse_value(indent, &rest);
            out.push(val);
        }
        out
    }

    /// Value after `key:` or `- ` — inline scalar/flow/alias/anchor or
    /// a nested block on the following lines.
    fn parse_value(&mut self, indent: usize, raw: &str) -> YNode {
        let rest = strip_comment(raw).trim();
        if rest.is_empty() {
            return if self.nested_follows(indent) {
                let ni = self.lines[self.pos].indent;
                self.parse_block(ni)
            } else {
                YNode::Scalar(String::new())
            };
        }
        if let Some(a) = rest.strip_prefix('&') {
            let (name, tail) = match a.find(|c: char| c.is_whitespace()) {
                Some(i) => (&a[..i], a[i..].trim()),
                None => (a, ""),
            };
            let val = if tail.is_empty() {
                if self.nested_follows(indent) {
                    let ni = self.lines[self.pos].indent;
                    self.parse_block(ni)
                } else {
                    YNode::Scalar(String::new())
                }
            } else {
                self.parse_inline(tail)
            };
            self.anchors.insert(name.to_string(), val.clone());
            return val;
        }
        // Literal/folded block scalar — consume deeper lines.
        if rest.starts_with('|') || rest.starts_with('>') {
            let mut parts = Vec::new();
            while self.pos < self.lines.len() && self.lines[self.pos].indent > indent {
                parts.push(self.lines[self.pos].text.clone());
                self.pos += 1;
            }
            return YNode::Scalar(parts.join("\n"));
        }
        self.parse_inline(rest)
    }

    /// Single-line inline value: quoted scalar, flow seq/map, alias,
    /// or plain scalar (comment already stripped).
    fn parse_inline(&mut self, s: &str) -> YNode {
        let s = s.trim();
        if s.starts_with('[') || s.starts_with('{') {
            return self.parse_flow(s);
        }
        if let Some(a) = s.strip_prefix('*') {
            return YNode::Alias(a.trim().to_string());
        }
        if s.starts_with('"') {
            let b = s.as_bytes();
            let mut i = 1;
            while i < b.len() {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if b[i] == b'"' {
                    break;
                }
                i += 1;
            }
            return YNode::Scalar(unquote_dq(&s[1..i.min(s.len())]));
        }
        if s.starts_with('\'') {
            let b = s.as_bytes();
            let mut i = 1;
            while i < b.len() {
                if b[i] == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            return YNode::Scalar(s[1..i.min(s.len())].replace("''", "'"));
        }
        YNode::Scalar(s.to_string())
    }

    /// `[ a, b, [c] ]` / `{ k: v }` — items may be quoted or aliases.
    /// Consumes extra lines until the brackets balance.
    fn parse_flow(&mut self, s: &str) -> YNode {
        let open = s.as_bytes()[0];
        let close = if open == b'[' { b']' } else { b'}' };
        let mut text = s.to_string();
        while depth_of(&text, open, close) > 0 && self.pos < self.lines.len() {
            text.push(' ');
            text.push_str(&self.lines[self.pos].text);
            self.pos += 1;
        }
        let inner = match text.rfind(|c: char| c == close as char) {
            Some(i) if i > 0 => &text[1..i],
            _ => &text[1..],
        };
        let items = split_top(inner);
        if open == b'[' {
            YNode::Seq(
                items
                    .into_iter()
                    .filter(|t| !t.trim().is_empty())
                    .map(|t| self.parse_inline(&strip_comment(&t)))
                    .collect(),
            )
        } else {
            let mut m = Vec::new();
            for item in items {
                if let Some((k, v)) = split_key(&item) {
                    let vv = self.parse_inline(strip_comment(v).trim());
                    map_insert(&mut m, k, vv);
                }
            }
            YNode::Map(m)
        }
    }
}

/// Top-level bracket depth of a flow collection, ignoring quotes.
fn depth_of(s: &str, open: u8, close: u8) -> i32 {
    let (mut d, mut dq, mut sq) = (0i32, false, false);
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' if !sq => dq = !dq,
            b'\'' if !dq => sq = !sq,
            _ if dq || sq => {}
            _ if b[i] == open => d += 1,
            _ if b[i] == close => d -= 1,
            _ => {}
        }
        i += 1;
    }
    d
}

/// Split a flow collection body on top-level commas (quotes and
/// nested brackets skipped).
fn split_top(inner: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut d, mut dq, mut sq, mut last) = (0i32, false, false, 0usize);
    let b = inner.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' if !sq => {
                if !(dq && i > 0 && b[i - 1] == b'\\') {
                    dq = !dq;
                }
            }
            b'\'' if !dq => sq = !sq,
            b'[' | b'{' if !dq && !sq => d += 1,
            b']' | b'}' if !dq && !sq => d -= 1,
            b',' if !dq && !sq && d == 0 => {
                out.push(inner[last..i].to_string());
                last = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(inner[last..].to_string());
    out
}

/// Follow an alias chain to its anchored node.
fn resolve<'a>(n: &'a YNode, anchors: &'a HashMap<String, YNode>, depth: usize) -> &'a YNode {
    match n {
        YNode::Alias(a) if depth < 8 => anchors
            .get(a.as_str())
            .map(|t| resolve(t, anchors, depth + 1))
            .unwrap_or(n),
        _ => n,
    }
}

fn map_get<'a>(m: &'a YNode, key: &str) -> Option<&'a YNode> {
    if let YNode::Map(entries) = m {
        entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    } else {
        None
    }
}

fn scalar_of<'a>(n: &'a YNode, anchors: &'a HashMap<String, YNode>) -> Option<&'a str> {
    match resolve(n, anchors, 0) {
        YNode::Scalar(s) => Some(s.as_str()),
        _ => None,
    }
}

// ======================================================================
// Patch model + DB parsing
// ======================================================================

/// One applicable patch row — a (patch name × game title × serial ×
/// app version) tuple, mirroring Rpcs3PatchDefinition.
#[derive(Debug, Clone)]
pub struct PatchDef {
    pub ppu_hash: String,
    pub name: String,
    pub game_title: String,
    pub serial: String,
    pub app_version: String,
    pub author: String,
    pub notes: String,
    pub group: String,
    pub patch_version: String,
    pub enabled: bool,
}

pub fn entry_key(d: &PatchDef) -> String {
    [
        d.ppu_hash.as_str(),
        d.name.as_str(),
        d.game_title.as_str(),
        d.serial.as_str(),
        d.app_version.as_str(),
    ]
    .join(&KEY_SEP.to_string())
}

/// Full DB walk — every (hash, name, title, serial, version) row in
/// the document (unfiltered).
pub fn parse_patch_db(text: &str) -> Vec<PatchDef> {
    let (root, anchors) = YParser::parse(text);
    let YNode::Map(top) = &root else { return Vec::new() };
    let mut out = Vec::new();
    for (hash, sections) in top {
        if hash == "Version" || hash == "Anchors" {
            continue;
        }
        let YNode::Map(patches) = resolve(sections, &anchors, 0) else {
            continue;
        };
        for (name, defv) in patches {
            let dm = resolve(&defv, &anchors, 0);
            let get = |k: &str| {
                map_get(dm, k)
                    .and_then(|n| scalar_of(n, &anchors))
                    .map(|s| s.to_string())
                    .unwrap_or_default()
            };
            let author = get("Author");
            let notes = get("Notes");
            let group = get("Group");
            let patch_version = get("Patch Version");
            let Some(games) = map_get(dm, "Games") else { continue };
            let YNode::Map(games_map) = resolve(games, &anchors, 0) else {
                continue;
            };
            for (title, serials) in games_map {
                let YNode::Map(ser_map) = resolve(serials, &anchors, 0) else {
                    continue;
                };
                for (serial, vers) in ser_map {
                    let vers: Vec<String> = match resolve(vers, &anchors, 0) {
                        YNode::Seq(items) => items
                            .iter()
                            .filter_map(|i| scalar_of(i, &anchors))
                            .map(|s| s.to_string())
                            .collect(),
                        YNode::Scalar(s) => vec![s.to_string()],
                        _ => Vec::new(),
                    };
                    for app_version in vers {
                        out.push(PatchDef {
                            ppu_hash: hash.clone(),
                            name: name.clone(),
                            game_title: title.clone(),
                            serial: serial.clone(),
                            app_version,
                            author: author.clone(),
                            notes: notes.clone(),
                            group: group.clone(),
                            patch_version: patch_version.clone(),
                            enabled: false,
                        });
                    }
                }
            }
        }
    }
    out
}

// ======================================================================
// Paths + game ids
// ======================================================================

/// `config/patch_config.yml` — Rpcs3PatchesService.GetPatchConfigPath.
fn config_path(emu_dir: &Path) -> PathBuf {
    emu_dir.join("config").join("patch_config.yml")
}

fn patches_dir(emu_dir: &Path) -> PathBuf {
    emu_dir.join("patches")
}

/// App-version applicability (IsAppVersionApplicable): no detected
/// version accepts everything; "All" accepts everything; else equal.
fn app_ver_ok(patch_ver: &str, detected: &str) -> bool {
    let detected = detected.trim().trim_start_matches(['v', 'V']);
    detected.is_empty()
        || patch_ver.eq_ignore_ascii_case("All")
        || patch_ver
            .trim()
            .trim_start_matches(['v', 'V'])
            .eq_ignore_ascii_case(detected)
}

/// Load the patch rows applicable to `title_id`/`app_ver` from
/// `patches/patch.yml` + `imported_patch.yml`, flagged with the
/// enabled state from `config/patch_config.yml`.
pub fn load_for_game(
    emu_dir: &Path,
    title_id: &str,
    app_ver: &str,
) -> Result<Vec<PatchDef>, String> {
    let dir = patches_dir(emu_dir);
    let mut defs = Vec::new();
    let mut found = false;
    for name in [OFFICIAL_FILE, IMPORTED_FILE] {
        let Ok(text) = std::fs::read_to_string(dir.join(name)) else {
            continue;
        };
        found = true;
        defs.extend(parse_patch_db(&text));
    }
    if !found {
        return Err("No patch file found — download patches first.".into());
    }
    let enabled = enabled_keys(emu_dir);
    let mut seen = HashSet::new();
    defs.retain(|d| {
        d.serial.eq_ignore_ascii_case(title_id)
            && !d.game_title.contains(ARTEMIS_MARKER)
            && app_ver_ok(&d.app_version, app_ver)
    });
    defs.retain(|d| seen.insert(entry_key(d)));
    for d in &mut defs {
        d.enabled = enabled.contains(&entry_key(d));
    }
    Ok(defs)
}

// ======================================================================
// patch_config.yml — enabled-state load/save
// ======================================================================

/// `hash␟name␟title␟serial␟version` keys whose leaf is Enabled: true.
fn enabled_keys(emu_dir: &Path) -> HashSet<String> {
    let Ok(text) = std::fs::read_to_string(config_path(emu_dir)) else {
        return HashSet::new();
    };
    let (root, anchors) = YParser::parse(&text);
    let YNode::Map(hashes) = &root else {
        return HashSet::new();
    };
    let mut out = HashSet::new();
    for (hash, hm) in hashes {
        let YNode::Map(names) = resolve(hm, &anchors, 0) else {
            continue;
        };
        for (name, nm) in names {
            let YNode::Map(titles) = resolve(nm, &anchors, 0) else {
                continue;
            };
            for (title, tm) in titles {
                let YNode::Map(serials) = tm else { continue };
                for (serial, sm) in serials {
                    let YNode::Map(vers) = sm else { continue };
                    for (ver, vm) in vers {
                        let on = map_get(vm, "Enabled")
                            .and_then(|n| scalar_of(n, &anchors))
                            .is_some_and(|v| v.eq_ignore_ascii_case("true"));
                        if on {
                            out.insert(
                                [hash, name, title, serial, ver]
                                    .iter()
                                    .map(|s| s.as_str())
                                    .collect::<Vec<_>>()
                                    .join(&KEY_SEP.to_string()),
                            );
                        }
                    }
                }
            }
        }
    }
    out
}

/// Persist toggle states — Lacrima's SaveEnabledStates: enabled
/// entries get/keep an `Enabled: true` leaf under
/// hash→name→title→serial→version; disabled ones are removed and
/// empty parents pruned. Entries for other games stay untouched.
pub fn save_toggles(emu_dir: &Path, toggles: &[PatchDef]) -> Result<(), String> {
    let path = config_path(emu_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Config dir not writable: {e}"))?;
    }
    let mut root: Vec<(String, YNode)> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| match YParser::parse(&t).0 {
            YNode::Map(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default();
    for t in toggles {
        let chain = [
            t.ppu_hash.as_str(),
            t.name.as_str(),
            t.game_title.as_str(),
            t.serial.as_str(),
            t.app_version.as_str(),
        ];
        if t.enabled {
            let leaf = get_or_create(&mut root, &chain);
            map_insert(leaf, "Enabled".into(), YNode::Scalar("true".into()));
        } else {
            remove_toggle(&mut root, &chain);
        }
    }
    let mut out = String::new();
    serialize_map(&root, 0, &mut out);
    std::fs::write(&path, out).map_err(|e| format!("Failed to write patch_config.yml: {e}"))
}

fn get_or_create<'a>(
    m: &'a mut Vec<(String, YNode)>,
    path: &[&str],
) -> &'a mut Vec<(String, YNode)> {
    let i = match m.iter().position(|(k, _)| k == path[0]) {
        Some(i) => i,
        None => {
            m.push((path[0].to_string(), YNode::Map(Vec::new())));
            m.len() - 1
        }
    };
    let entry = &mut m[i].1;
    if !matches!(entry, YNode::Map(_)) {
        *entry = YNode::Map(Vec::new());
    }
    let YNode::Map(children) = entry else { unreachable!() };
    if path.len() == 1 {
        children
    } else {
        get_or_create(children, &path[1..])
    }
}

fn remove_toggle(m: &mut Vec<(String, YNode)>, path: &[&str]) -> bool {
    let Some(i) = m.iter().position(|(k, _)| *k == path[0]) else {
        return m.is_empty();
    };
    if path.len() == 1 {
        if let YNode::Map(children) = &mut m[i].1 {
            children.retain(|(k, _)| k != "Enabled");
        }
    } else if let YNode::Map(children) = &mut m[i].1 {
        if remove_toggle(children, &path[1..]) {
            m.remove(i);
            return m.is_empty();
        }
        return false;
    }
    if matches!(&m[i].1, YNode::Map(c) if c.is_empty()) {
        m.remove(i);
    }
    m.is_empty()
}

/// Quote when the scalar wouldn't survive as a plain YAML key —
/// names/titles contain `:` (aspect-ratio patches), `#`, quotes;
/// versions like `01.00` get quoted too (RPCS3's own writer does).
fn yaml_key(s: &str) -> String {
    if s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | ' '))
        && !s.ends_with(' ')
    {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn serialize_map(m: &[(String, YNode)], indent: usize, out: &mut String) {
    for (k, v) in m {
        for _ in 0..indent {
            out.push(' ');
        }
        out.push_str(&yaml_key(k));
        match v {
            YNode::Map(children) if !children.is_empty() => {
                out.push_str(":\n");
                serialize_map(children, indent + 2, out);
            }
            YNode::Map(_) => out.push_str(": {}\n"),
            YNode::Scalar(s) => {
                out.push_str(": ");
                if s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("false") {
                    out.push_str(s);
                } else {
                    out.push_str(&yaml_key(s));
                }
                out.push('\n');
            }
            YNode::Seq(_) | YNode::Alias(_) => out.push('\n'),
        }
    }
}

// ======================================================================
// Download — Lacrima's Rpcs3PatchesDownloadService
// ======================================================================

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// GET the patch DB — `&sha256=<local-hash>` lets the server answer
/// "already up to date" (return_code 1). New content is checksum
/// verified, parse-validated, the old file backed up as `.old`.
fn download_db(emu_dir: &Path) -> Result<String, String> {
    let dir = patches_dir(emu_dir);
    let patch_path = dir.join(OFFICIAL_FILE);
    let existing = std::fs::read(&patch_path).ok();
    let url = match &existing {
        Some(bytes) => format!("{DOWNLOAD_URL}&sha256={}", sha256_hex(bytes)),
        None => DOWNLOAD_URL.to_string(),
    };
    let mut resp = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .build(),
    )
    .get(&url)
    .header("User-Agent", emu_dl::USER_AGENT_PUBLIC)
    .call()
    .map_err(|e| format!("Patch download failed: {e}"))?;
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("Patch download failed: {e}"))?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("Bad patch response: {e}"))?;
    let rc = json.get("return_code").and_then(|v| v.as_i64()).unwrap_or(-1);
    match rc {
        1 => return Ok("Patch database is already up to date.".into()),
        0 => {}
        _ => {
            let msg = json
                .get("error")
                .or_else(|| json.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("Patch server returned code {rc}: {msg}"));
        }
    }
    let patch = json
        .get("patch")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Patch response carried no content".to_string())?;
    let version = json.get("version").and_then(|v| v.as_str()).unwrap_or("");
    if version != PATCH_ENGINE_VERSION {
        return Err(format!(
            "Patch engine version mismatch — server {version}, expected {PATCH_ENGINE_VERSION}"
        ));
    }
    if let Some(expect) = json.get("sha256").and_then(|v| v.as_str()) {
        if !expect.is_empty() && !sha256_hex(patch.as_bytes()).eq_ignore_ascii_case(expect) {
            return Err("Patch checksum mismatch — download discarded".into());
        }
    }
    let defs = parse_patch_db(patch);
    if defs.is_empty() {
        return Err("Downloaded patch data failed to parse".into());
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("Patches dir not writable: {e}"))?;
    if patch_path.exists() {
        let _ = std::fs::copy(&patch_path, patch_path.with_extension("yml.old"));
    }
    std::fs::write(&patch_path, patch).map_err(|e| format!("Failed to write patch.yml: {e}"))?;
    Ok(format!("Downloaded {} patch entries (engine {version}).", defs.len()))
}

/// Async wrapper matching `emu_dl::download_patches` — the overlay
/// polls the shared progress.
pub fn download(emu_dir: PathBuf) -> Shared {
    let dl: Shared = Arc::new(Mutex::new(DlProgress {
        stage: "Downloading RPCS3 patches…".into(),
        ..Default::default()
    }));
    let shared = dl.clone();
    std::thread::spawn(move || {
        let res = download_db(&emu_dir);
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

    const SAMPLE: &str = r#"Version: 1.2

Anchors:
  my_patch: &my_patch
    - [ be32, 0x1000, 0x6000 ]
  my_games: &my_games
    "Demon's Souls":
      BLUS30443: [01.00]
  my_notes: &my_notes "60 fps unlock notes"

Anchors:
  other: &other
    - [ be16, 0x2000, 0x981f ]

PPU-83681f6110d33442329073b72b8dc88a2f677172:
  "Unlock FPS":
    Games: *my_games
    Author: "Whatcookie, Gibbed"
    Notes: *my_notes
    Patch Version: 2.1
    Patch:
      - [ load, *my_patch ]
  "Skip Intros":
    Games:
      "Demon's Souls":
        BLUS30443: [All]
        BLES00932: [01.00, 01.03]
    Author: "GalCiv"
    Group: "Intro"
    Patch Version: 1.0
    Patch:
      - [ be32, 0x008eef78, 0x38000003 ] # skip intro
"#;

    fn find<'a>(defs: &'a [PatchDef], name: &str, serial: &str, ver: &str) -> &'a PatchDef {
        defs.iter()
            .find(|d| d.name == name && d.serial == serial && d.app_version == ver)
            .unwrap()
    }

    #[test]
    fn parses_aliases_and_dup_anchors() {
        let defs = parse_patch_db(SAMPLE);
        assert_eq!(defs.len(), 4);
        let d = find(&defs, "Unlock FPS", "BLUS30443", "01.00");
        assert_eq!(d.ppu_hash, "PPU-83681f6110d33442329073b72b8dc88a2f677172");
        assert_eq!(d.game_title, "Demon's Souls");
        assert_eq!(d.author, "Whatcookie, Gibbed");
        assert_eq!(d.notes, "60 fps unlock notes");
        assert_eq!(d.patch_version, "2.1");
        let d = find(&defs, "Skip Intros", "BLES00932", "01.03");
        assert_eq!(d.group, "Intro");
    }

    #[test]
    fn app_version_rules() {
        assert!(app_ver_ok("All", "01.00"));
        assert!(app_ver_ok("01.00", ""));
        assert!(app_ver_ok("1.00", "v1.00"));
        assert!(!app_ver_ok("01.00", "01.03"));
    }

    #[test]
    fn save_and_load_enabled_roundtrip() {
        let dir = std::env::temp_dir().join(format!("aes_patch_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let defs = parse_patch_db(SAMPLE);
        let mut keep = find(&defs, "Unlock FPS", "BLUS30443", "01.00").clone();
        keep.enabled = true;
        let mut off = find(&defs, "Skip Intros", "BLUS30443", "All").clone();
        off.enabled = false;
        save_toggles(&dir, &[keep.clone(), off.clone()]).unwrap();
        let keys = enabled_keys(&dir);
        assert!(keys.contains(&entry_key(&keep)));
        assert!(!keys.contains(&entry_key(&off)));
        // Flip off → leaf removed, empty parents pruned.
        let mut keep2 = keep.clone();
        keep2.enabled = false;
        save_toggles(&dir, &[keep2]).unwrap();
        let keys = enabled_keys(&dir);
        assert!(!keys.contains(&entry_key(&keep)));
        let text = std::fs::read_to_string(config_path(&dir)).unwrap();
        assert!(!text.contains("Unlock FPS"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_patch_yml_parses() {
        let p = std::path::Path::new(r"C:\Users\Admin\AppData\Local\Temp\aesdiag\patch.yml");
        if !p.exists() {
            return; // dev-only fixture
        }
        let text = std::fs::read_to_string(p).unwrap();
        let defs = parse_patch_db(&text);
        assert!(defs.len() > 1000);
        let des = defs
            .iter()
            .filter(|d| d.serial == "BLUS30443")
            .collect::<Vec<_>>();
        assert!(des.iter().any(|d| d.name == "Unlock FPS"));
        assert!(des.iter().all(|d| d.game_title == "Demon's Souls"));
    }
}
