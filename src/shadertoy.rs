//! Shadertoy background layer — the full `Shaders/Shadertoys` .frag
//! collection from AES_Lacrima, selectable and persisted like the UI
//! tab's "Current" ComboBox (SettingsViewModel.SelectedShadertoy).
//!
//! The .frag files are GLSL with Lacrima's loose uniforms
//! (`iResolution`, `iTime`, `iChannel0` spectrum, `iChannel1` cover,
//! `u_primary/secondary/tertiary`, `u_grad0..4`, `u_disc`, `u_fade`).
//! They're wrapped in a Vulkan-flavored GLSL prelude (uniform block +
//! separated `texture2D`/`sampler` globals), translated to WGSL with
//! naga's GLSL frontend, and drawn fullscreen by egui-wgpu.
//!
//! `iChannel0` is a 512-wide spectrum texture fed from the spectrum
//! analyzer exactly like `GlShaderToyControl.UpdateAudioTexture` —
//! snapshot stretched to 512 bins, normalized, per-bin smoothing.
//! `iChannel1` is the playing album's top cover (`set_cover`).

use eframe::egui_wgpu::{self, wgpu};
use eframe::epaint::{Color32, ColorImage, PaintCallbackInfo};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::Instant;

/// Lacrima's audio texture: 512 float bins, GL_RED.
const SPEC_BINS: usize = 512;

/// The offscreen the .frag pipelines render into — raw Rgba8Unorm like
/// the GL side's FBO. On an sRGB surface the blit pass applies the
/// decode so the final encode lands on the same bytes GL produced.
const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Accent palette used until a cover provides dominant colors.
const PRIMARY: [f32; 3] = [0.118, 0.812, 1.000];
const SECONDARY: [f32; 3] = [0.431, 0.243, 1.000];
const TERTIARY: [f32; 3] = [0.060, 0.090, 0.220];

/// The shipped collection — `(name, source)`; the name is the .frag's
/// file stem, matching `ShaderItem.Name` in the settings ComboBox.
const EMBEDDED: &[(&str, &str)] = &[
    ("AnimatedBackground", include_str!("../shaders/shadertoys/AnimatedBackground.frag")),
    ("AnimatedWaves", include_str!("../shaders/shadertoys/AnimatedWaves.frag")),
    ("Atom", include_str!("../shaders/shadertoys/Atom.frag")),
    ("CarTuner", include_str!("../shaders/shadertoys/CarTuner.frag")),
    ("Cartoon", include_str!("../shaders/shadertoys/Cartoon.frag")),
    ("Cassette", include_str!("../shaders/shadertoys/Cassette.frag")),
    ("Cell_Shade", include_str!("../shaders/shadertoys/Cell_Shade.frag")),
    ("CircularSpectrumVisualizer", include_str!("../shaders/shadertoys/CircularSpectrumVisualizer.frag")),
    ("Cyberpunk", include_str!("../shaders/shadertoys/Cyberpunk.frag")),
    ("Demon_Vision", include_str!("../shaders/shadertoys/Demon_Vision.frag")),
    ("ElectricBlur", include_str!("../shaders/shadertoys/ElectricBlur.frag")),
    ("ElectricGalaxy", include_str!("../shaders/shadertoys/ElectricGalaxy.frag")),
    ("GalaxySpectrum", include_str!("../shaders/shadertoys/GalaxySpectrum.frag")),
    ("GreenWarpSpectrum", include_str!("../shaders/shadertoys/GreenWarpSpectrum.frag")),
    ("Lighting", include_str!("../shaders/shadertoys/Lighting.frag")),
    ("LiveOrb", include_str!("../shaders/shadertoys/LiveOrb.frag")),
    ("MilkdropWarpSpectrum", include_str!("../shaders/shadertoys/MilkdropWarpSpectrum.frag")),
    ("Pixelation", include_str!("../shaders/shadertoys/Pixelation.frag")),
    ("PlasticSpectrum", include_str!("../shaders/shadertoys/PlasticSpectrum.frag")),
    ("Retro_RPG_Menu", include_str!("../shaders/shadertoys/Retro_RPG_Menu.frag")),
    ("SpectrumBlackHole", include_str!("../shaders/shadertoys/SpectrumBlackHole.frag")),
    ("SpectrumWave", include_str!("../shaders/shadertoys/SpectrumWave.frag")),
    ("Waves", include_str!("../shaders/shadertoys/Waves.frag")),
];

/// std140 mirror of the GLSL `Globals` block in `GLSL_PRELUDE` —
/// every member lands on its natural alignment (192 bytes).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    resolution: [f32; 3],
    time: f32,
    frame: i32,
    fade: f32,
    ch1_size: [f32; 2],
    mouse: [f32; 4],
    primary: [f32; 3],
    corner: f32,
    secondary: [f32; 3],
    _p1: f32,
    tertiary: [f32; 3],
    _p2: f32,
    grad0: [f32; 3],
    _g0: f32,
    grad1: [f32; 3],
    _g1: f32,
    grad2: [f32; 3],
    _g2: f32,
    grad3: [f32; 3],
    _g3: f32,
    grad4: [f32; 3],
    _g4: f32,
    disc: [f32; 4],
}

/// GLSL wrapper the .frag bodies are injected into. naga's glsl-in has
/// no combined `sampler2D` global type — channels are separate
/// `texture2D` + `sampler` globals and `iChannelN` in the body is
/// rewritten to the `sampler2D(tex, sam)` constructor.
const GLSL_PRELUDE: &str = r#"
#version 450
layout(std140, set = 0, binding = 0) uniform Globals {
    vec3 iResolution;
    float iTime;
    int iFrame;
    float u_fade;
    vec2 iChannel1Size;
    vec4 iMouse;
    vec3 u_primary;
    float u_corner;
    vec3 u_secondary;
    float _p1;
    vec3 u_tertiary;
    float _p2;
    vec3 u_grad0;
    float _g0;
    vec3 u_grad1;
    float _g1;
    vec3 u_grad2;
    float _g2;
    vec3 u_grad3;
    float _g3;
    vec3 u_grad4;
    float _g4;
    vec4 u_disc;
};
layout(set = 0, binding = 1) uniform texture2D _ch0_tex;
layout(set = 0, binding = 2) uniform sampler _ch0_sam;
layout(set = 0, binding = 3) uniform texture2D _ch1_tex;
layout(set = 0, binding = 4) uniform sampler _ch1_sam;
layout(location = 0) out vec4 outFragColor;

"#;

/// Host `main()`: feeds `mainImage` bottom-left-origin frag coords
/// (wgpu's position builtin is top-left, GL's is bottom-left) and
/// applies the rounded-window alpha clip. `u_fade` is left to the
/// shader — the collection multiplies it internally.
const GLSL_MAIN: &str = r#"
void main() {
    vec4 c;
    vec2 fc = vec2(gl_FragCoord.x, iResolution.y - gl_FragCoord.y);
    mainImage(c, fc);
    vec2 halfRes = iResolution.xy * 0.5;
    vec2 q = abs(gl_FragCoord.xy - halfRes) - (halfRes - vec2(u_corner));
    float d = length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - u_corner;
    float a = 1.0 - smoothstep(-1.0, 1.0, d);
    outFragColor = vec4(c.rgb, c.a * a);
}
"#;

/// Uniform names the wrapper provides — their declarations inside a
/// .frag are stripped, same as ProcessShaderCode's forbidden list.
const FORBIDDEN: &[&str] = &[
    "iResolution", "iTime", "iFrame", "iMouse", "u_fade", "iChannel0", "iChannel1",
    "iChannel1Size", "u_primary", "u_secondary", "u_tertiary", "u_grad0", "u_grad1",
    "u_grad2", "u_grad3", "u_grad4", "u_disc", "u_corner",
];

/// Fullscreen triangle for the shared vertex stage.
const VS_WGSL: &str = r#"
@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> @builtin(position) vec4f {
    var positions = array<vec2f, 3>(
        vec2f(-1.0, -1.0),
        vec2f(3.0, -1.0),
        vec2f(-1.0, 3.0)
    );
    return vec4f(positions[idx], 0.0, 1.0);
}
"#;

/// Fullscreen blit: samples the offscreen shader render 1:1.
const BLIT_WGSL: &str = r#"
struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VOut {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0)
    );
    var out: VOut;
    let p = positions[idx];
    out.pos = vec4<f32>(p, 0.0, 1.0);
    out.uv = vec2<f32>(p.x * 0.5 + 0.5, 0.5 - p.y * 0.5);
    return out;
}

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_sam: sampler;

@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    return textureSample(src_tex, src_sam, in.uv);
}
"#;

/// sRGB-surface variant: the offscreen holds the shader's raw
/// display-space values; writing the sRGB-DECODED value lets the
/// hardware's store-encode land back on those same bytes, matching
/// the non-sRGB GL path (without this the fog lifts ~1/2.2 and
/// washes out).
const BLIT_WGSL_SRGB: &str = r#"
struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VOut {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0)
    );
    var out: VOut;
    let p = positions[idx];
    out.pos = vec4<f32>(p, 0.0, 1.0);
    out.uv = vec2<f32>(p.x * 0.5 + 0.5, 0.5 - p.y * 0.5);
    return out;
}

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_sam: sampler;

fn srgb_decode(c: f32) -> f32 {
    if c <= 0.04045 {
        return c / 12.92;
    }
    return pow((c + 0.055) / 1.055, 2.4);
}

@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    let c = textureSample(src_tex, src_sam, in.uv);
    return vec4<f32>(
        srgb_decode(c.r), srgb_decode(c.g), srgb_decode(c.b), c.a);
}
"#;

struct Shared {
    bind_group: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    spec_tex: wgpu::Texture,
}

/// Owns the GPU resources and produces per-frame paint callbacks.
/// Pipelines are translated lazily per shader name and cached.
pub struct Shadertoy {
    device: wgpu::Device,
    queue: wgpu::Queue,
    bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    cover_tex: wgpu::Texture,
    cover_view: wgpu::TextureView,
    cover_size: [u32; 2],
    vs: wgpu::ShaderModule,
    shared: Shared,
    /// name -> built pipeline (None = translation failed, cached so we
    /// don't retry every frame).
    pipelines: BTreeMap<String, Option<Arc<wgpu::RenderPipeline>>>,
    /// Builds run on worker threads — under DX12/FXC a heavy shader
    /// takes ~1s to compile, which froze the first frame (startup)
    /// and every shader switch when done on the UI thread.
    building: HashSet<String>,
    build_tx: mpsc::Sender<(String, Option<Arc<wgpu::RenderPipeline>>)>,
    build_rx: mpsc::Receiver<(String, Option<Arc<wgpu::RenderPipeline>>)>,
    /// Extra .frag files on disk (`<exe>/Shaders/Shadertoys`) — these
    /// can add to or override the embedded collection, like Lacrima's
    /// GetLocalShaders scan.
    disk: BTreeMap<String, PathBuf>,
    order: Vec<String>,
    current: String,
    smoothed: Vec<f32>,
    palette: [[f32; 3]; 8],
    mouse: [f32; 4],
    /// `u_disc`: the player widget's vinyl disc as a shader occluder —
    /// center + radius in physical px, y measured from the bottom.
    disc: [f32; 4],
    /// Fullscreen-triangle blit that copies the offscreen shader render
    /// to the surface (ALPHA_BLENDING compositing happens here — the
    /// shader pipelines themselves write raw output offscreen).
    blit_pipe: Arc<wgpu::RenderPipeline>,
    blit_bgl: Arc<wgpu::BindGroupLayout>,
    /// The pipeline the last callback drew + when it appeared — a new
    /// selection fades in (0.4s smoothstep) instead of popping.
    shown: Option<(Arc<wgpu::RenderPipeline>, Instant)>,
    /// Background warm-up cursor — one idle-slot build per call walks
    /// the whole set so a later pick is instant (wgpu has no on-disk
    /// pipeline cache on DX12; this is the next best thing).
    warm_idx: usize,
    start: Instant,
    frame: i32,
}

pub struct ShadertoyCallback {
    pipeline: Option<Arc<wgpu::RenderPipeline>>,
    shared: Arc<Shared>,
    uniforms: Uniforms,
    spectrum: Vec<u16>,
    /// Whether this frame re-renders the shader into the offscreen
    /// texture; otherwise `paint` just re-blits the last render.
    render_now: bool,
    blit_pipe: Arc<wgpu::RenderPipeline>,
    blit_bgl: Arc<wgpu::BindGroupLayout>,
    sampler: wgpu::Sampler,
}

/// f32 -> IEEE 754 half bits (round half-up); spectrum values are
/// 0..1, so subnormals flush to zero and overflow saturates to inf.
fn f16_bits(v: f32) -> u16 {
    let x = v.to_bits();
    let sign = ((x >> 16) & 0x8000) as u16;
    let e = ((x >> 23) & 0xff) as i32 - 127 + 15;
    if e <= 0 {
        return sign;
    }
    if e >= 31 {
        return sign | 0x7c00;
    }
    let mut h = ((e as u16) << 10) | ((x >> 13) as u16 & 0x3ff);
    if (x >> 12) & 1 == 1 {
        h += 1;
    }
    sign | h
}

/// Per-renderer offscreen target, stored in `CallbackResources` so it
/// persists across frames (recreated on surface resize).
struct OffscreenRes {
    _tex: wgpu::Texture,
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
    size: [u32; 2],
    /// Set once the shader has rendered into it — before that the blit
    /// would sample uninitialized pixels.
    has_frame: bool,
}

impl Shadertoy {
    pub fn new(render_state: &egui_wgpu::RenderState) -> Self {
        let device = &render_state.device;
        let queue = &render_state.queue;

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shadertoy_uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // iChannel0: 512x1 spectrum bins. R16Float is filterable in
        // core WebGPU (R32Float/R16Unorm need optional features) and
        // gives ~2048 levels over 0..1 — R8's 256 visibly banded in
        // shaders that amplify small bin deltas (ElectricBlur's shear).
        let spec_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("shadertoy_channel0"),
            size: wgpu::Extent3d {
                width: SPEC_BINS as u32,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let spec_view = spec_tex.create_view(&Default::default());

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("shadertoy_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // iChannel1 starts as the accent gradient stand-in; the real
        // album cover arrives through set_cover.
        let (cover_tex, cover_view) = cover_texture(device, queue, 128, 128, &cover_pixels());
        let cover_size = [128, 128];

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("shadertoy_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
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
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let bind_group = Self::bind_group(
            device,
            &bgl,
            &uniforms,
            &spec_view,
            &sampler,
            &cover_view,
        );

        let vs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shadertoy_vs"),
            source: wgpu::ShaderSource::Wgsl(VS_WGSL.into()),
        });

        // Offscreen->surface blit: the shader pipelines render into an
        // offscreen texture at a throttled rate; this pipeline copies
        // it to the surface every frame (alpha-composited here).
        let blit_bgl = Arc::new(device.create_bind_group_layout(
            &wgpu::BindGroupLayoutDescriptor {
                label: Some("shadertoy_blit_bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float {
                                filterable: true,
                            },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            },
        ));
        let blit_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shadertoy_blit"),
            source: wgpu::ShaderSource::Wgsl(
                if render_state.target_format.is_srgb() {
                    BLIT_WGSL_SRGB
                } else {
                    BLIT_WGSL
                }
                .into(),
            ),
        });
        let blit_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("shadertoy_blit_pl"),
            bind_group_layouts: &[Some(&blit_bgl)],
            immediate_size: 0,
        });
        let blit_pipe = Arc::new(device.create_render_pipeline(
            &wgpu::RenderPipelineDescriptor {
                label: Some("shadertoy_blit_pipe"),
                layout: Some(&blit_layout),
                vertex: wgpu::VertexState {
                    module: &blit_module,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &blit_module,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: render_state.target_format,
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
        ));

        // Shader list: embedded set plus any .frag files dropped into
        // %LOCALAPPDATA%/AES_Rustified/Shaders/Shadertoys (disk wins on
        // a name clash). Missing embedded shaders are seeded there so
        // the set is editable in place.
        let mut order: Vec<String> = EMBEDDED.iter().map(|(n, _)| n.to_string()).collect();
        let mut disk = BTreeMap::new();
        if let Some(dir) = shaders_dir() {
            seed_shader_dir(&dir);
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.extension().and_then(|x| x.to_str()) != Some("frag") {
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
            bgl,
            sampler,
            cover_tex,
            cover_view,
            cover_size,
            vs,
            shared: Shared {
                bind_group,
                uniforms,
                spec_tex,
            },
            pipelines: BTreeMap::new(),
            building: HashSet::new(),
            build_tx,
            build_rx,
            disk,
            order,
            current: String::new(),
            smoothed: vec![0.0; SPEC_BINS],
            palette: [PRIMARY, SECONDARY, TERTIARY, PRIMARY, SECONDARY, SECONDARY, TERTIARY, TERTIARY],
            mouse: [0.0; 4],
            disc: [0.0; 4],
            blit_pipe,
            blit_bgl,
            shown: None,
            warm_idx: 0,
            start: Instant::now(),
            frame: 0,
        }
    }

    /// All selectable shader names (sorted), for the settings ComboBox.
    pub fn names(&self) -> &[String] {
        &self.order
    }

    /// The selected shader's name — may name a shader that failed to
    /// build; `callback` falls back to the first working pipeline.
    pub fn current(&self) -> &str {
        &self.current
    }

    pub fn select(&mut self, name: &str) {
        self.current = name.to_owned();
    }

    /// Upload a cover image for `iChannel1` (album art, any size — the
    /// texture is recreated when the dimensions change).
    pub fn set_cover(&mut self, pixels: &[u8], w: u32, h: u32) {
        if w == 0 || h == 0 || pixels.len() != (4 * w * h) as usize {
            return;
        }
        if [w, h] != self.cover_size {
            let (tex, view) = cover_texture(&self.device, &self.queue, w, h, pixels);
            self.cover_tex = tex;
            self.cover_view = view;
            self.cover_size = [w, h];
            let spec_view = self.shared.spec_tex.create_view(&Default::default());
            self.shared.bind_group = Self::bind_group(
                &self.device,
                &self.bgl,
                &self.shared.uniforms,
                &spec_view,
                &self.sampler,
                &self.cover_view,
            );
            return;
        }
        // Same convention as cover_texture: store rows bottom-up.
        let flipped = flip_rows(pixels, w as usize, h as usize);
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.cover_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &flipped,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * w),
                rows_per_image: Some(h),
            },
            self.cover_tex.size(),
        );
    }

    /// Cover for `iChannel1` straight from `library::decode_cover`.
    pub fn set_cover_image(&mut self, img: &ColorImage) {
        let px: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
        self.set_cover(&px, img.size[0] as u32, img.size[1] as u32);
    }

    /// Feed the per-frame spectrum bars — `UpdateAudioTexture`: stretch
    /// the snapshot to 512 bins, normalize by the loudest, then smooth
    /// each bin toward the target by 0.15 per frame.
    pub fn update_spectrum(&mut self, bars: &[f32]) {
        if bars.is_empty() {
            return;
        }
        let mut max_v = 0.0001f32;
        for &v in bars {
            if v > max_v {
                max_v = v;
            }
        }
        let n = bars.len();
        // Raw FFT bins already at texture resolution: no stretch —
        // `UpdateAudioTexture` with step == 1, per-bin normalize +
        // 0.15/frame temporal smoothing only.
        if n == SPEC_BINS {
            for (i, b) in self.smoothed.iter_mut().enumerate() {
                *b += (bars[i] / max_v - *b) * 0.15;
            }
            return;
        }
        // Catmull-Rom stretch — a nearest-bin map leaves ~8-bin
        // plateaus (torn seams in spectrum-driven displacement) and
        // plain bilinear flattens peaks (dead-looking waves). CR keeps
        // continuity through the bar points AND their energy.
        let cr = |f: f32| -> f32 {
            let i1 = f.floor() as usize;
            let i0 = i1.saturating_sub(1);
            let i2 = (i1 + 1).min(n - 1);
            let i3 = (i1 + 2).min(n - 1);
            let (p0, p1, p2, p3) = (bars[i0], bars[i1], bars[i2], bars[i3]);
            let t = f - i1 as f32;
            0.5 * (2.0 * p1
                + (-p0 + p2) * t
                + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t * t
                + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t * t * t)
        };
        let mut target = [0.0f32; SPEC_BINS];
        for (i, t) in target.iter_mut().enumerate() {
            let f = i as f32 / (SPEC_BINS - 1) as f32 * (n - 1) as f32;
            *t = cr(f).max(0.0);
        }
        // Spatial blur: the ~50 source bars are ~10x coarser than the
        // texture, and hairline ribbons turn each bar-boundary slope
        // into a visibly flying dash. Two Gaussian passes make the
        // wave slopes gentle — connected lines — without flattening
        // the peaks much.
        let mut blurred = target;
        for _ in 0..2 {
            let src = blurred;
            for i in 0..SPEC_BINS {
                let mut acc = 0.0;
                for (k, &w) in [1.0f32, 4.0, 6.0, 4.0, 1.0].iter().enumerate() {
                    let j = (i as i32 + k as i32 - 2).clamp(0, SPEC_BINS as i32 - 1) as usize;
                    acc += src[j] * w;
                }
                blurred[i] = acc / 16.0;
            }
        }
        for (i, b) in self.smoothed.iter_mut().enumerate() {
            *b += (blurred[i] / max_v - *b) * 0.15;
        }
    }

    /// The cover-derived gradient — `u_primary/secondary/tertiary`
    /// plus the five `u_grad` stops (lerped across the 3 colors).
    pub fn set_palette(&mut self, colors: &[Color32]) {
        let c = |i: usize| -> [f32; 3] {
            let c = colors[i.min(colors.len().saturating_sub(1))];
            [
                c.r() as f32 / 255.0,
                c.g() as f32 / 255.0,
                c.b() as f32 / 255.0,
            ]
        };
        if colors.is_empty() {
            return;
        }
        let lerp = |a: [f32; 3], b: [f32; 3], t: f32| -> [f32; 3] {
            [
                a[0] + (b[0] - a[0]) * t,
                a[1] + (b[1] - a[1]) * t,
                a[2] + (b[2] - a[2]) * t,
            ]
        };
        let (p, s, t) = (c(0), c(1), c(2));
        self.palette = [
            p,
            s,
            t,
            p,
            lerp(p, s, 0.5),
            s,
            lerp(s, t, 0.5),
            t,
        ];
    }

    /// iMouse in physical pixels, bottom-left origin: xy = pointer
    /// position, z = click origin x (positive while pressed).
    pub fn set_mouse(&mut self, mouse: [f32; 4]) {
        self.mouse = mouse;
    }

    /// `SetDiscOccluder`: the player widget's disc center + ring radius
    /// in physical px, y-up from the bottom-left (GL space) — `u_disc`
    /// goes inactive (w=0) when the widget is hidden.
    pub fn set_disc(&mut self, cx: f32, cy: f32, radius: f32, active: bool) {
        self.disc = [cx, cy, radius, if active { 1.0 } else { 0.0 }];
    }

    /// A paint callback that draws the selected shader for this frame.
    pub fn callback(&mut self, corner_radius: f32, fade: f32) -> ShadertoyCallback {
        // The shader re-renders every frame — iFrame counts rendered
        // frames, iTime stays real elapsed.
        let render_now = true;
        self.frame = self.frame.wrapping_add(1);
        let pipeline = self.pipeline_for(&self.current.clone());
        // Fade the pipeline in over 0.4s when it changes — a worker-built
        // shader arriving (startup, a settings switch) eases on instead
        // of flashing at full opacity.
        let now = Instant::now();
        let arrive = match (&self.shown, &pipeline) {
            (Some((p, t)), Some(q)) if Arc::ptr_eq(p, q) => *t,
            _ => {
                self.shown = pipeline.clone().map(|p| (p, now));
                now
            }
        };
        let ft = ((now - arrive).as_secs_f32() / 0.4).min(1.0);
        let fade = fade * (ft * ft * (3.0 - 2.0 * ft));
        ShadertoyCallback {
            pipeline,
            shared: Arc::new(Shared {
                bind_group: self.shared.bind_group.clone(),
                uniforms: self.shared.uniforms.clone(),
                spec_tex: self.shared.spec_tex.clone(),
            }),
            uniforms: Uniforms {
                resolution: [0.0; 3], // filled per-frame in prepare
                time: self.start.elapsed().as_secs_f32(),
                frame: self.frame,
                fade,
                ch1_size: [self.cover_size[0] as f32, self.cover_size[1] as f32],
                mouse: self.mouse,
                primary: self.palette[0],
                corner: corner_radius,
                secondary: self.palette[1],
                _p1: 0.0,
                tertiary: self.palette[2],
                _p2: 0.0,
                grad0: self.palette[3],
                _g0: 0.0,
                grad1: self.palette[4],
                _g1: 0.0,
                grad2: self.palette[5],
                _g2: 0.0,
                grad3: self.palette[6],
                _g3: 0.0,
                grad4: self.palette[7],
                _g4: 0.0,
                disc: self.disc,
            },
            spectrum: self
                .smoothed
                .iter()
                .map(|&v| f16_bits(v.clamp(0.0, 1.0)))
                .collect(),
            render_now,
            blit_pipe: self.blit_pipe.clone(),
            blit_bgl: self.blit_bgl.clone(),
            sampler: self.sampler.clone(),
        }
    }

    /// Lazily translate+build the pipeline for `name` on a worker
    /// thread (a DX12/FXC compile takes ~1s — synchronous builds froze
    /// startup and shader switches). While the build is in flight
    /// nothing is drawn — showing a transient fallback flashed the
    /// default shader at startup before the real selection landed.
    /// The fallback only engages when `name` has failed permanently.
    fn pipeline_for(&mut self, name: &str) -> Option<Arc<wgpu::RenderPipeline>> {
        while let Ok((n, p)) = self.build_rx.try_recv() {
            self.building.remove(&n);
            self.pipelines.insert(n, p);
        }
        // Pace one extra build whenever no compile is in flight — the
        // whole set warms up in the background over the first seconds
        // after launch, so picking any shader later is instant.
        if self.building.is_empty() {
            while self.warm_idx < self.order.len() {
                let n = self.order[self.warm_idx].clone();
                self.warm_idx += 1;
                if !self.pipelines.contains_key(&n) && !self.building.contains(&n) {
                    self.spawn_build(&n);
                    break;
                }
            }
        }
        match self.pipelines.get(name) {
            Some(Some(p)) => return Some(Arc::clone(p)),
            None => {
                self.spawn_build(name);
                return None;
            }
            Some(None) => {}
        }
        // Some(None) — failed permanently: fall back to the first
        // ready pipeline (default first, matching Lacrima's
        // EnsureDefaultSelectedShaderToy), kicking one extra build
        // per call so the chain still converges on a working shader.
        let mut kicked = false;
        for i in 0..=self.order.len() {
            let n = if i == 0 {
                "AnimatedBackground"
            } else {
                self.order[i - 1].as_str()
            };
            if n == name {
                continue;
            }
            match self.pipelines.get(n) {
                Some(Some(p)) => return Some(Arc::clone(p)),
                Some(None) => {}
                None => {
                    if !kicked {
                        kicked = true;
                        let owned = n.to_owned();
                        self.spawn_build(&owned);
                    }
                }
            }
        }
        None
    }

    /// Kick a worker-thread build for `name` unless it's already
    /// built, failed, or in flight.
    fn spawn_build(&mut self, name: &str) {
        if self.building.contains(name) {
            return;
        }
        let Some(src) = self.source(name) else {
            self.pipelines.insert(name.to_owned(), None);
            return;
        };
        self.building.insert(name.to_owned());
        let (device, bgl, vs, tx) = (
            self.device.clone(),
            self.bgl.clone(),
            self.vs.clone(),
            self.build_tx.clone(),
        );
        let name = name.to_owned();
        std::thread::spawn(move || {
            crate::boot_mark(&format!("shader '{name}' build start"));
            let p = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Self::build_pipeline(&device, &bgl, &vs, OFFSCREEN_FORMAT, &src)
            }))
            .ok()
            .flatten()
            .map(Arc::new);
            crate::boot_mark(&format!("shader '{name}' build end"));
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

    /// GLSL -> naga module -> WGSL -> wgpu pipeline.
    /// Callable from a worker thread — all wgpu handles are Send+Sync.
    fn build_pipeline(
        device: &wgpu::Device,
        bgl: &wgpu::BindGroupLayout,
        vs: &wgpu::ShaderModule,
        format: wgpu::TextureFormat,
        src: &str,
    ) -> Option<wgpu::RenderPipeline> {
        let mut body = process_shader(src.trim_start_matches('\u{feff}'));
        body = replace_ident(&body, "iChannel0", "sampler2D(_ch0_tex, _ch0_sam)");
        body = replace_ident(&body, "iChannel1", "sampler2D(_ch1_tex, _ch1_sam)");
        body = body.replace("texture2D(", "texture(");
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
        let wgsl =
            naga::back::wgsl::write_string(&module, &info, naga::back::wgsl::WriterFlags::EXPLICIT_TYPES)
                .ok()?;
        let fs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shadertoy_fs"),
            source: wgpu::ShaderSource::Wgsl(wgsl.into()),
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("shadertoy_pl"),
            bind_group_layouts: &[Some(bgl)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("shadertoy_pipeline"),
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
                // REPLACE into the offscreen target — the blit pass
                // owns alpha compositing so the surface result is
                // identical to drawing the shader directly.
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
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
        Some(pipeline)
    }

    fn bind_group(
        device: &wgpu::Device,
        bgl: &wgpu::BindGroupLayout,
        uniforms: &wgpu::Buffer,
        spec_view: &wgpu::TextureView,
        sampler: &wgpu::Sampler,
        cover_view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("shadertoy_bg"),
            layout: bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(spec_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(cover_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        })
    }
}

impl egui_wgpu::CallbackTrait for ShadertoyCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen_descriptor: &egui_wgpu::ScreenDescriptor,
        egui_encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let mut uniforms = self.uniforms;
        uniforms.resolution = [
            screen_descriptor.size_in_pixels[0] as f32,
            screen_descriptor.size_in_pixels[1] as f32,
            1.0,
        ];
        queue.write_buffer(&self.shared.uniforms, 0, bytemuck::bytes_of(&uniforms));
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.shared.spec_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&self.spectrum),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(SPEC_BINS as u32 * 2),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: SPEC_BINS as u32,
                height: 1,
                depth_or_array_layers: 1,
            },
        );

        // Offscreen target matching the surface (recreated on resize).
        let size = [
            screen_descriptor.size_in_pixels[0].max(1),
            screen_descriptor.size_in_pixels[1].max(1),
        ];
        let recreate = callback_resources
            .get::<OffscreenRes>()
            .map(|r| r.size != size)
            .unwrap_or(true);
        if recreate {
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("shadertoy_offscreen"),
                size: wgpu::Extent3d {
                    width: size[0],
                    height: size[1],
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: OFFSCREEN_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = tex.create_view(&Default::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("shadertoy_blit_bg"),
                layout: &self.blit_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            callback_resources.insert(OffscreenRes {
                _tex: tex,
                view,
                bind_group,
                size,
                has_frame: false,
            });
        }

        // The heavy fragment pass re-renders on the ~30fps tick (and
        // always right after a resize, so the blit never samples a
        // fresh empty target).
        if let Some(pipeline) = &self.pipeline {
            if self.render_now || recreate {
                {
                    let res = callback_resources.get_mut::<OffscreenRes>().unwrap();
                    let mut pass =
                        egui_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("shadertoy_offscreen_pass"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &res.view,
                                resolve_target: None,
                                depth_slice: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                                    store: wgpu::StoreOp::Store,
                                },
                            })],
                            depth_stencil_attachment: None,
                            timestamp_writes: None,
                            occlusion_query_set: None,
                            multiview_mask: None,
                        });
                    pass.set_pipeline(pipeline);
                    pass.set_bind_group(0, &self.shared.bind_group, &[]);
                    pass.draw(0..3, 0..1);
                }
                callback_resources
                    .get_mut::<OffscreenRes>()
                    .unwrap()
                    .has_frame = true;
            }
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &egui_wgpu::CallbackResources,
    ) {
        if let Some(res) = callback_resources.get::<OffscreenRes>() {
            if res.has_frame {
                render_pass.set_pipeline(&self.blit_pipe);
                render_pass.set_bind_group(0, &res.bind_group, &[]);
                render_pass.draw(0..3, 0..1);
            }
        }
    }
}

/// `%LOCALAPPDATA%\AES_Rustified\Shaders\Shadertoys` — the disk-scanned
/// shader directory, next to the settings folder (was `<exe>/Shaders/
/// Shadertoys`, matching Lacrima's ApplicationPaths.ShadersDirectory).
fn shaders_dir() -> Option<PathBuf> {
    dirs::data_local_dir()
        .map(|d| d.join("AES_Rustified").join("Shaders").join("Shadertoys"))
}

/// Seed the disk dir: embedded shaders land where they're missing
/// (never overwriting user edits), and any .frag left in the old
/// exe-side folder carries over so nothing is lost in the move.
fn seed_shader_dir(dir: &std::path::Path) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    if let Some(legacy) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.join("Shaders").join("Shadertoys")))
    {
        if legacy != dir {
            if let Ok(rd) = std::fs::read_dir(&legacy) {
                for e in rd.flatten() {
                    let src = e.path();
                    if src.extension().and_then(|x| x.to_str()) != Some("frag") {
                        continue;
                    }
                    let dst = dir.join(src.file_name().unwrap());
                    if !dst.exists() {
                        let _ = std::fs::copy(&src, &dst);
                    }
                }
            }
        }
    }
    for (name, src) in EMBEDDED {
        let dst = dir.join(format!("{name}.frag"));
        if !dst.exists() {
            let _ = std::fs::write(&dst, src);
        }
    }
}

/// Mirror of `GlShaderToyControl.ProcessShaderCode`: drop `#version`,
/// `precision`, `out`, wrapper-provided uniform declarations, and any
/// `void main` (the host supplies it).
fn process_shader(content: &str) -> String {
    let mut keep = String::new();
    'lines: for raw in content.replace("\r\n", "\n").split('\n') {
        let line = raw.trim();
        if line.is_empty() {
            keep.push_str(raw);
            keep.push('\n');
            continue;
        }
        if line.starts_with("#version")
            || line.starts_with("precision")
            || line.starts_with("out ")
        {
            continue;
        }
        if line.starts_with("void main(") || line.starts_with("void main (") {
            break;
        }
        if line.starts_with("uniform ") {
            for name in FORBIDDEN {
                if line.contains(name) {
                    continue 'lines;
                }
            }
        }
        keep.push_str(raw);
        keep.push('\n');
    }
    keep
}

/// Replace whole-word `name` only — `iChannel1Size` must not be hit by
/// the `iChannel1` substitution. `pub(crate)` — the capture shaders
/// use it for `Texture`/`#define` expansion too.
pub(crate) fn replace_ident(src: &str, name: &str, repl: &str) -> String {
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(name.as_bytes()) {
            let before_ok = i == 0 || !is_word(bytes[i - 1]);
            let after = i + name.len();
            let after_ok = after >= bytes.len() || !is_word(bytes[after]);
            if before_ok && after_ok {
                out.push_str(repl);
                i = after;
                continue;
            }
        }
        let ch_len = src[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        out.push_str(&src[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// iChannel1 stores the image bottom-row-first, matching GL's
/// bottom-left texcoord convention: `texture(iChannel1, uv)` with
/// uv.y=0 at the bottom of the screen must sample the cover's bottom
/// row (Lacrima uploads the same way — callers pass top-down pixels).
fn flip_rows(pixels: &[u8], w: usize, h: usize) -> Vec<u8> {
    let row = 4 * w;
    let mut out = Vec::with_capacity(pixels.len());
    for y in (0..h).rev() {
        out.extend_from_slice(&pixels[y * row..(y + 1) * row]);
    }
    out
}

fn cover_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    w: u32,
    h: u32,
    pixels: &[u8],
) -> (wgpu::Texture, wgpu::TextureView) {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("shadertoy_channel1"),
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
    let flipped = flip_rows(pixels, w as usize, h as usize);
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &flipped,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4 * w),
            rows_per_image: Some(h),
        },
        tex.size(),
    );
    let view = tex.create_view(&Default::default());
    (tex, view)
}

/// Stand-in cover art: cyan -> violet diagonal gradient darkened toward navy.
fn cover_pixels() -> Vec<u8> {
    let size = 128u32;
    let mix = |a: [f32; 3], b: [f32; 3], t: f32| -> [f32; 3] {
        [
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
        ]
    };
    let mut data = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let u = x as f32 / (size - 1) as f32;
            let v = y as f32 / (size - 1) as f32;
            let c = mix(mix(PRIMARY, SECONDARY, u), TERTIARY, v * 0.7);
            let i = ((y * size + x) * 4) as usize;
            data[i] = (c[0].clamp(0.0, 1.0) * 255.0) as u8;
            data[i + 1] = (c[1].clamp(0.0, 1.0) * 255.0) as u8;
            data[i + 2] = (c[2].clamp(0.0, 1.0) * 255.0) as u8;
            data[i + 3] = 255;
        }
    }
    data
}
