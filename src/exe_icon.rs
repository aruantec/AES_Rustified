//! Extract an executable's icon into an egui `ColorImage` — the
//! Rust counterpart of Lacrima's
//! `EmulatorSetupLaunchIconService.TryLoadSetupLaunchIcon`, used for
//! the handler-setup button in the emulation band.
//!
//! `SHDefExtractIconW` renders the icon at an arbitrary size (unlike
//! `ExtractIconExW`, which only yields the 32px system icon), so the
//! button gets a crisp 128px source. The `HICON` is drawn onto a
//! top-down DIB section with `DrawIconEx` and read back as RGBA.

use std::path::Path;

use eframe::egui::ColorImage;

#[cfg(windows)]
use windows::core::PCWSTR;
#[cfg(windows)]
use windows::Win32::Graphics::Gdi::*;
#[cfg(windows)]
use windows::Win32::UI::Shell::{ExtractIconExW, SHDefExtractIconW};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::*;

#[cfg(windows)]
const SIZE: i32 = 128;

#[cfg(windows)]
pub fn load(exe: &Path) -> Option<ColorImage> {
    let wide: Vec<u16> = exe
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        // SHDefExtractIcon renders at the requested size — the exe's
        // best icon frame scaled by the shell, not the 32px metric.
        let mut icon = HICON::default();
        if SHDefExtractIconW(
            PCWSTR(wide.as_ptr()),
            0,
            0,
            Some(&mut icon),
            None,
            SIZE as u32,
        )
        .is_err()
            || icon.is_invalid()
        {
            // Fallback — the plain 32px large icon.
            let got = ExtractIconExW(PCWSTR(wide.as_ptr()), 0, Some(&mut icon), None, 1);
            if got == 0 || icon.is_invalid() {
                return None;
            }
        }
        let img = render(icon);
        let _ = DestroyIcon(icon);
        img
    }
}

/// Draw `icon` onto a 128x128 top-down DIB and read the BGRA pixels
/// back as premultiplied RGBA.
#[cfg(windows)]
fn render(icon: HICON) -> Option<ColorImage> {
    unsafe {
        // A memory DC compatible with the screen — no source DC
        // needed, the DIB itself selects the format.
        let dc = CreateCompatibleDC(None);
        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = SIZE;
        info.bmiHeader.biHeight = -SIZE; // top-down
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB.0;
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let old = SelectObject(dc, bmp.into());
        let _ = DrawIconEx(dc, 0, 0, icon, SIZE, SIZE, 0, None, DI_NORMAL);
        SelectObject(dc, old);
        let _ = DeleteDC(dc);

        if bits.is_null() {
            let _ = DeleteObject(bmp.into());
            return None;
        }
        let px = (SIZE * SIZE) as usize;
        let raw = std::slice::from_raw_parts(bits as *const u8, px * 4).to_vec();
        let _ = DeleteObject(bmp.into());

        // BGRA premultiplied → unmult RGBA (DrawIconEx blends over a
        // zeroed canvas, so channels are already premultiplied).
        let mut rgba = Vec::with_capacity(raw.len());
        for c in raw.chunks_exact(4) {
            let (b, g, r, a) = (c[0] as u32, c[1] as u32, c[2] as u32, c[3] as u32);
            if a == 0 {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                rgba.extend_from_slice(&[
                    (r * 255 / a).min(255) as u8,
                    (g * 255 / a).min(255) as u8,
                    (b * 255 / a).min(255) as u8,
                    a as u8,
                ]);
            }
        }
        Some(ColorImage::from_rgba_unmultiplied(
            [SIZE as usize, SIZE as usize],
            &rgba,
        ))
    }
}

// ---------------------------------------------------------------------------
// Linux: ELF binaries carry no embedded icon, so sources are (in order)
// the AppImage's own payload (.DirIcon / desktop Icon=), sibling files
// beside the exe, then freedesktop .desktop/icon-theme lookup.

#[cfg(not(windows))]
use std::path::PathBuf;

#[cfg(not(windows))]
pub fn load(exe: &Path) -> Option<ColorImage> {
    appimage_icon(exe)
        .or_else(|| sibling_icon(exe))
        .or_else(|| desktop_icon(exe))
}

/// Decode a PNG/etc or SVG file into a ColorImage (128px for SVG,
/// native size for rasters — egui scales on draw).
#[cfg(not(windows))]
fn decode_icon(path: &Path) -> Option<ColorImage> {
    let data = std::fs::read(path).ok()?;
    // `.DirIcon` etc. have no extension — sniff the payload too.
    let is_svg = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
        || data
            .iter()
            .take(256)
            .skip_while(|b| b.is_ascii_whitespace())
            .take(4)
            .eq(b"<svg".iter())
        || data
            .iter()
            .take(256)
            .skip_while(|b| b.is_ascii_whitespace())
            .take(5)
            .eq(b"<?xml".iter());
    if is_svg {
        let opts = resvg::usvg::Options::default();
        let hint = eframe::egui::SizeHint::Size {
            width: 128,
            height: 128,
            maintain_aspect_ratio: true,
        };
        egui_extras::image::load_svg_bytes_with_size(&data, hint, &opts).ok()
    } else {
        let img = image::load_from_memory(&data).ok()?.to_rgba8();
        let (w, h) = img.dimensions();
        Some(ColorImage::from_rgba_unmultiplied(
            [w as usize, h as usize],
            &img,
        ))
    }
}

/// Pick an icon inside a directory (an AppImage mount or the exe's
/// folder): `.DirIcon` first, then the .desktop file's Icon=, then
/// any root-level png/svg.
#[cfg(not(windows))]
fn icon_in_dir(dir: &Path) -> Option<ColorImage> {
    let dir_icon = dir.join(".DirIcon");
    if dir_icon.is_file() {
        if let Some(img) = decode_icon(&dir_icon) {
            return Some(img);
        }
    }
    // .desktop files carry Icon=<name-or-path>; AppImages keep one at
    // the mount root, icon payloads under usr/share/icons or root.
    if let Ok(rd) = std::fs::read_dir(dir) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().is_some_and(|e| e == "desktop") {
                if let Ok(text) = std::fs::read_to_string(&p) {
                    for line in text.lines() {
                        if let Some(name) = line.trim().strip_prefix("Icon=") {
                            let name = name.trim();
                            if name.is_empty() {
                                continue;
                            }
                            if let Some(img) = resolve_icon_name(dir, name) {
                                return Some(img);
                            }
                        }
                    }
                }
            }
        }
    }
    // Any root-level icon file.
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut best: Option<PathBuf> = None;
        for ent in rd.flatten() {
            let p = ent.path();
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !matches!(ext.as_str(), "png" | "svg" | "xpm") {
                continue;
            }
            // Prefer SVG (crisp), else the largest png.
            let better = match &best {
                None => true,
                Some(b) => {
                    ext == "svg"
                        || (b.extension().and_then(|e| e.to_str()) != Some("svg")
                            && ent.metadata().map(|m| m.len()).unwrap_or(0)
                                > b.metadata().map(|m| m.len()).unwrap_or(0))
                }
            };
            if better {
                best = Some(p);
            }
        }
        if let Some(p) = best {
            return decode_icon(&p);
        }
    }
    None
}

/// Resolve a .desktop `Icon=` value inside an AppImage mount or a
/// system icon dir: absolute path, `usr/share/icons/**/name.*`,
/// `<dir>/name.*`.
#[cfg(not(windows))]
fn resolve_icon_name(dir: &Path, name: &str) -> Option<ColorImage> {
    let np = PathBuf::from(name);
    if np.is_absolute() && np.is_file() {
        return decode_icon(&np);
    }
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    for ext in ["svg", "png", "xpm"] {
        let direct = dir.join(format!("{stem}.{ext}"));
        if direct.is_file() {
            if let Some(img) = decode_icon(&direct) {
                return Some(img);
            }
        }
    }
    // Shallow recursive search — AppImage icons live under
    // usr/share/icons/hicolor/<size>/apps/.
    fn walk(dir: &Path, stem: &str, depth: u32) -> Option<PathBuf> {
        if depth > 5 {
            return None;
        }
        let rd = std::fs::read_dir(dir).ok()?;
        let mut nested = Vec::new();
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                nested.push(p);
                continue;
            }
            if p.file_stem().and_then(|s| s.to_str()) == Some(stem)
                && matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("png" | "svg" | "xpm")
                )
            {
                return Some(p);
            }
        }
        for d in nested {
            if let Some(p) = walk(&d, stem, depth + 1) {
                return Some(p);
            }
        }
        None
    }
    walk(dir, stem, 0).and_then(|p| decode_icon(&p))
}

/// `--appimage-mount` mounts the payload read-only and prints the
/// mount dir on stdout — grab the icon, then kill it. DISPLAY et al
/// are stripped so runtimes that ignore the flag fail instead of
/// opening a window on the user's session.
#[cfg(not(windows))]
fn appimage_icon(exe: &Path) -> Option<ColorImage> {
    let name = exe
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !name.ends_with(".appimage") {
        return None;
    }
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    let mut child = Command::new(exe)
        .arg("--appimage-mount")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("XAUTHORITY")
        .spawn()
        .ok()?;
    // If the runtime ignores the flag it may block launching the app —
    // kill unconditionally after a short window.
    let pid = child.id();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(4));
        let _ = Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status();
    });
    let stdout = child.stdout.take()?;
    let mut line = String::new();
    let read = std::io::BufReader::new(stdout).read_line(&mut line).ok()?;
    let mount = line.trim().to_string();
    let dir = PathBuf::from(&mount);
    let img = if read > 0 && dir.is_dir() {
        icon_in_dir(&dir)
    } else {
        None
    };
    let _ = child.kill();
    let _ = child.wait();
    img
}

/// Sibling files next to the exe: `<stem>.png|svg`, `icon.*`,
/// `.DirIcon`.
#[cfg(not(windows))]
fn sibling_icon(exe: &Path) -> Option<ColorImage> {
    let dir = exe.parent()?;
    let stem = exe.file_stem().and_then(|s| s.to_str())?;
    for ext in ["svg", "png"] {
        for cand in [
            dir.join(format!("{stem}.{ext}")),
            dir.join(format!("icon.{ext}")),
        ] {
            if cand.is_file() {
                if let Some(img) = decode_icon(&cand) {
                    return Some(img);
                }
            }
        }
    }
    let dir_icon = dir.join(".DirIcon");
    if dir_icon.is_file() {
        return decode_icon(&dir_icon);
    }
    None
}

/// freedesktop lookup: a .desktop file whose Exec/Name mentions the
/// exe stem, then Icon= resolved through the icon theme dirs.
#[cfg(not(windows))]
fn desktop_icon(exe: &Path) -> Option<ColorImage> {
    let stem = exe
        .file_stem()
        .and_then(|s| s.to_str())?
        .to_ascii_lowercase();
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let mut apps = vec![
        home.join(".local/share/applications"),
        PathBuf::from("/usr/share/applications"),
        home.join(".local/share/flatpak/exports/share/applications"),
        PathBuf::from("/var/lib/flatpak/exports/share/applications"),
    ];
    apps.retain(|d| d.is_dir());
    for dir in &apps {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let fname_hit = p
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.to_ascii_lowercase().contains(&stem));
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            let body_hit = text
                .lines()
                .filter(|l| l.starts_with("Exec=") || l.starts_with("Name="))
                .any(|l| l.to_ascii_lowercase().contains(&stem));
            if !(fname_hit || body_hit) {
                continue;
            }
            for line in text.lines() {
                if let Some(name) = line.trim().strip_prefix("Icon=") {
                    let name = name.trim();
                    if name.is_empty() {
                        continue;
                    }
                    if let Some(img) = theme_icon(&home, name) {
                        return Some(img);
                    }
                }
            }
        }
    }
    None
}

/// Resolve an icon-theme name: absolute path, pixmaps, or
/// hicolor/<size>/apps under the standard icon roots (largest first).
#[cfg(not(windows))]
fn theme_icon(home: &Path, name: &str) -> Option<ColorImage> {
    let np = PathBuf::from(name);
    if np.is_absolute() && np.is_file() {
        return decode_icon(&np);
    }
    let roots = [
        home.join(".local/share/icons"),
        home.join(".icons"),
        PathBuf::from("/usr/share/icons"),
        PathBuf::from("/usr/local/share/icons"),
        home.join(".local/share/flatpak/exports/share/icons"),
        PathBuf::from("/var/lib/flatpak/exports/share/icons"),
    ];
    for ext in ["svg", "png", "xpm"] {
        for root in &roots {
            // hicolor/<size>/apps is the common layout; also try
            // <theme>/apps directly and plain <root>/name.ext.
            for sub in [
                root.join(format!("hicolor/scalable/apps/{name}.{ext}")),
                root.join(format!("hicolor/256x256/apps/{name}.{ext}")),
                root.join(format!("hicolor/128x128/apps/{name}.{ext}")),
                root.join(format!("hicolor/64x64/apps/{name}.{ext}")),
                root.join(format!("hicolor/48x48/apps/{name}.{ext}")),
                root.join(format!("{name}.{ext}")),
            ] {
                if sub.is_file() {
                    if let Some(img) = decode_icon(&sub) {
                        return Some(img);
                    }
                }
            }
        }
        let pixmap = PathBuf::from(format!("/usr/share/pixmaps/{name}.{ext}"));
        if pixmap.is_file() {
            if let Some(img) = decode_icon(&pixmap) {
                return Some(img);
            }
        }
    }
    None
}

#[cfg(all(test, not(windows)))]
mod tests {
    /// Manual check: `AES_TEST_EXE=<path> cargo test exe_icon -- --ignored`
    /// prints the decoded icon size for a real exe/AppImage.
    #[test]
    #[ignore]
    fn load_real_exe() {
        let exe = std::path::PathBuf::from(
            std::env::var("AES_TEST_EXE").expect("set AES_TEST_EXE"),
        );
        let img = super::load(&exe).expect("no icon found");
        eprintln!("icon {}x{}", img.size[0], img.size[1]);
        assert!(img.size[0] > 0 && img.size[1] > 0);
    }
}
