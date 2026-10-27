//! yt-dlp management + metadata — a port of AES_Lacrima's
//! `YtDlpManager` and `YtDlpMetadata`.
//!
//! The binary lives in the per-user tools dir
//! (`%LOCALAPPDATA%\AES_Oxide\Tools\yt-dlp.exe`) and is resolved
//! from there first, PATH second — Lacrima's `FindLocalExecutable`
//! order. Install/update download the latest GitHub release
//! (`yt-dlp_win.zip` preferred, plain `yt-dlp.exe` fallback), update
//! checks use the releases API with a 15-minute ETag cache.
//!
//! All blocking work runs on worker threads; the settings UI drives
//! [`Ui`] which reports status/progress back over a channel.

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;

const APP_DIR: &str = "AES_Oxide";
const REPO: &str = "yt-dlp/yt-dlp";
const USER_AGENT: &str = "Mozilla/5.0 (Compatible; YtDlpDownloader; AES_Oxide)";
/// GitHub latest-version results stay fresh for this long (Lacrima's
/// 15-minute cache window).
const CACHE_TTL_SECS: u64 = 15 * 60;

#[cfg(windows)]
const BIN_NAME: &str = "yt-dlp.exe";
#[cfg(not(windows))]
const BIN_NAME: &str = "yt-dlp";

// ------------------------------------------------------------------
// Paths / process plumbing
// ------------------------------------------------------------------

fn data_root() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join(APP_DIR))
}

/// `ApplicationPaths.ToolsDirectory`.
pub fn tools_dir() -> PathBuf {
    data_root()
        .map(|d| d.join("Tools"))
        .unwrap_or_else(|| PathBuf::from("Tools"))
}

/// `YtDlpManager.IsInstalled` — the managed binary exists.
pub fn is_installed() -> bool {
    exe_path().is_file()
}

fn exe_path() -> PathBuf {
    tools_dir().join(BIN_NAME)
}

/// `CreateNoWindow` for spawned helpers on Windows so no console
/// flashes during `--version` checks or metadata runs. UTF-8 mode is
/// forced: piped yt-dlp otherwise writes `--print`/`-J` output in the
/// console codepage (cp1252), and non-ASCII titles surface as U+FFFD.
fn command(exe: &Path) -> Command {
    let mut c = Command::new(exe);
    c.env("PYTHONUTF8", "1")
        .env("PYTHONIOENCODING", "utf-8");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

fn run_output(exe: &Path, args: &[&str]) -> Result<String, String> {
    let out = command(exe)
        .args(args)
        .output()
        .map_err(|e| format!("Failed to start {}: {e}", exe.display()))?;
    if !out.status.success() {
        let err = decode_output(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            format!("{} exited with {}", exe.display(), out.status)
        } else {
            err
        });
    }
    Ok(decode_output(&out.stdout))
}

/// Piped yt-dlp output arrives in the console codepage on Windows
/// (cp1252 — PyInstaller's bundled interpreter doesn't honor the UTF-8
/// env vars for `sys.stdout` here). Decode strictly as UTF-8 first,
/// then fall back to Windows-1252 so non-ASCII titles (×, ♪, Japanese)
/// don't surface as U+FFFD.
fn decode_output(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| cp1252_char(b)).collect(),
    }
}

/// Windows-1252 byte -> char: identity below 0x80 and Latin-1 above
/// 0xA0; the 0x80..0x9F gap is the printable-extension table.
fn cp1252_char(b: u8) -> char {
    const HI: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž',
        '\u{8F}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}',
        'ž', 'Ÿ',
    ];
    if (0x80..0xA0).contains(&b) {
        HI[(b - 0x80) as usize]
    } else {
        b as char
    }
}

/// `GetVersionAsync` — `--version` exit-0 also doubles as the
/// executable validity check.
fn version_at(path: &Path) -> Option<String> {
    run_output(path, &["--version"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Current version of the managed binary, if installed and runnable.
pub fn current_version() -> Option<String> {
    version_at(&exe_path())
}

fn find_in_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in names {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// `ResolveYtDlpPathAsync`: prefer the app-managed binary when it's
/// usable, else a PATH copy, else whichever candidate exists.
pub fn resolve_exe() -> Option<PathBuf> {
    #[cfg(windows)]
    const OTHER_NAME: &str = "yt-dlp";
    #[cfg(not(windows))]
    const OTHER_NAME: &str = "yt-dlp.exe";

    let local = exe_path();
    let system = find_in_path(&[BIN_NAME, OTHER_NAME]);

    let local_usable = local.is_file() && version_at(&local).is_some();
    let system_usable = system
        .as_deref()
        .map(|p| version_at(p).is_some())
        .unwrap_or(false);

    if local_usable {
        Some(local)
    } else if system_usable {
        system
    } else {
        // Diagnostics fallback — a candidate that may still run.
        if local.is_file() { Some(local) } else { system }
    }
}

fn require_exe() -> Result<PathBuf, String> {
    resolve_exe().ok_or_else(|| {
        "Failed to locate yt-dlp. Install it from Settings > Libraries.".to_string()
    })
}

// ------------------------------------------------------------------
// Release metadata (GitHub API + cache)
// ------------------------------------------------------------------

fn cache_path() -> Option<PathBuf> {
    data_root().map(|d| d.join("ytdlp_cache.json"))
}

#[derive(Serialize, Deserialize, Default)]
struct ReleaseCache {
    etag: Option<String>,
    latest_version: Option<String>,
    release_json: Option<String>,
    /// Unix seconds of the last refresh.
    last_updated: u64,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load_cache() -> ReleaseCache {
    cache_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_cache(cache: &ReleaseCache) {
    if let Some(p) = cache_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string(cache) {
            let _ = std::fs::write(p, json);
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(600)))
            .build(),
    )
}

/// Latest release JSON: fresh cache hit, API fetch (ETag/304, rate
/// limit fallback to the cached release), or `None`.
fn fetch_release_json(cache: &mut ReleaseCache) -> Option<serde_json::Value> {
    let api = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut req = agent().get(&api).header("User-Agent", USER_AGENT);
    if let Some(etag) = &cache.etag {
        req = req.header("If-None-Match", etag);
    }
    match req.call() {
        Ok(mut resp) => {
            let json = resp
                .body_mut()
                .read_to_string()
                .ok()?;
            cache.release_json = Some(json.clone());
            cache.etag = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            cache.last_updated = now_secs();
            let v: serde_json::Value = serde_json::from_str(&json).ok()?;
            if let Some(tag) = v.get("tag_name").and_then(|t| t.as_str()) {
                cache.latest_version = Some(tag.to_string());
            }
            save_cache(cache);
            Some(v)
        }
        Err(ureq::Error::StatusCode(304)) | Err(ureq::Error::StatusCode(403)) => {
            // Not modified or rate-limited — the cached release still
            // answers both version and asset lookups.
            cache.last_updated = now_secs();
            cache
                .release_json
                .as_deref()
                .and_then(|j| serde_json::from_str(j).ok())
        }
        Err(_) => cache
            .release_json
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok()),
    }
}

/// `GetLatestVersionAsync` — newest tag_name, cached 15 minutes.
pub fn latest_version() -> Option<String> {
    let mut cache = load_cache();
    let fresh = cache
        .latest_version
        .is_some()
        && now_secs().saturating_sub(cache.last_updated) < CACHE_TTL_SECS;
    if fresh {
        return cache.latest_version;
    }
    fetch_release_json(&mut cache)
        .and_then(|v| v.get("tag_name").and_then(|t| t.as_str()).map(String::from))
        .or(cache.latest_version)
}

// ------------------------------------------------------------------
// Download / install / uninstall (worker-thread entry points)
// ------------------------------------------------------------------

/// Lacrima's `GetPlatformAssetNames`, preference order.
fn platform_asset_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["yt-dlp_win.zip", "yt-dlp.exe"]
    }
    #[cfg(target_os = "macos")]
    {
        &["yt-dlp_macos", "yt-dlp"]
    }
    #[cfg(target_os = "linux")]
    {
        if cfg!(target_arch = "aarch64") {
            &["yt-dlp_linux_aarch64", "yt-dlp_linux", "yt-dlp"]
        } else if cfg!(target_arch = "arm") {
            &["yt-dlp_linux_armv7l", "yt-dlp_linux", "yt-dlp"]
        } else {
            &["yt-dlp_linux", "yt-dlp"]
        }
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        &["yt-dlp"]
    }
}

/// `DownloadWithProgressAsync` — stream the asset to a temp file,
/// reporting 0..=1 via `progress`. Returns the file + asset name.
fn download_asset(progress: &mut dyn FnMut(f32)) -> Result<(PathBuf, String), String> {
    let mut cache = load_cache();
    let release = fetch_release_json(&mut cache);

    // Pick the first matching asset in preference order (Lacrima's
    // targetAssets.Any). No release metadata -> the stable
    // latest/download URL.
    let (name, url) = match release.as_ref().and_then(|v| v.get("assets")) {
        Some(assets) if assets.is_array() => {
            let mut found = None;
            for want in platform_asset_names() {
                if let Some(a) = assets.as_array().unwrap().iter().find(|a| {
                    a.get("name")
                        .and_then(|n| n.as_str())
                        .map(|n| n.eq_ignore_ascii_case(want))
                        .unwrap_or(false)
                }) {
                    if let Some(u) = a.get("browser_download_url").and_then(|u| u.as_str()) {
                        found = Some((want.to_string(), u.to_string()));
                        break;
                    }
                }
            }
            found.ok_or_else(|| {
                format!(
                    "No suitable yt-dlp build found for this platform. Tried: {}",
                    platform_asset_names().join(", ")
                )
            })?
        }
        _ => {
            let name = platform_asset_names()[0].to_string();
            (
                name.clone(),
                format!("https://github.com/{REPO}/releases/latest/download/{name}"),
            )
        }
    };

    let mut resp = agent()
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| format!("Download failed: {e}"))?;
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let file = tempfile();
    let mut f = std::fs::File::create(&file)
        .map_err(|e| format!("Could not create temp file: {e}"))?;
    let mut reader = resp.body_mut().as_reader();
    let mut buf = [0u8; 8192];
    let mut read_total: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("Download failed: {e}"))?;
        if n == 0 {
            break;
        }
        use std::io::Write;
        f.write_all(&buf[..n])
            .map_err(|e| format!("Write failed: {e}"))?;
        read_total += n as u64;
        if let Some(t) = total {
            if t > 0 {
                progress((read_total as f32 / t as f32).clamp(0.0, 1.0));
            }
        }
    }
    Ok((file, name))
}

fn tempfile() -> PathBuf {
    std::env::temp_dir().join(format!("aes_ytdlp_{}.tmp", std::process::id()))
}

/// `EnsureInstalledAsync` + `UpdateAsync` shared core: download the
/// latest release into the tools dir (zip extracts `yt-dlp.exe` plus
/// its `_internal` runtime folder; the onefile binary is moved over).
fn download_and_install(progress: &mut dyn FnMut(f32)) -> Result<(), String> {
    let tools = tools_dir();
    std::fs::create_dir_all(&tools)
        .map_err(|e| format!("Could not create tools dir: {e}"))?;
    let (file, name) = download_asset(progress)?;
    if name.to_ascii_lowercase().ends_with(".zip") {
        let f = std::fs::File::open(&file)
            .map_err(|e| format!("Could not open download: {e}"))?;
        let mut archive =
            zip::ZipArchive::new(f).map_err(|e| format!("Invalid zip: {e}"))?;
        archive
            .extract(&tools)
            .map_err(|e| format!("Extract failed: {e}"))?;
        let _ = std::fs::remove_file(&file);
    } else {
        let dest = tools.join(BIN_NAME);
        let _ = std::fs::remove_file(&dest);
        std::fs::rename(&file, &dest)
            .or_else(|_| {
                std::fs::copy(&file, &dest).map(|_| ())
            })
            .map_err(|e| format!("Install failed: {e}"))?;
        let _ = std::fs::remove_file(&file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&dest) {
                let mut perms = meta.permissions();
                perms.set_mode(0o755);
                let _ = std::fs::set_permissions(&dest, perms);
            }
        }
    }
    Ok(())
}

fn uninstall_work() -> Result<(), String> {
    let _tools = tools_dir();
    #[cfg(windows)]
    let tools = &_tools;
    #[cfg(windows)]
    {
        let internal = tools.join("_internal");
        if internal.is_dir() {
            let _ = std::fs::remove_dir_all(&internal);
        }
    }
    let exe = exe_path();
    if exe.is_file() {
        std::fs::remove_file(&exe).map_err(|e| format!("Uninstall failed: {e}"))?;
    }
    Ok(())
}

// ------------------------------------------------------------------
// UI-side state (the settings Libraries tab drives this)
// ------------------------------------------------------------------

/// Channel events from worker threads.
enum Event {
    Progress(f32),
    /// Operation finished: (status line, refreshed install state).
    Done {
        status: String,
        installed: bool,
        version: Option<String>,
        update_version: Option<String>,
    },
}

pub struct YtDlp {
    pub status: String,
    pub installed: bool,
    pub version: Option<String>,
    pub update_version: Option<String>,
    pub busy: bool,
    /// 0..=1 while downloading; meaningless otherwise.
    pub progress: f32,
    /// An update newer than the installed version exists.
    pub update_available: bool,
    /// Version info was never fetched — triggers the first refresh.
    queried: bool,
    rx: Option<mpsc::Receiver<Event>>,
}

impl Default for YtDlp {
    fn default() -> Self {
        Self::new()
    }
}

impl YtDlp {
    pub fn new() -> Self {
        Self {
            status: "Idle".to_string(),
            installed: is_installed(),
            version: None,
            update_version: None,
            busy: false,
            progress: 0.0,
            update_available: false,
            queried: false,
            rx: None,
        }
    }

    /// Drain worker events; returns true when state changed (repaint).
    pub fn pump(&mut self) -> bool {
        let mut events = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(e) = rx.try_recv() {
                events.push(e);
            }
        }
        let mut changed = !events.is_empty();
        for e in events {
            match e {
                Event::Progress(p) => self.progress = p,
                Event::Done {
                    status,
                    installed,
                    version,
                    update_version,
                } => {
                    self.busy = false;
                    self.rx = None;
                    self.status = status;
                    self.installed = installed;
                    self.version = version.clone();
                    self.update_version = update_version.clone();
                    self.update_available = version.is_some()
                        && update_version.is_some()
                        && version != update_version;
                }
            }
        }
        if self.busy {
            changed = true; // keep the indeterminate bar animating
        }
        changed
    }

    fn spawn(&mut self, f: impl FnOnce(mpsc::Sender<Event>) + Send + 'static) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.progress = 0.0;
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::Builder::new()
            .name("aes-ytdlp".into())
            .spawn(move || f(tx))
            .expect("ytdlp worker");
    }

    /// Post-op status + state refresh (versions queried off-thread).
    fn finish(tx: &mpsc::Sender<Event>, status: String) {
        let installed = is_installed();
        let version = current_version();
        let update_version = latest_version();
        let _ = tx.send(Event::Done {
            status,
            installed,
            version,
            update_version,
        });
    }

    /// `RefreshYtDlpInfo` — installed check + version + update check.
    pub fn refresh(&mut self) {
        self.status = "Checking yt-dlp...".to_string();
        self.spawn(|tx| {
            let installed = is_installed();
            let version = current_version();
            let update_version = latest_version();
            let status = if !installed {
                "yt-dlp check completed: Not found.".to_string()
            } else {
                match (&version, &update_version) {
                    (Some(v), Some(u)) if v != u => {
                        format!("yt-dlp update found: {u}.")
                    }
                    (Some(v), _) => format!("yt-dlp is up to date ({v})."),
                    _ => "yt-dlp check failed: version unknown.".to_string(),
                }
            };
            let _ = tx.send(Event::Done {
                status,
                installed,
                version,
                update_version,
            });
        });
    }

    /// First-show trigger, like Lacrima's settings-open refresh.
    pub fn ensure_queried(&mut self) {
        if !self.queried && !self.busy {
            self.queried = true;
            self.refresh();
        }
    }

    /// `InstallYtDlp` / `UpdateYtDlp` — the download path is shared.
    pub fn install_or_update(&mut self) {
        self.status = if self.installed {
            "Updating yt-dlp...".to_string()
        } else {
            "yt-dlp not found. Starting automatic setup...".to_string()
        };
        let was_installed = self.installed;
        self.spawn(move |tx| {
            let status = {
                let txp = tx.clone();
                match download_and_install(&mut |p| {
                    let _ = txp.send(Event::Progress(p));
                }) {
                    Ok(()) => {
                        if was_installed {
                            "yt-dlp update successful.".to_string()
                        } else {
                            "yt-dlp installation successful.".to_string()
                        }
                    }
                    Err(e) => format!("yt-dlp installation failed: {e}"),
                }
            };
            Self::finish(&tx, status);
        });
    }

    /// `UninstallYtDlp` — removes the exe and the `_internal` runtime.
    pub fn uninstall(&mut self) {
        self.status = "Uninstalling yt-dlp...".to_string();
        self.spawn(|tx| {
            let status = match uninstall_work() {
                Ok(()) => "yt-dlp uninstalled.".to_string(),
                Err(e) => format!("yt-dlp uninstall failed: {e}"),
            };
            Self::finish(&tx, status);
        });
    }
}

// ------------------------------------------------------------------
// YtDlpMetadata — process API used the same way Lacrima calls it.
// Nothing calls this yet: Lacrima uses it for URL media items
// (GetBasicMetadataAsync) and stream resolution (OnlineStreamSelector),
// which are a separate feature. Kept public so that work drops in
// without touching the manager.
// ------------------------------------------------------------------

/// `YtDlpExtractorProfile` — which Innertube clients yt-dlp uses.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExtractorProfile {
    /// Reliable metadata; mostly low-quality muxed URLs.
    Default,
    /// Separate DASH video/audio URLs up to 1080p.
    HighQualityStreams,
    /// yt-dlp's own extractor defaults.
    Unrestricted,
    /// `android_music,android_creator,tv_embedded` — serves DASH
    /// audio-only itags (m4a/AAC-LC) without the sign-in gate that
    /// Unrestricted's default clients now hit.
    AndroidMusic,
}

/// `AddCommonYtDlpArgs` — extractor client profile, available JS
/// runtimes, and the optional AES_YTDLP_COOKIES_* env hooks.
fn common_args(profile: ExtractorProfile) -> Vec<String> {
    let mut args = Vec::new();
    if profile != ExtractorProfile::Unrestricted {
        args.push("--extractor-args".to_string());
        args.push(
            match profile {
                ExtractorProfile::HighQualityStreams => {
                    "youtube:player_client=android_sdkless"
                }
                ExtractorProfile::AndroidMusic => {
                    "youtube:player_client=android_music,android_creator,tv_embedded;player_skip=webpage,configs"
                }
                _ => "youtube:player_client=android,web_safari,tv;player_skip=webpage,configs",
            }
            .to_string(),
        );
    }
    if let Some(rt) = js_runtime_list() {
        args.push("--js-runtimes".to_string());
        args.push(rt);
    }
    if let Ok(cookies) = std::env::var("AES_YTDLP_COOKIES_FILE") {
        if !cookies.is_empty() && Path::new(&cookies).is_file() {
            args.push("--cookies".to_string());
            args.push(cookies);
            return args;
        }
    }
    if let Ok(browser) = std::env::var("AES_YTDLP_COOKIES_FROM_BROWSER") {
        if !browser.is_empty() {
            args.push("--cookies-from-browser".to_string());
            args.push(browser);
        }
    }
    args
}

/// `GetJsRuntimeList` — yt-dlp can shell out to a JS engine for
/// challenge solving when one is on PATH.
fn js_runtime_list() -> Option<String> {
    let found: Vec<&str> = ["node", "deno", "bun"]
        .iter()
        .copied()
        .filter(|cmd| {
            #[cfg(windows)]
            let names = [
                format!("{cmd}.exe"),
                format!("{cmd}.cmd"),
                cmd.to_string(),
            ];
            #[cfg(not(windows))]
            let names = [cmd.to_string()];
            let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
            find_in_path(&refs).is_some()
        })
        .collect();
    if found.is_empty() {
        None
    } else {
        Some(found.join(","))
    }
}

/// `IsYtdlSourcePageUrl` — YouTube watch/share/shorts pages; direct
/// googlevideo CDN URLs are explicitly NOT source pages.
pub fn is_ytdl_source_page_url(url: &str) -> bool {
    let u = url.trim();
    let Some(rest) = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
    else {
        return false;
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if host.is_empty() || host.contains("googlevideo.com") {
        return false;
    }
    host.contains("youtube.com")
        || host == "youtu.be"
        || host.contains("youtube-nocookie.com")
}

/// Parsed `yt-dlp -J` output — formats split like Lacrima's
/// MediaInfo: video-only, audio-only and muxed streams.
#[derive(Default)]
pub struct MediaInfo {
    pub id: String,
    pub title: String,
    pub duration: Option<f64>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track: Option<String>,
    pub track_number: Option<i64>,
    pub disc_number: Option<i64>,
    pub release_year: Option<i64>,
    pub genre: Option<String>,
    pub uploader: Option<String>,
    pub channel: Option<String>,
    pub channel_id: Option<String>,
    pub thumbnail_url: Option<String>,
    pub videos: Vec<VideoFormat>,
    pub audios: Vec<AudioFormat>,
    pub muxed: Vec<MuxedFormat>,
}

pub struct VideoFormat {
    pub format_id: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub fps: Option<f64>,
    pub codec: String,
    pub bitrate: Option<f64>,
    pub filesize: Option<i64>,
    pub url: String,
}

pub struct AudioFormat {
    pub format_id: String,
    pub codec: String,
    pub bitrate: Option<f64>,
    pub filesize: Option<i64>,
    pub url: String,
}

pub struct MuxedFormat {
    pub format_id: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub fps: Option<f64>,
    pub video_codec: String,
    pub audio_codec: String,
    pub url: String,
}

fn jstr(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)?.as_str().map(String::from)
}
fn jint(v: &serde_json::Value, key: &str) -> Option<i64> {
    v.get(key)?.as_i64().or_else(|| v.get(key)?.as_f64().map(|f| f as i64))
}
fn jf64(v: &serde_json::Value, key: &str) -> Option<f64> {
    v.get(key)?.as_f64()
}

/// `GetMetaDataAsync` — `yt-dlp --no-playlist -J <url>` parsed into
/// [`MediaInfo`]. Blocking; call off the UI thread.
pub fn metadata(url: &str, profile: ExtractorProfile) -> Result<MediaInfo, String> {
    let exe = require_exe()?;
    let mut args = common_args(profile);
    args.extend(["--no-playlist".into(), "-J".into(), url.to_string()]);
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let out = run_output(&exe, &arg_refs)?;
    let root: serde_json::Value =
        serde_json::from_str(out.trim()).map_err(|e| format!("Bad yt-dlp output: {e}"))?;

    let mut info = MediaInfo {
        id: jstr(&root, "id").unwrap_or_default(),
        title: jstr(&root, "title").unwrap_or_default(),
        duration: jf64(&root, "duration"),
        artist: ["artist", "creator", "uploader", "channel"]
            .iter()
            .find_map(|k| jstr(&root, k)),
        album: jstr(&root, "album"),
        track: jstr(&root, "track"),
        track_number: jint(&root, "track_number"),
        disc_number: jint(&root, "disc_number"),
        release_year: jint(&root, "release_year"),
        genre: jstr(&root, "genre"),
        uploader: jstr(&root, "uploader"),
        channel: jstr(&root, "channel"),
        channel_id: jstr(&root, "channel_id"),
        thumbnail_url: jstr(&root, "thumbnail"),
        ..Default::default()
    };

    if let Some(formats) = root.get("formats").and_then(|f| f.as_array()) {
        for f in formats {
            let Some(url) = jstr(f, "url") else { continue };
            let vcodec = jstr(f, "vcodec").unwrap_or_else(|| "none".into());
            let acodec = jstr(f, "acodec").unwrap_or_else(|| "none".into());
            let format_id = jstr(f, "format_id").unwrap_or_default();
            if vcodec != "none" && acodec == "none" {
                info.videos.push(VideoFormat {
                    format_id,
                    width: jint(f, "width"),
                    height: jint(f, "height"),
                    fps: jf64(f, "fps"),
                    codec: vcodec,
                    bitrate: jf64(f, "vbr"),
                    filesize: jint(f, "filesize"),
                    url,
                });
            } else if vcodec == "none" && acodec != "none" {
                info.audios.push(AudioFormat {
                    format_id,
                    codec: acodec,
                    bitrate: jf64(f, "abr"),
                    filesize: jint(f, "filesize"),
                    url,
                });
            } else {
                info.muxed.push(MuxedFormat {
                    format_id,
                    width: jint(f, "width"),
                    height: jint(f, "height"),
                    fps: jf64(f, "fps"),
                    video_codec: vcodec,
                    audio_codec: acodec,
                    url,
                });
            }
        }
    }
    Ok(info)
}

/// `GetBasicMetadataAsync` — `--print` fields in a fixed order
/// (cheap header info; the empty `--output-na-placeholder` keeps
/// missing fields as blank lines so indices stay aligned).
pub fn basic_metadata(url: &str) -> Result<MediaInfo, String> {
    let exe = require_exe()?;
    let fields = [
        "id", "title", "duration", "artist", "uploader", "channel", "album", "track",
        "track_number", "release_year", "genre", "thumbnail",
    ];
    let mut args = common_args(ExtractorProfile::Default);
    args.push("--no-playlist".into());
    args.extend(["--output-na-placeholder".into(), String::new()]);
    for f in fields {
        args.extend(["--print".into(), f.into()]);
    }
    args.push(url.to_string());
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let out = run_output(&exe, &arg_refs)?;
    let lines: Vec<String> = out
        .replace("\r\n", "\n")
        .split('\n')
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect();
    let line = |i: usize| {
        lines
            .get(i)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    Ok(MediaInfo {
        id: line(0).unwrap_or_default(),
        title: line(1).unwrap_or_default(),
        duration: line(2).and_then(|s| s.parse().ok()),
        artist: line(3).or_else(|| line(4)).or_else(|| line(5)),
        uploader: line(4),
        channel: line(5),
        album: line(6),
        track: line(7),
        track_number: line(8).and_then(|s| s.parse().ok()),
        release_year: line(9).and_then(|s| s.parse().ok()),
        genre: line(10),
        thumbnail_url: line(11),
        ..Default::default()
    })
}

/// `GetStreamUrlsAsync` — `yt-dlp -g -f <selector>`: one URL for
/// muxed selectors, video+audio lines for merged ones.
pub fn stream_urls(
    url: &str,
    format_selector: &str,
    profile: ExtractorProfile,
) -> Result<Vec<String>, String> {
    let exe = require_exe()?;
    let mut args = common_args(profile);
    args.extend([
        "--no-playlist".into(),
        "-g".into(),
        "-f".into(),
        format_selector.to_string(),
        url.to_string(),
    ]);
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let out = run_output(&exe, &arg_refs)?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("http://") || l.starts_with("https://"))
        .map(String::from)
        .collect())
}

// ------------------------------------------------------------------
// YouTube helpers (YouTubeThumbnail.cs parity) — the parts of the
// pipeline that don't need yt-dlp at all: ID extraction, canonical
// links, thumbnail candidates, oEmbed/page metadata, playlist HTML
// expansion. All blocking; run off the UI thread.
// ------------------------------------------------------------------

fn is_id_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b'_'
}

/// `ExtractVideoId` — the 11-char video id from watch/share/embed/
/// v/shorts URLs or `v=` query params, or the bare id itself.
pub fn extract_video_id(url: &str) -> Option<String> {
    const PREFIXES: [&str; 5] = [
        "youtube.com/watch?v=",
        "youtu.be/",
        "youtube.com/embed/",
        "youtube.com/v/",
        "youtube.com/shorts/",
    ];
    let lower = url.to_ascii_lowercase();
    for p in PREFIXES {
        if let Some(i) = lower.find(p) {
            let rest = &url[i + p.len()..];
            let id: String = rest
                .bytes()
                .take(11)
                .take_while(|&c| is_id_char(c))
                .map(char::from)
                .collect();
            if id.len() == 11 {
                return Some(id);
            }
        }
    }
    // [?&]v=<id>
    for marker in ["?v=", "&v="] {
        if let Some(i) = lower.find(marker) {
            let rest = &url[i + marker.len()..];
            let id: String = rest
                .bytes()
                .take(11)
                .take_while(|&c| is_id_char(c))
                .map(char::from)
                .collect();
            if id.len() == 11 {
                return Some(id);
            }
        }
    }
    if url.len() == 11 && url.bytes().all(is_id_char) {
        return Some(url.to_string());
    }
    None
}

/// `GetCleanVideoLink` — canonical watch URL, or the input unchanged.
pub fn clean_video_link(url: &str) -> String {
    match extract_video_id(url) {
        Some(id) => format!("https://www.youtube.com/watch?v={id}"),
        None => url.to_string(),
    }
}

/// `TryLoadYouTubeThumbnailFastAsync` candidate order.
pub fn thumbnail_candidates(video_id: &str) -> Vec<String> {
    [
        format!("https://img.youtube.com/vi/{video_id}/maxresdefault.jpg"),
        format!("https://img.youtube.com/vi/{video_id}/hqdefault.jpg"),
        format!("https://img.youtube.com/vi/{video_id}/mqdefault.jpg"),
    ]
    .into()
}

/// Thin fetch used by cover decode and the metadata scrapes.
pub fn fetch_bytes(url: &str, timeout_secs: u64) -> Option<Vec<u8>> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(timeout_secs)))
            .build(),
    );
    let mut resp = agent
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .call()
        .ok()?;
    resp.body_mut().read_to_vec().ok()
}

fn fetch_text(url: &str, timeout_secs: u64) -> Option<String> {
    String::from_utf8(fetch_bytes(url, timeout_secs)?).ok()
}

/// Cheap online metadata — Lacrima's MetadataScrapper path for URL
/// items (`YouTubeThumbnail.GetOnlineMetadataAsync`): YouTube oEmbed
/// first, yt-dlp `--print` when installed, og:/twitter meta tags as
/// the generic last resort.
pub struct OnlineMeta {
    pub title: String,
    pub author: String,
    pub thumb: Option<String>,
    /// Stream length when the resolver knows it (yt-dlp metadata).
    pub duration: Option<f64>,
}

/// Duration intentionally has no watch-page fallback — per-item page
/// fetches tripped YouTube's bot/rate defenses (regression: resolved
/// streams stopped being served). See `durations` for the sanctioned
/// batch path instead.
///
/// Stream lengths for several items in ONE yt-dlp pass —
/// `--print "%(webpage_url)s|%(duration)s"`. Player-API calls only
/// (`player_skip=webpage`), the same traffic shape as a resolve, and
/// yt-dlp paces the URLs sequentially inside the single process, so
/// the request rate stays far below the per-item webpage scrape that
/// got us flagged. Results are keyed back by canonical URL —
/// `clean_video_link` on both sides — since yt-dlp omits output lines
/// for failed inputs.
pub fn durations(urls: &[String]) -> Vec<(String, f64)> {
    let Ok(exe) = require_exe() else {
        return Vec::new();
    };
    let mut args = common_args(ExtractorProfile::Default);
    args.push("--no-playlist".into());
    args.extend(["--print".into(), "%(webpage_url)s|%(duration)s".into()]);
    args.extend(urls.iter().cloned());
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let Ok(out) = run_output(&exe, &arg_refs) else {
        return Vec::new();
    };
    parse_durations(&out, urls)
}

/// `webpage_url|duration` lines → `(input url, secs)` — matched by
/// canonical link so failed inputs (yt-dlp skips their lines) can't
/// misalign the results.
fn parse_durations(out: &str, urls: &[String]) -> Vec<(String, f64)> {
    let mut got = std::collections::HashSet::new();
    let mut res = Vec::new();
    for line in out.replace("\r\n", "\n").lines() {
        let Some((page, dur)) = line.trim().split_once('|') else {
            continue;
        };
        let Ok(d) = dur.parse::<f64>() else {
            continue;
        };
        if d <= 0.0 {
            continue;
        }
        let key = clean_video_link(page);
        if let Some(u) = urls
            .iter()
            .find(|u| !got.contains(*u) && clean_video_link(u) == key)
        {
            got.insert(u.clone());
            res.push((u.clone(), d));
        }
    }
    res
}

pub fn online_metadata(url: &str) -> Option<OnlineMeta> {
    if let Some(id) = extract_video_id(url) {
        let api = format!(
            "https://www.youtube.com/oembed?url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3D{id}&format=json"
        );
        if let Some(text) = fetch_text(&api, 15) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                let title = jstr(&v, "title").unwrap_or_default();
                if !title.is_empty() {
                    return Some(OnlineMeta {
                        title,
                        author: jstr(&v, "author_name").unwrap_or_default(),
                        thumb: jstr(&v, "thumbnail_url"),
                        duration: None,
                    });
                }
            }
        }
    }
    // Non-YouTube (or oEmbed failed): yt-dlp knows far more sources.
    if let Ok(info) = basic_metadata(url) {
        if !info.title.is_empty() {
            return Some(OnlineMeta {
                title: info.title,
                author: info
                    .artist
                    .or(info.uploader)
                    .or(info.channel)
                    .unwrap_or_default(),
                thumb: info.thumbnail_url,
                duration: info.duration,
            });
        }
    }
    // Generic og:/twitter meta scrape (Lacrima's last-resort branch).
    let html = fetch_text(url, 15)?;
    let title = meta_content(&html, "og:title")
        .or_else(|| meta_content(&html, "twitter:title"))
        .or_else(|| meta_content(&html, "title"))
        .or_else(|| {
            let s = html.find("<title>")? + "<title>".len();
            let e = html[s..].find("</title>")? + s;
            let t = html[s..e].trim().to_string();
            // "<name> - YouTube" style suffix
            let t = t.strip_suffix("- YouTube").map(str::trim).unwrap_or(&t);
            (!t.is_empty()).then(|| t.to_string())
        })?;
    Some(OnlineMeta {
        title: html_unescape(title.trim()),
        author: html_unescape(
            meta_content(&html, "og:site_name")
                .or_else(|| meta_content(&html, "author"))
                .or_else(|| meta_content(&html, "twitter:creator"))
                .unwrap_or_default()
                .trim(),
        ),
        thumb: meta_content(&html, "og:image")
            .or_else(|| meta_content(&html, "twitter:image")),
        duration: None,
    })
}

/// `<meta property|name="key" content="value">` — attribute order is
/// not guaranteed, so scan meta tags rather than regex a fixed form.
fn meta_content(html: &str, key: &str) -> Option<String> {
    let mut pos = 0;
    while let Some(i) = html[pos..].find("<meta") {
        let start = pos + i;
        let end = html[start..].find('>').map(|e| start + e)?;
        let tag = &html[start..=end];
        if (tag.contains(&format!("property=\"{key}\""))
            || tag.contains(&format!("name=\"{key}\"")))
            && !tag.contains("content=\"\"")
        {
            if let Some(c) = tag.find("content=\"") {
                let v = &tag[c + 9..];
                if let Some(e) = v.find('"') {
                    let out = v[..e].to_string();
                    if !out.is_empty() {
                        return Some(out);
                    }
                }
            }
        }
        pos = end + 1;
    }
    None
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// `GetPlaylistVideoUrls` — fetch the playlist page HTML and extract
/// the member video ids. Pass 1: `"videoId":"X","playlistId":"P"`
/// pairs filtered to the URL's own `list=` param (kills
/// recommendations/Shorts). Pass 2: playlist*Renderer blocks. Pass 3:
/// any videoId whose surrounding context mentions playlist renderers
/// and not reel/compact renderers.
pub fn playlist_video_urls(playlist_url: &str) -> Vec<String> {
    let Some(html) = fetch_text(playlist_url, 20) else {
        return Vec::new();
    };
    let target_list = query_param(playlist_url, "list");
    let mut seen: std::collections::HashSet<String> = Default::default();
    let mut urls = Vec::new();

    // Pass 1: `"videoId":"X","playlistId":"P"` pairs, filtered to the
    // URL's own `list=` param when present (kills recommendations).
    let mut pos = 0;
    while let Some(i) = html[pos..].find("\"videoId\"") {
        let vi = pos + i;
        if let Some((vid, after)) = quoted_value(&html[vi + 9..]) {
            let rest = html[vi + 9 + after..].trim_start();
            if let Some(rest) = rest.strip_prefix(',') {
                if let Some(rest) = rest.trim_start().strip_prefix("\"playlistId\"") {
                    if let Some((pid, _)) = quoted_value(rest) {
                        let ok = match &target_list {
                            Some(t) => pid == *t,
                            None => true,
                        };
                        if ok && seen.insert(vid.clone()) {
                            urls.push(format!(
                                "https://www.youtube.com/watch?v={vid}"
                            ));
                        }
                    }
                }
            }
        }
        pos = vi + 9;
    }
    // Pass 2 fallback: playlist renderers containing a videoId.
    if urls.is_empty() {
        for marker in [
            "\"playlistVideoRenderer\"",
            "\"playlistPanelVideoRenderer\"",
            "\"playlistVideoListRenderer\"",
        ] {
            let mut p = 0;
            while let Some(i) = html[p..].find(marker) {
                let end = (p + i + marker.len() + 4096).min(html.len());
                if let Some(j) = html[p + i..end].find("\"videoId\"") {
                    let vi = p + i + j;
                    if let Some((vid, _)) = quoted_value(&html[vi + 9..]) {
                        if seen.insert(vid.clone()) {
                            urls.push(format!(
                                "https://www.youtube.com/watch?v={vid}"
                            ));
                        }
                    }
                }
                p += i + marker.len();
            }
        }
    }
    // Pass 3: bare videoIds whose local context looks playlist-ish.
    if urls.is_empty() {
        let mut p = 0;
        while let Some(i) = html[p..].find("\"videoId\":\"") {
            let vi = p + i + 11;
            if let Some(e) = html[vi..].find('"') {
                let vid = html[vi..vi + e].to_string();
                if seen.insert(vid.clone()) {
                    let lo = vi.saturating_sub(200);
                    let hi = (vi + 400).min(html.len());
                    let ctx = &html[lo..hi];
                    let bad = ctx.contains("compactVideoRenderer")
                        || ctx.contains("reelWatchEndpoint")
                        || ctx.contains("shortsLockupViewModel");
                    let good = ctx.contains("playlistVideoRenderer")
                        || ctx.contains("playlistPanelVideoRenderer")
                        || ctx.contains("playlistId");
                    if good && !bad {
                        urls.push(format!("https://www.youtube.com/watch?v={vid}"));
                    }
                }
            }
            p = vi;
        }
    }
    urls
}

/// `:<ws>"value"` at the start of `s` — returns (value, byte offset
/// in `s` just past the closing quote).
fn quoted_value(s: &str) -> Option<(String, usize)> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if b.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if b.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;
    let start = i;
    let end = s[start..].find('"')? + start;
    Some((s[start..end].to_string(), end + 1))
}

/// `?list=PL...` from a playlist URL.
fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split('?').nth(1)?;
    for pair in q.split('&') {
        let mut kv = pair.splitn(2, '=');
        if kv.next() == Some(key) {
            return kv.next().map(|v| {
                v.split('#').next().unwrap_or(v).to_string()
            });
        }
    }
    None
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: playlist/mix suffixes must strip to the canonical
    /// watch link — the link text is also the stream-cache key, so an
    /// uncleaned `&list=…&start_radio=1` item built its own poisoned
    /// cache instead of sharing the video's healthy one.
    #[test]
    fn clean_video_link_strips_playlist_params() {
        assert_eq!(
            clean_video_link(
                "https://www.youtube.com/watch?v=PiwDxH2HgIM&list=RDPiwDxH2HgIM&start_radio=1"
            ),
            "https://www.youtube.com/watch?v=PiwDxH2HgIM"
        );
        assert_eq!(
            clean_video_link("https://youtu.be/PiwDxH2HgIM?t=5"),
            "https://www.youtube.com/watch?v=PiwDxH2HgIM"
        );
        // Already-clean and non-YouTube urls are untouched.
        assert_eq!(
            clean_video_link("https://www.youtube.com/watch?v=PiwDxH2HgIM"),
            "https://www.youtube.com/watch?v=PiwDxH2HgIM"
        );
        assert_eq!(
            clean_video_link("https://example.com/radio.pls"),
            "https://example.com/radio.pls"
        );
    }

    /// Batch output lines map back to their input urls by canonical
    /// link — a dropped line (failed input) or a playlist-decorated
    /// input can't misalign or lose the result.
    #[test]
    fn parse_durations_maps_lines_by_canonical_link() {
        let urls = vec![
            "https://www.youtube.com/watch?v=AAAAAAAAAAA&list=RDAAAAAAAAAAA".to_string(),
            "https://www.youtube.com/watch?v=BBBBBBBBBBB".to_string(),
            "https://www.youtube.com/watch?v=CCCCCCCCCCC".to_string(),
        ];
        let out = "https://www.youtube.com/watch?v=AAAAAAAAAAA|101\n\
                   https://www.youtube.com/watch?v=CCCCCCCCCCC|303\n";
        let res = parse_durations(out, &urls);
        assert_eq!(
            res,
            vec![
                (urls[0].clone(), 101.0),
                (urls[2].clone(), 303.0),
            ]
        );
        // Garbage/NA/zero lines are skipped.
        assert!(parse_durations("junk\nhttps://www.youtube.com/watch?v=AAAAAAAAAAA|NA\nhttps://www.youtube.com/watch?v=AAAAAAAAAAA|0\n", &urls).is_empty());
    }
}
