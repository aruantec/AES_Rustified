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
use windows::Win32::Foundation::{CloseHandle, HMODULE, HWND, HANDLE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGIResource1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE};
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

/// Live render adjustments shared between the UI and the capture
/// thread — the UI writes atomics, the swizzle folds them into the
/// BGRA→RGBA pass so brightness/saturation/tint cost zero extra
/// per-frame work. Lacrima's RenderBrightness / RenderSaturation /
/// PortalCaptureTint.
pub struct RenderShared {
    /// f32 bits — brightness multiplier, 1.0 = unchanged.
    pub brightness: std::sync::atomic::AtomicU32,
    /// f32 bits — saturation multiplier, 1.0 = unchanged.
    pub saturation: std::sync::atomic::AtomicU32,
    /// 0xRRGGBB tint multiply, 0xFFFFFF = unchanged.
    pub tint: std::sync::atomic::AtomicU32,
    /// Whether the CPU readback (staging map + memcpy) is still
    /// wanted — the UI sets it false once the zero-copy GPU path is
    /// live and nothing else needs pixels on the CPU (pillarbox
    /// detection reads the frame buffer).
    pub want_cpu: AtomicBool,
}

impl Default for RenderShared {
    fn default() -> Self {
        Self {
            brightness: std::sync::atomic::AtomicU32::new(1.0f32.to_bits()),
            saturation: std::sync::atomic::AtomicU32::new(1.0f32.to_bits()),
            tint: std::sync::atomic::AtomicU32::new(0xFF_FF_FF),
            want_cpu: AtomicBool::new(true),
        }
    }
}

/// GPU-side frame sharing — the zero-copy path. Each captured frame
/// is `CopyResource`d into one slot of a 3-texture NT-shared ring and
/// the cross-API `ID3D11Fence` signals its slot index; the UI opens
/// the handles on wgpu's DX12 device, queues a fence wait, and
/// samples the texture directly. `seq` is the fence value — the slot
/// is `seq % 3`.
#[derive(Default)]
pub struct GpuState {
    /// Set on (re)creation of the ring — `gen` bumps so the UI
    /// re-imports after a capture-size change.
    pub share: Mutex<Option<GpuShare>>,
    /// Latest signaled frame — also the fence value.
    pub seq: std::sync::atomic::AtomicU64,
    /// First interop failure — the capture stays on the CPU mirror
    /// permanently after this is set (surfaced in the stats overlay).
    pub err: Mutex<Option<String>>,
}

/// NT handles for the shared ring — stored as `isize` so the struct
/// stays plain data (`HANDLE` is a raw pointer). The fence handle is
/// the same for every generation.
#[derive(Clone)]
pub struct GpuShare {
    pub tex: [isize; 3],
    pub fence: isize,
    pub w: u32,
    pub h: u32,
    pub gen: u64,
}

/// A live capture session. Drop it to stop capturing — the thread
/// joins quickly since the stop flag is checked every few ms.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Start capturing `hwnd` on a background thread, publishing
    /// frames into `frames` (CPU readback) and `gpu` (the zero-copy
    /// shared-texture ring). Frame delivery starts asynchronously —
    /// poll the slots for `seq`/`err`. `low_latency` shrinks the WGC
    /// frame pool to a single buffer (Lacrima's LowLatencyCapture).
    pub fn start(
        hwnd: HWND,
        frames: Arc<Mutex<FrameSlot>>,
        gpu: Arc<GpuState>,
        render: Arc<RenderShared>,
        low_latency: bool,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            // HWND isn't Send — carry it as a usize across the spawn.
            let hwnd_raw = hwnd.0 as usize;
            std::thread::Builder::new()
                .name("wgc-capture".into())
                .spawn(move || {
                    let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
                    if let Err(e) =
                        run(hwnd, frames.clone(), gpu.clone(), stop.clone(), render, low_latency)
                    {
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

/// First DXGI adapter — the "GPU: name (vendor)" line of Lacrima's
/// detailed GPU info in the stats overlay. One factory enum, so the
/// caller should cache the result.
pub fn gpu_desc() -> Option<(String, String)> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let adapter = factory.EnumAdapters1(0).ok()?;
        let desc = adapter.GetDesc1().ok()?;
        let name = String::from_utf16_lossy(&desc.Description)
            .trim_end_matches('\0')
            .to_string();
        let vendor = match desc.VendorId {
            0x10DE => "NVIDIA".to_string(),
            0x1002 | 0x1022 => "AMD".to_string(),
            0x8086 => "Intel".to_string(),
            v => format!("0x{v:04X}"),
        };
        Some((name, vendor))
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
    render: Arc<RenderShared>,
    /// Frame-pool depth — 1 in low-latency mode, 2 otherwise; also
    /// used when the pool recreates after a content resize.
    pool_frames: i32,
    // ---- Zero-copy GPU path -------------------------------------
    gpu: Arc<GpuState>,
    /// The 3-slot NT-shared ring the freshest frame is copied into;
    /// recreated when the capture size changes.
    gpu_ring: Option<([ID3D11Texture2D; 3], u32, u32)>,
    /// Cross-API fence — signaled after each ring copy; the D3D12
    /// side waits on it before sampling.
    gpu_fence: Option<ID3D11Fence>,
    /// Its NT handle, opened once and published with every GpuShare.
    gpu_fence_handle: HANDLE,
    ctx4: Option<ID3D11DeviceContext4>,
    gpu_seq: u64,
    gpu_gen: u64,
    /// Set false permanently when interop setup fails — the capture
    /// then runs CPU-only without retrying every frame.
    gpu_ok: bool,
}

// The callback fires on the frame pool's own threads — access is
// serialized by the Mutex, and the D3D11 device is put in
// multithread-protected mode like WgcBridge does.
unsafe impl Send for Ctx {}

fn run(
    hwnd: HWND,
    frames: Arc<Mutex<FrameSlot>>,
    gpu: Arc<GpuState>,
    stop: Arc<AtomicBool>,
    render: Arc<RenderShared>,
    low_latency: bool,
) -> Result<()> {
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

    let pool_frames = if low_latency { 1 } else { 2 };
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        pool_frames,
        size,
    )?;

    let ctx4 = context.cast::<ID3D11DeviceContext4>().ok();
    // The interop path needs D3D11.5 (shared fences) — available
    // since Windows 10 1809; on anything older we just run CPU.
    let gpu_ok = ctx4.is_some();
    if !gpu_ok {
        *gpu.err.lock().unwrap_or_else(|p| p.into_inner()) =
            Some("no ID3D11DeviceContext4 (pre-1809 Windows)".to_string());
    }
    let ctx = Arc::new(Mutex::new(Ctx {
        device,
        context,
        staging: None,
        pool: pool.clone(),
        winrt_device: winrt_device.clone(),
        pool_size: size,
        frames: frames.clone(),
        scratch: Vec::new(),
        render,
        pool_frames,
        gpu,
        gpu_ring: None,
        gpu_fence: None,
        gpu_fence_handle: HANDLE::default(),
        ctx4,
        gpu_seq: 0,
        gpu_gen: 0,
        gpu_ok,
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
                ctx.pool_frames,
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

    // Zero-copy GPU path — copy into the NT-shared ring + signal the
    // cross-API fence (Lacrima's GPU interop; the CPU mirror below is
    // its fallback). Disabled permanently on the first failure.
    if ctx.gpu_ok {
        if let Err(e) = gpu_copy(ctx, &tex, &desc) {
            ctx.gpu_ok = false;
            ctx.gpu_ring = None;
            ctx.gpu_fence = None;
            if !ctx.gpu_fence_handle.is_invalid() {
                unsafe {
                    let _ = CloseHandle(ctx.gpu_fence_handle);
                }
                ctx.gpu_fence_handle = HANDLE::default();
            }
            *ctx.gpu.err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
        }
    }

    if !ctx.render.want_cpu.load(Ordering::Relaxed) {
        // The frame must be released back to the pool on EVERY path —
        // skipping Close starves it (fatal at pool_size=1).
        let _ = frame.Close();
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
        // Render adjustments (brightness/saturation/tint) fold into
        // the same pass — at defaults the plain swizzle runs.
        let bright = f32::from_bits(ctx.render.brightness.load(Ordering::Relaxed));
        let sat = f32::from_bits(ctx.render.saturation.load(Ordering::Relaxed));
        let tint = ctx.render.tint.load(Ordering::Relaxed);
        let adjust = (bright - 1.0).abs() > 0.001
            || (sat - 1.0).abs() > 0.001
            || tint != 0xFF_FF_FF;
        for y in 0..h {
            let srow = src.add(y * row_pitch) as *const u32;
            let drow = dst.add(y * w);
            for x in 0..w {
                let p = srow.add(x).read_unaligned();
                if !adjust {
                    drow.add(x).write(
                        (p & 0x0000_FF00)
                            | ((p & 0xFF) << 16)
                            | ((p >> 16) & 0xFF)
                            | 0xFF00_0000,
                    );
                } else {
                    let b = (p & 0xFF) as f32;
                    let g = ((p >> 8) & 0xFF) as f32;
                    let r = ((p >> 16) & 0xFF) as f32;
                    // Saturation — lerp each channel toward the pixel's
                    // luma by `sat` (0 = grayscale, 1 = unchanged).
                    let luma = 0.299 * r + 0.587 * g + 0.114 * b;
                    let adj = |c: f32, t: u32| {
                        ((luma + (c - luma) * sat) * bright * t as f32 / 255.0)
                            .clamp(0.0, 255.0) as u32
                    };
                    // RGBA byte order — R in bits 0-7 like the fast
                    // path's `((p >> 16) & 0xFF)` term.
                    let pk = adj(r, (tint >> 16) & 0xFF)
                        | (adj(g, (tint >> 8) & 0xFF) << 8)
                        | (adj(b, tint & 0xFF) << 16)
                        | 0xFF00_0000;
                    drow.add(x).write(pk);
                }
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

/// The zero-copy path: copy the WGC frame into one slot of the
/// NT-shared ring, then signal the cross-API fence so the D3D12
/// consumer can queue a `Wait` and sample the slot in place —
/// Lacrima's GPU interop, minus the GL layer (wgpu samples the
/// imported D3D12 resource directly).
fn gpu_copy(
    ctx: &mut Ctx,
    src: &ID3D11Texture2D,
    desc: &D3D11_TEXTURE2D_DESC,
) -> std::result::Result<(), String> {
    unsafe {
        // The shared fence is created once — its NT handle rides in
        // every published GpuShare.
        if ctx.gpu_fence.is_none() {
            let dev5: ID3D11Device5 = ctx
                .device
                .cast()
                .map_err(|e| format!("ID3D11Device5: {e}"))?;
            let mut fence: Option<ID3D11Fence> = None;
            dev5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence)
                .map_err(|e| format!("CreateFence: {e}"))?;
            let fence = fence.ok_or_else(|| "CreateFence empty".to_string())?;
            let h = fence
                .CreateSharedHandle(
                    None,
                    DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                    windows::core::PCWSTR::null(),
                )
                .map_err(|e| format!("fence share handle: {e}"))?;
            ctx.gpu_fence = Some(fence);
            ctx.gpu_fence_handle = h;
        }

        // (Re)create the ring on a size/format change and publish a
        // new generation so the UI re-imports the handles.
        let need_ring = ctx.gpu_ring.as_ref().is_none_or(|(_, w, h)| {
            *w != desc.Width || *h != desc.Height
        });
        if need_ring {
            let mut ring: Option<[ID3D11Texture2D; 3]> = None;
            let mut handles = [0isize; 3];
            let mut err = None;
            let mut slot_texs = Vec::with_capacity(3);
            for i in 0..3 {
                let mut sd = *desc;
                sd.Usage = D3D11_USAGE_DEFAULT;
                sd.BindFlags = D3D11_BIND_SHADER_RESOURCE.0 as u32;
                sd.CPUAccessFlags = 0;
                // NTHANDLE is only valid in conjunction with
                // KEYEDMUTEX — without it CreateTexture2D returns
                // E_INVALIDARG. The mutex itself is ignored: sync is
                // the cross-API fence (D3D12 has no keyed mutexes).
                sd.MiscFlags = D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 as u32
                    | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32;
                // The pool's format is known anyway — don't trust a
                // possibly typeless source desc.
                sd.Format = windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
                sd.MipLevels = 1;
                sd.ArraySize = 1;
                sd.SampleDesc.Count = 1;
                sd.SampleDesc.Quality = 0;
                let mut t = None;
                match ctx.device.CreateTexture2D(&sd, None, Some(&mut t)) {
                    Ok(()) if t.is_some() => {
                        let t = t.unwrap();
                        match t
                            .cast::<IDXGIResource1>()
                            .and_then(|r| {
                                r.CreateSharedHandle(
                                    None,
                                    DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                                    windows::core::PCWSTR::null(),
                                )
                            }) {
                            Ok(h) => {
                                handles[i] = h.0 as isize;
                                slot_texs.push(t);
                            }
                            Err(e) => {
                                err = Some(format!("ring share handle: {e}"));
                                break;
                            }
                        }
                    }
                    r => {
                        err = Some(format!(
                            "ring texture: {:?}",
                            r.err().map(|e| e.code())
                        ));
                        break;
                    }
                }
            }
            if slot_texs.len() == 3 {
                ring = Some([
                    slot_texs[0].clone(),
                    slot_texs[1].clone(),
                    slot_texs[2].clone(),
                ]);
            }
            let Some(ring) = ring else {
                for h in handles {
                    if h != 0 {
                        let _ = CloseHandle(HANDLE(h as *mut _));
                    }
                }
                return Err(err.unwrap_or_else(|| "shared ring".to_string()));
            };
            ctx.gpu_gen += 1;
            let gen = ctx.gpu_gen;
            ctx.gpu_ring = Some((ring, desc.Width, desc.Height));
            *ctx.gpu.share.lock().unwrap_or_else(|p| p.into_inner()) = Some(GpuShare {
                tex: handles,
                fence: ctx.gpu_fence_handle.0 as isize,
                w: desc.Width,
                h: desc.Height,
                gen,
            });
        }

        let Some((ring, _, _)) = ctx.gpu_ring.as_ref() else {
            return Err("ring missing".to_string());
        };
        let v = ctx.gpu_seq + 1;
        let slot = (v % 3) as usize;
        // The KEYEDMUTEX flag is required for NTHANDLE sharing and
        // some drivers silently drop CopyResource into a keyed-mutex
        // texture written without an acquire — bracket the copy. A
        // keyed mutex only grants the key it currently holds: first
        // acquire is 0 (initial state), later acquires 1 (our own
        // release) so the wait is always instant.
        if let Ok(km) = ring[slot].cast::<windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex>() {
            // Per-slot mutex state — the first ring cycle acquires
            // each slot's initial key 0, later cycles acquire 1.
            let key = if v <= 3 { 0 } else { 1 };
            let _ = km.AcquireSync(key, 10);
            ctx.context.CopyResource(&ring[slot], src);
            let _ = km.ReleaseSync(1);
        } else {
            ctx.context.CopyResource(&ring[slot], src);
        }
        ctx.ctx4
            .as_ref()
            .unwrap()
            .Signal(ctx.gpu_fence.as_ref().unwrap(), v)
            .map_err(|e| format!("fence signal: {e}"))?;
        // Flush so the copy+signal actually reach the GPU queue now —
        // an idle context can otherwise batch them until much later.
        ctx.context.Flush();
        ctx.gpu_seq = v;
        ctx.gpu.seq.store(v, Ordering::Release);
        Ok(())
    }
}
