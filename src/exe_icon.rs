//! Extract an executable's icon into an egui `ColorImage` — the
//! Rust counterpart of Lacrima's
//! `EmulatorSetupLaunchIconService.TryLoadSetupLaunchIcon`, used for
//! the handler-setup button in the emulation band.
//!
//! `SHDefExtractIconW` renders the icon at an arbitrary size (unlike
//! `ExtractIconExW`, which only yields the 32px system icon), so the
//! button gets a crisp 128px source. The `HICON` is drawn onto a
//! top-down DIB section with `DrawIconEx` and read back as RGBA.

#![cfg(windows)]

use std::path::Path;

use eframe::egui::ColorImage;
use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Shell::{ExtractIconExW, SHDefExtractIconW};
use windows::Win32::UI::WindowsAndMessaging::*;

const SIZE: i32 = 128;

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
