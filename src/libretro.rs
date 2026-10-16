//! Libretro core discovery and installation — each RetroArch-backed
//! section (3DS, N64) downloads its own core set here and hands the
//! selected one to RetroArch via `-L`.
//!
//! Core metadata comes from the official Libretro info bundle; only
//! cores advertising the section's file extensions + system metadata
//! are offered.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

const INFO_BUNDLE_URL: &str = "https://buildbot.libretro.com/assets/frontend/info.zip";
const CORE_DOWNLOAD_ROOT: &str = "https://buildbot.libretro.com/nightly";
const USER_AGENT: &str = "AES_Rustified-Libretro/1.0";

/// A RetroArch-backed section with its own Libretro core set —
/// `Emulators/<dir>/Libretro` holds `cores/`, `info/` and the
/// per-section catalog json. Each section's download only offers
/// cores matching its system filter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    ThreeDs,
    N64,
    Nes,
    Genesis,
    Snes,
    Gba,
}

impl Section {
    /// Section key ("3DS", "N64", "NES", "GENESIS", "SNES", "GBA") →
    /// core section. `None` for sections with no RetroArch handler.
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "3DS" => Some(Self::ThreeDs),
            "N64" => Some(Self::N64),
            "NES" => Some(Self::Nes),
            "GENESIS" => Some(Self::Genesis),
            "SNES" => Some(Self::Snes),
            "GBA" => Some(Self::Gba),
            _ => None,
        }
    }

    /// The catalog section key — same string as the `Emulators/`
    /// subdir for every RetroArch-backed section.
    pub fn key(self) -> &'static str {
        self.dir()
    }

    fn dir(self) -> &'static str {
        match self {
            Self::ThreeDs => "3DS",
            Self::N64 => "N64",
            Self::Nes => "NES",
            Self::Genesis => "GENESIS",
            Self::Snes => "SNES",
            Self::Gba => "GBA",
        }
    }

    fn catalog(self) -> &'static str {
        match self {
            Self::ThreeDs => "3ds_cores.json",
            Self::N64 => "n64_cores.json",
            Self::Nes => "nes_cores.json",
            Self::Genesis => "genesis_cores.json",
            Self::Snes => "snes_cores.json",
            Self::Gba => "gba_cores.json",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ThreeDs => "3DS",
            Self::N64 => "N64",
            Self::Nes => "NES",
            Self::Genesis => "Genesis",
            Self::Snes => "SNES",
            Self::Gba => "GBA",
        }
    }

    /// The section's core filter — which info-file metadata makes a
    /// core an offer for this section.
    fn accepts(self, fields: &std::collections::BTreeMap<String, String>) -> bool {
        match self {
            Self::ThreeDs => is_3ds_info(fields),
            Self::N64 => is_n64_info(fields),
            Self::Nes => is_nes_info(fields),
            Self::Genesis => is_genesis_info(fields),
            Self::Snes => is_snes_info(fields),
            Self::Gba => is_gba_info(fields),
        }
    }
}

struct CoreTarget {
    buildbot_subdir: &'static str,
    library_extension: &'static str,
}

fn core_target() -> Result<CoreTarget, String> {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        return Ok(CoreTarget {
            buildbot_subdir: "windows/x86_64/latest",
            library_extension: "dll",
        });
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        return Ok(CoreTarget {
            buildbot_subdir: "windows/arm64/latest",
            library_extension: "dll",
        });
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return Ok(CoreTarget {
            buildbot_subdir: "linux/x86_64/latest",
            library_extension: "so",
        });
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        return Ok(CoreTarget {
            buildbot_subdir: "linux/aarch64/latest",
            library_extension: "so",
        });
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        return Ok(CoreTarget {
            buildbot_subdir: "apple/osx/x86_64/latest",
            library_extension: "dylib",
        });
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Ok(CoreTarget {
            buildbot_subdir: "apple/osx/arm64/latest",
            library_extension: "dylib",
        });
    }
    #[allow(unreachable_code)]
    Err(format!(
        "Libretro core downloads are not published for {}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ))
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreInfo {
    /// Buildbot basename, e.g. `azahar_libretro`.
    pub id: String,
    pub display_name: String,
    pub corename: String,
    pub supported_extensions: Vec<String>,
    pub required_hw_api: String,
    pub core_path: PathBuf,
    pub info_path: PathBuf,
}

pub type CoreDownload = crate::emu_dl::Shared;

pub fn core_root(section: Section) -> Option<PathBuf> {
    crate::settings::data_root().map(|p| p.join("Emulators").join(section.dir()).join("Libretro"))
}

pub fn installed_cores(section: Section) -> Vec<CoreInfo> {
    let Some(root) = core_root(section) else {
        return Vec::new();
    };
    let Ok(data) = std::fs::read(root.join(section.catalog())) else {
        return Vec::new();
    };
    let Ok(mut cores) = serde_json::from_slice::<Vec<CoreInfo>>(&data) else {
        return Vec::new();
    };
    cores.retain(|c| c.core_path.is_file() && c.info_path.is_file());
    sort_cores(&mut cores);
    cores
}

/// Resolve a stored core id, falling back to the first installed core.
/// There is deliberately no separate "Set default" action.
pub fn resolve_core(section: Section, selected_id: &str) -> Option<CoreInfo> {
    let cores = installed_cores(section);
    select_core(&cores, selected_id)
}

fn select_core(cores: &[CoreInfo], selected_id: &str) -> Option<CoreInfo> {
    cores
        .iter()
        .find(|c| c.id.eq_ignore_ascii_case(selected_id))
        .cloned()
        .or_else(|| cores.first().cloned())
}

/// Download every core the section's filter accepts — the N64
/// section pulls only N64 cores, the 3DS section only 3DS cores.
pub fn download_cores(section: Section) -> CoreDownload {
    let progress = Arc::new(Mutex::new(crate::emu_dl::DlProgress {
        stage: format!("Preparing {} core catalog…", section.label()),
        ..Default::default()
    }));
    let worker = progress.clone();
    std::thread::spawn(move || {
        let result = install_cores(section, &worker);
        worker.lock().unwrap_or_else(|e| e.into_inner()).done = Some(result);
    });
    progress
}

fn install_cores(section: Section, progress: &CoreDownload) -> Result<String, String> {
    let target = core_target()?;
    let root = core_root(section).ok_or_else(|| "AES data directory is unavailable".to_string())?;
    let temp = root.join("Emu_Update");
    let cores_dir = root.join("cores");
    let info_dir = root.join("info");
    std::fs::create_dir_all(&temp).map_err(|e| format!("Cannot create update directory: {e}"))?;
    std::fs::create_dir_all(&cores_dir)
        .map_err(|e| format!("Cannot create core directory: {e}"))?;
    std::fs::create_dir_all(&info_dir).map_err(|e| format!("Cannot create info directory: {e}"))?;

    let bundle_path = temp.join("info.zip");
    set_progress(progress, "Downloading Libretro core catalog…", 0.0);
    download_file(progress, INFO_BUNDLE_URL, &bundle_path, 0.0, 0.12)?;
    let candidates = read_core_infos(&bundle_path, section)?;
    if candidates.is_empty() {
        return Err(format!(
            "The Libretro core catalog contains no {} cores",
            section.label()
        ));
    }

    let total = candidates.len();
    let mut installed = installed_cores(section);
    let mut failures = Vec::new();
    for (index, (id, info_bytes, metadata)) in candidates.into_iter().enumerate() {
        let start = 0.12 + 0.86 * index as f32 / total as f32;
        let end = 0.12 + 0.86 * (index + 1) as f32 / total as f32;
        if installed.iter().any(|c| c.id.eq_ignore_ascii_case(&id)) {
            set_progress(
                progress,
                &format!("{} is already installed", metadata.display_name),
                end,
            );
            continue;
        }
        let core_url = format!(
            "{CORE_DOWNLOAD_ROOT}/{}/{id}.{}.zip",
            target.buildbot_subdir, target.library_extension
        );
        let archive_path = temp.join(format!("{id}.{}.zip", target.library_extension));
        set_progress(
            progress,
            &format!("Downloading {}…", metadata.display_name),
            start,
        );
        if let Err(e) = download_file(
            progress,
            &core_url,
            &archive_path,
            start,
            start + (end - start) * 0.75,
        ) {
            failures.push(format!("{}: {e}", metadata.display_name));
            continue;
        }

        let core_dir = cores_dir.join(&id);
        if let Err(e) = extract_core_zip(&archive_path, &core_dir) {
            failures.push(format!("{}: {e}", metadata.display_name));
            continue;
        }
        let Some(core_path) = find_core_library(&core_dir, &id, target.library_extension) else {
            failures.push(format!(
                "{}: package did not contain its core library",
                metadata.display_name
            ));
            continue;
        };
        let info_path = info_dir.join(format!("{id}.info"));
        if let Err(e) = std::fs::write(&info_path, info_bytes) {
            failures.push(format!(
                "{}: cannot save metadata: {e}",
                metadata.display_name
            ));
            continue;
        }
        installed.push(CoreInfo {
            core_path,
            info_path,
            ..metadata
        });
    }

    if installed.is_empty() {
        return Err(if failures.is_empty() {
            format!("No {} core packages were installed", section.label())
        } else {
            failures.join("\n")
        });
    }
    sort_cores(&mut installed);
    let manifest = serde_json::to_vec_pretty(&installed)
        .map_err(|e| format!("Cannot serialize {} core catalog: {e}", section.label()))?;
    std::fs::write(root.join(section.catalog()), manifest)
        .map_err(|e| format!("Cannot save {} core catalog: {e}", section.label()))?;
    let _ = std::fs::remove_dir_all(&temp);
    set_progress(
        progress,
        &format!("{} cores installed", section.label()),
        1.0,
    );
    let summary = format!("Installed {} {} core(s)", installed.len(), section.label());
    if failures.is_empty() {
        Ok(summary)
    } else {
        Ok(format!(
            "{summary}; some packages failed: {}",
            failures.join("; ")
        ))
    }
}

fn read_core_infos(
    archive_path: &Path,
    section: Section,
) -> Result<Vec<(String, Vec<u8>, CoreInfo)>, String> {
    let file =
        std::fs::File::open(archive_path).map_err(|e| format!("Invalid info archive: {e}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("Invalid info archive: {e}"))?;
    let mut cores = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("Cannot read info archive: {e}"))?;
        if entry.is_dir() {
            continue;
        }
        let entry_name = entry
            .name()
            .map_err(|e| format!("Cannot read info entry name: {e}"))?;
        if entry_name
            .rsplit('/')
            .next()
            .is_none_or(|n| !n.to_ascii_lowercase().ends_with(".info"))
        {
            continue;
        }
        let filename = entry_name
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string();
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| format!("Cannot read {filename}: {e}"))?;
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let fields = parse_info_fields(text);
        if !section.accepts(&fields) {
            continue;
        }
        let id = filename.trim_end_matches(".info").to_string();
        if !id.ends_with("_libretro")
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            continue;
        }
        let corename = fields
            .get("corename")
            .cloned()
            .unwrap_or_else(|| id.clone());
        let display_name = fields
            .get("display_name")
            .cloned()
            .unwrap_or_else(|| format!("Nintendo {} ({corename})", section.label()));
        let supported_extensions = fields
            .get("supported_extensions")
            .map(|v| {
                v.split('|')
                    .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        cores.push((
            id.clone(),
            bytes,
            CoreInfo {
                id,
                display_name,
                corename,
                supported_extensions,
                required_hw_api: fields.get("required_hw_api").cloned().unwrap_or_default(),
                core_path: PathBuf::new(),
                info_path: PathBuf::new(),
            },
        ));
    }
    cores.sort_by(|a, b| a.0.cmp(&b.0));
    cores.dedup_by(|a, b| a.0.eq_ignore_ascii_case(&b.0));
    Ok(cores)
}

fn parse_info_fields(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut fields = std::collections::BTreeMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim().trim_matches('"').replace("\\\"", "\"");
        fields.insert(key.to_ascii_lowercase(), value);
    }
    fields
}

fn is_3ds_info(fields: &std::collections::BTreeMap<String, String>) -> bool {
    let system = fields.get("systemname").map(String::as_str).unwrap_or("");
    let database = fields.get("database").map(String::as_str).unwrap_or("");
    let extensions = fields
        .get("supported_extensions")
        .map(String::as_str)
        .unwrap_or("");
    let has_3ds_extension = extensions.split('|').any(|e| {
        matches!(
            e.trim()
                .trim_start_matches('.')
                .to_ascii_lowercase()
                .as_str(),
            "3ds" | "3dsx" | "cci" | "cxi" | "cia" | "app"
        )
    });
    has_3ds_extension
        && (system.to_ascii_lowercase().contains("3ds")
            || database.to_ascii_lowercase().contains("nintendo 3ds"))
}

/// N64 counterpart — `systemname`/`database` must say Nintendo 64
/// and the extension list must carry a real N64 container
/// (mupen64plus_next, parallel_n64).
fn is_n64_info(fields: &std::collections::BTreeMap<String, String>) -> bool {
    let system = fields.get("systemname").map(String::as_str).unwrap_or("");
    let database = fields.get("database").map(String::as_str).unwrap_or("");
    let extensions = fields
        .get("supported_extensions")
        .map(String::as_str)
        .unwrap_or("");
    let has_n64_extension = extensions.split('|').any(|e| {
        matches!(
            e.trim()
                .trim_start_matches('.')
                .to_ascii_lowercase()
                .as_str(),
            "n64" | "z64" | "v64"
        )
    });
    let meta = [system, database]
        .iter()
        .any(|v| v.to_ascii_lowercase().contains("nintendo 64"));
    has_n64_extension && meta
}

/// NES counterpart — "nes" is a substring of "snes" and "genesis"
/// and SNES metadata contains the whole "nintendo entertainment
/// system" phrase, so match metadata as *tokens* (alphanumeric runs)
/// plus the Famicom name, and reject any SNES hint first.
/// (nestopia, fceumm, mesen, quicknes.)
fn is_nes_info(fields: &std::collections::BTreeMap<String, String>) -> bool {
    let system = fields.get("systemname").map(String::as_str).unwrap_or("");
    let database = fields.get("database").map(String::as_str).unwrap_or("");
    let display = fields.get("display_name").map(String::as_str).unwrap_or("");
    let extensions = fields
        .get("supported_extensions")
        .map(String::as_str)
        .unwrap_or("");
    let has_nes_extension = extensions.split('|').any(|e| {
        matches!(
            e.trim()
                .trim_start_matches('.')
                .to_ascii_lowercase()
                .as_str(),
            "nes" | "fds" | "unf" | "unif"
        )
    });
    let tokens = |v: &str| -> Vec<String> {
        v.to_ascii_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    };
    let mut nes_meta = false;
    for v in [system, database, display] {
        let lower = v.to_ascii_lowercase();
        let toks = tokens(v);
        // "Super Nintendo Entertainment System" contains the whole
        // NES phrase — skip SNES-marked fields; a clean field on a
        // multi-system core can still qualify it.
        if lower.contains("super nintendo") || toks.iter().any(|t| t == "snes") {
            continue;
        }
        if lower.contains("nintendo entertainment system")
            || toks.iter().any(|t| t == "nes" || t == "famicom")
        {
            nes_meta = true;
        }
    }
    has_nes_extension && nes_meta
}

/// Sega Genesis/Mega Drive counterpart — genplusgx, picodrive,
/// blastem. Meta must name a 16-bit Sega system (the token set
/// covers "Sega - MS/GG/MD/CD" and "Mega Drive" spellings) and the
/// extension list must carry a Genesis container — Saturn/Game Gear
/// cores stay out on both axes.
fn is_genesis_info(fields: &std::collections::BTreeMap<String, String>) -> bool {
    let system = fields.get("systemname").map(String::as_str).unwrap_or("");
    let database = fields.get("database").map(String::as_str).unwrap_or("");
    let display = fields.get("display_name").map(String::as_str).unwrap_or("");
    let extensions = fields
        .get("supported_extensions")
        .map(String::as_str)
        .unwrap_or("");
    let has_gen_extension = extensions.split('|').any(|e| {
        matches!(
            e.trim()
                .trim_start_matches('.')
                .to_ascii_lowercase()
                .as_str(),
            "md" | "gen" | "smd" | "32x" | "68k" | "sg"
        )
    });
    let meta = [system, database, display].iter().any(|v| {
        let lower = v.to_ascii_lowercase();
        lower.contains("genesis")
            || lower.contains("mega drive")
            || lower.contains("megadrive")
            || lower.contains("mega cd")
            || lower.contains("sega cd")
            || lower.contains("32x")
            || lower
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|t| t == "md")
    });
    has_gen_extension && meta
}

/// SNES counterpart — snes9x/bsnes/mesen-s. "snes" appears in core
/// names as a substring (bsnes, snes9x) so a plain `contains` is the
/// right test here; the extension list still has to carry a real
/// Super Famicom container.
fn is_snes_info(fields: &std::collections::BTreeMap<String, String>) -> bool {
    let system = fields.get("systemname").map(String::as_str).unwrap_or("");
    let database = fields.get("database").map(String::as_str).unwrap_or("");
    let display = fields.get("display_name").map(String::as_str).unwrap_or("");
    let extensions = fields
        .get("supported_extensions")
        .map(String::as_str)
        .unwrap_or("");
    let has_snes_extension = extensions.split('|').any(|e| {
        matches!(
            e.trim()
                .trim_start_matches('.')
                .to_ascii_lowercase()
                .as_str(),
            "sfc" | "smc" | "fig" | "swc" | "bs" | "bsx" | "st"
        )
    });
    let meta = [system, database, display].iter().any(|v| {
        let lower = v.to_ascii_lowercase();
        lower.contains("snes") || lower.contains("super nintendo")
    });
    has_snes_extension && meta
}

/// GBA counterpart — mgba/vbam/gpsp. Meta must name the handheld
/// (token "gba" or the "game boy advance" phrase — GB/GBC-only cores
/// like gambatte match neither) and carry a `.gba` extension.
fn is_gba_info(fields: &std::collections::BTreeMap<String, String>) -> bool {
    let system = fields.get("systemname").map(String::as_str).unwrap_or("");
    let database = fields.get("database").map(String::as_str).unwrap_or("");
    let display = fields.get("display_name").map(String::as_str).unwrap_or("");
    let extensions = fields
        .get("supported_extensions")
        .map(String::as_str)
        .unwrap_or("");
    let has_gba_extension = extensions.split('|').any(|e| {
        matches!(
            e.trim()
                .trim_start_matches('.')
                .to_ascii_lowercase()
                .as_str(),
            "gba" | "agb"
        )
    });
    let meta = [system, database, display].iter().any(|v| {
        let lower = v.to_ascii_lowercase();
        lower.contains("game boy advance")
            || lower
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|t| t == "gba")
    });
    has_gba_extension && meta
}

fn extract_core_zip(archive_path: &Path, destination: &Path) -> Result<(), String> {
    std::fs::create_dir_all(destination)
        .map_err(|e| format!("Cannot create core directory: {e}"))?;
    let file =
        std::fs::File::open(archive_path).map_err(|e| format!("Invalid core package: {e}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("Invalid core package: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("Cannot read core package: {e}"))?;
        let Some(relative) = entry.enclosed_name() else {
            continue;
        };
        let output = destination.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&output)
                .map_err(|e| format!("Cannot create package directory: {e}"))?;
            continue;
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Cannot create package directory: {e}"))?;
        }
        let mut out =
            std::fs::File::create(&output).map_err(|e| format!("Cannot write core file: {e}"))?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|e| format!("Cannot extract core file: {e}"))?;
    }
    Ok(())
}

fn find_core_library(root: &Path, id: &str, extension: &str) -> Option<PathBuf> {
    let exact = format!("{id}.{extension}");
    let mut files = Vec::new();
    collect_files(root, &mut files);
    files
        .iter()
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case(&exact))
        })
        .cloned()
        .or_else(|| {
            files.into_iter().find(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case(extension))
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.to_ascii_lowercase().contains("libretro"))
            })
        })
}

fn collect_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn download_file(
    progress: &CoreDownload,
    url: &str,
    destination: &Path,
    progress_start: f32,
    progress_end: f32,
) -> Result<(), String> {
    let mut response = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(600)))
            .build(),
    )
    .get(url)
    .header("User-Agent", USER_AGENT)
    .call()
    .map_err(|e| format!("Download failed: {e}"))?;
    let total = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let mut output =
        std::fs::File::create(destination).map_err(|e| format!("Cannot create download: {e}"))?;
    let mut reader = response.body_mut().as_reader();
    let mut buffer = [0u8; 32 * 1024];
    let mut received = 0u64;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|e| format!("Download failed: {e}"))?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(|e| format!("Cannot write download: {e}"))?;
        received += count as u64;
        if let Some(total) = total {
            let fraction = (received as f32 / total.max(1) as f32).clamp(0.0, 1.0);
            set_progress(
                progress,
                "Downloading core packages…",
                progress_start + (progress_end - progress_start) * fraction,
            );
        }
    }
    Ok(())
}

fn sort_cores(cores: &mut [CoreInfo]) {
    cores.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
}

fn set_progress(progress: &CoreDownload, stage: &str, fraction: f32) {
    let mut state = progress.lock().unwrap_or_else(|e| e.into_inner());
    state.stage = stage.to_string();
    state.frac = fraction.clamp(0.0, 1.0);
}

#[cfg(test)]
mod tests {
    use super::{
        is_3ds_info, is_gba_info, is_genesis_info, is_n64_info, is_nes_info,
        is_snes_info, parse_info_fields, select_core, CoreInfo,
    };

    #[test]
    fn core_selection_defaults_to_first_and_tracks_user_choice() {
        let cores = vec![
            CoreInfo {
                id: "azahar_libretro".into(),
                display_name: "Azahar".into(),
                ..Default::default()
            },
            CoreInfo {
                id: "citra_libretro".into(),
                display_name: "Citra".into(),
                ..Default::default()
            },
        ];
        assert_eq!(select_core(&cores, "").unwrap().id, "azahar_libretro");
        assert_eq!(
            select_core(&cores, "citra_libretro").unwrap().id,
            "citra_libretro"
        );
    }

    #[test]
    fn core_info_filter_requires_3ds_metadata_and_extension() {
        let info = parse_info_fields(
            r#"
                corename = "Azahar"
                display_name = "Nintendo - 3DS (Azahar)"
                systemname = "3DS"
                supported_extensions = "3ds|3dsx|cci|cxi|cia"
                required_hw_api = "OpenGL Core >= 3.3"
            "#,
        );
        assert!(is_3ds_info(&info));
        assert_eq!(info["corename"], "Azahar");
        assert_eq!(info["required_hw_api"], "OpenGL Core >= 3.3");
    }

    #[test]
    fn core_info_filter_rejects_non_3ds_systems() {
        let info = parse_info_fields(
            r#"
                corename = "Some Core"
                systemname = "Nintendo DS"
                supported_extensions = "3ds|3dsx"
            "#,
        );
        assert!(!is_3ds_info(&info));
    }

    #[test]
    fn n64_info_filter_accepts_mupen_and_parallel() {
        let mupen = parse_info_fields(
            r#"
                corename = "Mupen64Plus-Next"
                display_name = "Nintendo - Nintendo 64 (Mupen64Plus-Next)"
                systemname = "Nintendo 64"
                database = "Nintendo - Nintendo 64"
                supported_extensions = "n64|v64|z64|bin|u1|ndd"
            "#,
        );
        assert!(is_n64_info(&mupen));
        assert!(!is_3ds_info(&mupen));
    }

    #[test]
    fn n64_info_filter_rejects_non_n64() {
        let azahar = parse_info_fields(
            r#"
                corename = "Azahar"
                systemname = "3DS"
                supported_extensions = "3ds|3dsx|cci|cxi|cia"
            "#,
        );
        assert!(!is_n64_info(&azahar));
        // Extension match without Nintendo-64 metadata is not enough
        // — keeps odd multi-system cores out of the N64 offer.
        let stray = parse_info_fields(
            r#"
                corename = "Arcade Core"
                systemname = "Arcade"
                supported_extensions = "n64|zip"
            "#,
        );
        assert!(!is_n64_info(&stray));
    }

    #[test]
    fn nes_info_filter_accepts_fceumm_and_nestopia() {
        let fceumm = parse_info_fields(
            r#"
                corename = "FCEUmm"
                display_name = "Nintendo - NES / Famicom (FCEUmm)"
                systemname = "Nintendo Entertainment System"
                database = "Nintendo - Nintendo Entertainment System"
                supported_extensions = "fds|nes|unf|unif"
            "#,
        );
        assert!(is_nes_info(&fceumm));
        assert!(!is_3ds_info(&fceumm));
        assert!(!is_n64_info(&fceumm));
        let nestopia = parse_info_fields(
            r#"
                corename = "Nestopia UE"
                display_name = "Nintendo - NES / Famicom (Nestopia UE)"
                systemname = "Nintendo Entertainment System"
                supported_extensions = "nes|fds|unf|unif|nsf|nsfe"
            "#,
        );
        assert!(is_nes_info(&nestopia));
    }

    #[test]
    fn nes_info_filter_rejects_snes_and_genesis() {
        // SNES metadata contains the whole "nintendo entertainment
        // system" phrase — must be rejected, not substring-matched.
        let snes9x = parse_info_fields(
            r#"
                corename = "Snes9x"
                display_name = "Nintendo - SNES / SFC (Snes9x)"
                systemname = "Super Nintendo Entertainment System"
                database = "Nintendo - Super Nintendo Entertainment System"
                supported_extensions = "smc|sfc|swc|fig|bs|st"
            "#,
        );
        assert!(!is_nes_info(&snes9x));
        // "genesis" contains "nes" — and a stray nes extension alone
        // without NES system metadata is not enough.
        let genesis = parse_info_fields(
            r#"
                corename = "Genesis Plus GX"
                display_name = "Sega - MS/GG/MD/CD (Genesis Plus GX)"
                systemname = "Mega Drive - Genesis"
                supported_extensions = "md|smd|gen|bin|nes|sm64"
            "#,
        );
        assert!(!is_nes_info(&genesis));
        // Citra-family stays out too.
        let azahar = parse_info_fields(
            r#"
                corename = "Azahar"
                systemname = "3DS"
                supported_extensions = "3ds|3dsx|cci|cxi|cia"
            "#,
        );
        assert!(!is_nes_info(&azahar));
    }

    #[test]
    fn genesis_info_filter_accepts_genplusgx_and_picodrive() {
        let genplusgx = parse_info_fields(
            r#"
                corename = "Genesis Plus GX"
                display_name = "Sega - MS/GG/MD/CD (Genesis Plus GX)"
                systemname = "Sega 8/16-bit (Various)"
                supported_extensions = "mdx|md|smd|gen|bin|cue|iso|sms|68k|sg|chd|m3u"
            "#,
        );
        assert!(is_genesis_info(&genplusgx));
        let picodrive = parse_info_fields(
            r#"
                corename = "PicoDrive"
                display_name = "Sega - Mega Drive/Sega CD/32X (PicoDrive)"
                systemname = "Sega Mega Drive"
                supported_extensions = "bin|gen|smd|md|32x|cue|iso|chd|sms"
            "#,
        );
        assert!(is_genesis_info(&picodrive));
    }

    #[test]
    fn genesis_info_filter_rejects_saturn_and_sms() {
        let saturn = parse_info_fields(
            r#"
                corename = "Beetle Saturn"
                display_name = "Sega - Saturn (Beetle Saturn)"
                systemname = "Sega Saturn"
                database = "Sega - Saturn"
                supported_extensions = "cue|ccd|chd|iso|img|mds|toc"
            "#,
        );
        assert!(!is_genesis_info(&saturn));
        // Master System only — no Genesis metadata, no Genesis
        // cartridge extension.
        let sms = parse_info_fields(
            r#"
                corename = "SMS Plus GX"
                display_name = "Sega - MS/GG (SMS Plus GX)"
                systemname = "Sega Master System"
                supported_extensions = "sms|gg|sg|col|rom|bin"
            "#,
        );
        assert!(!is_genesis_info(&sms));
    }

    #[test]
    fn snes_info_filter_accepts_snes9x_and_bsnes() {
        let snes9x = parse_info_fields(
            r#"
                corename = "Snes9x"
                display_name = "Nintendo - SNES / SFC (Snes9x)"
                systemname = "Super Nintendo Entertainment System"
                supported_extensions = "smc|fig|sfc|gd3|gd7|dx2|bsx|bs|swc"
            "#,
        );
        assert!(is_snes_info(&snes9x));
        let bsnes = parse_info_fields(
            r#"
                corename = "bsnes-mercury"
                display_name = "Nintendo - SNES / Famicom (bsnes-mercury)"
                database = "Nintendo - Super Nintendo Entertainment System"
                supported_extensions = "sfc|smc|bml|rom"
            "#,
        );
        assert!(is_snes_info(&bsnes));
    }

    #[test]
    fn snes_info_filter_rejects_nes_and_genesis() {
        let fceumm = parse_info_fields(
            r#"
                corename = "FCEUmm"
                display_name = "Nintendo - NES / Famicom (FCEUmm)"
                systemname = "Nintendo Entertainment System"
                supported_extensions = "fds|nes|unf|unif"
            "#,
        );
        assert!(!is_snes_info(&fceumm));
        let genplusgx = parse_info_fields(
            r#"
                corename = "Genesis Plus GX"
                display_name = "Sega - MS/GG/MD/CD (Genesis Plus GX)"
                supported_extensions = "mdx|md|smd|gen|bin|cue|iso|sms|68k|sg"
            "#,
        );
        assert!(!is_snes_info(&genplusgx));
    }

    #[test]
    fn gba_info_filter_accepts_mgba_rejects_gambatte() {
        let mgba = parse_info_fields(
            r#"
                corename = "mGBA"
                display_name = "Nintendo - Game Boy Advance (mGBA)"
                systemname = "Game Boy Advance"
                supported_extensions = "gba|agb|gb|gbc|mb|elf"
            "#,
        );
        assert!(is_gba_info(&mgba));
        // GB/GBC only — "game boy" alone is not "game boy advance".
        let gambatte = parse_info_fields(
            r#"
                corename = "Gambatte"
                display_name = "Nintendo - Game Boy / Color (Gambatte)"
                systemname = "Game Boy/Game Boy Color"
                supported_extensions = "gb|gbc|dmg"
            "#,
        );
        assert!(!is_gba_info(&gambatte));
        let snes9x = parse_info_fields(
            r#"
                corename = "Snes9x"
                systemname = "Super Nintendo Entertainment System"
                supported_extensions = "smc|sfc|swc"
            "#,
        );
        assert!(!is_gba_info(&snes9x));
    }

    #[test]
    fn gba_info_filter_accepts_real_buildbot_metadata() {
        // The actual mgba_libretro.info from the buildbot info.zip —
        // its extension list leads with gb|gbc and its metadata is a
        // combined "Game Boy/Game Boy Color/Game Boy Advance" string,
        // both of which a naive "starts with gba" check misses.
        let mgba = parse_info_fields(
            r#"
                corename = "mGBA"
                display_name = "Nintendo - Game Boy Advance (mGBA)"
                systemname = "Game Boy/Game Boy Color/Game Boy Advance"
                database = "Nintendo - Game Boy|Nintendo - Game Boy Color|Nintendo - Game Boy Advance"
                supported_extensions = "gb|gbc|gba"
            "#,
        );
        assert!(is_gba_info(&mgba));
        // And the other real GBA entries from the same bundle.
        for (name, system) in [
            ("gpSP", "Game Boy Advance"),
            ("VBA-M", "Game Boy/Game Boy Color/Game Boy Advance"),
            ("SkyEmu", "Game Boy/Game Boy Color/Game Boy Advance"),
        ] {
            let info = parse_info_fields(&format!(
                "corename = \"{name}\"\nsystemname = \"{system}\"\nsupported_extensions = \"gba|gb|gbc\""
            ));
            assert!(is_gba_info(&info), "{name} should be a GBA core");
        }
    }

    #[test]
    fn section_key_roundtrips_from_card_key() {
        // The settings cards and download jobs key sections by the
        // same string — a mapping slip shows the wrong card's
        // progress or refreshes the wrong catalog.
        for key in ["3DS", "N64", "NES", "GENESIS", "SNES", "GBA"] {
            let section = super::Section::from_key(key).unwrap();
            assert_eq!(section.key(), key);
        }
        assert!(super::Section::from_key("PS2").is_none());
    }
}
