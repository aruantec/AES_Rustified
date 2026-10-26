//! Textures shared between the main egui context and the Wayland
//! mini-window's private context.
//!
//! egui TextureHandles are per-Context: a handle made on the main
//! context resolves its TextureId against THAT context's manager,
//! so it paints the wrong texture (usually the font atlas) in any
//! other context. But the GPU side is just id→texture — the mini
//! renderer resolves whatever ids its paint jobs carry. Shared
//! textures therefore keep their single main-context handle while
//! a copy of each ImageDelta is queued here; `MiniWin` injects the
//! queue into the mini context's own `textures_delta` before the
//! gpu upload, and the same ids resolve there too.
//!
//! Off the Wayland platforms this is a plain `Context::load_texture`
//! passthrough.
use eframe::egui;

#[cfg(all(unix, not(target_os = "macos")))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(all(unix, not(target_os = "macos")))]
use std::sync::{Arc, Mutex, RwLock};

#[cfg(all(unix, not(target_os = "macos")))]
type TexMgr = Arc<egui::epaint::mutex::RwLock<egui::epaint::TextureManager>>;
#[cfg(all(unix, not(target_os = "macos")))]
static MAIN_TEX: RwLock<Option<TexMgr>> = RwLock::new(None);
#[cfg(all(unix, not(target_os = "macos")))]
static PENDING: Mutex<Vec<(egui::TextureId, egui::epaint::ImageDelta)>> =
    Mutex::new(Vec::new());
/// Set when no MiniWin exists (X11, plumbing failure) — nothing will
/// ever drain the queue, so queueing stops instead of leaking.
#[cfg(all(unix, not(target_os = "macos")))]
static DISABLED: AtomicBool = AtomicBool::new(false);

/// Call once for the main context before any `load_tex` use.
pub(crate) fn register(ctx: &egui::Context) {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        *MAIN_TEX.write().unwrap() = Some(ctx.tex_manager());
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    let _ = ctx;
}

/// `Context::load_texture` replacement for anything the mini shell
/// might paint (covers, placeholders, chrome icons): allocates on
/// the main manager like normal, and queues a copy of the delta for
/// injection into the mini renderer.
pub(crate) fn load_tex(
    ctx: &egui::Context,
    name: impl Into<String>,
    img: egui::ColorImage,
    options: egui::TextureOptions,
) -> egui::TextureHandle {
    // A zero-dimension image is a wgpu validation panic on upload —
    // substitute a 1px transparent texture.
    let img = if img.width() == 0 || img.height() == 0 {
        egui::ColorImage::filled([1, 1], egui::Color32::TRANSPARENT)
    } else {
        img
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = ctx;
        let mgr = MAIN_TEX
            .read()
            .unwrap()
            .clone()
            .expect("shared_tex::register before load_tex");
        let id = mgr
            .write()
            .alloc(name.into(), img.clone().into(), options);
        if !DISABLED.load(Ordering::Relaxed) {
            PENDING
                .lock()
                .unwrap()
                .push((id, egui::epaint::ImageDelta::full(img, options)));
        }
        egui::TextureHandle::new(mgr, id)
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        ctx.load_texture(name, img, options)
    }
}

/// MiniWin-side drain of queued shared deltas.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn take_pending() -> Vec<(egui::TextureId, egui::epaint::ImageDelta)> {
    std::mem::take(&mut *PENDING.lock().unwrap())
}

/// True while the main manager still holds `id` — MiniWin frees its
/// mapped copy once the shared handle drops.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn is_allocated(id: &egui::TextureId) -> bool {
    MAIN_TEX
        .read()
        .unwrap()
        .as_ref()
        .is_some_and(|m| m.read().meta(*id).is_some())
}

/// No MiniWin will ever exist — stop queueing deltas.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn disable() {
    DISABLED.store(true, Ordering::Relaxed);
    PENDING.lock().unwrap().clear();
}
