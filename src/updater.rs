//! AES Oxide self-update: GitHub release check → download the
//! platform asset → stage it beside the running binary → swap and
//! relaunch.
//!
//! The swap never overwrites a running exe (impossible on Windows):
//! it renames the running file aside first — same-volume renames of a
//! running executable are allowed — then moves the staged binary into
//! place. If even the rename is denied (read-only install dir), the
//! staged copy is launched in `--apply-update` helper mode: it waits
//! for this process to exit, then performs the swap itself.
//!
//! Update AND rollback are the same path — the Updates tab lists all
//! releases (stable + pre) and any of them can be installed.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// GitHub repo the release pipeline publishes to.
pub const REPO: &str = "aruantec/AES_Rustified";
const USER_AGENT: &str = "AES_Oxide-SelfUpdate/1.0";

/// One GitHub release with this platform's asset attached.
#[derive(Clone)]
pub struct Release {
    /// `v0.2.0` — the tag is the version.
    pub tag: String,
    /// Pre-release flag — shown as a badge in the list.
    pub prerelease: bool,
    /// `published_at` date (YYYY-MM-DD).
    pub date: String,
    /// `browser_download_url` for this platform's asset.
    pub asset_url: String,
}

/// Shared download/apply state the UI polls each frame — reuses
/// emu_dl's progress shape (`stage`/`frac`/`done`), so the overlay
/// code reads identically.
pub use crate::emulation::emu_dl::Shared;

fn set(p: &Shared, stage: &str, frac: f32) {
    if let Ok(mut g) = p.lock() {
        g.stage = stage.to_string();
        g.frac = frac;
    }
}

// ------------------------------------------------------------------
// Versions / release feed
// ------------------------------------------------------------------

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// This platform's release asset name (matches release.yml).
fn platform_asset() -> &'static str {
    if cfg!(windows) {
        "AES-Oxide-windows-x64.zip"
    } else {
        "AES-Oxide-Linux-x86_64.AppImage"
    }
}

/// Semver-ish compare: `v0.2.0` > `0.1.9`; a pre-release suffix
/// (`-rc1`) orders *before* the same bare version, and suffixes
/// order semver-ish among themselves (`rc2` < `rc10`).
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn parts(s: &str) -> (Vec<u64>, &str) {
        let s = s.trim().trim_start_matches('v');
        let (core, pre) = s.split_once('-').unwrap_or((s, ""));
        (
            core.split('.')
                .map(|p| p.parse::<u64>().unwrap_or(0))
                .collect(),
            pre,
        )
    }
    let (an, apre) = parts(a);
    let (bn, bpre) = parts(b);
    use std::cmp::Ordering::*;
    for i in 0..an.len().max(bn.len()) {
        match an.get(i).copied().unwrap_or(0).cmp(&bn.get(i).copied().unwrap_or(0)) {
            Equal => {}
            o => return o,
        }
    }
    pre_cmp(apre, bpre)
}

/// Semver pre-release compare: digit and alpha runs compared
/// separately (`rc2` < `rc10`, `alpha` < `beta`, numeric < alpha);
/// `.`, `-`, `_` are separators, and an empty suffix (bare release)
/// sorts above any pre-release.
fn pre_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Equal,
        (true, false) => return Greater,
        (false, true) => return Less,
        _ => {}
    }
    fn take(s: &str) -> Option<(bool, &str, &str)> {
        let s = s.trim_start_matches(['.', '-', '_']);
        if s.is_empty() {
            return None;
        }
        let num = s.starts_with(|c: char| c.is_ascii_digit());
        let end = s
            .find(|c: char| c.is_ascii_digit() != num)
            .unwrap_or(s.len());
        Some((num, &s[..end], &s[end..]))
    }
    let (mut ar, mut br) = (a, b);
    loop {
        match (take(ar), take(br)) {
            (None, None) => return Equal,
            (None, Some(_)) => return Less,
            (Some(_), None) => return Greater,
            (Some((an, av, a_rest)), Some((bn, bv, b_rest))) => {
                let ord = match (an, bn) {
                    (true, true) => av
                        .parse::<u64>()
                        .unwrap_or(u64::MAX)
                        .cmp(&bv.parse::<u64>().unwrap_or(u64::MAX)),
                    (true, false) => Less,
                    (false, true) => Greater,
                    (false, false) => av.cmp(bv),
                };
                if ord != Equal {
                    return ord;
                }
                ar = a_rest;
                br = b_rest;
            }
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

fn release_from(v: &Value) -> Option<Release> {
    let want = platform_asset();
    let asset = v.get("assets")?.as_array()?.iter().find_map(|a| {
        let name = a.get("name")?.as_str()?;
        (name == want).then(|| {
            a.get("browser_download_url")
                .and_then(|u| u.as_str())
                .map(str::to_string)
        })?
    })?;
    Some(Release {
        tag: v.get("tag_name")?.as_str()?.to_string(),
        prerelease: v
            .get("prerelease")
            .and_then(|p| p.as_bool())
            .unwrap_or(false),
        date: v
            .get("published_at")
            .and_then(|p| p.as_str())
            .unwrap_or_default()
            .chars()
            .take(10)
            .collect(),
        asset_url: asset,
    })
}

/// All releases (stable + pre) that carry this platform's asset.
/// Spawned on a worker — HTTP blocks.
pub fn fetch_releases() -> Result<Vec<Release>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=30");
    let mut resp = agent()
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| format!("release list failed: {e}"))?;
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("release list failed: {e}"))?;
    let list: Vec<Value> = serde_json::from_str(&body).map_err(|e| format!("bad json: {e}"))?;
    Ok(list.iter().filter_map(release_from).collect())
}

/// Newest non-prerelease — what the startup prompt tracks.
/// `Err` means the check itself failed (offline, rate limit); `Ok(None)`
/// means there's no stable release carrying our asset yet.
pub fn latest_release() -> Result<Option<Release>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    match agent().get(&url).header("User-Agent", USER_AGENT).call() {
        Ok(mut resp) => {
            let body = resp
                .body_mut()
                .read_to_string()
                .map_err(|e| format!("latest release failed: {e}"))?;
            let v: Value = serde_json::from_str(&body).map_err(|e| format!("bad json: {e}"))?;
            Ok(release_from(&v))
        }
        // 404 = no non-prerelease exists yet — that's "nothing to do",
        // not an error worth surfacing.
        Err(ureq::Error::StatusCode(404)) => Ok(None),
        Err(e) => Err(format!("latest release failed: {e}")),
    }
}

// ------------------------------------------------------------------
// Paths
// ------------------------------------------------------------------

/// The binary the update actually replaces. Inside an AppImage
/// `current_exe()` resolves into the squashfs mount — the real file
/// is `$APPIMAGE`.
fn installed_exe() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
    {
        return Ok(p);
    }
    std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))
}

/// `AES-Oxide.exe` → `AES-Oxide.new` (staged download, sits beside it).
fn staged_path(exe: &Path) -> PathBuf {
    let mut n = exe.file_name().unwrap_or_default().to_os_string();
    n.push(".new");
    exe.with_file_name(n)
}

/// `AES-Oxide.exe` → `AES-Oxide.old` (rename-aside backup, cleaned on
/// the next startup).
fn backup_path(exe: &Path) -> PathBuf {
    let mut n = exe.file_name().unwrap_or_default().to_os_string();
    n.push(".old");
    exe.with_file_name(n)
}

/// Startup housekeeping: drop the previous update's `.old` backup and
/// any stale `.new` that never got applied. Helper mode skips itself.
pub fn startup_cleanup() {
    let Ok(exe) = installed_exe() else { return };
    let _ = std::fs::remove_file(backup_path(&exe));
    let staged = staged_path(&exe);
    if staged.is_file()
        && std::env::current_exe().map(|c| c != staged).unwrap_or(true)
    {
        let _ = std::fs::remove_file(staged);
    }
}

// ------------------------------------------------------------------
// Download + stage (worker thread)
// ------------------------------------------------------------------

/// Download `rel`'s asset, extract/stage it beside the running binary
/// as `<exe>.new`. `done` lands when staged; the app calls
/// `apply_and_relaunch` afterwards.
pub fn start(rel: Release) -> Shared {
    let p = Shared::default();
    let p2 = p.clone();
    std::thread::spawn(move || {
        let r = run(&rel, &p2);
        if let Ok(mut g) = p2.lock() {
            g.done = Some(r.map(|_| rel.tag.clone()));
        }
    });
    p
}

fn run(rel: &Release, p: &Shared) -> Result<(), String> {
    let exe = installed_exe()?;
    let staged = staged_path(&exe);
    let dir = crate::settings::data_root()
        .ok_or("no data dir")?
        .join("Updates");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Updates dir: {e}"))?;
    let dl_path = dir.join(platform_asset());

    crate::emulation::emu_dl::download(
        p,
        &format!("Downloading {}", rel.tag),
        &rel.asset_url,
        &dl_path,
    )?;

    set(p, "Extracting", 0.0);
    if cfg!(windows) {
        // The zip holds AES-Oxide.exe at its root.
        let f = std::fs::File::open(&dl_path).map_err(|e| format!("open zip: {e}"))?;
        let mut zip = zip::ZipArchive::new(f).map_err(|e| format!("bad zip: {e}"))?;
        let mut entry = zip
            .by_name("AES-Oxide.exe")
            .map_err(|_| "AES-Oxide.exe not in zip".to_string())?;
        let mut out =
            std::fs::File::create(&staged).map_err(|e| format!("stage: {e}"))?;
        std::io::copy(&mut entry, &mut out).map_err(|e| format!("stage: {e}"))?;
    } else {
        // The AppImage IS the binary — stage it executable.
        std::fs::copy(&dl_path, &staged).map_err(|e| format!("stage: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &staged,
                std::fs::Permissions::from_mode(0o755),
            );
        }
    }

    // Sanity: a truncated/empty download must never swap in.
    let size = std::fs::metadata(&staged).map(|m| m.len()).unwrap_or(0);
    if size < 1_000_000 {
        let _ = std::fs::remove_file(&staged);
        return Err("downloaded file looks truncated".into());
    }
    let _ = std::fs::remove_file(&dl_path);
    Ok(())
}

// ------------------------------------------------------------------
// Swap + relaunch
// ------------------------------------------------------------------

/// Swap the staged binary in and relaunch. The caller closes the
/// window right after `Ok` — the new process replaces this one.
///
/// Rename-aside of a running exe is allowed on Windows and Linux.
/// When it's denied anyway, the staged copy becomes the helper: it
/// waits for this process to exit, then performs the swap itself.
pub fn apply_and_relaunch() -> Result<(), String> {
    let exe = installed_exe()?;
    let staged = staged_path(&exe);
    let backup = backup_path(&exe);
    if !staged.is_file() {
        return Err("no staged update".into());
    }

    if std::fs::rename(&exe, &backup).is_err() {
        // Install dir won't let us rename the running exe — the staged
        // copy can still run, so it does the swap post-exit.
        std::process::Command::new(&staged)
            .arg("--apply-update")
            .arg(&exe)
            .spawn()
            .map_err(|e| format!("update helper failed to start: {e}"))?;
        return Ok(());
    }
    if let Err(e) = std::fs::rename(&staged, &exe) {
        // Keep the app runnable — restore the original.
        let _ = std::fs::rename(&backup, &exe);
        return Err(format!("install failed: {e}"));
    }
    std::process::Command::new(&exe)
        .spawn()
        .map_err(|e| format!("relaunch failed: {e}"))?;
    Ok(())
}

/// `--apply-update <target>` entry point: this process IS the staged
/// binary; wait for the parent to release `target`, swap, relaunch.
pub fn apply_update_helper(target: &Path) -> ! {
    let selfp = std::env::current_exe().unwrap_or_default();
    let backup = backup_path(target);
    // Wait for the parent to exit — the rename succeeding IS the
    // "target released" signal (max ~30s, then give up gracefully).
    let mut ok = false;
    for _ in 0..300 {
        if !target.exists() || std::fs::rename(target, &backup).is_ok() {
            ok = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if ok {
        // Self-rename works while running on both Windows and Linux;
        // fall back to copy if it doesn't.
        if std::fs::rename(&selfp, target).is_err() {
            let _ = std::fs::copy(&selfp, target);
        }
        let _ = std::process::Command::new(target).spawn();
    }
    std::process::exit(0);
}

// ------------------------------------------------------------------
// Async check helpers (worker → channel, polled per frame)
// ------------------------------------------------------------------

/// Startup check: newest non-prerelease, on a worker.
pub fn spawn_latest() -> mpsc::Receiver<Result<Option<Release>, String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(latest_release());
    });
    rx
}

/// Updates tab list: all releases incl. pre, on a worker.
pub fn spawn_list() -> mpsc::Receiver<Result<Vec<Release>, String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(fetch_releases());
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_cmp_semverish() {
        use std::cmp::Ordering::*;
        assert_eq!(version_cmp("v0.2.0", "0.1.9"), Greater);
        assert_eq!(version_cmp("0.1.0", "0.1.0"), Equal);
        assert_eq!(version_cmp("v0.1.0", "v0.2.0"), Less);
        assert_eq!(version_cmp("0.10.0", "0.9.9"), Greater);
        // Same numbers: release beats its own rc.
        assert_eq!(version_cmp("0.2.0", "0.2.0-rc1"), Greater);
        assert_eq!(version_cmp("0.2.0-rc1", "0.2.0"), Less);
        // Sequential rcs order; numeric runs compare numerically.
        assert_eq!(version_cmp("0.1.1-rc2", "v0.1.1-rc1"), Greater);
        assert_eq!(version_cmp("0.1.1-rc10", "0.1.1-rc2"), Greater);
        assert_eq!(version_cmp("0.1.1-rc1", "0.1.1-rc2"), Less);
        assert_eq!(version_cmp("0.1.1-beta", "0.1.1-alpha"), Greater);
    }

    #[test]
    fn staged_and_backup_names() {
        let exe = Path::new("/x/AES-Oxide.exe");
        assert_eq!(staged_path(exe), Path::new("/x/AES-Oxide.exe.new"));
        assert_eq!(backup_path(exe), Path::new("/x/AES-Oxide.exe.old"));
    }
}
