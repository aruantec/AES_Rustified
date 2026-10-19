//! Music library model: a directory that directly contains audio files
//! is an album (Lacrima's FolderMediaItem model). Scanning only walks
//! the filesystem — covers are resolved lazily at draw time, either
//! from a folder art file or embedded tag art via lofty.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use eframe::egui;

const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "ogg", "oga", "opus", "m4a", "aac", "wav", "aif", "aiff", "wma", "ape",
    "mpc", "wv",
];
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp"];
/// Preferred names for a folder's own cover image.
const COVER_STEMS: &[&str] = &["cover", "folder", "front", "album", "artwork"];
const MAX_DEPTH: usize = 8;
/// Covers decoded into textures this wide; plenty for tile/carousel use.
const COVER_MAX_PX: u32 = 256;
/// Covers presented per album tile (Lacrima's AlbumTilePresentationCoverCount).
pub const MAX_COVERS: usize = 4;

/// Where a cover image comes from; resolved to pixels on demand.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CoverSrc {
    /// An image file on disk (folder cover like cover.jpg, or loose art).
    File(PathBuf),
    /// Embedded art inside this audio file, read lazily via lofty.
    Tag(PathBuf),
    /// Remote image candidates, tried in order (online media items —
    /// e.g. img.youtube.com maxres/hq/mq fallbacks).
    Web(Vec<String>),
}

/// PathBuf doubles as the id for URL media items — a "path" is remote
/// when it looks like an http(s) link (Lacrima's IsOnlineMediaItem).
pub fn is_url(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.starts_with("http://") || s.starts_with("https://")
}

#[derive(Clone)]
pub struct Album {
    pub title: String,
    #[allow(dead_code)] // used by "Open Folder" actions later
    pub dir: PathBuf,
    /// Audio files in this folder, sorted by name.
    pub tracks: Vec<PathBuf>,
    /// Up to MAX_COVERS presentation covers, bottom of the fan first;
    /// the last entry is the top cover (folder art when available).
    pub covers: Vec<CoverSrc>,
    /// Persisted URL items under this album's dir key (patched by the
    /// caller — the scanner doesn't see settings). Gates the tile
    /// menu's "Export Playlist".
    pub url_count: usize,
}

/// Scan `roots` on a background thread; each album is sent the moment
/// its directory walk completes so the row fills in procedurally
/// instead of staying empty until the whole scan finishes (the
/// channel closing marks the scan done).
pub fn spawn_scan(roots: &[PathBuf]) -> mpsc::Receiver<Album> {
    let roots = roots.to_vec();
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("aes-library-scan".into())
        .spawn(move || {
            scan_roots(&roots, &tx);
        })
        .expect("library scan thread");
    rx
}

pub fn scan_roots(roots: &[PathBuf], tx: &mpsc::Sender<Album>) {
    for root in roots {
        if root.is_dir() {
            scan_dir(root, 0, tx);
        }
    }
}

/// Depth-first walk; every directory holding audio files yields one
/// album, and descent continues for nested albums (Artist/Album trees).
/// Each result is sent immediately — the UI side inserts it into the
/// sorted position so discovery order doesn't scramble the row.
fn scan_dir(dir: &Path, depth: usize, tx: &mpsc::Sender<Album>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut audio = Vec::new();
    let mut images = Vec::new();
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            if !entry
                .file_name()
                .to_str()
                .map(|n| n.starts_with('.'))
                .unwrap_or(true)
            {
                subdirs.push(path);
            }
        } else if entry
            .file_name()
            .to_str()
            .map(|n| n.starts_with('.'))
            .unwrap_or(true)
        {
            // Hidden/dot-prefixed files (macOS AppleDouble `._*.mp3`,
            // desktop.ini sidecars) — not real tracks.
        } else if let Some(ext) = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
        {
            if AUDIO_EXTS.contains(&ext.as_str()) {
                audio.push(path);
            } else if IMAGE_EXTS.contains(&ext.as_str()) {
                images.push(path);
            }
        }
    }
    if !audio.is_empty() {
        audio.sort();
        images.sort();
        let title = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.display().to_string());
        if tx.send(build_album(title, dir.to_path_buf(), audio, images)).is_err() {
            return;
        }
    }
    // Descend in case-insensitive name order — streamed albums then
    // arrive roughly alphabetically, so the row fills left-to-right
    // instead of tiles popping in at random positions mid-scan.
    subdirs.sort_by_key(|p| {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned().to_lowercase())
            .unwrap_or_default()
    });
    for sub in subdirs {
        scan_dir(&sub, depth + 1, tx);
    }
}

fn build_album(
    title: String,
    dir: PathBuf,
    tracks: Vec<PathBuf>,
    images: Vec<PathBuf>,
) -> Album {
    // Fan layers: embedded art of the first tracks (up to 3).
    let mut covers: Vec<CoverSrc> = tracks
        .iter()
        .take(MAX_COVERS - 1)
        .cloned()
        .map(CoverSrc::Tag)
        .collect();
    // Top cover: the folder's own art file when present, else the first
    // track's embedded art (Lacrima's ResolveAlbumTileTopCover).
    let folder_cover = images
        .iter()
        .find(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| COVER_STEMS.contains(&s.to_lowercase().as_str()))
                .unwrap_or(false)
        })
        .or_else(|| images.first());
    covers.push(match folder_cover {
        Some(p) => CoverSrc::File(p.clone()),
        None => CoverSrc::Tag(tracks[0].clone()),
    });
    Album {
        title,
        dir,
        tracks,
        covers,
        url_count: 0,
    }
}

impl CoverSrc {
    /// A url item's thumbnail source — the guessed img.youtube.com
    /// candidate ladder (real resolved thumbs arrive via metadata
    /// and land on the item's `cover` directly). `None` for urls
    /// without a known thumbnail host. A cover edited into the local
    /// meta cache wins over every remote candidate.
    pub fn web_for(url: &str) -> Option<CoverSrc> {
        let local = crate::meta_cache::cover_path(url);
        if local.exists() {
            return Some(CoverSrc::File(local));
        }
        crate::ytdlp::extract_video_id(url)
            .map(|id| CoverSrc::Web(crate::ytdlp::thumbnail_candidates(&id)))
    }
}

impl Album {
    /// A pre-scan stand-in built from the persisted manifest (cached
    /// dir order): the tile shows instantly — title + default art —
    /// and the streamed scan result replaces it in place once the
    /// folder is walked.
    pub fn placeholder(dir: &str) -> Album {
        let title = Path::new(dir)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.to_string());
        Album {
            title,
            dir: dir.into(),
            tracks: Vec::new(),
            covers: Vec::new(),
            url_count: 0,
        }
    }

    /// Rebuild the web-derived fan layers after the album's url items
    /// change — a created/imported album starts with no covers and the
    /// tile must pick up thumbnails as items are added, dropped on
    /// remove/clear, and refreshed as real thumbs resolve. Folder/tag
    /// art is kept, and the tile's top cover keeps its position (web
    /// layers fan in beneath it). `url_count` is the true url-item
    /// count — `web` may be shorter when items have no thumb host.
    pub fn sync_url_covers(&mut self, web: Vec<CoverSrc>, url_count: usize) {
        let base: Vec<CoverSrc> = self
            .covers
            .iter()
            .filter(|c| !matches!(c, CoverSrc::Web(_)))
            .cloned()
            .collect();
        let (fan, top) = match base.split_last() {
            Some((t, rest)) => (rest.to_vec(), vec![t.clone()]),
            None => (Vec::new(), Vec::new()),
        };
        if top.is_empty() {
            // No folder/tag art — a url-only album leads with its
            // first item: fan = the first MAX_COVERS-1 thumbs, top =
            // the FIRST item's thumb (build_album puts the first
            // child's art on top when there's no folder cover file;
            // the same duplication keeps url albums identical).
            self.covers = web
                .iter()
                .take(MAX_COVERS - 1)
                .cloned()
                .chain(web.first().cloned())
                .collect();
        } else {
            // Folder art stays on top — web thumbs fan in beneath it.
            let web_slots = MAX_COVERS.saturating_sub(fan.len() + top.len());
            self.covers = fan
                .into_iter()
                .chain(web.into_iter().take(web_slots))
                .chain(top)
                .collect();
        }
        self.url_count = url_count;
    }
}

/// Synthetic dir scheme for virtual albums — the "dir" has no
/// backing folder; it just keys `UrlItems` like a real path does.
pub const VIRTUAL_SCHEME: &str = "apl://";

pub fn is_virtual_dir(dir: &Path) -> bool {
    dir.to_string_lossy().starts_with(VIRTUAL_SCHEME)
}

impl Album {
    /// A virtual album: no files on disk, URL items only (persisted
    /// in `UrlItems` under `dir`). Identical whether created empty
    /// via Create Album or imported from an .apl. Presentation covers
    /// are the first items' web thumbnails.
    pub fn virtual_album(dir: &str, title: String, urls: &[String]) -> Self {
        // Same convention as build_album/url-only sync: fan = first
        // items' thumbs, top cover = the FIRST item's thumb.
        let web: Vec<CoverSrc> =
            urls.iter().filter_map(|u| CoverSrc::web_for(u)).collect();
        let covers: Vec<CoverSrc> = web
            .iter()
            .take(MAX_COVERS - 1)
            .cloned()
            .chain(web.first().cloned())
            .collect();
        Album {
            title,
            dir: PathBuf::from(dir),
            tracks: Vec::new(),
            covers,
            url_count: urls.len(),
        }
    }
}

/// Decode a cover to a ColorImage (256px max), for the UI thread.
/// `Web` tries each candidate URL in order (img.youtube.com quality
/// fallbacks); fetches are blocking and run on cover worker threads.
pub fn decode_cover(src: &CoverSrc) -> Option<egui::ColorImage> {
    let bytes = match src {
        CoverSrc::File(p) => std::fs::read(p).ok()?,
        CoverSrc::Tag(p) => tag_picture(p)?,
        CoverSrc::Web(urls) => {
            // Cached candidates win instantly — thumbnail bytes
            // persist on disk, so a hit skips the network entirely.
            // Corrupt entries are dropped and re-fetched.
            let mut found = None;
            for u in urls {
                if let Some(b) = crate::meta_cache::load_image(u) {
                    if image::load_from_memory(&b).is_ok() {
                        found = Some(b);
                        break;
                    }
                    crate::meta_cache::remove_image(u);
                }
            }
            // Race the remaining candidates in parallel — the serial
            // ladder cost up to candidates x timeout when a host was
            // slow, which froze URL-album tiles for tens of seconds.
            // First valid image wins; other threads still land their
            // bytes in the cache when they finish.
            if found.is_none() {
                let (tx, rx) = mpsc::channel();
                for u in urls.clone() {
                    let tx = tx.clone();
                    std::thread::Builder::new()
                        .name("aes-thumb-candidate".into())
                        .spawn(move || {
                            if let Some(b) = crate::ytdlp::fetch_bytes(&u, 6) {
                                if image::load_from_memory(&b).is_ok() {
                                    crate::meta_cache::save_image(&u, &b);
                                    let _ = tx.send(b);
                                }
                            }
                        })
                        .ok();
                }
                drop(tx);
                found = rx.recv().ok();
            }
            found?
        }
    };
    let img = image::load_from_memory(&bytes).ok()?;
    // YouTube thumbnails bake letterbox/pillarbox bars into the image
    // (hqdefault is 4:3 holding 16:9 content) — crop uniform near-black
    // edges so the cover shows actual pixels (web sources only; local
    // art is kept exactly as authored).
    let img = if matches!(src, CoverSrc::Web(_)) {
        crop_black_bars(img)
    } else {
        img
    };
    let img = img.thumbnail(COVER_MAX_PX, COVER_MAX_PX).to_rgba8();
    let (w, h) = img.dimensions();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        img.as_raw(),
    ))
}

/// Crop uniform near-black letterbox/pillarbox bars baked into a
/// thumbnail. An edge row/column counts as bar only when ≥98% of its
/// pixels are near-black, so genuinely dark artwork keeps its edges.
/// Each side is capped at 40% of the dimension as a safety bound.
pub(crate) fn crop_black_bars(img: image::DynamicImage) -> image::DynamicImage {
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w < 32 || h < 32 {
        return rgba.into();
    }
    let dark = |x: u32, y: u32| {
        let p = rgba.get_pixel(x, y);
        p[0].max(p[1]).max(p[2]) < 24
    };
    let row_dark = |y: u32| (0..w).filter(|&x| dark(x, y)).count() as f32 / w as f32;
    let col_dark = |x: u32| (0..h).filter(|&y| dark(x, y)).count() as f32 / h as f32;
    let max_v = h * 2 / 5;
    let max_h = w * 2 / 5;
    let mut top = 0;
    while top < max_v && row_dark(top) >= 0.98 {
        top += 1;
    }
    let mut bot = 0;
    while bot < max_v && row_dark(h - 1 - bot) >= 0.98 {
        bot += 1;
    }
    let mut left = 0;
    while left < max_h && col_dark(left) >= 0.98 {
        left += 1;
    }
    let mut right = 0;
    while right < max_h && col_dark(w - 1 - right) >= 0.98 {
        right += 1;
    }
    let nw = w.saturating_sub(left + right);
    let nh = h.saturating_sub(top + bot);
    if nw < 16 || nh < 16 || (top + bot + left + right) == 0 {
        return rgba.into();
    }
    image::DynamicImage::ImageRgba8(rgba).crop_imm(left, top, nw, nh)
}

/// First embedded picture of any tag on the file.
fn tag_picture(path: &Path) -> Option<Vec<u8>> {
    use lofty::file::TaggedFileExt;
    let tagged = lofty::read_from_path(path).ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    tag.pictures().first().map(|p| p.data().to_vec())
}

/// Per-track display metadata for the loaded album's cover items
/// (Lacrima's MediaItem Title/Artist/Album bindings).
#[derive(Clone, Debug)]
pub struct TrackInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Track number parsed from tags or the filename prefix, when any.
    pub number: Option<u32>,
    /// Online items only — the real thumbnail URL (replaces the
    /// guessed img.youtube.com candidate list once known).
    pub cover_url: Option<String>,
    /// Tag/probe duration in seconds — the mini list's duration
    /// column and footer total.
    pub duration: Option<f64>,
}

/// Read tags for `tracks` on a worker thread; results arrive in order.
/// Tag reads can hit slow storage, so nothing runs on the UI thread.
pub fn spawn_track_info(
    tracks: Vec<PathBuf>,
    album_title: String,
) -> mpsc::Receiver<Vec<TrackInfo>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("aes-track-info".into())
        .spawn(move || {
            let infos = tracks
                .iter()
                .map(|p| read_track_info(p, &album_title))
                .collect();
            let _ = tx.send(infos);
        })
        .expect("track info thread");
    rx
}

/// Fetch metadata for each path on a small worker pool — every
/// `(path, info)` pair is sent as it lands so labels trickle in
/// instead of waiting for the slowest fetch (online metadata costs
/// seconds per item; a serial pass over a playlist took a minute+
/// before anything updated). Works for both file paths and urls.
pub fn spawn_url_info(
    paths: Vec<PathBuf>,
    album_title: String,
) -> mpsc::Receiver<Vec<(PathBuf, TrackInfo)>> {
    let (tx, rx) = mpsc::channel();
    const WORKERS: usize = 8;
    let paths = std::sync::Arc::new(paths);
    let next = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for _ in 0..paths.len().min(WORKERS) {
        let (tx, paths, next, album_title) =
            (tx.clone(), paths.clone(), next.clone(), album_title.clone());
        std::thread::Builder::new()
            .name("aes-url-info".into())
            .spawn(move || loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(p) = paths.get(i) else {
                    break;
                };
                let info = read_track_info(p, &album_title);
                if tx.send(vec![(p.clone(), info)]).is_err() {
                    break;
                }
            })
            .expect("url info thread");
    }
    rx
}

/// Filename-stem fallback when a file has no (or partial) tags —
/// mirrors how FolderMediaItem displays untagged files. URL items
/// have no local tags: cheap online scrape instead (Lacrima's
/// MetadataScrapper → GetOnlineMetadataAsync).
fn read_track_info(path: &Path, album_title: &str) -> TrackInfo {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::tag::Accessor;
    if is_url(path) {
        let url = path.to_string_lossy().into_owned();
        let mut t = TrackInfo {
            title: crate::ytdlp::extract_video_id(&url).unwrap_or_else(|| url.clone()),
            artist: String::new(),
            album: album_title.to_string(),
            number: None,
            cover_url: None,
            duration: None,
        };
        // Resolved labels persist in the metadata cache — a hit means
        // no network at all (Lacrima's .meta store semantics).
        if let Some(c) = crate::meta_cache::load(&url) {
            if !c.title.is_empty() {
                t.title = c.title;
            }
            t.artist = c.artist;
            // An edited Album field overrides the folder/tile label.
            if !c.album.trim().is_empty() {
                t.album = c.album;
            }
            t.cover_url = c.thumb;
            return t;
        }
        if let Some(m) = crate::ytdlp::online_metadata(&url) {
            if !m.title.is_empty() {
                t.title = m.title;
            }
            t.artist = m.author;
            t.cover_url = m.thumb;
            t.duration = m.duration;
            if !t.title.is_empty() || !t.artist.is_empty() {
                crate::meta_cache::save(
                    &url,
                    &crate::meta_cache::CachedMeta {
                        title: t.title.clone(),
                        artist: t.artist.clone(),
                        thumb: t.cover_url.clone(),
                        ..Default::default()
                    },
                );
            }
        }
        return t;
    }
    let stem = || {
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
    };
    let Some(tagged) = lofty::read_from_path(path).ok() else {
        return TrackInfo {
            title: stem(),
            artist: String::new(),
            album: album_title.to_string(),
            number: filename_track_number(&stem()),
            cover_url: None,
            duration: None,
        };
    };
    let duration = tagged.properties().duration().as_secs_f64();
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let title = tag
        .and_then(|t| t.title().map(|s| s.into_owned()))
        .filter(|s| !s.trim().is_empty());
    let number = tag
        .and_then(|t| t.track())
        .or_else(|| filename_track_number(&stem()));
    TrackInfo {
        title: title.unwrap_or_else(stem),
        artist: tag
            .and_then(|t| t.artist().map(|s| s.into_owned()))
            .unwrap_or_default(),
        album: tag
            .and_then(|t| t.album().map(|s| s.into_owned()))
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| album_title.to_string()),
        number,
        cover_url: None,
        duration: Some(duration),
    }
}

/// Leading "07 - Name" / "07_Name" style track numbers in filenames.
fn filename_track_number(stem: &str) -> Option<u32> {
    let digits: String = stem.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let n: u32 = digits.parse().ok()?;
    (n > 0 && n < 1000).then_some(n)
}

/// Lacrima's `PlaceholderGenerator.GenerateMusicPlaceholder`: a light
/// radial gradient (#E0E0E0 -> #A0A0A0, center 0.5/0.4) with a dark
/// (#2D2D2D) double-eighth note — the default cover for items with
/// no art at all.
pub fn placeholder_image() -> egui::ColorImage {
    // Lacrima renders this at 480x400 (MetadataScrapper); the wider
    // aspect matters — tiles UniformToFill-crop its dark gradient edges
    // off the sides rather than showing them.
    const W: usize = 480;
    const H: usize = 400;
    /// Virtual units per pixel; the note geometry is authored on a
    /// 400x400 canvas.
    const KX: f32 = 400.0 / W as f32;
    const KY: f32 = 400.0 / H as f32;
    let mut pixels = vec![egui::Color32::WHITE; W * H];
    for y in 0..H {
        for x in 0..W {
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;
            // Radial gradient: origin (0.5, 0.5) like Avalonia's default
            // GradientOrigin, falloff eased so tile crops stay light.
            let dx = (fx / W as f32 - 0.5) / 0.5;
            let dy = (fy / H as f32 - 0.5) / 0.5;
            let t = ((dx * dx + dy * dy).sqrt() * 0.62).min(1.0);
            let g = 224.0 + (160.0 - 224.0) * t;
            // Note glyph coverage, 2x2 supersampled in virtual space.
            let mut cov = 0.0f32;
            for sy in 0..2 {
                for sx in 0..2 {
                    let u = (fx + sx as f32 * 0.5 - 0.25) * KX;
                    let v = (fy + sy as f32 * 0.5 - 0.25) * KY;
                    if note_contains(u, v) {
                        cov += 0.25;
                    }
                }
            }
            let v = g * (1.0 - cov) + 45.0 * cov;
            pixels[y * W + x] = egui::Color32::from_rgb(v as u8, v as u8, v as u8);
        }
    }
    egui::ColorImage::new([W, H], pixels)
}

/// Point-in-shape test for the placeholder's note glyph (400x400 space).
fn note_contains(u: f32, v: f32) -> bool {
    // Stems.
    if (165.0..180.0).contains(&u) && (110.0..280.0).contains(&v) {
        return true;
    }
    if (285.0..300.0).contains(&u) && (90.0..260.0).contains(&v) {
        return true;
    }
    // Note heads (ellipses).
    let in_ellipse = |cx: f32, cy: f32| {
        let dx = (u - cx) / 40.0;
        let dy = (v - cy) / 30.0;
        dx * dx + dy * dy <= 1.0
    };
    if in_ellipse(140.0, 290.0) || in_ellipse(260.0, 270.0) {
        return true;
    }
    // Beam: (165,110) (300,90) (300,140) (165,160) — even-odd ray cast.
    const P: [(f32, f32); 4] = [
        (165.0, 110.0),
        (300.0, 90.0),
        (300.0, 140.0),
        (165.0, 160.0),
    ];
    let mut inside = false;
    let mut j = P.len() - 1;
    for i in 0..P.len() {
        let (xi, yi) = P[i];
        let (xj, yj) = P[j];
        if (yi > v) != (yj > v) && u < (xj - xi) * (v - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

