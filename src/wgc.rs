//! Native Windows Graphics Capture — the Rust counterpart of
//! Lacrima's WgcBridge.dll CPU path: a dedicated MTA thread owns a
//! D3D11 device + `Direct3D11CaptureFramePool`, drains each
//! `FrameArrived` burst to the freshest frame, copies the GPU texture
//! into a CPU-readable staging texture, swizzles BGRA→RGBA, and
//! publishes the pixels into a shared slot the UI thread uploads to an
//! egui texture. No C++ library involved.
//!
//! The capture target is an HWND — hidden (moved + opacity-0) windows
//! keep rendering to DWM, so WGC still produces frames for them, which
//! is exactly the trick Lacrima uses to "embed" an emulator.

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::{Interface, Result};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

/// The frame slot the capture thread publishes into. `seq` bumps on
/// every new frame — the UI compares it against the last-uploaded seq
/// to avoid re-uploading a stale frame.
#[derive(Default)]
pub struct FrameSlot {
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
    pub seq: u64,
    /// Set once when session setup fails (device creation, capture
    /// attach, …) — the session reports it as a launch error.
    pub err: Option<String>,
}

/// A live capture session. Drop it to stop capturing — the thread
/// joins quickly since the stop flag is checked every few ms.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Start capturing `hwnd` on a background thread, publishing
    /// frames into `frames`. Frame delivery starts asynchronously —
    /// poll the slot for `seq`/`err`.
    pub fn start(hwnd: HWND, frames: Arc<Mutex<FrameSlot>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            // HWND isn't Send — carry it as a usize across the spawn.
            let hwnd_raw = hwnd.0 as usize;
            std::thread::Builder::new()
                .name("wgc-capture".into())
                .spawn(move || {
                    let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
                    if let Err(e) = run(hwnd, frames.clone(), stop.clone()) {
                        frames
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .err = Some(format!("{e}"));
                    }
                })
                .expect("spawn wgc thread")
        };
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Everything the FrameArrived callback needs — Mutex-guarded so
/// concurrent pool threads serialize through it (the device is also
/// put in multithread-protected mode like WgcBridge does).
struct Ctx {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: Option<ID3D11Texture2D>,
    pool: Direct3D11CaptureFramePool,
    winrt_device: IDirect3DDevice,
    pool_size: SizeInt32,
    frames: Arc<Mutex<FrameSlot>>,
    /// Persistent swizzle buffer — swapped into the frame slot so the
    /// steady-state path allocates nothing per frame.
    scratch: Vec<u8>,
}

// The callback fires on the frame pool's own threads — access is
// serialized by the Mutex, and the D3D11 device is put in
// multithread-protected mode like WgcBridge does.
unsafe impl Send for Ctx {}

fn run(hwnd: HWND, frames: Arc<Mutex<FrameSlot>>, stop: Arc<AtomicBool>) -> Result<()> {
    // WGC + CreateFreeThreaded wants an MTA thread.
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }

    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let device = device.ok_or(windows::core::Error::empty())?;
    let context = context.ok_or(windows::core::Error::empty())?;

    // D3D11 immediate context calls can land on several pool threads.
    unsafe {
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
    }

    let dxgi: IDXGIDevice = device.cast()?;
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
    let winrt_device: IDirect3DDevice = inspectable.cast()?;

    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd)? };
    let size = item.Size()?;

    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )?;

    let ctx = Arc::new(Mutex::new(Ctx {
        device,
        context,
        staging: None,
        pool: pool.clone(),
        winrt_device: winrt_device.clone(),
        pool_size: size,
        frames: frames.clone(),
        scratch: Vec::new(),
    }));

    let handler = {
        let ctx = ctx.clone();
        TypedEventHandler::new(
            move |sender: windows::core::Ref<'_, Direct3D11CaptureFramePool>,
                  _args: windows::core::Ref<'_, windows::core::IInspectable>| {
                let Some(pool) = sender.as_ref() else { return Ok(()) };
                if let Ok(mut ctx) = ctx.lock() {
                    // A panic here unwinds across the WinRT callback
                    // boundary = process abort. Contain it so a dying
                    // capture (game crash mid-frame) can't take the
                    // app down.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        drain_frames(pool, &mut ctx);
                    }));
                }
                Ok(())
            },
        )
    };
    let _token = pool.FrameArrived(&handler)?;

    let session = pool.CreateCaptureSession(&item)?;
    // No cursor composition, no yellow border (same toggles WgcBridge
    // applies — the session casts to the session2/3 interfaces itself).
    let _ = session.SetIsCursorCaptureEnabled(false);
    let _ = session.SetIsBorderRequired(false);
    session.StartCapture()?;

    // Park: the frame pool runs on its own threads; we just hold the
    // session alive until asked to stop.
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(15));
    }
    session.Close()?;
    pool.Close()?;
    Ok(())
}

/// Drain the pool to the freshest frame, copy it to the staging
/// texture, map, and publish RGBA pixels into the frame slot.
fn drain_frames(pool: &Direct3D11CaptureFramePool, ctx: &mut Ctx) {
    let mut frame: Option<Direct3D11CaptureFrame> = None;
    while let Ok(next) = pool.TryGetNextFrame() {
        frame = Some(next);
    }
    let Some(frame) = frame else { return };

    // Window resized — recreate the pool so frames keep flowing
    // (recreate is async; next callbacks arrive at the new size).
    if let Ok(cs) = frame.ContentSize() {
        if cs.Width != ctx.pool_size.Width || cs.Height != ctx.pool_size.Height {
            let _ = ctx.pool.Recreate(
                &ctx.winrt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                cs,
            );
            ctx.pool_size = cs;
        }
    }

    let Ok(surface) = frame.Surface() else { return };
    let Ok(access) = surface.cast::<IDirect3DDxgiInterfaceAccess>() else {
        return;
    };
    let tex: ID3D11Texture2D = match unsafe { access.GetInterface() } {
        Ok(t) => t,
        Err(_) => return,
    };

    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut desc) };
    let (w, h) = (desc.Width as usize, desc.Height as usize);
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return;
    }

    // (Re)create the CPU-readable staging texture on size change.
    let need_new = ctx.staging.as_ref().is_none_or(|s| {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        unsafe { s.GetDesc(&mut d) };
        d.Width != desc.Width || d.Height != desc.Height || d.Format != desc.Format
    });
    if need_new {
        let mut sd = desc;
        sd.Usage = D3D11_USAGE_STAGING;
        sd.BindFlags = 0;
        sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        sd.MiscFlags = 0;
        let mut staging = None;
        if unsafe { ctx.device.CreateTexture2D(&sd, None, Some(&mut staging)) }.is_err() {
            return;
        }
        ctx.staging = staging;
    }
    let staging = ctx.staging.as_ref().unwrap().clone();

    unsafe {
        ctx.context.CopyResource(&staging, &tex);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if ctx
            .context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .is_err()
        {
            return;
        }
        if mapped.pData.is_null() {
            ctx.context.Unmap(&staging, 0);
            return;
        }

        let row_pitch = mapped.RowPitch as usize;
        // u32-wide BGRA->RGBA: swap the B and R bytes, force alpha —
        // one read+write per pixel instead of eight byte ops.
        let px = w * h;
        ctx.scratch.clear();
        ctx.scratch.resize(px * 4, 0);
        let src = mapped.pData as *const u8;
        let dst = ctx.scratch.as_mut_ptr() as *mut u32;
        for y in 0..h {
            let srow = src.add(y * row_pitch) as *const u32;
            let drow = dst.add(y * w);
            for x in 0..w {
                let p = srow.add(x).read_unaligned();
                drow.add(x).write(
                    (p & 0x0000_FF00) | ((p & 0xFF) << 16) | ((p >> 16) & 0xFF) | 0xFF00_0000,
                );
            }
        }
        ctx.context.Unmap(&staging, 0);

        let mut slot = ctx.frames.lock().unwrap_or_else(|p| p.into_inner());
        slot.w = w;
        slot.h = h;
        std::mem::swap(&mut ctx.scratch, &mut slot.rgba);
        slot.seq += 1;
    }
    let _ = frame.Close();
}
