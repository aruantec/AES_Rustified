//! Capture post-processing shaders — Lacrima's `Shaders/glsl`
//! collection (the Rendering tab's "Shader file" ComboBox) applied to
//! the WGC capture entirely on the GPU.
//!
//! Each `.glsl` is a single-pass fragment shader with a fixed uniform
//! interface (`Texture`, `TextureSize`, `InputSize`, `OutputSize`,
//! `FrameCount`, `FrameDirection`, `uBrightness`, `uSaturation`,
//! `uColorTint`). The VERTEX half is dropped (the host draws a
//! fullscreen triangle + scissor); the FRAGMENT half is wrapped in a
//! prelude providing those uniforms, translated to WGSL with naga and
//! cached per name. The captured RGBA frame is uploaded straight to a
//! wgpu texture — when a shader is active the egui texture upload +
//! `painter.image` path is bypassed and a `PaintCallback` draws the
//! dest rect through the selected pipeline instead.
//!
//! Brightness/saturation/tint stay applied by the WGC swizzle, so the
//! shader is fed identity values for them — applying them twice would
//! square the effect.

use eframe::egui::Rect;
use eframe::egui_wgpu::{self, wgpu};
use eframe::epaint::PaintCallbackInfo;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
#[cfg(windows)]
use windows::Win32::Foundation::HANDLE;
#[cfg(windows)]
use windows::Win32::Graphics::Direct3D12::{ID3D12Fence, ID3D12Resource};

// The cross-API wait fence is a real COM object only on Windows — the
// zero-copy ring never exists elsewhere, so the field types just need
// somewhere to typecheck against.
#[cfg(not(windows))]
#[allow(non_camel_case_types)]
type ID3D12Fence = ();

/// The shipped collection — `(name, source)`; the name is the file
/// stem shown in the panel ComboBox ("None" is implicit, not a file).
const EMBEDDED: &[(&str, &str)] = &[
    ("Anime_HD", include_str!("../../shaders/capture/Anime_HD.glsl")),
    ("Anime_Soft", include_str!("../../shaders/capture/Anime_Soft.glsl")),
    ("Arcade_Clarity", include_str!("../../shaders/capture/Arcade_Clarity.glsl")),
    ("CRT_Phosphor", include_str!("../../shaders/capture/CRT_Phosphor.glsl")),
    ("CRT_TV", include_str!("../../shaders/capture/CRT_TV.glsl")),
    ("Cartoon", include_str!("../../shaders/capture/Cartoon.glsl")),
    ("Cell_Shade", include_str!("../../shaders/capture/Cell_Shade.glsl")),
    ("Demon_Vision", include_str!("../../shaders/capture/Demon_Vision.glsl")),
    ("Drunk_Vision", include_str!("../../shaders/capture/Drunk_Vision.glsl")),
    ("Eagle_2x", include_str!("../../shaders/capture/Eagle_2x.glsl")),
    ("GBA_Screen", include_str!("../../shaders/capture/GBA_Screen.glsl")),
    ("HDR_Pop", include_str!("../../shaders/capture/HDR_Pop.glsl")),
    ("HQ2x_Pixel", include_str!("../../shaders/capture/HQ2x_Pixel.glsl")),
    ("Lacrima_Glitch", include_str!("../../shaders/capture/Lacrima_Glitch.glsl")),
    ("LCD_Handheld", include_str!("../../shaders/capture/LCD_Handheld.glsl")),
    ("Neon_Cabinet", include_str!("../../shaders/capture/Neon_Cabinet.glsl")),
    ("NTSC_Composite", include_str!("../../shaders/capture/NTSC_Composite.glsl")),
    ("Night_Vision", include_str!("../../shaders/capture/Night_Vision.glsl")),
    ("PVM_Monitor", include_str!("../../shaders/capture/PVM_Monitor.glsl")),
    ("Phosphor", include_str!("../../shaders/capture/Phosphor.glsl")),
    ("Pixel_Retro", include_str!("../../shaders/capture/Pixel_Retro.glsl")),
    ("Psychedelic_Vision", include_str!("../../shaders/capture/Psychedelic_Vision.glsl")),
    ("Retro_Enhance", include_str!("../../shaders/capture/Retro_Enhance.glsl")),
    ("Retro_RPG_Menu", include_str!("../../shaders/capture/Retro_RPG_Menu.glsl")),
    ("Retro_Warm", include_str!("../../shaders/capture/Retro_Warm.glsl")),
    ("Scale2x_Enhance", include_str!("../../shaders/capture/Scale2x_Enhance.glsl")),
    ("Scanline_Arcade", include_str!("../../shaders/capture/Scanline_Arcade.glsl")),
    ("Sharp_Clean", include_str!("../../shaders/capture/Sharp_Clean.glsl")),
    ("VHS_Tape", include_str!("../../shaders/capture/VHS_Tape.glsl")),
];

/// std140 mirror of the `Globals` block — the shader uniforms plus the
/// host fields the vertex stage needs (`uDest` rect + `uViewport` in
/// physical px, `uUvOff`/`uUvScale` crop region, `uFade` layer alpha).
/// NB std140 alignment: vec4 members must sit at 16-byte offsets, so
/// `_pad0` pushes `tint` from 40 to 48 — without it every field from
/// tint on reads 8 bytes early in the shader (uFade=0 -> black).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    texture_size: [f32; 2],
    input_size: [f32; 2],
    output_size: [f32; 2],
    frame_count: f32,
    frame_direction: f32,
    brightness: f32,
    saturation: f32,
    _pad0: [f32; 2],
    tint: [f32; 4],
    dest: [f32; 4],
    viewport: [f32; 2],
    uv_off: [f32; 2],
    uv_scale: [f32; 2],
    fade: f32,
    _pad: f32,
}

/// GLSL wrapper the FRAGMENT body is injected into. Same naga
/// constraint as the shadertoys: no `sampler2D` global type, so
/// `Texture` in the body is rewritten to `sampler2D(_tex, _sam)`.
const GLSL_PRELUDE: &str = r#"
#version 450
layout(std140, set = 0, binding = 0) uniform Globals {
    vec2 TextureSize;
    vec2 InputSize;
    vec2 OutputSize;
    float FrameCount;
    float FrameDirection;
    float uBrightness;
    float uSaturation;
    vec4 uColorTint;
    vec4 uDest;
    vec2 uViewport;
    vec2 uUvOff;
    vec2 uUvScale;
    float uFade;
    float _pad;
};
layout(set = 0, binding = 1) uniform texture2D _cap_tex;
layout(set = 0, binding = 2) uniform sampler _cap_sam;

"#;

/// Host `main()`: runs the shader's body, applies brightness/
/// saturation/tint, then folds the layer fade into the output alpha
/// (the pipeline alpha-blends the result). On the CPU capture path
/// the adjust values are identity (the swizzle already applied
/// them), so this is a no-op there — on the zero-copy GPU path it is
/// where the live adjustments actually happen.
const GLSL_MAIN: &str = r#"
void main() {
    shaderMain();
    float _l = dot(fragColor.rgb, vec3(0.299, 0.587, 0.114));
    fragColor.rgb = clamp(mix(vec3(_l), fragColor.rgb, uSaturation) * uBrightness * uColorTint.rgb, 0.0, 1.0);
    fragColor.a *= uFade;
}
"#;

/// Passthrough fragment — the no-shader draw for the zero-copy GPU
/// path: sample + the same brightness/saturation/tint/fade math the
/// GLSL host main applies, so every pipeline honors the adjustments.
const PASSTHROUGH_FS: &str = r#"
struct Globals {
    texture_size: vec2f,
    input_size: vec2f,
    output_size: vec2f,
    frame_count: f32,
    frame_direction: f32,
    brightness: f32,
    saturation: f32,
    tint: vec4f,
    dest: vec4f,
    viewport: vec2f,
    uv_off: vec2f,
    uv_scale: vec2f,
    fade: f32,
    _pad: f32,
};
@group(0) @binding(0) var<uniform> G: Globals;
@group(0) @binding(1) var _cap_tex: texture_2d<f32>;
@group(0) @binding(2) var _cap_sam: sampler;
struct VOut {
    @builtin(position) pos: vec4f,
    @location(0) vTex: vec2f,
};
@fragment
fn main(in: VOut) -> @location(0) vec4f {
    let c = textureSample(_cap_tex, _cap_sam, in.vTex);
    let luma = dot(c.rgb, vec3f(0.299, 0.587, 0.114));
    let rgb = clamp(
        (vec3f(luma) + (c.rgb - vec3f(luma)) * G.saturation) * G.brightness * G.tint.rgb,
        vec3f(0.0), vec3f(1.0));
    return vec4f(rgb, c.a * G.fade);
}
"#;

/// Fullscreen-triangle vertex stage (WGSL — written directly since it
/// only needs `uDest`/`uViewport`/`uUvOff`/`uUvScale`). `vTex` maps
/// pixel position -> 0..1 over the dest rect -> the crop region, so
/// the fragment shader sees its usual 0..1 coordinate space and the
/// scissor rect does the actual clipping.
const VS_WGSL: &str = r#"
struct Globals {
    texture_size: vec2f,
    input_size: vec2f,
    output_size: vec2f,
    frame_count: f32,
    frame_direction: f32,
    brightness: f32,
    saturation: f32,
    tint: vec4f,
    dest: vec4f,
    viewport: vec2f,
    uv_off: vec2f,
    uv_scale: vec2f,
    fade: f32,
    _pad: f32,
};
@group(0) @binding(0) var<uniform> G: Globals;

struct VOut {
    @builtin(position) pos: vec4f,
    @location(0) vTex: vec2f,
};

// egui-wgpu sets the render-pass viewport to the callback's rect
// before paint(), so NDC -1..1 lands exactly on the capture rect —
// vTex is just the triangle's own coverage, remapped through the
// stretch/pillar crop region.
@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VOut {
    var positions = array<vec2f, 3>(
        vec2f(-1.0, -1.0),
        vec2f(3.0, -1.0),
        vec2f(-1.0, 3.0)
    );
    let ndc = positions[idx];
    var out: VOut;
    out.pos = vec4f(ndc, 0.0, 1.0);
    out.vTex = G.uv_off
        + vec2f(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5) * G.uv_scale;
    return out;
}
"#;

/// `uniform` declarations the wrapper provides — stripped from the
/// body (Lacrima's ProcessShaderCode-style forbidden list).
const STRIPPED_UNIFORMS: &[&str] = &[
    "Texture",
    "TextureSize",
    "InputSize",
    "OutputSize",
    "FrameCount",
    "FrameDirection",
    "uBrightness",
    "uSaturation",
    "uColorTint",
    "MVPMatrix",
];

/// GPU resources + pipeline cache for the capture shaders.
pub struct CaptureShader {
    device: wgpu::Device,
    queue: wgpu::Queue,
    format: wgpu::TextureFormat,
    bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    vs: wgpu::ShaderModule,
    uniforms: wgpu::Buffer,
    /// The capture frame on the GPU + its bind group (rebuilt whenever
    /// the texture is recreated on a frame-size change).
    frame: Option<FrameTex>,
    /// seq of the last uploaded frame — write_texture only on change.
    frame_seq: u64,
    /// name -> built pipeline (None = translation failed; cached so a
    /// bad shader doesn't recompile every frame).
    pipelines: BTreeMap<String, Option<Arc<wgpu::RenderPipeline>>>,
    /// Builds run on worker threads — a DX12/FXC compile can take ~1s
    /// and would otherwise freeze the frame the shader is picked on.
    building: HashSet<String>,
    build_tx: mpsc::Sender<(String, Option<Arc<wgpu::RenderPipeline>>)>,
    build_rx: mpsc::Receiver<(String, Option<Arc<wgpu::RenderPipeline>>)>,
    /// The no-shader GPU-path pipeline — lazily built, same BGL. Also
    /// the stand-in while a named shader compiles on a worker.
    passthrough: Option<Arc<wgpu::RenderPipeline>>,
    /// Extra .glsl files on disk — these can add to or override the
    /// embedded collection, like Lacrima's GetLocalShaders scan.
    disk: BTreeMap<String, PathBuf>,
    order: Vec<String>,
}

struct FrameTex {
    _tex: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    size: [u32; 2],
}

/// The imported zero-copy ring — `bgs[seq % 3]` samples the shared
/// slot; `fence` is the cross-API `ID3D12Fence` opened from the
/// published NT handle. The shared allocation stays alive as long as
/// these wgpu textures do, so a fade-out keeps working even after
/// the D3D11 capture side is torn down.
pub struct ImportedRing {
    pub _texs: Vec<wgpu::Texture>,
    pub bgs: Vec<wgpu::BindGroup>,
    pub fence: ID3D12Fence,
    pub size: [u32; 2],
    pub generation: u64,
}

/// Per-frame paint work — the uniform values are captured at callback
/// creation (egui points -> physical px happens in `prepare`, where
/// the real surface size is known).
pub struct CapCallback {
    pipeline: Arc<wgpu::RenderPipeline>,
    bind_group: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    values: Uniforms,
    /// Zero-copy path only: the D3D12 queue waits on this fence value
    /// before the submit that samples the imported ring — the D3D11
    /// side's CopyResource is then guaranteed complete.
    #[allow(dead_code)] // read only in the cfg(windows) prepare path
    wait: Option<(ID3D12Fence, u64)>,
}

impl CaptureShader {
    pub fn new(render_state: &egui_wgpu::RenderState) -> Self {
        let device = &render_state.device;
        let queue = &render_state.queue;

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("capshader_uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("capshader_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("capshader_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let vs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("capshader_vs"),
            source: wgpu::ShaderSource::Wgsl(VS_WGSL.into()),
        });

        // Shader list: embedded set plus any .glsl files dropped into
        // %LOCALAPPDATA%/AES_Rustified/Shaders/glsl (disk wins on a
        // name clash). Missing embedded shaders are seeded there so
        // the set is editable in place, same as the shadertoys.
        let mut order: Vec<String> = EMBEDDED.iter().map(|(n, _)| n.to_string()).collect();
        let mut disk = BTreeMap::new();
        if let Some(dir) = shaders_dir() {
            seed_shader_dir(&dir);
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.extension().and_then(|x| x.to_str()) != Some("glsl") {
                        continue;
                    }
                    let name = p.file_stem().unwrap().to_string_lossy().into_owned();
                    disk.insert(name.clone(), p);
                    if !order.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                        order.push(name);
                    }
                }
            }
        }
        order.sort_by_key(|a| a.to_lowercase());
        let (build_tx, build_rx) = mpsc::channel();

        Self {
            device: device.clone(),
            queue: queue.clone(),
            format: render_state.target_format,
            bgl,
            sampler,
            vs,
            uniforms,
            frame: None,
            frame_seq: u64::MAX,
            pipelines: BTreeMap::new(),
            building: HashSet::new(),
            build_tx,
            build_rx,
            passthrough: None,
            disk,
            order,
        }
    }

    /// All selectable shader names (sorted) for the panel ComboBox.
    pub fn names(&self) -> &[String] {
        &self.order
    }

    /// Open a `wgc::GpuShare`'s NT handles on this device — the
    /// textures become ordinary `wgpu::Texture`s (Bgra8) the
    /// pipelines sample in place. `Err(reason)` when the backend
    /// isn't DX12 or an import step fails — the caller then stays on
    /// the CPU mirror path and can show the reason in the overlay.
    #[cfg(not(windows))]
    pub fn import_share(
        &mut self,
        _share: &crate::emulation::wgc::GpuShare,
    ) -> Result<ImportedRing, String> {
        Err("zero-copy capture import requires the DX12 backend (Windows)".into())
    }

    #[cfg(windows)]
    pub fn import_share(
        &mut self,
        share: &crate::emulation::wgc::GpuShare,
    ) -> Result<ImportedRing, String> {
        unsafe {
            let hal = self
                .device
                .as_hal::<wgpu::hal::api::Dx12>()
                .ok_or_else(|| "backend is not DX12".to_string())?;
            let raw = hal.raw_device();
            let mut texs = Vec::with_capacity(3);
            let mut bgs = Vec::with_capacity(3);
            let size = wgpu::Extent3d {
                width: share.w,
                height: share.h,
                depth_or_array_layers: 1,
            };
            for &h in &share.tex {
                let mut res: Option<ID3D12Resource> = None;
                raw.OpenSharedHandle(HANDLE(h as *mut core::ffi::c_void), &mut res)
                    .map_err(|e| format!("OpenSharedHandle(tex): {e}"))?;
                let res = res.ok_or_else(|| "OpenSharedHandle(tex) empty".to_string())?;
                let hal_tex = wgpu::hal::dx12::Device::texture_from_raw(
                    res,
                    wgpu::TextureFormat::Bgra8Unorm,
                    wgpu::TextureDimension::D2,
                    size,
                    1,
                    1,
                );
                let tex = self
                    .device
                    .create_texture_from_hal::<wgpu::hal::api::Dx12>(
                        hal_tex,
                        &wgpu::TextureDescriptor {
                            label: Some("wgc-shared"),
                            size,
                            mip_level_count: 1,
                            sample_count: 1,
                            dimension: wgpu::TextureDimension::D2,
                            format: wgpu::TextureFormat::Bgra8Unorm,
                            usage: wgpu::TextureUsages::TEXTURE_BINDING,
                            view_formats: &[],
                        },
                        // The resource sits in D3D12 COMMON state —
                        // shader-resource reads need no transition.
                        wgpu::TextureUses::RESOURCE,
                    );
                let view = tex.create_view(&Default::default());
                bgs.push(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("wgc-shared-bg"),
                    layout: &self.bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: self.uniforms.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(&self.sampler),
                        },
                    ],
                }));
                texs.push(tex);
            }
            let mut fence: Option<ID3D12Fence> = None;
            raw.OpenSharedHandle(HANDLE(share.fence as *mut core::ffi::c_void), &mut fence)
                .map_err(|e| format!("OpenSharedHandle(fence): {e}"))?;
            let fence = fence.ok_or_else(|| "OpenSharedHandle(fence) empty".to_string())?;
            Ok(ImportedRing {
                _texs: texs,
                bgs,
                fence,
                size: [share.w, share.h],
                generation: share.generation,
            })
        }
    }

    /// Upload the latest capture frame — dedup'd by seq so the caller
    /// can invoke it unconditionally after the frame-slot swap.
    pub fn upload_frame(&mut self, seq: u64, pixels: &[u8], w: u32, h: u32) {
        if w == 0 || h == 0 || pixels.len() != (4 * w * h) as usize || seq == self.frame_seq {
            return;
        }
        self.frame_seq = seq;
        let size = [w, h];
        if self.frame.as_ref().map(|f| f.size) != Some(size) {
            let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("capshader_frame"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = tex.create_view(&Default::default());
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("capshader_bg"),
                layout: &self.bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.uniforms.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.frame = Some(FrameTex {
                _tex: tex,
                bind_group,
                size,
            });
        }
        let frame = self.frame.as_ref().unwrap();
        // Rows are top-down — v=0 in `vTex` maps to the top row, which
        // is also where `painter.image` puts uv.y=0, so no flip.
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &frame._tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * w),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
    }

    /// A paint callback drawing `dest` through the named shader, or
    /// `None` (caller falls back to `painter.image`) when the shader
    /// name is empty/unknown or its pipeline failed to build.
    pub fn callback(
        &mut self,
        name: &str,
        dest: Rect,
        uv: Rect,
        fade: f32,
        src_size: [u32; 2],
        frame_count: f32,
        pixels_per_point: f32,
    ) -> Option<CapCallback> {
        if name.is_empty() {
            return None;
        }
        let pipeline = self.pipeline_for(name)?;
        let bind_group = self.frame.as_ref()?.bind_group.clone();
        // Identity adjust — the CPU mirror's swizzle already applied
        // brightness/saturation/tint to the uploaded pixels.
        Some(self.mk_callback(
            pipeline,
            bind_group,
            dest,
            uv,
            fade,
            src_size,
            frame_count,
            pixels_per_point,
            None,
            (1.0, 1.0, [1.0; 4]),
        ))
    }

    /// The zero-copy GPU-path variant: samples an `ImportedRing` slot
    /// (`bind_group`), queues a DX12 fence wait for the D3D11 copy
    /// (`wait`), and passes the live render adjustments through since
    /// no CPU swizzle ran (`adjust` = brightness, saturation, tint
    /// rgba). An empty `name` selects the passthrough pipeline.
    pub fn callback_gpu(
        &mut self,
        name: &str,
        dest: Rect,
        uv: Rect,
        fade: f32,
        src_size: [u32; 2],
        frame_count: f32,
        pixels_per_point: f32,
        bind_group: wgpu::BindGroup,
        wait: Option<(ID3D12Fence, u64)>,
        adjust: (f32, f32, [f32; 4]),
    ) -> Option<CapCallback> {
        let pipeline = self.pipeline_for(name)?;
        Some(self.mk_callback(
            pipeline,
            bind_group,
            dest,
            uv,
            fade,
            src_size,
            frame_count,
            pixels_per_point,
            wait,
            adjust,
        ))
    }

    fn mk_callback(
        &self,
        pipeline: Arc<wgpu::RenderPipeline>,
        bind_group: wgpu::BindGroup,
        dest: Rect,
        uv: Rect,
        fade: f32,
        src_size: [u32; 2],
        frame_count: f32,
        pixels_per_point: f32,
        wait: Option<(ID3D12Fence, u64)>,
        adjust: (f32, f32, [f32; 4]),
    ) -> CapCallback {
        let dest_px = [
            (dest.min.x * pixels_per_point).max(0.0) as u32,
            (dest.min.y * pixels_per_point).max(0.0) as u32,
            (dest.width() * pixels_per_point).max(1.0) as u32,
            (dest.height() * pixels_per_point).max(1.0) as u32,
        ];
        CapCallback {
            pipeline,
            bind_group,
            uniforms: self.uniforms.clone(),
            wait,
            values: Uniforms {
                texture_size: [src_size[0] as f32, src_size[1] as f32],
                input_size: [src_size[0] as f32, src_size[1] as f32],
                output_size: [dest_px[2] as f32, dest_px[3] as f32],
                frame_count,
                frame_direction: 1.0,
                brightness: adjust.0,
                saturation: adjust.1,
                _pad0: [0.0; 2],
                tint: adjust.2,
                dest: [
                    dest_px[0] as f32,
                    dest_px[1] as f32,
                    dest_px[2] as f32,
                    dest_px[3] as f32,
                ],
                // Filled in prepare() with the real surface size.
                viewport: [0.0; 2],
                uv_off: [uv.min.x, uv.min.y],
                uv_scale: [uv.width(), uv.height()],
                fade,
                _pad: 0.0,
            },
        }
    }

    /// Lazily translate+build the pipeline for `name` on a worker
    /// thread (cached — a failed shader is never retried per-frame).
    /// While it compiles, and if it fails, the passthrough pipeline
    /// stands in so the frame keeps drawing instead of going black.
    /// An empty name selects the passthrough directly.
    fn pipeline_for(&mut self, name: &str) -> Option<Arc<wgpu::RenderPipeline>> {
        while let Ok((n, p)) = self.build_rx.try_recv() {
            self.building.remove(&n);
            self.pipelines.insert(n, p);
        }
        if !name.is_empty() {
            if !self.pipelines.contains_key(name) && !self.building.contains(name) {
                self.spawn_build(name);
            }
            if let Some(Some(p)) = self.pipelines.get(name) {
                return Some(Arc::clone(p));
            }
        }
        if self.passthrough.is_none() {
            self.passthrough =
                Self::build_wgsl(&self.device, &self.bgl, &self.vs, self.format, PASSTHROUGH_FS);
        }
        self.passthrough.clone()
    }

    /// Kick a worker-thread build for `name` — `create_render_pipeline`
    /// is thread-safe (all wgpu handles are Send+Sync) and the caller
    /// already filtered in-flight/cached names.
    fn spawn_build(&mut self, name: &str) {
        let Some(src) = self.source(name) else {
            self.pipelines.insert(name.to_owned(), None);
            return;
        };
        self.building.insert(name.to_owned());
        let (device, bgl, vs, format, tx) = (
            self.device.clone(),
            self.bgl.clone(),
            self.vs.clone(),
            self.format,
            self.build_tx.clone(),
        );
        let name = name.to_owned();
        std::thread::spawn(move || {
            crate::boot_mark(&format!("capshader '{name}' build start"));
            let p = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let wgsl = translate(&src)?;
                Self::build_wgsl(&device, &bgl, &vs, format, &wgsl)
            }))
            .ok()
            .flatten();
            crate::boot_mark(&format!("capshader '{name}' build end"));
            let _ = tx.send((name, p));
        });
    }

    fn source(&self, name: &str) -> Option<String> {
        if let Some(p) = self.disk.get(name) {
            if let Ok(s) = std::fs::read_to_string(p) {
                return Some(s);
            }
        }
        EMBEDDED
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, s)| s.to_string())
    }

    /// Pipeline from WGSL — shared by the translated shaders and the
    /// built-in passthrough (same vertex stage + bind group layout).
    /// Callable from a worker thread.
    fn build_wgsl(
        device: &wgpu::Device,
        bgl: &wgpu::BindGroupLayout,
        vs: &wgpu::ShaderModule,
        format: wgpu::TextureFormat,
        wgsl: &str,
    ) -> Option<Arc<wgpu::RenderPipeline>> {
        let fs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("capshader_fs"),
            source: wgpu::ShaderSource::Wgsl(wgsl.to_string().into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("capshader_pl"),
            bind_group_layouts: &[Some(bgl)],
            immediate_size: 0,
        });
        Some(Arc::new(device.create_render_pipeline(
            &wgpu::RenderPipelineDescriptor {
                label: Some("capshader_pipeline"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: vs,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &fs,
                    entry_point: Some("main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            },
        )))
    }
}

impl egui_wgpu::CallbackTrait for CapCallback {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen_descriptor: &egui_wgpu::ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        _callback_resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        // Zero-copy path: stage the cross-API wait so the submit that
        // samples the imported ring slot runs after the D3D11
        // CopyResource signaled this fence value.
        #[cfg(windows)]
        if let Some((fence, v)) = &self.wait {
            unsafe {
                if let Some(q) = queue.as_hal::<wgpu::hal::api::Dx12>() {
                    q.add_wait_fence(fence.clone(), *v);
                }
            }
        }
        let mut values = self.values;
        values.viewport = [
            screen_descriptor.size_in_pixels[0] as f32,
            screen_descriptor.size_in_pixels[1] as f32,
        ];
        queue.write_buffer(&self.uniforms, 0, bytemuck::bytes_of(&values));
        Vec::new()
    }

    fn paint(
        &self,
        info: PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        _callback_resources: &egui_wgpu::CallbackResources,
    ) {
        // Scissor to the callback rect ∩ egui clip — the fullscreen
        // triangle is only ever shaded inside the capture rect.
        // `viewport_in_pixels` is the same rect egui set as the
        // render-pass viewport.
        let vp = info.viewport_in_pixels();
        let clip = info.clip_rect_in_pixels();
        let [sw, sh] = info.screen_size_px;
        let x0 = vp.left_px.max(clip.left_px).clamp(0, sw as i32);
        let y0 = vp.top_px.max(clip.top_px).clamp(0, sh as i32);
        let x1 = vp
            .left_px
            .saturating_add(vp.width_px)
            .min(clip.left_px.saturating_add(clip.width_px))
            .clamp(0, sw as i32);
        let y1 = vp
            .top_px
            .saturating_add(vp.height_px)
            .min(clip.top_px.saturating_add(clip.height_px))
            .clamp(0, sh as i32);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let (x0, y0, x1, y1) = (x0 as u32, y0 as u32, x1 as u32, y1 as u32);
        render_pass.set_scissor_rect(x0, y0, x1 - x0, y1 - y0);
        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }
}

/// FRAGMENT body -> wrapped GLSL -> naga -> WGSL. Pure function —
/// factored out of `build` so the whole bundled collection can be
/// compile-tested without a GPU.
fn translate(src: &str) -> Option<String> {
    let body = process_shader(src.trim_start_matches('\u{feff}'));
    let body = body.replace("texture2D(", "texture(");
    let body = crate::shadertoy::replace_ident(&body, "Texture", "sampler2D(_cap_tex, _cap_sam)");
    // `smooth` is a GLSL reserved word naga won't parse as a local
    // variable name (Anime_Soft uses it).
    let body = crate::shadertoy::replace_ident(&body, "smooth", "smoothed");
    let body = body.replace("void main(", "void shaderMain(");
    let body = body.replace("void main (", "void shaderMain(");
    let glsl = format!("{}{}\n{}", GLSL_PRELUDE, body, GLSL_MAIN);

    let mut fe = naga::front::glsl::Frontend::default();
    let module = fe
        .parse(&naga::front::glsl::Options::from(naga::ShaderStage::Fragment), &glsl)
        .ok()?;
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .ok()?;
    naga::back::wgsl::write_string(&module, &info, naga::back::wgsl::WriterFlags::EXPLICIT_TYPES).ok()
}

/// `%LOCALAPPDATA%\AES_Rustified\Shaders\glsl` — the disk-scanned
/// shader dir, matching Lacrima's `ApplicationPaths.ShadersDirectory`
/// layout (`Shaders/glsl` is the runtime-usable set on Windows too —
/// the HLSL twins only matter for the D3D compositor we don't have).
fn shaders_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("AES_Rustified").join("Shaders").join("glsl"))
}

/// Seed the disk dir with the embedded collection where missing
/// (never overwriting user edits) so the set is editable in place.
fn seed_shader_dir(dir: &std::path::Path) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    for (name, src) in EMBEDDED {
        let dst = dir.join(format!("{name}.glsl"));
        if !dst.exists() {
            let _ = std::fs::write(&dst, src);
        }
    }
}

/// Extract the `#ifdef FRAGMENT` body and prepare it for the prelude:
/// drop `uniform` decls the wrapper provides, collect + expand the
/// object-like `#define`s (all the collection uses), drop the
/// preprocessor lines themselves. The shader's `void main` is renamed
/// afterwards in `build`.
fn process_shader(content: &str) -> String {
    // FRAGMENT block only — the VERTEX half is unused.
    let text = content.replace("\r\n", "\n");
    let frag = text
        .split("#ifdef FRAGMENT")
        .nth(1)
        .and_then(|s| s.split("#endif").next())
        .unwrap_or(&text);

    // First pass: collect #define NAME VALUE pairs (object-like only).
    let mut defines: Vec<(String, String)> = Vec::new();
    for raw in frag.split('\n') {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("#define") {
            let mut it = rest.trim().splitn(2, char::is_whitespace);
            if let (Some(name), Some(val)) = (it.next(), it.next()) {
                if !name.contains('(') {
                    defines.push((name.to_string(), val.trim().to_string()));
                }
            }
        }
    }

    let mut body = String::new();
    'lines: for raw in frag.split('\n') {
        let line = raw.trim();
        if line.starts_with('#')
            || line.is_empty()
            || line.starts_with("precision")
        {
            continue;
        }
        if line.starts_with("uniform ") {
            for name in STRIPPED_UNIFORMS {
                if line.contains(name) {
                    continue 'lines;
                }
            }
        }
        body.push_str(raw);
        body.push('\n');
    }
    // Expand the collected defines — `replace_ident` keeps word
    // boundaries so `brightness` can't hit `uBrightness`.
    for (name, val) in defines {
        body = crate::shadertoy::replace_ident(&body, &name, &val);
    }
    body
}

#[cfg(test)]
mod tests {
    use eframe::egui_wgpu::wgpu;

    /// Every bundled shader must survive the GLSL->WGSL translation —
    /// a failure here would silently fall back to the plain image at
    /// runtime, so catch it at build time instead.
    #[test]
    fn all_embedded_shaders_translate() {
        let mut failed = Vec::new();
        for (name, src) in super::EMBEDDED {
            if super::translate(src).is_none() {
                failed.push(*name);
            }
        }
        assert!(failed.is_empty(), "shaders failing naga: {failed:?}");
    }

    /// The passthrough fragment (the no-shader GPU path) must parse
    /// and validate — it never goes through `translate` so a broken
    /// WGSL here would draw nothing with no fallback.
    #[test]
    fn passthrough_wgsl_validates() {
        use eframe::egui_wgpu::wgpu::naga;
        let module = naga::front::wgsl::parse_str(super::PASSTHROUGH_FS)
            .expect("passthrough WGSL parse");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("passthrough WGSL validate");
    }

    /// Minimal spin block_on — wgpu's request_adapter/request_device
    /// resolve on re-poll; a noop waker + busy poll is enough here.
    fn block_on<F: std::future::Future>(mut fut: F) -> F::Output {
        use std::task::*;
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        fn noop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        let mut fut = unsafe { std::pin::Pin::new_unchecked(&mut fut) };
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// End-to-end on the real GPU: run every bundled shader over a
    /// known-bright source frame and read the pixels back — a shader
    /// whose pipeline fails, draws nothing, or outputs black is the
    /// exact "capture goes black" failure mode this guards.
    #[test]
    fn pipelines_draw_on_gpu() {
        let inst =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = block_on(inst.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .expect("no wgpu adapter");
        let (device, queue) =
            block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .expect("no wgpu device");

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let vs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(super::VS_WGSL.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let format = wgpu::TextureFormat::Bgra8UnormSrgb;

        // 8x8 mid-tone source (a pure black or saturated frame could
        // legitimately zero out under edge/glow filters).
        let src_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: 8,
                height: 8,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut src_px = [0u8; 8 * 8 * 4];
        for (i, px) in src_px.chunks_exact_mut(4).enumerate() {
            // Varied bright-ish colors so masks/filters keep energy.
            px[0] = (90 + (i * 37) % 150) as u8;
            px[1] = (70 + (i * 53) % 160) as u8;
            px[2] = (60 + (i * 29) % 170) as u8;
            px[3] = 255;
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &src_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &src_px,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(32),
                rows_per_image: Some(8),
            },
            src_tex.size(),
        );
        let src_view = src_tex.create_view(&Default::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: std::mem::size_of::<super::Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(
            &uniforms,
            0,
            bytemuck::bytes_of(&super::Uniforms {
                texture_size: [8.0, 8.0],
                input_size: [8.0, 8.0],
                output_size: [8.0, 8.0],
                frame_count: 60.0,
                frame_direction: 1.0,
                brightness: 1.0,
                saturation: 1.0,
                _pad0: [0.0; 2],
                tint: [1.0; 4],
                dest: [0.0, 0.0, 8.0, 8.0],
                viewport: [8.0, 8.0],
                uv_off: [0.0, 0.0],
                uv_scale: [1.0, 1.0],
                fade: 1.0,
                _pad: 0.0,
            }),
        );
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&src_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let mut failed = Vec::new();
        for (name, src) in super::EMBEDDED {
            let Some(wgsl) = super::translate(src) else {
                failed.push(format!("{name}: translate"));
                continue;
            };
            let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
            let fs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: None,
                source: wgpu::ShaderSource::Wgsl(wgsl.into()),
            });
            let pipe = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: None,
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &vs,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &fs,
                    entry_point: Some("main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
            // A draw into a throwaway target — shader-compile errors
            // surface here, not at create_render_pipeline.
            let target = device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d {
                    width: 8,
                    height: 8,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = target.create_view(&Default::default());
            let mut enc = device.create_command_encoder(&Default::default());
            {
                let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations::default(),
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&pipe);
                pass.set_bind_group(0, &bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
            // Read the target back — a live pipeline still fails the
            // test if every output channel comes out zero.
            let readback = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: (256 * 8) as u64,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            enc.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &target,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(256),
                        rows_per_image: Some(8),
                    },
                },
                target.size(),
            );
            queue.submit([enc.finish()]);
            let _ = device.poll(wgpu::PollType::wait_indefinitely());
            if let Some(err) = block_on(scope.pop()) {
                failed.push(format!("{name}: {err}"));
                continue;
            }
            let (tx, rx) = std::sync::mpsc::channel();
            readback.map_async(wgpu::MapMode::Read, .., move |r| {
                let _ = tx.send(r);
            });
            let _ = device.poll(wgpu::PollType::wait_indefinitely());
            let dark = rx
                .recv()
                .ok()
                .and_then(|r| r.ok())
                .map(|_| {
                    let view = match readback.get_mapped_range(..) {
                        Ok(v) => v,
                        Err(_) => return true,
                    };
                    let data: &[u8] = &view;
                    // BGRA8 target, 256-byte rows — sample the first
                    // 32 bytes of each of the 8 rows for energy.
                    let max = data
                        .chunks_exact(256)
                        .take(8)
                        .flat_map(|row| row[..32].iter())
                        .copied()
                        .max()
                        .unwrap_or(0);
                    max <= 10
                })
                .unwrap_or(true);
            readback.unmap();
            if dark {
                failed.push(format!("{name}: all-black output"));
            }
        }
        assert!(failed.is_empty(), "GPU pipeline failures: {failed:?}");
    }
}
