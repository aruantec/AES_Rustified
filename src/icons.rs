use eframe::egui::{self, SizeHint, TextureHandle};

/// Menu icons: gamepad/music-note/gear from the same path data used by
/// AES_Lacrima's `MainMenuView.axaml`; the Re/Decompilation chip is an
/// original glyph drawn for this port.
pub struct MenuIcons {
    pub emulation: TextureHandle,
    pub music: TextureHandle,
    pub recomp: TextureHandle,
    pub settings: TextureHandle,
    /// Material arrow-left, used by the top-bar back button.
    pub back: TextureHandle,
}

// Rasterize well above the largest on-screen size so dock magnification
// stays crisp (the SVGs' natural sizes are only 24-100px).
const ICON_TEXEL_SIZE: u32 = 256;

pub fn load(ctx: &egui::Context) -> MenuIcons {
    let options = resvg::usvg::Options::default();
    let size_hint = SizeHint::Size {
        width: ICON_TEXEL_SIZE,
        height: ICON_TEXEL_SIZE,
        maintain_aspect_ratio: true,
    };
    let load = |name: &str, svg: &str| -> TextureHandle {
        let image = egui_extras::image::load_svg_bytes_with_size(svg.as_bytes(), size_hint, &options)
            .unwrap_or_else(|err| panic!("invalid {name} icon: {err}"));
        crate::shared_tex::load_tex(ctx, name, image, egui::TextureOptions::LINEAR)
    };
    MenuIcons {
        emulation: load("icon-emulation", EMULATION_SVG),
        music: load("icon-music", MUSIC_SVG),
        recomp: load("icon-recomp", RECOMP_SVG),
        settings: load("icon-settings", SETTINGS_SVG),
        back: load("icon-back", BACK_SVG),
    }
}

// EvenOdd like Avalonia's Path default: the d-pad cross, buttons and head
// inset are subpaths that must punch holes through the body.
const EMULATION_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 28"><path fill="white" fill-rule="evenodd" d="M7.5,10 M8,1 L24,1 C25.7,1 27,2.3 27,4 L27,12 L25,12 L25,4 C25,3.4 24.6,3 24,3 L8,3 C7.4,3 7,3.4 7,4 L7,12 L5,12 L5,4 C5,2.3 6.3,1 8,1 Z M9,4.7 L23,4.7 C23.7,4.7 24.3,5.3 24.3,6 L24.3,10.3 C24.3,11 23.7,11.6 23,11.6 L9,11.6 C8.3,11.6 7.7,11 7.7,10.3 L7.7,6 C7.7,5.3 8.3,4.7 9,4.7 Z C5.0,10 2.7,12.3 2.3,14.9 L1.2,21.7 C0.8,24.1 2.6,26.3 5.0,26.3 L8.7,26.3 C10.0,26.3 11.2,25.6 11.9,24.4 L13.0,22.6 C13.5,21.9 14.2,21.5 15.0,21.5 L17.0,21.5 C17.8,21.5 18.5,21.9 19.0,22.6 L20.1,24.4 C20.8,25.6 22.0,26.3 23.3,26.3 L27.0,26.3 C29.4,26.3 31.2,24.1 30.8,21.7 L29.7,14.9 C29.3,12.3 27.0,10 24.5,10 L7.5,10 Z M10.2,14.8 L8.7,14.8 L8.7,16.6 L6.9,16.6 L6.9,18.1 L8.7,18.1 L8.7,19.9 L10.2,19.9 L10.2,18.1 L12.0,18.1 L12.0,16.6 L10.2,16.6 L10.2,14.8 Z M22.5,15.3 A1.6,1.6 0 1 1 19.3,15.3 A1.6,1.6 0 1 1 22.5,15.3 Z M25.3,18.1 A1.6,1.6 0 1 1 22.1,18.1 A1.6,1.6 0 1 1 25.3,18.1 Z"/></svg>"##;

const MUSIC_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><rect x="15" y="6" width="70" height="8" rx="4" fill="white"/><rect x="15" y="18" width="70" height="8" rx="4" fill="white"/><rect x="6" y="30" width="88" height="64" rx="12" fill="white"/><path fill="black" d="M62,76 C62,84 56,90 48,90 C40,90 34,84 34,76 C34,68 40,62 48,62 C50,62 54,63 58,66 L58,34 L74,32 L74,40 L62,44 L62,76 Z"/></svg>"##;

/// Re/Decompilation: a pinned chip with a two-arrow cycle punched
/// through it — the forward and reverse arrows read as re-compile and
/// de-compile (binary -> native -> back).
const RECOMP_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="2.9 2.9 26.2 26.2"><rect x="6.5" y="6.5" width="19" height="19" rx="3.5" fill="white"/><g fill="white"><rect x="4.2" y="10.2" width="3.0" height="1.6" rx="0.8"/><rect x="4.2" y="15.2" width="3.0" height="1.6" rx="0.8"/><rect x="4.2" y="20.2" width="3.0" height="1.6" rx="0.8"/><rect x="24.8" y="10.2" width="3.0" height="1.6" rx="0.8"/><rect x="24.8" y="15.2" width="3.0" height="1.6" rx="0.8"/><rect x="24.8" y="20.2" width="3.0" height="1.6" rx="0.8"/><rect x="10.2" y="4.2" width="1.6" height="3.0" rx="0.8"/><rect x="15.2" y="4.2" width="1.6" height="3.0" rx="0.8"/><rect x="20.2" y="4.2" width="1.6" height="3.0" rx="0.8"/><rect x="10.2" y="24.8" width="1.6" height="3.0" rx="0.8"/><rect x="15.2" y="24.8" width="1.6" height="3.0" rx="0.8"/><rect x="20.2" y="24.8" width="1.6" height="3.0" rx="0.8"/></g><path fill="none" stroke="black" stroke-width="2.0" stroke-linecap="round" d="M19.64,13.9 A4.2,4.2 0 0 0 12.36,13.9 M12.36,18.1 A4.2,4.2 0 0 0 19.64,18.1"/><path fill="black" d="M11.16,15.99 L13.67,14.65 L11.05,13.15 Z M20.84,16.01 L18.33,17.35 L20.95,18.85 Z"/></svg>"##;

const SETTINGS_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="1.5 1.5 21 21"><path fill="white" d="M19.14,12.94c0.04-0.3,0.06-0.61,0.06-0.94c0-0.32-0.02-0.64-0.07-0.94l2.03-1.58c0.18-0.14,0.23-0.41,0.12-0.61 l-1.92-3.32c-0.12-0.22-0.37-0.29-0.59-0.22l-2.39,0.96c-0.5-0.38-1.03-0.7-1.62-0.94L14.4,2.81c-0.04-0.24-0.24-0.41-0.48-0.41 h-3.84c-0.24,0-0.43,0.17-0.47,0.41L9.25,5.35C8.66,5.59,8.12,5.91,7.62,6.29L5.23,5.33c-0.22-0.08-0.47,0-0.59,0.22L2.72,8.87 c-0.11,0.21-0.06,0.47,0.12,0.61l2.03,1.58C4.82,11.36,4.8,11.69,4.8,12c0,0.32,0.02,0.64,0.07,0.94l-2.03,1.58 c-0.18,0.14-0.23,0.41-0.12,0.61l1.92,3.32c0.12,0.22,0.37,0.29,0.59,0.22l2.39-0.96c0.5,0.38,1.03,0.7,1.62,0.94l0.36,2.54 c0.05,0.24,0.24,0.41,0.48,0.41h3.84c0.24,0,0.44-0.17,0.47-0.41l0.36-2.54c0.59-0.24,1.13-0.56,1.62-0.94l2.39,0.96 c0.22,0.08,0.47,0,0.59-0.22l1.92-3.32c0.12-0.22,0.07-0.47-0.12-0.61L19.14,12.94z M12,15.6c-1.98,0-3.6-1.62-3.6-3.6 s1.62-3.6,3.6-3.6s3.6,1.62,3.6,3.6S13.98,15.6,12,15.6z"/></svg>"##;

// Material arrow-left, same path as MainTopBar's back button.
const BACK_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="white" d="M20,11V13H8L13.5,18.5L12.08,19.92L4.16,12L12.08,4.08L13.5,5.5L8,11H20Z"/></svg>"##;
