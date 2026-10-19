# AES Oxide

**AES Oxide is a Rust/egui port of [AES_Lacrima](https://github.com/aruantec/AES_Lacrima).**

AES stands for **"Aruan's Entertainment Software"**, formerly known as **@ES** in a small emulation
community. AES_Lacrima is a personal research project, emulation frontend and multimedia app
created with the Avalonia UI framework — this port brings it to Rust with
[egui](https://github.com/emilk/egui). It's by no means the perfect project, but it helps me learn
how to use the framework.

This port focuses on the multimedia side: a cover-flow carousel and card grids for the music
library, local folder scanning, online media playback (YouTube and friends via yt-dlp), playlist
import/export, a metadata editor, an equalizer, and the spectrum-driven shader backgrounds.

Some of the included shaders react to the music by using the player's spectrum data (`iChannel0`
is fed a 512-wide spectrum texture every frame).

## Screenshots

![Music view — cover-flow carousel, spectrum analyzer and album strip](screenshots/music-view.png)

![Audio settings — spectrum, ReplayGain and playback options](screenshots/settings.png)

![Shader view — spectrum-reactive background with the clock widget](screenshots/shader-view.png)

## Controls

### Global

| Key | Function |
|---|---|
| `F12` | Toggle fullscreen — entering is instant, exiting shows a confirmation prompt |
| `Esc` | Layered back-out: close prompt → close search → exit game-fullscreen → ask to close emulator → fullscreen prompt → navigate back → ask to exit the app (Main view) |
| `Backspace` | Back navigation |
| `Ctrl+F` | Focus search (album filter, item search, or the emulation pill) |
| `Ctrl+R` | Cycle repeat mode (Off → All → One) |
| Media keys | `⏯` play/pause · `⏹` stop · `⏭`/`⏮` next/previous track — active while the window is focused |

### Music view

| Key | Function |
|---|---|
| `←` `→` `↑` `↓` | Navigate the card grid / album strip |
| `Shift` + arrows | Reorder the selection (cards or albums) |
| `Ctrl+A` | Select all in the owning list |
| `Delete` | Delete the selection (with confirmation) |
| `Tab` | Cycle keyboard ownership between the album strip and the card grid |
| `Enter` | Activate like a double-click — open the album / play the track |
| `Alt` (clean tap) | Cycle the cover layout (carousel → vertical → horizontal) |
| `Ctrl` + `↑`/`↓` | Volume ±5% |
| `Ctrl` + `←`/`→` | Seek ∓5s / ±5s |
| `1`–`9` | Activate the numbered main-menu entries (Main view only) |

### Re/Decompilation view

A curated catalog of decomp, recomp and disassembly projects (zeldaret, n64decomp, pret, RSDK,
devilution and friends). Each card shows the game's box art (fetched once via the same image
pipeline that powers emulation covers, then cached under `Decomps/covers/`), install state,
and the band's refresh button checks `pushed_at` on every repo and flags updates. Install/Update
downloads the project's latest release build for your OS when it publishes one (DevilutionX,
OpenGOAL, Zelda64Recomp and friends ship real binaries); projects that don't publish releases
fall back to the source zipball — those still need the project's own toolchain and a baserom.
Installs land in `Decomps/` under the app data directory.

| Key | Function |
|---|---|
| `←` `→` `↑` `↓` | Navigate the project grid |
| `Enter` | Run the card's action — Install / Update / Open folder |
| `Esc` | Back to the main menu |

### Emulation view

| Key | Function |
|---|---|
| `←` `→` `↑` `↓` | Navigate the section strip / ROM card grid — the strip stays navigable while a game is captured |
| `Shift` + arrows | Reorder sections or ROMs |
| `Ctrl+A` | Select all on the owning list |
| `Delete` | Remove selected ROMs (with confirmation) |
| `Tab` | Cycle keyboard ownership between the section strip and the ROM grid — while capturing, switches back to the list |
| `Enter` | Open the selected section / launch the selected ROM — or return to the running game's capture |
| `Ctrl+S` | Switch between the capture viewport and the ROM list (the ⇄ band button) |
| `Ctrl+E` | Toggle capture fullscreen (same as double-clicking the capture) |
| `Ctrl+M` | Toggle the input tunnel — clicks/keys over the capture reach the emulator; `Esc` also releases it |
| `Ctrl+Q` | Ask to close the running emulator (same prompt as the power button) |
| `Esc` | Release the input tunnel → exit game-fullscreen → ask to close the emulator |
| `F12` | Application fullscreen |
| Double-click on capture | Toggle game-fullscreen (capture covers the whole screen) |

### Dialogs & widgets

| Key | Function |
|---|---|
| `Tab` / arrows | Move focus between dialog buttons |
| `Enter` | Run the focused dialog button |
| `Esc` | Close overlays and prompts (settings, tag editor, ROM editor, patches, custom config) |
| Arrows on a focused slider | Nudge the value (`Shift` = ×10 step) |

Text fields capture their own keys while focused — no shortcuts fire while typing.

## Build

- Compile: `cargo build --release`
- Binary: `target/release/AES-Rustified.exe`

Optional: **yt-dlp** for online media — manage it from *Settings → Libraries* and the app
downloads it into `%LOCALAPPDATA%\AES_Rustified\Tools` automatically (no PATH needed).

App data (settings, metadata/cover caches, stream cache, tools) lives under
`%LOCALAPPDATA%\AES_Rustified`.

## Disclaimer

This software is a personal research project made for **research and learning purposes
only**. It is not intended for production use or any commercial purpose.

This software is provided **"as is"**, without warranty of any kind, express or implied,
including but not limited to the warranties of merchantability, fitness for a particular
purpose and noninfringement. In no event shall the author be liable for any claim, damages
or other liability — whether in an action of contract, tort or otherwise — arising from,
out of or in connection with the software or the use or other dealings in the software.

**You, the user, are solely responsible for how you use this software** and for any
consequences of that use, including any damage caused directly or indirectly by the
software, and for ensuring your use complies with the laws and terms of service applicable
to you.