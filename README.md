# AES - Rustified

**AES Rustified is a Rust/egui port of [AES_Lacrima](https://github.com/aruantec/AES_Lacrima).**

AES stands for **"Aruan's Entertainment Software"**, formerly known as **@ES** in a small emulation
community. AES_Lacrima is a personal research project, emulation frontend and multimedia app
created with the Avalonia UI framework — this port brings it to Rust with
[egui](https://github.com/emilk/egui). It's by no means the perfect project, but it helps me learn
how to use the framework.

This port focuses on the multimedia side: a cover-flow carousel and card grids for the music
library, local folder scanning, online media playback (YouTube and friends via yt-dlp), playlist
import/export, a metadata editor, an equalizer, and the spectrum-driven shader backgrounds.

Supports Plasma6 Wallpaper (Shadertoy/Fragment) Shaders in the same loose uniform convention as
Lacrima (`iResolution`, `iTime`, `iChannel0` spectrum, `iChannel1` cover) — GLSL is translated to
WGSL at runtime via naga. The bundled collection lives in `shaders/shadertoys`.
<br />[https://github.com/y4my4my4m/kde-shader-wallpaper](https://github.com/y4my4my4m/kde-shader-wallpaper/tree/master/package/contents/ui/Shaders)<br />

Some of the included shaders react to the music by using the player's spectrum data (`iChannel0`
is fed a 512-wide spectrum texture every frame).

## Build

- Compile: `cargo build --release`
- Binary: `target/release/AES-Rustified.exe`

Optional: **yt-dlp** for online media — manage it from *Settings → Libraries* and the app
downloads it into `%LOCALAPPDATA%\AES_Rustified\Tools` automatically (no PATH needed).

App data (settings, metadata/cover caches, stream cache, tools) lives under
`%LOCALAPPDATA%\AES_Rustified`.
