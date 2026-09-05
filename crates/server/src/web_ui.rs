//! Embedded, optional browser player.
//!
//! The UI assets, paginated library API, and compatibility media endpoint
//! live together here so disabling `[web]` removes the entire browser-facing
//! surface without changing DLNA behavior.

use super::*;
use crate::web_caption::{caption_to_webvtt, BrowserCaptionError};
use rusty_dlna_protocol::{
    caption_format_for_extension, CaptionWebVttConversion, CompactStreamMetadata,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::Ordering;

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_CSS: &str = include_str!("../web/app.css");
const APP_JS: &str = include_str!("../web/app.js");
const API_JS: &str = include_str!("../web/api.js");
const CORE_JS: &str = include_str!("../web/core.js");
const LIBRARY_JS: &str = include_str!("../web/library.js");
const PLAYER_JS: &str = include_str!("../web/player.js");
const PREFERENCES_JS: &str = include_str!("../web/preferences.js");
const STORE_JS: &str = include_str!("../web/store.js");
const FAVICON_PNG: &[u8] = include_bytes!("../../../assets/icon-48.png");
const WEB_MEDIA_PREFIX: &str = "/web/media/";
const WEB_DOWNLOAD_PREFIX: &str = "/web/download/";
const WEB_ITEM_PREFIX: &str = "/api/web/item/";
const WEB_PREVIEW_MANIFEST_PREFIX: &str = "/api/web/preview/";
const WEB_PREVIEW_SHEET_PREFIX: &str = "/web/preview/";

#[cfg(test)]
type ItemSnapshotHook = (
    usize,
    i64,
    std::sync::Arc<std::sync::Barrier>,
    std::sync::Arc<std::sync::Barrier>,
);

#[cfg(test)]
static ITEM_SNAPSHOT_HOOK: std::sync::LazyLock<std::sync::Mutex<Option<ItemSnapshotHook>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
pub(crate) fn pause_item_snapshot_for_test(
    app: &App,
    detail_id: i64,
    reached: std::sync::Arc<std::sync::Barrier>,
    release: std::sync::Arc<std::sync::Barrier>,
) {
    *ITEM_SNAPSHOT_HOOK
        .lock()
        .unwrap_or_else(|error| error.into_inner()) =
        Some((app as *const App as usize, detail_id, reached, release));
}

#[cfg(test)]
fn pause_item_snapshot_if_requested(app: &App, detail_id: i64) {
    let app_identity = app as *const App as usize;
    let mut guard = ITEM_SNAPSHOT_HOOK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let matches = guard
        .as_ref()
        .is_some_and(|(expected_app, expected_id, _, _)| {
            *expected_app == app_identity && *expected_id == detail_id
        });
    let hook = if matches { guard.take() } else { None };
    drop(guard);
    if let Some((_, _, reached, release)) = hook {
        reached.wait();
        release.wait();
    }
}
const WEB_TRANSCODE_STATUS_PREFIX: &str = "/api/web/transcode/";
const DEFAULT_PAGE_SIZE: usize = 60;
const MAX_PAGE_SIZE: usize = 200;
const MAX_CONTINUE_IDS: usize = 100;
const MAX_HLS_RESOURCE_BYTES: u64 = 64 * 1024 * 1024;
const WEB_SCHEMA_VERSION: u8 = 2;
// Change when a browser API representation can differ without a catalog
// generation change. This keeps conditional requests from reusing capability
// or media metadata cached from an older rustyDLNA build.
const WEB_API_CACHE_REVISION: u8 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WebItemId(i64);

impl From<i64> for WebItemId {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

impl Serialize for WebItemId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

#[derive(Serialize)]
struct WebApiError<'a> {
    schema_version: u8,
    error: WebErrorBody<'a>,
}

#[derive(Serialize)]
struct WebErrorBody<'a> {
    code: &'a str,
    message: &'a str,
    recoverable: bool,
    action: Option<&'a str>,
}

#[derive(Clone, Serialize)]
struct WebCapabilities {
    transcoding: bool,
    captions: bool,
    resume: &'static str,
    queue: bool,
    media_session: bool,
    quality_profiles: Vec<WebQualityProfile>,
    encoding_presets: Vec<WebEncodingPreset>,
    video_outputs: Vec<WebVideoOutput>,
    ai_upscale: Option<WebAiUpscaleCapability>,
}

#[derive(Clone, Serialize)]
struct WebAiUpscaleCapability {
    label: &'static str,
    max_scale: u8,
    sdr_only: bool,
    bit_depth: u8,
    profiles: Vec<WebAiUpscaleEnvelope>,
}

#[derive(Clone, Serialize)]
struct WebAiUpscaleEnvelope {
    name: String,
    max_source_width: u32,
    max_source_height: u32,
    max_source_pixels_per_second: u64,
}

#[derive(Clone, Serialize)]
struct WebEncodingPreset {
    id: &'static str,
    label: &'static str,
    description: &'static str,
}

#[derive(Clone, Serialize)]
struct WebQualityProfile {
    id: &'static str,
    label: &'static str,
    max_width: u32,
    max_height: u32,
    max_fps: u32,
    h264_profile: &'static str,
    h264_level: &'static str,
    pixel_format: &'static str,
    max_video_kbps: u32,
    audio_kbps: u32,
    audio_layout: &'static str,
    expected_bandwidth_kbps: u32,
    automatic_fallback: bool,
}

#[derive(Clone, Serialize)]
struct WebVideoOutput {
    id: &'static str,
    label: &'static str,
    codec: &'static str,
    video_content_type: &'static str,
    mse_content_type: &'static str,
    dynamic_range: &'static str,
    bit_depth: u8,
    hdr_metadata_type: Option<&'static str>,
    color_gamut: Option<&'static str>,
    transfer_function: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BrowserVideoOutput {
    H264Sdr,
    HevcHdr10,
}

#[derive(Serialize)]
struct WebLibraryPage {
    schema_version: u8,
    generation: u32,
    server_name: String,
    root_folder_id: String,
    capabilities: WebCapabilities,
    library_state: &'static str,
    view: &'static str,
    folder: Option<WebFolderRef>,
    breadcrumbs: Vec<WebFolderRef>,
    offset: usize,
    limit: usize,
    total: usize,
    has_more: bool,
    query: String,
    sort: &'static str,
    entries: Vec<WebEntryDto>,
}

#[derive(Serialize)]
struct WebItemDetails {
    schema_version: u8,
    id: WebItemId,
    item: WebMediaItem,
    audio_tracks: Vec<WebAudioTrack>,
    chapters: Vec<WebChapter>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrickplaySidecarManifest {
    schema_version: u8,
    source_size: u64,
    source_mtime_ns: i64,
    duration_seconds: f64,
    interval_seconds: u32,
    frame_width: u32,
    frame_height: u32,
    columns: u32,
    rows: u32,
    frame_count: u32,
    asset_revision: String,
    #[serde(default, rename = "scale_divisor")]
    _scale_divisor: Option<u32>,
}

#[derive(Serialize)]
struct WebTrickplayManifest {
    schema_version: u8,
    item_id: WebItemId,
    available: bool,
    duration_seconds: f64,
    interval_seconds: u32,
    frame_width: u32,
    frame_height: u32,
    columns: u32,
    rows: u32,
    frame_count: u32,
    sheet_urls: Vec<String>,
}

#[derive(Serialize)]
struct WebTrickplayUnavailable {
    schema_version: u8,
    item_id: WebItemId,
    available: bool,
}

#[derive(Clone, Serialize)]
struct WebChapter {
    index: usize,
    title: String,
    start_seconds: f64,
    end_seconds: f64,
}

#[derive(Serialize)]
struct WebTranscodeStatus {
    schema_version: u8,
    item_id: WebItemId,
    request_id: Option<u64>,
    state: &'static str,
    retry_after_seconds: Option<u64>,
}

#[derive(Serialize)]
struct WebFolderRef {
    id: String,
    title: String,
}

#[derive(Serialize)]
#[serde(tag = "entry_type", rename_all = "snake_case")]
enum WebEntryDto {
    Folder {
        id: String,
        title: String,
        child_count: usize,
    },
    Media(Box<WebMediaItem>),
}

#[derive(Serialize)]
struct WebMovieCollection {
    id: String,
    title: String,
    sequence: u32,
}

#[derive(Serialize)]
struct WebMediaItem {
    id: WebItemId,
    title: String,
    file_name: String,
    collection: Option<WebMovieCollection>,
    kind: &'static str,
    mime: String,
    ext: String,
    date: Option<String>,
    duration: Option<String>,
    duration_seconds: Option<usize>,
    resolution: Option<String>,
    width: u32,
    height: u32,
    artist: Option<String>,
    album_artist: Option<String>,
    album: Option<String>,
    disc: Option<i64>,
    track: Option<i64>,
    about: Option<String>,
    plot: Option<String>,
    /// Kept in schema v2 for compatibility; video values are the full plot.
    summary: Option<String>,
    genre: Option<String>,
    creator: Option<String>,
    composer: Option<String>,
    bitrate: Option<i64>,
    channels: Option<i64>,
    sample_rate: Option<i64>,
    size_bytes: u64,
    container: String,
    video_codec: String,
    video_profile: Option<String>,
    video_level: Option<u32>,
    pixel_format: Option<String>,
    bit_depth: Option<u32>,
    frame_rate: Option<String>,
    video_timestamp_mode: Option<String>,
    video_repair_required: bool,
    audio_codec: String,
    audio_layout: Option<String>,
    codec_string: Option<String>,
    video_content_type: Option<String>,
    hdr: String,
    audio_tracks: Vec<WebAudioTrack>,
    default_audio_index: usize,
    captions: Vec<WebCaption>,
    chapters: Vec<WebChapter>,
    stream_metadata_complete: bool,
    art_url: Option<String>,
    preview_url: Option<String>,
    download_url: Option<String>,
    source_url: String,
    fallback_url: String,
    transcode_likely: bool,
    compatible_video_encoder: String,
    repair_video_encoder: String,
}

#[derive(Clone, Serialize)]
struct WebAudioTrack {
    index: usize,
    codec: String,
    content_type: Option<String>,
    channels: u32,
    language: Option<String>,
    title: Option<String>,
    default: bool,
}

#[derive(Serialize)]
struct WebCaption {
    index: u32,
    label: String,
    language: Option<String>,
    default: bool,
    source_format: String,
    browser_supported: bool,
    url: Option<String>,
}

fn web_capabilities(app: &App) -> WebCapabilities {
    let quality = |profile: rusty_dlna_transcode::BrowserQuality, label| WebQualityProfile {
        id: profile.id(),
        label,
        max_width: profile.max_width(),
        max_height: profile.max_height(),
        max_fps: profile.max_fps(),
        h264_profile: profile.h264_profile(),
        h264_level: profile.h264_level(),
        pixel_format: "yuv420p",
        max_video_kbps: profile.max_video_kbps(),
        audio_kbps: profile.audio_kbps(),
        audio_layout: "stereo",
        expected_bandwidth_kbps: profile.expected_bandwidth_kbps(),
        automatic_fallback: profile.automatic_fallback(),
    };
    let quality_profiles = vec![
        quality(
            rusty_dlna_transcode::BrowserQuality::Auto,
            "Auto · up to 4K",
        ),
        quality(
            rusty_dlna_transcode::BrowserQuality::UhdHigh,
            "4K High · 25 Mbps",
        ),
        quality(
            rusty_dlna_transcode::BrowserQuality::UhdOptimized,
            "4K Optimized · 16 Mbps",
        ),
        quality(
            rusty_dlna_transcode::BrowserQuality::FullHd,
            "1080p · 8 Mbps",
        ),
        quality(
            rusty_dlna_transcode::BrowserQuality::DataSaver,
            "720p · 3 Mbps",
        ),
        quality(
            rusty_dlna_transcode::BrowserQuality::Sd480,
            "480p · 1.5 Mbps",
        ),
        quality(
            rusty_dlna_transcode::BrowserQuality::Low360,
            "360p · 0.8 Mbps",
        ),
    ];
    let mut video_outputs = Vec::new();
    if app.cfg.transcode.enable {
        video_outputs.push(WebVideoOutput {
            id: "h264_sdr",
            label: "H.264 SDR",
            codec: "avc1.640033",
            video_content_type: "video/mp4; codecs=\"avc1.640033\"",
            mse_content_type: "video/mp4; codecs=\"avc1.640033,mp4a.40.2\"",
            dynamic_range: "sdr",
            bit_depth: 8,
            hdr_metadata_type: None,
            color_gamut: None,
            transfer_function: None,
        });
        if app.cfg.web.encoder == "h264_nvenc" {
            video_outputs.push(WebVideoOutput {
                id: "hevc_hdr10",
                label: "HEVC Main 10 · HDR10",
                codec: "hvc1.2.4.L153.B0",
                video_content_type: "video/mp4; codecs=\"hvc1.2.4.L153.B0\"",
                mse_content_type: "video/mp4; codecs=\"hvc1.2.4.L153.B0,mp4a.40.2\"",
                dynamic_range: "hdr",
                bit_depth: 10,
                // The browser HDR encode preserves PQ/BT.2020 signaling but
                // does not promise source mastering-display metadata.
                hdr_metadata_type: None,
                color_gamut: Some("rec2020"),
                transfer_function: Some("pq"),
            });
        }
    }
    WebCapabilities {
        transcoding: app.cfg.transcode.enable,
        captions: app.scan_cfg.subtitles,
        // Web progress is intentionally browser-local. It must not overwrite
        // the accountless DLNA/Kodi bookmark identity stored in the database.
        resume: "browser_local",
        queue: true,
        media_session: true,
        quality_profiles,
        encoding_presets: rusty_dlna_transcode::BrowserEncodingPreset::ALL
            .into_iter()
            .map(|preset| WebEncodingPreset {
                id: preset.id(),
                label: preset.label(),
                description: preset.description(),
            })
            .collect(),
        video_outputs,
        ai_upscale: (!app.ai_upscale_profiles.is_empty()).then(|| WebAiUpscaleCapability {
            label: "AI upscale",
            max_scale: 2,
            sdr_only: true,
            bit_depth: 8,
            profiles: app
                .ai_upscale_profiles
                .iter()
                .map(|profile| WebAiUpscaleEnvelope {
                    name: profile.name.clone(),
                    max_source_width: profile.max_source_width,
                    max_source_height: profile.max_source_height,
                    max_source_pixels_per_second: profile.max_source_pixels_per_second,
                })
                .collect(),
        }),
    }
}

fn api_error(
    status_code: u16,
    code: &'static str,
    message: &'static str,
    recoverable: bool,
    action: Option<&'static str>,
) -> HttpResponse {
    json_response_with_status(
        status_code,
        &WebApiError {
            schema_version: WEB_SCHEMA_VERSION,
            error: WebErrorBody {
                code,
                message,
                recoverable,
                action,
            },
        },
    )
}

pub(crate) fn transcode_stream_error(status_code: u16, code: &'static str) -> HttpResponse {
    match code {
        "transcode_busy" => {
            let mut response = api_error(
                status_code,
                code,
                "The server is preparing other media. Try again shortly.",
                true,
                Some("retry_media"),
            );
            response.set("Retry-After", "1");
            response
        }
        "transcode_cancelled" => api_error(
            status_code,
            code,
            "Preparing this title was cancelled.",
            true,
            Some("retry_media"),
        ),
        _ => api_error(
            status_code,
            "transcode_failed",
            "The server could not prepare this title.",
            true,
            Some("retry_media"),
        ),
    }
}

fn generation_json_response<T: Serialize>(
    req: &HttpRequest,
    generation: u32,
    value: &T,
) -> HttpResponse {
    let etag = format!("W/\"web-v{WEB_SCHEMA_VERSION}-r{WEB_API_CACHE_REVISION}-{generation}\"");
    if req.header("If-None-Match") == Some(etag.as_str()) {
        let mut response = HttpResponse::new(304, "Not Modified");
        response.set("ETag", etag);
        response.set("Cache-Control", "private, max-age=0, must-revalidate");
        return response;
    }
    let mut response = json_response_with_status_and_cache_control(
        200,
        value,
        "private, max-age=0, must-revalidate",
    );
    response.set("ETag", etag);
    response
}

pub(crate) fn presentation(app: &App) -> HttpResponse {
    if !app.cfg.web.enable {
        return html_response(status::status_html(app));
    }
    let mut response = html_response(INDEX_HTML.to_owned());
    response.set(
        "Content-Security-Policy",
        "default-src 'self'; img-src 'self' data:; media-src 'self' blob:; script-src 'self'; style-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
    );
    response.set("Referrer-Policy", "no-referrer");
    response
}

pub(crate) fn asset(app: &App, path: &str) -> HttpResponse {
    if !app.cfg.web.enable {
        return not_found();
    }
    if path == "/favicon.ico" {
        return bytes_response("image/png", FAVICON_PNG, "public, max-age=86400");
    }
    let (mime, body) = match path {
        "/web/app.css" => ("text/css; charset=utf-8", APP_CSS),
        "/web/app.js" => ("text/javascript; charset=utf-8", APP_JS),
        "/web/api.js" => ("text/javascript; charset=utf-8", API_JS),
        "/web/core.js" => ("text/javascript; charset=utf-8", CORE_JS),
        "/web/library.js" => ("text/javascript; charset=utf-8", LIBRARY_JS),
        "/web/player.js" => ("text/javascript; charset=utf-8", PLAYER_JS),
        "/web/preferences.js" => ("text/javascript; charset=utf-8", PREFERENCES_JS),
        "/web/store.js" => ("text/javascript; charset=utf-8", STORE_JS),
        _ => return not_found(),
    };
    bytes_response(mime, body.as_bytes(), "no-cache")
}

pub(crate) fn library(app: &App, req: &HttpRequest) -> HttpResponse {
    if !app.cfg.web.enable {
        return not_found();
    }
    let params = QueryParams::parse(&req.query);
    if params.get("view") == Some("continue") {
        return continue_library(app, req, &params);
    }
    if params.has_unknown(&[
        "view",
        "folder",
        "kind",
        "q",
        "offset",
        "limit",
        "generation",
        "sort",
    ]) {
        return api_error(
            400,
            "invalid_parameter",
            "The library request contains an unsupported parameter.",
            false,
            None,
        );
    }
    let view = match params.get("view").unwrap_or("folders") {
        "folders" => "folders",
        "library" => "library",
        _ => {
            return api_error(
                400,
                "invalid_view",
                "Choose either the folder or library view.",
                false,
                None,
            );
        }
    };
    let kind = match params.get("kind").unwrap_or("all") {
        "all" => "all",
        "video" => "video",
        "audio" => "audio",
        _ => {
            return api_error(
                400,
                "invalid_kind",
                "Choose all media, video, or audio.",
                false,
                None,
            );
        }
    };
    if view == "folders" && kind != "all" {
        return api_error(
            400,
            "invalid_kind",
            "Folder browsing does not accept a media-kind filter.",
            false,
            None,
        );
    }
    if view == "library" && params.get("folder").is_some() {
        return api_error(
            400,
            "invalid_folder",
            "A folder can only be used in folder view.",
            false,
            None,
        );
    }
    let offset = match params.optional_usize("offset") {
        Ok(value) => value.unwrap_or(0),
        Err(()) => {
            return api_error(
                400,
                "invalid_offset",
                "The page offset must be a non-negative whole number.",
                false,
                None,
            );
        }
    };
    let limit = match params.optional_usize("limit") {
        Ok(Some(value @ 1..=MAX_PAGE_SIZE)) => value,
        Ok(None) => DEFAULT_PAGE_SIZE,
        Ok(Some(_)) | Err(()) => {
            return api_error(
                400,
                "invalid_limit",
                "The page size must be between 1 and 200.",
                false,
                None,
            );
        }
    };
    let requested_generation = match params.optional_u32("generation") {
        Ok(value) => value,
        Err(()) => {
            return api_error(
                400,
                "invalid_generation",
                "The catalog generation must be a non-negative whole number.",
                false,
                None,
            );
        }
    };
    let initial_generation = app.update_id.load(Ordering::Acquire);
    if requested_generation.is_some_and(|requested| requested != initial_generation) {
        return api_error(
            409,
            "catalog_changed",
            "The library changed while this list was loading. Refresh the list and try again.",
            true,
            Some("retry_library"),
        );
    }
    let sort = match params.get("sort").unwrap_or("title") {
        "title" => "title",
        "date_desc" => "date_desc",
        "episode" => "episode",
        _ => {
            return api_error(
                400,
                "invalid_sort",
                "Choose title, recently added, or episode/track order.",
                false,
                None,
            );
        }
    };
    let query = params.get("q").unwrap_or("").trim();
    if query.len() > 256 {
        return api_error(
            400,
            "invalid_query",
            "Search text must be 256 bytes or fewer.",
            false,
            None,
        );
    }
    let normalized_query = query.to_lowercase();
    let root_folder_id = rusty_dlna_protocol::object_id::BROWSEDIR_ID.to_owned();
    let capabilities = web_capabilities(app);

    if view == "folders" {
        let catalog = read_recover(&app.catalog);
        let generation = app.update_id.load(Ordering::Acquire);
        if requested_generation.is_some_and(|requested| requested != generation) {
            return api_error(
                409,
                "catalog_changed",
                "The library changed while this list was loading. Refresh the list and try again.",
                true,
                Some("retry_library"),
            );
        }
        let folder_id = params
            .get("folder")
            .filter(|value| !value.is_empty())
            .unwrap_or(rusty_dlna_protocol::object_id::BROWSEDIR_ID);
        let Some(breadcrumbs) = physical_folder_chain(&catalog, folder_id) else {
            return api_error(
                404,
                "folder_missing",
                "That folder is no longer available.",
                true,
                Some("return_to_library"),
            );
        };
        let current = breadcrumbs
            .last()
            .expect("a valid physical folder chain is never empty");
        let mut entries = current
            .children
            .iter()
            .filter_map(|object_id| {
                if let Some(folder) = catalog.containers.get(object_id) {
                    return Some(WebEntry::Folder(folder));
                }
                catalog.items.get(object_id).and_then(|item| {
                    (item.mime.starts_with("video/") || item.mime.starts_with("audio/"))
                        .then_some(WebEntry::Media(item))
                })
            })
            .filter(|entry| normalized_query.is_empty() || entry.matches(&normalized_query))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.rank()
                .cmp(&right.rank())
                .then_with(|| left.sort_title().cmp(&right.sort_title()))
                .then_with(|| left.stable_id().cmp(right.stable_id()))
        });
        let total = entries.len();
        let page = entries
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|entry| match entry {
                WebEntry::Folder(folder) => {
                    let child_count = folder
                        .children
                        .iter()
                        .filter(|object_id| {
                            catalog.containers.contains_key(*object_id)
                                || catalog.items.get(*object_id).is_some_and(|item| {
                                    item.mime.starts_with("video/")
                                        || item.mime.starts_with("audio/")
                                })
                        })
                        .count();
                    WebEntryDto::Folder {
                        id: folder.object_id.clone(),
                        title: folder.title.clone(),
                        child_count,
                    }
                }
                WebEntry::Media(item) => WebEntryDto::Media(Box::new(media_dto(app, item))),
            })
            .collect::<Vec<_>>();
        let breadcrumb_dtos = breadcrumbs
            .iter()
            .enumerate()
            .map(|(index, folder)| WebFolderRef {
                id: folder.object_id.clone(),
                title: if index == 0 {
                    "Media".to_owned()
                } else {
                    folder.title.clone()
                },
            })
            .collect::<Vec<_>>();

        return generation_json_response(
            req,
            generation,
            &WebLibraryPage {
                schema_version: WEB_SCHEMA_VERSION,
                generation,
                server_name: app.cfg.friendly_name.clone(),
                root_folder_id,
                capabilities,
                library_state: if total == 0 { "empty" } else { "ready" },
                view: "folders",
                folder: Some(WebFolderRef {
                    id: current.object_id.clone(),
                    title: if current.object_id == rusty_dlna_protocol::object_id::BROWSEDIR_ID {
                        "Media".to_owned()
                    } else {
                        current.title.clone()
                    },
                }),
                breadcrumbs: breadcrumb_dtos,
                offset,
                limit,
                total,
                has_more: offset.saturating_add(page.len()) < total,
                query: query.to_owned(),
                sort,
                entries: page,
            },
        );
    }

    let db_kind = match kind {
        "video" => rusty_dlna_scan::WebMediaKind::Video,
        "audio" => rusty_dlna_scan::WebMediaKind::Audio,
        _ => rusty_dlna_scan::WebMediaKind::All,
    };
    let db_sort = match sort {
        "date_desc" => rusty_dlna_scan::WebMediaSort::DateDescending,
        "episode" => rusty_dlna_scan::WebMediaSort::EpisodeTrack,
        _ => rusty_dlna_scan::WebMediaSort::Title,
    };
    const MAX_GENERATION_ATTEMPTS: usize = 3;
    let mut consistent = None;
    for attempt in 0..MAX_GENERATION_ATTEMPTS {
        let db_page = match query_catalog_snapshot_once(app, || {
            query_db_web_media(
                app.db_pool.as_deref(),
                app.scan_cfg.db_path.as_deref(),
                db_kind,
                query,
                db_sort,
                offset,
                limit,
            )
        }) {
            Ok(page) => page,
            Err(()) => {
                if attempt + 1 < MAX_GENERATION_ATTEMPTS {
                    std::thread::yield_now();
                    continue;
                }
                return api_error(
                    409,
                    "catalog_changed",
                    "The library changed while this list was loading. Refresh the list and try again.",
                    true,
                    Some("retry_library"),
                );
            }
        };
        let catalog = read_recover(&app.catalog);
        let generation = app.update_id.load(Ordering::Acquire);
        if db_page
            .as_ref()
            .is_some_and(|snapshot| snapshot.generation != generation)
        {
            drop(catalog);
            if attempt + 1 < MAX_GENERATION_ATTEMPTS {
                std::thread::yield_now();
                continue;
            }
            return api_error(
                409,
                "catalog_changed",
                "The library changed while this list was loading. Refresh the list and try again.",
                true,
                Some("retry_library"),
            );
        }
        consistent = Some((db_page, catalog, generation));
        break;
    }
    let Some((db_page, catalog, generation)) = consistent else {
        return api_error(
            503,
            "catalog_busy",
            "The library is being updated. Try again in a moment.",
            true,
            Some("retry_library"),
        );
    };
    if requested_generation.is_some_and(|requested| requested != generation) {
        return api_error(
            409,
            "catalog_changed",
            "The library changed while this list was loading. Refresh the list and try again.",
            true,
            Some("retry_library"),
        );
    }
    let db_materialized = db_page.as_ref().and_then(|snapshot| {
        materialize_db_page(&catalog, &snapshot.page).map(|entries| {
            let items = entries
                .into_iter()
                .filter_map(|entry| match entry {
                    CatalogChildRef::Item(item) => Some(item),
                    CatalogChildRef::Container(_) => None,
                })
                .collect::<Vec<_>>();
            (items, snapshot.page.total as usize)
        })
    });
    let (items, total) = if let Some(page) = db_materialized {
        page
    } else {
        memory_web_page(&catalog, kind, &normalized_query, sort, offset, limit)
    };
    let entries = items
        .into_iter()
        .map(|item| WebEntryDto::Media(Box::new(media_dto(app, item))))
        .collect::<Vec<_>>();
    generation_json_response(
        req,
        generation,
        &WebLibraryPage {
            schema_version: WEB_SCHEMA_VERSION,
            generation,
            server_name: app.cfg.friendly_name.clone(),
            root_folder_id,
            capabilities,
            library_state: if total == 0 { "empty" } else { "ready" },
            view: "library",
            folder: None,
            breadcrumbs: Vec::new(),
            offset,
            limit,
            total,
            has_more: offset.saturating_add(entries.len()) < total,
            query: query.to_owned(),
            sort,
            entries,
        },
    )
}

fn continue_library(app: &App, req: &HttpRequest, params: &QueryParams) -> HttpResponse {
    if params.has_unknown(&["view", "ids", "generation"]) {
        return api_error(
            400,
            "invalid_parameter",
            "The Continue Watching request contains an unsupported parameter.",
            false,
            None,
        );
    }
    let ids = match continue_detail_ids(params.get("ids").unwrap_or("")) {
        Ok(ids) => ids,
        Err(()) => {
            return api_error(
                400,
                "invalid_ids",
                "Continue Watching accepts up to 100 distinct media item IDs.",
                false,
                None,
            );
        }
    };
    let requested_generation = match params.optional_u32("generation") {
        Ok(value) => value,
        Err(()) => {
            return api_error(
                400,
                "invalid_generation",
                "The catalog generation must be a non-negative whole number.",
                false,
                None,
            );
        }
    };
    // Catalog publication updates the in-memory catalog and generation while
    // holding this lock. Sample the generation only after acquiring it so a
    // response can never label newer entries with an older generation.
    let catalog = read_recover(&app.catalog);
    let generation = app.update_id.load(Ordering::Acquire);
    if requested_generation.is_some_and(|requested| requested != generation) {
        return api_error(
            409,
            "catalog_changed",
            "The library changed while Continue Watching was loading. Refresh the list and try again.",
            true,
            Some("retry_library"),
        );
    }
    // These canonical virtual containers own every browser-playable audio and
    // video detail, so their child lists provide an O(1) state check for each
    // batch without walking a large catalog.
    let library_has_media = [
        rusty_dlna_protocol::object_id::VIDEO_ALL_ID,
        rusty_dlna_protocol::object_id::MUSIC_ALL_ID,
    ]
    .iter()
    .any(|id| {
        catalog
            .containers
            .get(*id)
            .is_some_and(|container| !container.children.is_empty())
    });
    let entries = ids
        .iter()
        .filter_map(|id| catalog.get_item_by_detail(*id))
        .filter(|item| item.mime.starts_with("video/") || item.mime.starts_with("audio/"))
        .map(|item| WebEntryDto::Media(Box::new(media_dto(app, item))))
        .collect::<Vec<_>>();
    generation_json_response(
        req,
        generation,
        &WebLibraryPage {
            schema_version: WEB_SCHEMA_VERSION,
            generation,
            server_name: app.cfg.friendly_name.clone(),
            root_folder_id: rusty_dlna_protocol::object_id::BROWSEDIR_ID.to_owned(),
            capabilities: web_capabilities(app),
            library_state: if library_has_media { "ready" } else { "empty" },
            view: "continue",
            folder: None,
            breadcrumbs: Vec::new(),
            offset: 0,
            limit: ids.len(),
            total: entries.len(),
            has_more: false,
            query: String::new(),
            sort: "recent_progress",
            entries,
        },
    )
}

fn continue_detail_ids(value: &str) -> Result<Vec<i64>, ()> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    for value in value.split(',') {
        if ids.len() >= MAX_CONTINUE_IDS {
            return Err(());
        }
        let id = decimal_id(value).ok_or(())?;
        if !seen.insert(id) {
            return Err(());
        }
        ids.push(id);
    }
    Ok(ids)
}

fn memory_web_page<'a>(
    catalog: &'a Catalog,
    kind: &str,
    query: &str,
    sort: &str,
    offset: usize,
    limit: usize,
) -> (Vec<&'a MediaItem>, usize) {
    let mut items = catalog
        .by_detail
        .values()
        .filter_map(|object_id| catalog.items.get(object_id))
        .filter(|item| {
            (item.mime.starts_with("video/") || item.mime.starts_with("audio/"))
                && match kind {
                    "video" => item.mime.starts_with("video/"),
                    "audio" => item.mime.starts_with("audio/"),
                    _ => true,
                }
        })
        .filter(|item| query.is_empty() || media_matches(item, query))
        .collect::<Vec<_>>();
    // Match SQLite's MIN(detail_id) representative before ordering, even when
    // a symlink alias has a different NFO title that would otherwise sort first.
    items.sort_unstable_by_key(|item| item.detail_id);
    let mut physical_files = HashSet::new();
    items.retain(|item| item.inode == 0 || physical_files.insert((item.device, item.inode)));
    if sort == "title" {
        items.sort_by_cached_key(|item| {
            (
                rusty_dlna_scan::web_media_title_key(
                    item.collection_path.as_deref().unwrap_or(&item.path),
                    &item.mime,
                    &item.title,
                ),
                item.detail_id,
            )
        });
    } else {
        items.sort_by(|left, right| {
            let ordering = match sort {
                "date_desc" => right.date.cmp(&left.date),
                _ => left
                    .disc
                    .unwrap_or(0)
                    .cmp(&right.disc.unwrap_or(0))
                    .then_with(|| left.track.unwrap_or(0).cmp(&right.track.unwrap_or(0))),
            };
            ordering.then_with(|| left.detail_id.cmp(&right.detail_id))
        });
    }
    let total = items.len();
    (items.into_iter().skip(offset).take(limit).collect(), total)
}

enum WebEntry<'a> {
    Folder(&'a CatalogContainer),
    Media(&'a MediaItem),
}

impl WebEntry<'_> {
    fn rank(&self) -> u8 {
        match self {
            Self::Folder(_) => 0,
            Self::Media(_) => 1,
        }
    }

    fn sort_title(&self) -> String {
        match self {
            Self::Folder(folder) => folder.title.to_lowercase(),
            Self::Media(item) => media_file_name(item).to_lowercase(),
        }
    }

    fn stable_id(&self) -> &str {
        match self {
            Self::Folder(folder) => &folder.object_id,
            Self::Media(item) => &item.object_id,
        }
    }

    fn matches(&self, query: &str) -> bool {
        match self {
            Self::Folder(folder) => folder.title.to_lowercase().contains(query),
            Self::Media(item) => media_matches(item, query),
        }
    }
}

fn physical_folder_chain<'a>(
    catalog: &'a Catalog,
    folder_id: &str,
) -> Option<Vec<&'a CatalogContainer>> {
    let root = rusty_dlna_protocol::object_id::BROWSEDIR_ID;
    let mut current_id = folder_id.to_owned();
    let mut reversed = Vec::new();
    for _ in 0..=catalog.containers.len() {
        let current = catalog.containers.get(&current_id)?;
        reversed.push(current);
        if current.object_id == root {
            reversed.reverse();
            return Some(reversed);
        }
        if current.parent_id == current.object_id {
            return None;
        }
        current_id.clone_from(&current.parent_id);
    }
    None
}

fn media_matches(item: &MediaItem, query: &str) -> bool {
    if media_file_name(item).to_lowercase().contains(query) {
        return true;
    }
    [
        Some(item.title.as_str()),
        item.artist.as_deref(),
        item.album_artist.as_deref(),
        item.album.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| value.to_lowercase().contains(query))
}

fn media_file_name(item: &MediaItem) -> String {
    item.path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| item.title.clone())
}

fn stored_audio_tracks(
    metadata: Option<CompactStreamMetadata<'_>>,
    fallback_audio_csv: &str,
    fallback_channels: u32,
) -> Vec<WebAudioTrack> {
    let mut tracks = metadata
        .into_iter()
        .flat_map(CompactStreamMetadata::audio_records)
        .filter_map(|record| {
            let channels = record.channels?;
            Some(WebAudioTrack {
                index: record.audio_index,
                codec: record.codec.to_owned(),
                content_type: browser_audio_content_type(record.codec),
                channels,
                language: record
                    .decoded_language()
                    .ok()
                    .flatten()
                    .map(|value| value.into_owned()),
                title: record
                    .decoded_title()
                    .ok()
                    .flatten()
                    .map(|value| value.into_owned()),
                default: record.default,
            })
        })
        .collect::<Vec<_>>();
    if tracks.is_empty() {
        tracks.extend(
            fallback_audio_csv
                .split(',')
                .map(str::trim)
                .filter(|codec| !codec.is_empty())
                .enumerate()
                .map(|(index, codec)| WebAudioTrack {
                    index,
                    codec: codec.to_owned(),
                    content_type: browser_audio_content_type(codec),
                    channels: fallback_channels,
                    language: None,
                    title: None,
                    default: index == 0,
                }),
        );
    }
    tracks
}

fn stored_chapters(metadata: Option<CompactStreamMetadata<'_>>) -> Vec<WebChapter> {
    metadata
        .into_iter()
        .flat_map(CompactStreamMetadata::chapters)
        .map(|chapter| WebChapter {
            index: chapter.source_index,
            title: chapter
                .title
                .map(|title| title.into_owned())
                .unwrap_or_else(|| format!("Chapter {}", chapter.source_index + 1)),
            start_seconds: chapter.start_millis as f64 / 1000.0,
            end_seconds: chapter.end_millis as f64 / 1000.0,
        })
        .collect()
}

fn stream_metadata_complete(
    metadata: Option<CompactStreamMetadata<'_>>,
    mime: &str,
    container: &str,
    video_codec: &str,
) -> bool {
    let Some(metadata) = metadata else {
        return false;
    };
    let needs_timestamp_check = mime.starts_with("video/")
        && matches!(container, "mp4" | "mov")
        && matches!(video_codec, "h264" | "hevc");
    metadata.has_video_capabilities_marker()
        && (!needs_timestamp_check || metadata.has_timestamp_marker())
}

fn default_audio_index(
    tracks: &[WebAudioTrack],
    metadata: Option<CompactStreamMetadata<'_>>,
    fallback_audio_csv: &str,
) -> usize {
    tracks
        .iter()
        .find(|track| track.default && is_english_audio_language(track.language.as_deref()))
        .or_else(|| {
            tracks
                .iter()
                .find(|track| is_english_audio_language(track.language.as_deref()))
        })
        .or_else(|| tracks.iter().find(|track| track.default))
        .map(|track| track.index)
        .unwrap_or_else(|| match metadata {
            Some(metadata) => {
                rusty_dlna_transcode::pick_audio_index_from_metadata(metadata, fallback_audio_csv)
            }
            None => rusty_dlna_transcode::pick_audio_index(fallback_audio_csv),
        })
}

fn is_english_audio_language(language: Option<&str>) -> bool {
    let Some(language) = language
        .map(str::trim)
        .filter(|language| !language.is_empty())
    else {
        return false;
    };
    language.eq_ignore_ascii_case("english")
        || language.split(['-', '_']).next().is_some_and(|primary| {
            primary.eq_ignore_ascii_case("en") || primary.eq_ignore_ascii_case("eng")
        })
}

fn original_audio_index(tracks: &[WebAudioTrack]) -> usize {
    tracks
        .iter()
        .find(|track| track.default)
        .or_else(|| tracks.first())
        .map(|track| track.index)
        .unwrap_or(0)
}

fn browser_direct_codec_string(
    item: &MediaItem,
    tracks: &[WebAudioTrack],
    original_audio_index: usize,
) -> Option<String> {
    if !item.mime.starts_with("video/") {
        return (!item.probe.codec_string.is_empty()).then(|| item.probe.codec_string.clone());
    }
    // The scanner can retain an AAC RFC 6381 value even when the video codec
    // has no exact browser capability string. Do not promote that first audio
    // value (or a raw internal codec name) into the video's position: a query
    // such as `mp4a.40.2,mp4a.40.2` makes browsers answer for audio while the
    // unsupported video stream goes untested.
    let video = browser_stored_video_codec_string(&item.probe.video, &item.probe.codec_string)?;
    let audio = tracks
        .iter()
        .find(|track| track.index == original_audio_index)
        .map(|track| {
            browser_audio_codec_string(&track.codec)
                .map(str::to_owned)
                .unwrap_or_else(|| track.codec.clone())
        });
    let codecs = std::iter::once(video.to_owned())
        .chain(audio)
        .collect::<Vec<_>>();
    Some(codecs.join(","))
}

fn browser_stored_video_codec_string<'a>(video: &str, codec_string: &'a str) -> Option<&'a str> {
    let video = video.split(',').next()?.trim().to_ascii_lowercase();
    let stored = codec_string.split(',').next()?.trim();
    let expected_prefix = match video.as_str() {
        "h264" => "avc1.",
        "hevc" => {
            if stored.starts_with("hvc1.") {
                "hvc1."
            } else {
                "hev1."
            }
        }
        "mpeg4" => "mp4v.",
        "vp9" => "vp09.",
        "av1" => "av01.",
        _ => return None,
    };
    stored.starts_with(expected_prefix).then_some(stored)
}

fn caption_language(media_path: &Path, caption_path: &Path) -> Option<String> {
    rusty_dlna_scan::caption_language_for_media(caption_path, media_path)
}

fn browser_caption_conversion(extension: &str) -> Option<CaptionWebVttConversion> {
    caption_format_for_extension(extension)?.webvtt_conversion
}

fn browser_caption_url(detail_id: i64, index: u32, extension: &str) -> Option<String> {
    browser_caption_conversion(extension)
        .map(|_| format!("/Captions/{detail_id}/{index}.vtt?format=webvtt"))
}

fn caption_label(index: u32, language: Option<&str>) -> String {
    let friendly = match language {
        Some("en") | Some("eng") => Some("English"),
        Some("es") | Some("spa") => Some("Spanish"),
        Some("fr") | Some("fra") | Some("fre") => Some("French"),
        Some("de") | Some("deu") | Some("ger") => Some("German"),
        Some("it") | Some("ita") => Some("Italian"),
        Some("ja") | Some("jpn") => Some("Japanese"),
        Some("ko") | Some("kor") => Some("Korean"),
        Some("zh") | Some("zho") | Some("chi") => Some("Chinese"),
        _ => None,
    };
    friendly
        .map(str::to_owned)
        .or_else(|| language.map(str::to_uppercase))
        .unwrap_or_else(|| format!("Caption {}", index + 1))
}

fn caption_dtos(item: &MediaItem) -> Vec<WebCaption> {
    item.captions
        .iter()
        .map(|caption| {
            let language = caption_language(&item.path, &caption.path);
            let url = browser_caption_url(item.detail_id, caption.index, &caption.ext);
            WebCaption {
                index: caption.index,
                label: caption_label(caption.index, language.as_deref()),
                language,
                // Sidecar filenames do not carry a standards-defined default
                // disposition. Keep captions opt-in unless that fact is
                // persisted explicitly in a future schema.
                default: false,
                source_format: caption.ext.clone(),
                browser_supported: url.is_some(),
                url,
            }
        })
        .collect()
}

fn media_dto(app: &App, item: &MediaItem) -> WebMediaItem {
    let media_kind = if item.mime.starts_with("video/") {
        "video"
    } else {
        "audio"
    };
    let art_url = if item.album_art > 0 {
        Some(format!(
            "/AlbumArt/{}-{}.jpg",
            item.album_art, item.detail_id
        ))
    } else if media_kind == "video" && app.cfg.thumbnails {
        Some(format!("/Thumbnails/{}.jpg", item.detail_id))
    } else {
        None
    };
    let source = probe_to_source(
        &item.probe.container,
        &item.probe.video,
        &item.probe.hdr,
        &item.probe.audio,
        item.probe.width,
        item.probe.height,
    );
    let stream_metadata = CompactStreamMetadata::parse(&item.probe.audio_streams).ok();
    let fallback_channels = u32::try_from(item.channels.unwrap_or(0).max(0)).unwrap_or(0);
    let audio_tracks = stored_audio_tracks(stream_metadata, &item.probe.audio, fallback_channels);
    let default_audio_index =
        default_audio_index(&audio_tracks, stream_metadata, &item.probe.audio);
    let direct_codec_string =
        browser_direct_codec_string(item, &audio_tracks, original_audio_index(&audio_tracks));
    let file_name = media_file_name(item);
    let video_content_type = browser_video_content_type(item, &source);
    let compatible_video_encoder = if media_kind == "video" {
        rusty_dlna_transcode::browser_video_encoder(&app.cfg.web.encoder).to_owned()
    } else {
        "none".to_owned()
    };
    let video_repair_required = item.probe.video_timestamp_mode == "broken-reordered";
    let repair_video_encoder = if media_kind == "video" && video_repair_required {
        rusty_dlna_transcode::browser_repair_video_encoder(
            &app.cfg.web.encoder,
            source.video_codec,
            source.hdr,
            item.probe.bit_depth,
        )
        .to_owned()
    } else {
        compatible_video_encoder.clone()
    };
    WebMediaItem {
        id: item.detail_id.into(),
        title: item.title.clone(),
        file_name,
        collection: rusty_dlna_scan::video_collection(
            item.collection_path.as_deref().unwrap_or(&item.path),
            &item.mime,
        )
        .map(|group| WebMovieCollection {
            id: group.id,
            title: group.title,
            sequence: group.sequence,
        }),
        kind: media_kind,
        mime: item.mime.clone(),
        ext: item.ext.clone(),
        date: (!item.date.is_empty()).then(|| item.date.clone()),
        duration: item.duration.clone(),
        duration_seconds: item.duration.as_deref().and_then(media_duration_seconds),
        resolution: item.resolution.clone(),
        width: item.probe.width,
        height: item.probe.height,
        artist: item.artist.clone(),
        album_artist: item.album_artist.clone(),
        album: item.album.clone(),
        disc: item.disc,
        track: item.track,
        about: item.about.clone(),
        plot: item.plot.clone(),
        summary: if media_kind == "video" {
            item.plot.clone()
        } else {
            item.about.clone()
        },
        genre: item.genre.clone(),
        creator: item.creator.clone(),
        composer: item.composer.clone(),
        bitrate: item.bitrate,
        channels: item.channels,
        sample_rate: item.samplerate,
        size_bytes: item.size,
        container: item.probe.container.clone(),
        video_codec: item.probe.video.clone(),
        video_profile: (!item.probe.video_profile.is_empty())
            .then(|| item.probe.video_profile.clone()),
        video_level: (item.probe.video_level > 0).then_some(item.probe.video_level),
        pixel_format: (!item.probe.pixel_format.is_empty())
            .then(|| item.probe.pixel_format.clone()),
        bit_depth: (item.probe.bit_depth > 0).then_some(item.probe.bit_depth),
        frame_rate: (!item.probe.frame_rate.is_empty()).then(|| item.probe.frame_rate.clone()),
        video_timestamp_mode: (!item.probe.video_timestamp_mode.is_empty())
            .then(|| item.probe.video_timestamp_mode.clone()),
        video_repair_required,
        audio_codec: item.probe.audio.clone(),
        audio_layout: (!item.probe.audio_layout.is_empty())
            .then(|| item.probe.audio_layout.clone()),
        codec_string: direct_codec_string,
        video_content_type,
        hdr: item.probe.hdr.clone(),
        audio_tracks,
        default_audio_index,
        captions: caption_dtos(item),
        chapters: stored_chapters(stream_metadata),
        stream_metadata_complete: stream_metadata_complete(
            stream_metadata,
            &item.mime,
            &item.probe.container,
            &item.probe.video,
        ),
        art_url,
        preview_url: (media_kind == "video")
            .then(|| format!("{WEB_PREVIEW_MANIFEST_PREFIX}{}", item.detail_id)),
        download_url: (media_kind == "video")
            .then(|| format!("{WEB_DOWNLOAD_PREFIX}{}", item.detail_id)),
        source_url: format!("{WEB_MEDIA_PREFIX}{}.mp4?mode=direct", item.detail_id),
        fallback_url: format!("{WEB_MEDIA_PREFIX}{}.mp4", item.detail_id),
        transcode_likely: !likely_browser_native(item, &source),
        compatible_video_encoder,
        repair_video_encoder,
    }
}

pub(crate) fn item(app: &App, req: &HttpRequest) -> HttpResponse {
    if !app.cfg.web.enable {
        return not_found();
    }
    let params = QueryParams::parse(&req.query);
    if params.has_unknown(&["enrich"]) {
        return api_error(
            400,
            "invalid_parameter",
            "The item request contains an unsupported parameter.",
            false,
            None,
        );
    }
    let enrich = match params.get("enrich") {
        None | Some("0") => false,
        Some("1") => true,
        Some(_) => {
            return api_error(
                400,
                "invalid_enrichment",
                "Item enrichment must be either 0 or 1.",
                false,
                None,
            )
        }
    };
    let Some(id) = web_item_id(&req.path) else {
        return api_error(
            400,
            "invalid_item",
            "The media item ID is invalid.",
            false,
            None,
        );
    };
    let catalog = read_recover(&app.catalog);
    let Some(item) = catalog.get_item_by_detail(id).cloned() else {
        return api_error(
            404,
            "media_missing",
            "This title is no longer in the library.",
            true,
            Some("return_to_library"),
        );
    };
    #[cfg(test)]
    pause_item_snapshot_if_requested(app, id);
    let generation = app.update_id.load(Ordering::Acquire);
    drop(catalog);
    let etag = format!(
        "W/\"web-v{WEB_SCHEMA_VERSION}-r{WEB_API_CACHE_REVISION}-{generation}-item-{id}{}\"",
        if enrich { "-enriched" } else { "" }
    );
    if req.header("If-None-Match") == Some(etag.as_str()) {
        let mut response = HttpResponse::new(304, "Not Modified");
        response.set("ETag", etag);
        response.set("Cache-Control", "private, max-age=0, must-revalidate");
        return response;
    }
    if !(item.mime.starts_with("video/") || item.mime.starts_with("audio/")) {
        return api_error(
            415,
            "unsupported_media",
            "This library item cannot be played in the browser.",
            false,
            Some("return_to_library"),
        );
    }
    let source_path = rusty_dlna_scan::rebase_media_path_for_config(&item.path, &app.scan_cfg);
    let opened = match rusty_dlna_scan::open_allowed_file(&source_path, &app.scan_cfg) {
        Ok(opened) => opened,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return api_error(
                403,
                "media_missing",
                "The media file is not available.",
                false,
                Some("return_to_library"),
            );
        }
        Err(_) => {
            return api_error(
                404,
                "media_missing",
                "The media file is not available.",
                true,
                Some("retry_item"),
            );
        }
    };
    let mut item_dto = media_dto(app, &item);
    let mut audio_tracks = item_dto.audio_tracks.clone();
    let mut chapters = item_dto.chapters.clone();
    if enrich {
        let _helper_permit = match app.helpers.acquire_timeout_cancelled(
            Duration::from_secs(app.cfg.helper_queue_timeout_secs),
            &app.scan_cfg.cancellation,
        ) {
            Ok(permit) => permit,
            Err(error) => {
                tracing::warn!(%error, detail_id = id, "web stream probe admission rejected");
                let mut response = api_error(
                    503,
                    "transcode_busy",
                    "The server is preparing other media. Try again shortly.",
                    true,
                    Some("retry_item"),
                );
                response.set("Retry-After", "1");
                return response;
            }
        };
        let Some(probe) = rusty_dlna_scan::probe::probe_media_with_cancellation(
            &opened.proc_path(),
            Duration::from_secs(app.cfg.scan_command_timeout_secs.min(10)),
            &app.scan_cfg.cancellation,
        ) else {
            return api_error(
                422,
                "probe_failed",
                "Audio-track details could not be loaded.",
                true,
                Some("retry_item"),
            );
        };
        let mut enriched = item.clone();
        enriched.probe = probe.probe;
        enriched.duration = probe.av.duration.or(enriched.duration);
        enriched.bitrate = probe.av.bitrate.or(enriched.bitrate);
        enriched.resolution = probe.av.resolution.or(enriched.resolution);
        enriched.channels = probe.av.channels.or(enriched.channels);
        enriched.samplerate = probe.av.samplerate.or(enriched.samplerate);
        item_dto = media_dto(app, &enriched);
        item_dto.stream_metadata_complete = true;
        audio_tracks.clone_from(&item_dto.audio_tracks);
        chapters.clone_from(&item_dto.chapters);
    }
    let mut response = json_response_with_status_and_cache_control(
        200,
        &WebItemDetails {
            schema_version: WEB_SCHEMA_VERSION,
            id: id.into(),
            item: item_dto,
            audio_tracks,
            chapters,
        },
        "private, max-age=0, must-revalidate",
    );
    response.set("ETag", etag);
    response
}

enum TrickplayRequest<'a> {
    Manifest(i64),
    Sheet {
        item_id: i64,
        revision: &'a str,
        index: u32,
    },
}

fn decimal_id(value: &str) -> Option<i64> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse::<i64>().ok())
        .flatten()
        .filter(|id| *id > 0 && id.to_string() == value)
}

fn trickplay_request(path: &str) -> Option<TrickplayRequest<'_>> {
    if let Some(value) = path.strip_prefix(WEB_PREVIEW_MANIFEST_PREFIX) {
        return decimal_id(value).map(TrickplayRequest::Manifest);
    }
    let value = path.strip_prefix(WEB_PREVIEW_SHEET_PREFIX)?;
    let mut parts = value.split('/');
    let item_id = decimal_id(parts.next()?)?;
    let revision = parts.next()?;
    let index = parts.next()?.strip_suffix(".jpg")?.parse::<u32>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(TrickplayRequest::Sheet {
        item_id,
        revision,
        index,
    })
}

fn read_bounded(mut file: std::fs::File, limit: u64) -> std::io::Result<Vec<u8>> {
    let length = file.metadata()?.len();
    if length == 0 || length > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "generated preview file has an invalid size",
        ));
    }
    let capacity = usize::try_from(length).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "generated preview file is too large",
        )
    })?;
    let bytes = match rusty_dlna_helper::read_to_end_bounded(&mut file, capacity) {
        Ok(bytes) => bytes,
        Err(rusty_dlna_helper::BoundedReadError::LimitExceeded { .. }) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "generated preview file grew while it was read",
            ));
        }
        Err(rusty_dlna_helper::BoundedReadError::Io(error)) => return Err(error),
    };
    if bytes.len() != capacity {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "generated preview file changed while it was read",
        ));
    }
    Ok(bytes)
}

fn metadata_mtime_ns(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .mtime()
        .checked_mul(1_000_000_000)?
        .checked_add(metadata.mtime_nsec())
}

fn valid_asset_revision(value: &str) -> bool {
    value.len() == rusty_dlna_protocol::TRICKPLAY_ASSET_REVISION_HEX_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_trickplay_manifest(
    manifest: &TrickplaySidecarManifest,
    source: &std::fs::Metadata,
) -> Option<u32> {
    let interval = rusty_dlna_protocol::trickplay_interval_seconds_for_layout(
        manifest.duration_seconds,
        manifest.columns,
        manifest.rows,
    )?;
    let frame_count = rusty_dlna_protocol::trickplay_frame_count(
        manifest.duration_seconds,
        manifest.interval_seconds,
    )?;
    let sheet_count = rusty_dlna_protocol::trickplay_sheet_count_for_layout(
        frame_count,
        manifest.columns,
        manifest.rows,
    )?;
    (manifest.schema_version == rusty_dlna_protocol::TRICKPLAY_SCHEMA_VERSION
        && manifest.source_size == source.len()
        && Some(manifest.source_mtime_ns) == metadata_mtime_ns(source)
        && manifest.interval_seconds == interval
        && rusty_dlna_protocol::trickplay_layout_is_valid(
            manifest.frame_width,
            manifest.frame_height,
            manifest.columns,
            manifest.rows,
        )
        && manifest.frame_count == frame_count
        && valid_asset_revision(&manifest.asset_revision))
    .then_some(sheet_count)
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
        return None;
    }
    let mut offset = 2usize;
    while offset < bytes.len() {
        while bytes.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = *bytes.get(offset)?;
        offset += 1;
        if marker == 0xd9 || marker == 0xda {
            break;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let length = usize::from(u16::from_be_bytes([
            *bytes.get(offset)?,
            *bytes.get(offset + 1)?,
        ]));
        if length < 2 || offset.checked_add(length)? > bytes.len() {
            return None;
        }
        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) {
            if length < 7 {
                return None;
            }
            let height = u32::from(u16::from_be_bytes([
                *bytes.get(offset + 3)?,
                *bytes.get(offset + 4)?,
            ]));
            let width = u32::from(u16::from_be_bytes([
                *bytes.get(offset + 5)?,
                *bytes.get(offset + 6)?,
            ]));
            return (width > 0 && height > 0).then_some((width, height));
        }
        offset += length;
    }
    None
}

fn preview_unavailable(item_id: i64, manifest_request: bool) -> HttpResponse {
    if manifest_request {
        return json_response_with_status_and_cache_control(
            200,
            &WebTrickplayUnavailable {
                schema_version: WEB_SCHEMA_VERSION,
                item_id: item_id.into(),
                available: false,
            },
            "private, max-age=0, must-revalidate",
        );
    }
    api_error(
        404,
        "preview_unavailable",
        "Timeline previews are not available for this title.",
        false,
        None,
    )
}

fn trickplay_sheet_path(directory: &Path, revision: &str, index: u32) -> PathBuf {
    directory.join(format!("sheet-{revision}-{index:04}.jpg"))
}

fn load_trickplay_manifest(
    app: &App,
    source_paths: &[PathBuf],
    source_metadata: &std::fs::Metadata,
) -> Option<(PathBuf, TrickplaySidecarManifest, u32)> {
    let mut directories = Vec::new();
    for source_path in source_paths {
        let Some(directory) = rusty_dlna_protocol::trickplay_directory_for_media(source_path)
        else {
            continue;
        };
        if directories.contains(&directory) {
            continue;
        }
        directories.push(directory);
    }
    for directory in directories {
        let manifest_path = directory.join(rusty_dlna_protocol::TRICKPLAY_MANIFEST_NAME);
        let Ok(manifest_file) = rusty_dlna_scan::open_allowed_file(&manifest_path, &app.scan_cfg)
        else {
            continue;
        };
        let Ok(manifest_bytes) = read_bounded(
            manifest_file.file,
            rusty_dlna_protocol::TRICKPLAY_MAX_MANIFEST_BYTES,
        ) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_slice::<TrickplaySidecarManifest>(&manifest_bytes)
        else {
            continue;
        };
        let Some(sheet_count) = validate_trickplay_manifest(&manifest, source_metadata) else {
            continue;
        };
        return Some((directory, manifest, sheet_count));
    }
    None
}

pub(crate) fn preview(app: &App, req: &HttpRequest) -> HttpResponse {
    if !app.cfg.web.enable {
        return not_found();
    }
    if !req.method.eq_ignore_ascii_case("GET") && !req.method.eq_ignore_ascii_case("HEAD") {
        return api_error(
            405,
            "method_not_allowed",
            "This timeline preview operation is not supported.",
            false,
            None,
        );
    }
    if !req.query.is_empty() {
        return api_error(
            400,
            "invalid_parameter",
            "The timeline preview request contains an unsupported parameter.",
            false,
            None,
        );
    }
    let Some(request) = trickplay_request(&req.path) else {
        return api_error(
            400,
            "invalid_preview",
            "The timeline preview path is invalid.",
            false,
            None,
        );
    };
    let item_id = match &request {
        TrickplayRequest::Manifest(item_id) | TrickplayRequest::Sheet { item_id, .. } => *item_id,
    };
    let manifest_request = matches!(&request, TrickplayRequest::Manifest(_));
    let Some(item) = read_recover(&app.catalog)
        .get_item_by_detail(item_id)
        .filter(|item| item.mime.starts_with("video/"))
        .cloned()
    else {
        return preview_unavailable(item_id, manifest_request);
    };
    let source_path = rusty_dlna_scan::rebase_media_path_for_config(&item.path, &app.scan_cfg);
    let Ok(source) = rusty_dlna_scan::open_allowed_file(&source_path, &app.scan_cfg) else {
        return preview_unavailable(item_id, manifest_request);
    };
    let Ok(source_metadata) = source.file.metadata() else {
        return preview_unavailable(item_id, manifest_request);
    };
    let Some((preview_dir, manifest, sheet_count)) = load_trickplay_manifest(
        app,
        &[source_path, source.resolved_path.clone()],
        &source_metadata,
    ) else {
        return preview_unavailable(item_id, manifest_request);
    };

    match request {
        TrickplayRequest::Manifest(_) => {
            let complete = (0..sheet_count).all(|index| {
                rusty_dlna_scan::open_allowed_file(
                    &trickplay_sheet_path(&preview_dir, &manifest.asset_revision, index),
                    &app.scan_cfg,
                )
                .and_then(|opened| opened.file.metadata())
                .is_ok_and(|metadata| {
                    metadata.is_file()
                        && metadata.len() > 0
                        && metadata.len() <= rusty_dlna_protocol::TRICKPLAY_MAX_SHEET_BYTES
                })
            });
            if !complete {
                return preview_unavailable(item_id, true);
            }
            let etag = format!(
                "\"trickplay-api-v{WEB_SCHEMA_VERSION}-{}\"",
                manifest.asset_revision
            );
            if req.header("If-None-Match") == Some(etag.as_str()) {
                let mut response = HttpResponse::new(304, "Not Modified");
                response.set("ETag", etag);
                response.set("Cache-Control", "private, max-age=0, must-revalidate");
                return response;
            }
            let sheet_urls = (0..sheet_count)
                .map(|index| {
                    format!(
                        "{WEB_PREVIEW_SHEET_PREFIX}{item_id}/{}/{index}.jpg",
                        manifest.asset_revision
                    )
                })
                .collect();
            let mut response = json_response_with_status_and_cache_control(
                200,
                &WebTrickplayManifest {
                    schema_version: WEB_SCHEMA_VERSION,
                    item_id: item_id.into(),
                    available: true,
                    duration_seconds: manifest.duration_seconds,
                    interval_seconds: manifest.interval_seconds,
                    frame_width: manifest.frame_width,
                    frame_height: manifest.frame_height,
                    columns: manifest.columns,
                    rows: manifest.rows,
                    frame_count: manifest.frame_count,
                    sheet_urls,
                },
                "private, max-age=0, must-revalidate",
            );
            response.set("ETag", etag);
            response
        }
        TrickplayRequest::Sheet {
            revision, index, ..
        } => {
            if revision != manifest.asset_revision || index >= sheet_count {
                return preview_unavailable(item_id, false);
            }
            let Ok(sheet_file) = rusty_dlna_scan::open_allowed_file(
                &trickplay_sheet_path(&preview_dir, revision, index),
                &app.scan_cfg,
            ) else {
                return preview_unavailable(item_id, false);
            };
            let Ok(bytes) = read_bounded(
                sheet_file.file,
                rusty_dlna_protocol::TRICKPLAY_MAX_SHEET_BYTES,
            ) else {
                return preview_unavailable(item_id, false);
            };
            let expected_dimensions = (
                manifest.frame_width.saturating_mul(manifest.columns),
                manifest.frame_height.saturating_mul(manifest.rows),
            );
            if jpeg_dimensions(&bytes) != Some(expected_dimensions) {
                return preview_unavailable(item_id, false);
            }
            let etag = format!("\"trickplay-v1-{revision}-{index}\"");
            if req.header("If-None-Match") == Some(etag.as_str()) {
                let mut response = HttpResponse::new(304, "Not Modified");
                response.set("ETag", etag);
                response.set("Cache-Control", "private, max-age=31536000, immutable");
                return response;
            }
            let mut response =
                bytes_response("image/jpeg", &bytes, "private, max-age=31536000, immutable");
            response.set("ETag", etag);
            response.set("X-Content-Type-Options", "nosniff");
            response
        }
    }
}

pub(crate) fn transcode_status(app: &App, req: &HttpRequest) -> HttpResponse {
    if !app.cfg.web.enable {
        return not_found();
    }
    let cancel_request = req.method.eq_ignore_ascii_case("DELETE");
    let startup_report = req.method.eq_ignore_ascii_case("POST");
    if !cancel_request
        && !startup_report
        && !req.method.eq_ignore_ascii_case("GET")
        && !req.method.eq_ignore_ascii_case("HEAD")
    {
        return api_error(
            405,
            "method_not_allowed",
            "This transcode operation is not supported.",
            false,
            None,
        );
    }
    let params = QueryParams::parse(&req.query);
    if params.has_unknown(&["request", "session", "event"]) {
        return api_error(
            400,
            "invalid_parameter",
            "The status request contains an unsupported parameter.",
            false,
            None,
        );
    }
    let request_id = match params.optional_u64("request") {
        Ok(value) => value,
        Err(()) => {
            return api_error(
                400,
                "invalid_request",
                "The playback request ID is invalid.",
                false,
                None,
            )
        }
    };
    let session_id = match params.optional_u64("session") {
        Ok(value) => value,
        Err(()) => {
            return api_error(
                400,
                "invalid_session",
                "The playback session ID is invalid.",
                false,
                None,
            )
        }
    };
    let startup_event = if startup_report {
        let Some(event) = params
            .get("event")
            .and_then(crate::remux::WebStartupEvent::from_wire)
        else {
            return api_error(
                400,
                "invalid_event",
                "The playback timing event is invalid.",
                false,
                None,
            );
        };
        Some(event)
    } else {
        None
    };
    if !startup_report && params.get("event").is_some() {
        return api_error(
            400,
            "invalid_parameter",
            "Playback timing events require POST.",
            false,
            None,
        );
    }
    let Some(id) = web_transcode_status_id(&req.path) else {
        return api_error(
            400,
            "invalid_item",
            "The media item ID is invalid.",
            false,
            None,
        );
    };
    let Some(item) = read_recover(&app.catalog).get_item_by_detail(id).cloned() else {
        return api_error(
            404,
            "media_missing",
            "This title is no longer in the library.",
            true,
            Some("return_to_library"),
        );
    };
    let source_path = rusty_dlna_scan::rebase_media_path_for_config(&item.path, &app.scan_cfg);
    if rusty_dlna_scan::open_allowed_file(&source_path, &app.scan_cfg).is_err() {
        return api_error(
            404,
            "media_missing",
            "The media file is not available.",
            true,
            Some("return_to_library"),
        );
    }
    if cancel_request && request_id.is_none() {
        return api_error(
            400,
            "invalid_request",
            "A playback request ID is required for cancellation.",
            false,
            None,
        );
    }
    if startup_report && (request_id.is_none() || session_id.is_none()) {
        return api_error(
            400,
            "invalid_request",
            "Playback request and session IDs are required for timing events.",
            false,
            None,
        );
    }
    let cancelled = cancel_request
        && crate::remux::cancel_web_request(app, id, session_id, request_id.unwrap_or_default());
    if !cancel_request {
        if let Some(request_id) = request_id {
            crate::remux::keep_web_request_alive(app, id, session_id, request_id);
        }
    }
    if let Some(event) = startup_event {
        crate::remux::record_web_startup_event(
            app,
            id,
            session_id.unwrap_or_default(),
            request_id.unwrap_or_default(),
            event,
        );
    }
    let (state, retry_after_seconds) = if cancelled {
        ("cancelled", None)
    } else {
        crate::remux::web_job_state(app, id, request_id)
    };
    let mut response = json_response(&WebTranscodeStatus {
        schema_version: WEB_SCHEMA_VERSION,
        item_id: id.into(),
        request_id,
        state,
        retry_after_seconds,
    });
    if let Some(retry_after) = retry_after_seconds {
        response.set("Retry-After", retry_after);
    }
    response
}

pub(crate) fn browser_caption_response(app: &App, ext: &str, body: &[u8]) -> HttpResponse {
    if !app.cfg.web.enable {
        return not_found();
    }
    let Some(conversion) = browser_caption_conversion(ext) else {
        return api_error(
            415,
            "caption_unsupported",
            "This caption format cannot be shown in the browser.",
            false,
            None,
        );
    };
    match caption_to_webvtt(conversion, body) {
        Ok(vtt) => bytes_response("text/vtt; charset=utf-8", &vtt, "no-store"),
        Err(BrowserCaptionError::Encoding) => api_error(
            422,
            "caption_encoding",
            "The caption file is not valid UTF-8.",
            false,
            None,
        ),
        Err(BrowserCaptionError::Malformed) => api_error(
            422,
            "caption_malformed",
            "The caption file is malformed.",
            false,
            None,
        ),
    }
}

fn source_pixel_rate(width: u32, height: u32, frame_rate: &str) -> Option<u64> {
    let pixels = u64::from(width).checked_mul(u64::from(height))?;
    if let Some((numerator, denominator)) = frame_rate.split_once('/') {
        let numerator = numerator.parse::<u64>().ok()?;
        let denominator = denominator.parse::<u64>().ok()?;
        return (numerator > 0 && denominator > 0)
            .then(|| pixels.saturating_mul(numerator).div_ceil(denominator));
    }
    let fps = frame_rate.parse::<f64>().ok()?;
    (fps.is_finite() && fps > 0.0).then(|| (pixels as f64 * fps).ceil() as u64)
}

fn quality_requests_upscale(
    quality: rusty_dlna_transcode::BrowserQuality,
    source: &rusty_dlna_transcode::SourceMedia,
) -> bool {
    quality != rusty_dlna_transcode::BrowserQuality::Auto
        && quality.max_width() > source.width
        && quality.max_height() > source.height
}

fn quality_within_ai_scale_limit(
    quality: rusty_dlna_transcode::BrowserQuality,
    source: &rusty_dlna_transcode::SourceMedia,
) -> bool {
    source.width > 0
        && source.height > 0
        && (quality.max_width() <= source.width.saturating_mul(2)
            || quality.max_height() <= source.height.saturating_mul(2))
}

pub(crate) fn media(app: &App, req: &HttpRequest, peer: SocketAddr) -> HttpResponse {
    if !app.cfg.web.enable {
        return not_found();
    }
    let Some(id) = web_media_id(&req.path) else {
        return api_error(
            400,
            "invalid_item",
            "The media item ID is invalid.",
            false,
            None,
        );
    };
    let Some(item) = read_recover(&app.catalog).get_item_by_detail(id).cloned() else {
        return api_error(
            404,
            "media_missing",
            "This title is no longer in the library.",
            true,
            Some("return_to_library"),
        );
    };
    if !(item.mime.starts_with("video/") || item.mime.starts_with("audio/")) {
        return api_error(
            415,
            "unsupported_media",
            "This item cannot be played in a browser.",
            false,
            Some("return_to_library"),
        );
    }

    let params = QueryParams::parse(&req.query);
    if params.has_unknown(&[
        "audio",
        "start",
        "quality",
        "encoding_preset",
        "reason",
        "request",
        "session",
        "mode",
        "video_mode",
        "video_output",
        "audio_mode",
        "delivery",
        "hls_offset",
        "hls_length",
        "mse_after",
    ]) {
        return api_error(
            400,
            "invalid_parameter",
            "The media request contains an unsupported parameter.",
            false,
            None,
        );
    }
    let start_seconds = match params.optional_usize("start") {
        Ok(value) => value.unwrap_or(0),
        Err(()) => {
            return api_error(
                400,
                "invalid_start",
                "The start offset must be a non-negative whole number.",
                false,
                None,
            )
        }
    };
    let request_id = match params.optional_u64("request") {
        Ok(value) => value,
        Err(()) => {
            return api_error(
                400,
                "invalid_request",
                "The playback request ID is invalid.",
                false,
                None,
            )
        }
    };
    let session_id = match params.optional_u64("session") {
        Ok(value) => value,
        Err(()) => {
            return api_error(
                400,
                "invalid_session",
                "The playback session ID is invalid.",
                false,
                None,
            )
        }
    };
    let source_mode = match params.get("mode").unwrap_or("compatible") {
        "direct" => "direct",
        "compatible" => "compatible",
        _ => {
            return api_error(
                400,
                "invalid_mode",
                "Choose direct or compatible playback.",
                false,
                None,
            )
        }
    };
    let delivery = match params.get("delivery").unwrap_or("mp4") {
        "mp4" | "hls" | "hls_init" | "hls_segment" | "mse" | "mse_init" | "mse_segment" => {
            params.get("delivery").unwrap_or("mp4")
        }
        _ => {
            return api_error(
                400,
                "invalid_delivery",
                "Choose MP4, HLS, or Media Source compatible delivery.",
                false,
                None,
            )
        }
    };
    let hls_offset = match params.optional_u64("hls_offset") {
        Ok(value) => value,
        Err(()) => {
            return api_error(
                400,
                "invalid_delivery",
                "The HLS resource offset is invalid.",
                false,
                None,
            )
        }
    };
    let hls_length = match params.optional_u64("hls_length") {
        Ok(value) => value,
        Err(()) => {
            return api_error(
                400,
                "invalid_delivery",
                "The HLS resource length is invalid.",
                false,
                None,
            )
        }
    };
    let mse_after = match params.optional_usize("mse_after") {
        Ok(value) if value.is_none_or(|cursor| cursor <= crate::remux::MAX_MSE_FRAGMENT_CURSOR) => {
            value
        }
        Ok(_) | Err(()) => {
            return api_error(
                400,
                "invalid_delivery",
                "The Media Source fragment cursor is invalid.",
                false,
                None,
            )
        }
    };
    let raw_parameter_is = |name: &str, expected: &str| {
        req.query.split('&').any(|entry| {
            entry
                .split_once('=')
                .is_some_and(|(raw_name, raw_value)| raw_name == name && raw_value == expected)
        })
    };
    let canonical_fragment_query = params
        .get("delivery")
        .is_none_or(|_| raw_parameter_is("delivery", delivery))
        && hls_offset.is_none_or(|value| raw_parameter_is("hls_offset", &value.to_string()))
        && hls_length.is_none_or(|value| raw_parameter_is("hls_length", &value.to_string()))
        && mse_after.is_none_or(|value| raw_parameter_is("mse_after", &value.to_string()));
    if !canonical_fragment_query {
        return api_error(
            400,
            "invalid_delivery",
            "The fragmented-media query is not canonical.",
            false,
            None,
        );
    }
    let fixed_resource = matches!(
        delivery,
        "hls_init" | "hls_segment" | "mse_init" | "mse_segment"
    );
    let valid_hls_range = matches!(
        (hls_offset, hls_length),
        (Some(_), Some(1..=MAX_HLS_RESOURCE_BYTES))
    );
    let valid_delivery_path = match delivery {
        "mp4" => {
            req.path.ends_with(".mp4")
                && hls_offset.is_none()
                && hls_length.is_none()
                && mse_after.is_none()
        }
        "hls" => {
            req.path.ends_with(".m3u8")
                && hls_offset.is_none()
                && hls_length.is_none()
                && mse_after.is_none()
        }
        "mse" => req.path.ends_with(".m3u8") && hls_offset.is_none() && hls_length.is_none(),
        "hls_init" | "mse_init" => {
            req.path.ends_with(".mp4") && valid_hls_range && mse_after.is_none()
        }
        "hls_segment" | "mse_segment" => {
            req.path.ends_with(".m4s") && valid_hls_range && mse_after.is_none()
        }
        _ => false,
    };
    if !valid_delivery_path || fixed_resource != hls_offset.is_some() {
        return api_error(
            400,
            "invalid_delivery",
            "The HLS resource request is invalid.",
            false,
            None,
        );
    }
    let quality = match params.get("quality").unwrap_or("auto") {
        "auto" => rusty_dlna_transcode::BrowserQuality::Auto,
        "uhd_high" => rusty_dlna_transcode::BrowserQuality::UhdHigh,
        "uhd_optimized" => rusty_dlna_transcode::BrowserQuality::UhdOptimized,
        "full_hd" => rusty_dlna_transcode::BrowserQuality::FullHd,
        "data_saver" => rusty_dlna_transcode::BrowserQuality::DataSaver,
        "sd_480" => rusty_dlna_transcode::BrowserQuality::Sd480,
        "low_360" => rusty_dlna_transcode::BrowserQuality::Low360,
        _ => {
            return api_error(
                400,
                "invalid_quality",
                "Choose an available playback quality.",
                false,
                None,
            )
        }
    };
    let Some(encoding_preset) = rusty_dlna_transcode::BrowserEncodingPreset::parse(
        params.get("encoding_preset").unwrap_or("balanced"),
    ) else {
        return api_error(
            400,
            "invalid_encoding_preset",
            "Choose an available encoding preset.",
            false,
            None,
        );
    };
    let requested_video_mode = match params.get("video_mode").unwrap_or("transcode") {
        "copy" | "repair" | "transcode" => params.get("video_mode").unwrap_or("transcode"),
        _ => {
            return api_error(
                400,
                "invalid_video_mode",
                "Choose copy, repair, or transcode video mode.",
                false,
                None,
            )
        }
    };
    let copy_video_requested = requested_video_mode == "copy";
    let repair_video_requested = requested_video_mode == "repair";
    let requested_video_output = match params.get("video_output").unwrap_or("h264_sdr") {
        "h264_sdr" => BrowserVideoOutput::H264Sdr,
        "hevc_hdr10" => BrowserVideoOutput::HevcHdr10,
        _ => {
            return api_error(
                400,
                "invalid_video_output",
                "Choose an available video output format.",
                false,
                None,
            )
        }
    };
    if requested_video_mode != "transcode" && params.get("video_output").is_some() {
        return api_error(
            400,
            "invalid_video_output",
            "Copied and repaired video chooses its output format automatically.",
            false,
            None,
        );
    }
    let copy_audio_requested = match params.get("audio_mode").unwrap_or("transcode") {
        "copy" => true,
        "transcode" => false,
        _ => {
            return api_error(
                400,
                "invalid_audio_mode",
                "Choose copy or transcode audio mode.",
                false,
                None,
            )
        }
    };
    let fallback_reason = params.get("reason").unwrap_or("unspecified");
    if fallback_reason.is_empty()
        || fallback_reason.len() > 64
        || !fallback_reason
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return api_error(
            400,
            "invalid_reason",
            "The source reason is invalid.",
            false,
            None,
        );
    }

    if source_mode == "direct" {
        if [
            "audio",
            "start",
            "quality",
            "video_mode",
            "video_output",
            "audio_mode",
            "delivery",
            "hls_offset",
            "hls_length",
            "mse_after",
        ]
        .iter()
        .any(|name| params.get(name).is_some())
        {
            return api_error(
                400,
                "invalid_parameter",
                "Direct playback does not accept transcode parameters.",
                false,
                None,
            );
        }
        tracing::info!(
            detail_id = item.detail_id,
            title = %item.title,
            source_mode,
            fallback_reason,
            "web original media GET"
        );
        return serve_original(app, req, peer, &item);
    }

    if !app.cfg.transcode.enable {
        return api_error(
            409,
            "transcode_disabled",
            "Compatible playback is disabled on this server.",
            true,
            Some("play_original"),
        );
    }

    let stream_metadata = CompactStreamMetadata::parse(&item.probe.audio_streams).ok();
    let fallback_channels = u32::try_from(item.channels.unwrap_or(0).max(0)).unwrap_or(0);
    let tracks = stored_audio_tracks(stream_metadata, &item.probe.audio, fallback_channels);
    let default_audio_index = default_audio_index(&tracks, stream_metadata, &item.probe.audio);
    let audio_index = match params.optional_usize("audio") {
        Ok(Some(index)) if tracks.iter().any(|track| track.index == index) => index,
        Ok(Some(0)) if tracks.is_empty() => 0,
        Ok(Some(_)) | Err(()) => {
            return api_error(
                400,
                "invalid_audio",
                "Choose an available audio track.",
                false,
                None,
            )
        }
        Ok(None) => default_audio_index,
    };
    if item
        .duration
        .as_deref()
        .and_then(media_duration_seconds)
        .is_some_and(|duration| duration > 0 && start_seconds >= duration)
    {
        return api_error(
            400,
            "invalid_start",
            "The start offset is past the end of this title.",
            false,
            None,
        );
    }
    let is_video = item.mime.starts_with("video/");
    let source = probe_to_source(
        &item.probe.container,
        &item.probe.video,
        &item.probe.hdr,
        &item.probe.audio,
        item.probe.width,
        item.probe.height,
    );
    let requests_ai_upscale = is_video && quality_requests_upscale(quality, &source);
    let ai_upscale_profile = if requests_ai_upscale {
        let source_pixel_rate =
            source_pixel_rate(source.width, source.height, &item.probe.frame_rate);
        if source.hdr != rusty_dlna_transcode::HdrKind::Sdr
            || item.probe.bit_depth != 8
            || !quality_within_ai_scale_limit(quality, &source)
            || source_pixel_rate.is_none()
        {
            None
        } else {
            let source_pixel_rate = source_pixel_rate.unwrap_or(u64::MAX);
            app.ai_upscale_profiles.iter().find(|profile| {
                source.width <= profile.max_source_width
                    && source.height <= profile.max_source_height
                    && source_pixel_rate <= profile.max_source_pixels_per_second
            })
        }
    } else {
        None
    };
    let hevc_hdr10_available = is_video
        && app.cfg.web.encoder == "h264_nvenc"
        && source.video_codec == rusty_dlna_transcode::VideoCodec::Hevc
        && item.probe.bit_depth > 8
        && matches!(
            source.hdr,
            rusty_dlna_transcode::HdrKind::Hdr10
                | rusty_dlna_transcode::HdrKind::DolbyVisionProfile7
                | rusty_dlna_transcode::HdrKind::DolbyVisionProfile8
        );
    if requested_video_output == BrowserVideoOutput::HevcHdr10 && !hevc_hdr10_available {
        return api_error(
            400,
            "video_output_unavailable",
            "HDR10 output is not available for this source and server.",
            true,
            Some("play_compatible"),
        );
    }
    let copy_video = if copy_video_requested {
        if !is_video
            || quality != rusty_dlna_transcode::BrowserQuality::Auto
            || !browser_can_remux_video(&item, &source)
        {
            return api_error(
                400,
                "video_copy_unavailable",
                "The source video cannot be copied into compatible playback.",
                false,
                None,
            );
        }
        true
    } else {
        false
    };
    if repair_video_requested
        && (!is_video
            || quality != rusty_dlna_transcode::BrowserQuality::Auto
            || !matches!(
                source.video_codec,
                rusty_dlna_transcode::VideoCodec::H264 | rusty_dlna_transcode::VideoCodec::Hevc
            ))
    {
        return api_error(
            400,
            "video_repair_unavailable",
            "This source cannot use display-order repair playback.",
            false,
            None,
        );
    }
    let selected_audio_codec = tracks
        .iter()
        .find(|track| track.index == audio_index)
        .map(|track| track.codec.as_str())
        .or_else(|| {
            item.probe
                .audio
                .split(',')
                .map(str::trim)
                .filter(|codec| !codec.is_empty())
                .nth(audio_index)
        })
        .unwrap_or("");
    let copy_audio = if copy_audio_requested {
        if !browser_can_remux_audio(selected_audio_codec) {
            return api_error(
                400,
                "audio_copy_unavailable",
                "The selected audio track cannot be copied into compatible playback.",
                false,
                None,
            );
        }
        true
    } else {
        false
    };
    let repair_video_encoder = repair_video_requested.then(|| {
        rusty_dlna_transcode::browser_repair_video_encoder(
            &app.cfg.web.encoder,
            source.video_codec,
            source.hdr,
            item.probe.bit_depth,
        )
    });
    let preserve_hevc_hdr = repair_video_encoder == Some("hevc_nvenc")
        || requested_video_output == BrowserVideoOutput::HevcHdr10;
    let video_encoder = if !is_video || copy_video {
        "copy"
    } else if requested_video_output == BrowserVideoOutput::HevcHdr10 {
        "hevc_nvenc"
    } else {
        repair_video_encoder
            .unwrap_or_else(|| rusty_dlna_transcode::browser_video_encoder(&app.cfg.web.encoder))
    };
    let hardware_decode = if requested_video_output == BrowserVideoOutput::HevcHdr10 {
        rusty_dlna_transcode::browser_hdr_hardware_decode(
            video_encoder,
            source.video_codec,
            source.hdr,
        )
    } else {
        rusty_dlna_transcode::browser_hardware_decode(video_encoder, source.video_codec)
    };
    let plan = TranscodePlan {
        decision: Decision::Recode,
        action: RecodeAction::Browser,
        rule: Some("embedded-web-player".into()),
        keep_hdr10: preserve_hevc_hdr,
        drop_dolby_vision: true,
        video_encoder: video_encoder.into(),
        hardware_decode,
        audio: if copy_audio {
            AudioAction::Copy
        } else {
            AudioAction::ToAac
        },
        container: "mp4",
        audio_index,
        browser_quality: Some(quality),
        browser_ai_upscale: ai_upscale_profile.map(|profile| BrowserAiUpscale {
            model: profile.name.clone(),
            shader_sha256: profile.shader_sha256.clone(),
        }),
    };
    let browser_options = rusty_dlna_transcode::BrowserOutputOptions {
        encoding_preset,
        source_video: is_video.then_some(source.video_codec),
        selected_audio: rusty_dlna_transcode::browser_audio_codec_from_name(selected_audio_codec),
        source_hdr: source.hdr,
        start_seconds,
        hls: delivery != "mp4",
    };
    let all_fragments_independent = browser_options.hls && plan.video_encoder != "copy";
    let tone_map_to_sdr =
        rusty_dlna_transcode::browser_cache_uses_sdr_tonemap_revision(&plan, browser_options);
    let output_mime = if is_video { "video/mp4" } else { "audio/mp4" };
    let remux_audio = if copy_audio {
        RemuxAudio::Copy
    } else {
        RemuxAudio::Aac
    };
    // Audio files have no video stream; retaining an explicit copy plan keeps
    // the optional video map harmless while producing an AAC-only MP4.

    if let Some(spec) =
        crate::remux::active_web_job_spec(app, item.detail_id, session_id, request_id)
    {
        let expected_args = spec.verified_ffmpeg.as_ref().map_or_else(
            || {
                rusty_dlna_transcode::browser_ffmpeg_os_args(
                    std::path::Path::new("/proc/self/fd/3"),
                    &cache_part(&spec.dest),
                    &plan,
                    browser_options,
                )
            },
            |ffmpeg| {
                rusty_dlna_transcode::browser_ffmpeg_os_args_for_verified_ffmpeg(
                    std::path::Path::new("/proc/self/fd/3"),
                    &cache_part(&spec.dest),
                    &plan,
                    browser_options,
                    ffmpeg,
                )
            },
        );
        if spec.mime != output_mime
            || spec.args != expected_args
            || spec.audio_index != plan.audio_index
            || spec.audio != remux_audio
            || spec.cacheable != (start_seconds == 0)
            || spec.hls_all_fragments_independent != all_fragments_independent
        {
            return api_error(
                409,
                "invalid_request",
                "This playback generation already has different stream settings.",
                false,
                None,
            );
        }
        // One browser generation has immutable output semantics. Playlist
        // polls, fixed fragment reads, and native media reconnects reuse the
        // validated source descriptor and tool snapshot established by its
        // first request instead of re-sampling the source for every resource.
        tracing::debug!(
            detail_id = item.detail_id,
            request_id,
            session_id,
            delivery,
            "reusing active web compatibility job"
        );
        let mut response = live_transcode_response(spec.mime);
        response.remux_job = Some(spec);
        return response;
    }

    let prepared = crate::remux::prepared_web_transcode(
        app,
        item.detail_id,
        session_id,
        &plan,
        browser_options,
    );
    let (source_file, source_path, cache_identity) = if let Some(prepared) = prepared {
        prepared
    } else {
        let configured_path =
            rusty_dlna_scan::rebase_media_path_for_config(&item.path, &app.scan_cfg);
        let opened = match rusty_dlna_scan::open_allowed_file(&configured_path, &app.scan_cfg) {
            Ok(opened) => opened,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return api_error(
                    403,
                    "media_missing",
                    "The media file is not available.",
                    false,
                    Some("return_to_library"),
                );
            }
            Err(error) => {
                tracing::warn!(path = %configured_path.display(), %error, "web media missing");
                return api_error(
                    404,
                    "media_missing",
                    "The media file is not available.",
                    true,
                    Some("retry_media"),
                );
            }
        };
        let tool_control = ToolQueryControl::new(
            &app.helpers,
            &app.scan_cfg.cancellation,
            Duration::from_secs(app.cfg.helper_queue_timeout_secs),
        );
        let cache_identity =
            match rusty_dlna_transcode::browser_transcode_cache_identity_file_controlled(
                &opened.file,
                &opened.resolved_path,
                &plan,
                browser_options,
                tool_control,
            ) {
                Ok(Some(identity)) => identity,
                Ok(None) => {
                    return api_error(
                        500,
                        "transcode_failed",
                        "The server could not prepare this title.",
                        true,
                        Some("retry_media"),
                    );
                }
                Err(error) => {
                    tracing::warn!(%error, detail_id = item.detail_id, "web transcode tool fingerprint unavailable");
                    let code = match error {
                        ToolQueryError::Busy(_) => "transcode_busy",
                        ToolQueryError::Cancelled => "transcode_cancelled",
                        ToolQueryError::Deadline { .. } | ToolQueryError::Query { .. } => {
                            "transcode_failed"
                        }
                    };
                    return transcode_stream_error(503, code);
                }
            };
        let source_file = Arc::new(opened.file);
        crate::remux::remember_web_transcode_preparation(
            app,
            session_id,
            crate::remux::WebTranscodePreparation::new(
                item.detail_id,
                plan.clone(),
                browser_options,
                Arc::clone(&source_file),
                opened.resolved_path.clone(),
                cache_identity.clone(),
            ),
        );
        (source_file, opened.resolved_path, cache_identity)
    };
    let cache_key = cache_identity.cache_key().to_owned();
    let verified_ffmpeg = cache_identity.ffmpeg().clone();
    let destination = cache_dest_for_key(
        &app.cache_dir,
        item.detail_id,
        RecodeAction::Browser,
        &cache_key,
    );
    let part = cache_part(&destination);
    let transcode_args = |selected_plan: &TranscodePlan| {
        rusty_dlna_transcode::browser_ffmpeg_os_args_for_verified_ffmpeg(
            std::path::Path::new("/proc/self/fd/3"),
            &part,
            selected_plan,
            browser_options,
            &verified_ffmpeg,
        )
    };
    let args = transcode_args(&plan);
    let fallback_args =
        if (is_video && plan.video_encoder != "libx264") || plan.audio != AudioAction::ToAac {
            let mut fallback = plan.clone();
            if is_video {
                fallback.video_encoder = "libx264".into();
            }
            fallback.hardware_decode = HardwareDecode::None;
            fallback.audio = AudioAction::ToAac;
            Some(transcode_args(&fallback))
        } else {
            None
        };
    let job_key = format!("web:{}:{cache_key}:{args:?}", item.detail_id);
    tracing::info!(
        title = %item.title,
        input_mime = %item.mime,
        output_mime,
        video_encoder = %plan.video_encoder,
        hardware_decode = ?plan.hardware_decode,
        source_hdr = ?source.hdr,
        tone_map_to_sdr,
        copy_video,
        repair_video = repair_video_requested,
        copy_audio,
        audio_index = plan.audio_index,
        start_seconds,
        source_mode = "compatible",
        fallback_reason,
        quality = quality.id(),
        encoding_preset = if is_video && !copy_video { encoding_preset.id() } else { "not_used" },
        delivery,
        path = %req.path,
        range = req.header("Range").unwrap_or("-"),
        "web compatibility transcode GET"
    );
    let mut response = live_transcode_response(output_mime);
    response.remux_job = Some(RemuxJobSpec {
        detail_id: item.detail_id,
        web_session_id: session_id,
        web_request_id: request_id,
        mime: output_mime,
        job_key,
        cache_key,
        src: source_path,
        source_file: Some(source_file),
        ai_upscale_shader_file: ai_upscale_profile.map(|profile| Arc::clone(&profile.shader_file)),
        dest: destination,
        args,
        fallback_args,
        continue_after_disconnect: false,
        cacheable: start_seconds == 0,
        hls_all_fragments_independent: all_fragments_independent,
        remux_p8: false,
        verified_ffmpeg: Some(verified_ffmpeg),
        profile8_toolchain: None,
        audio_index: plan.audio_index,
        audio: remux_audio,
    });
    response
}

pub(crate) fn download(app: &App, req: &HttpRequest, peer: SocketAddr) -> HttpResponse {
    if !app.cfg.web.enable {
        return not_found();
    }
    let Some(id) = web_download_id(&req.path) else {
        return api_error(
            400,
            "invalid_item",
            "The download item ID is invalid.",
            false,
            None,
        );
    };
    if !req.query.is_empty() {
        return api_error(
            400,
            "invalid_parameter",
            "The download request does not accept parameters.",
            false,
            None,
        );
    }
    let Some(item) = read_recover(&app.catalog).get_item_by_detail(id).cloned() else {
        return api_error(
            404,
            "media_missing",
            "This title is no longer in the library.",
            true,
            Some("return_to_library"),
        );
    };
    if !item.mime.starts_with("video/") {
        return api_error(
            415,
            "unsupported_media",
            "Only original video files can be downloaded.",
            false,
            Some("return_to_library"),
        );
    }

    tracing::info!(
        detail_id = item.detail_id,
        title = %item.title,
        range = req.header("Range").unwrap_or("-"),
        "web original video download"
    );
    let mut response = serve_original(app, req, peer, &item);
    if matches!(response.status, 200 | 206) {
        response.set(
            "Content-Disposition",
            download_content_disposition(&media_file_name(&item), item.detail_id, &item.ext),
        );
        response.set("Cache-Control", "private, no-store");
        response.set("X-Content-Type-Options", "nosniff");
    }
    response
}

fn likely_browser_native(item: &MediaItem, source: &rusty_dlna_transcode::SourceMedia) -> bool {
    if item.mime.starts_with("audio/") {
        return item.mime == "audio/mpeg"
            || item.mime == "audio/aac"
            || (item.mime == "audio/mp4" && source.audio == rusty_dlna_transcode::AudioCodec::Aac);
    }
    (source.container == rusty_dlna_transcode::Container::Mp4
        && source.video_codec == rusty_dlna_transcode::VideoCodec::H264
        && source.audio == rusty_dlna_transcode::AudioCodec::Aac)
        || item.mime == "video/webm"
}

fn browser_video_content_type(
    item: &MediaItem,
    source: &rusty_dlna_transcode::SourceMedia,
) -> Option<String> {
    if !browser_video_codec_compatible(item, source) {
        return None;
    }
    let stored = item.probe.codec_string.split(',').next()?.trim();
    let codec = match source.video_codec {
        rusty_dlna_transcode::VideoCodec::H264 if stored.starts_with("avc1.") => stored.to_owned(),
        rusty_dlna_transcode::VideoCodec::Hevc if stored.starts_with("hvc1.") => stored.to_owned(),
        rusty_dlna_transcode::VideoCodec::Hevc if stored.starts_with("hev1.") => {
            format!("hvc1.{}", &stored[5..])
        }
        _ => return None,
    };
    Some(format!("video/mp4; codecs=\"{codec}\""))
}

fn browser_can_remux_video(item: &MediaItem, source: &rusty_dlna_transcode::SourceMedia) -> bool {
    item.probe.video_timestamp_mode != "broken-reordered"
        && browser_video_codec_compatible(item, source)
}

fn browser_video_codec_compatible(
    item: &MediaItem,
    source: &rusty_dlna_transcode::SourceMedia,
) -> bool {
    let codec = item
        .probe
        .codec_string
        .split(',')
        .next()
        .unwrap_or("")
        .trim();
    match source.video_codec {
        rusty_dlna_transcode::VideoCodec::H264 => {
            let profile = item.probe.video_profile.trim().to_ascii_lowercase();
            let profile_safe = profile.is_empty()
                || matches!(
                    profile.as_str(),
                    "baseline" | "constrained baseline" | "main" | "high"
                );
            let pixel_format_safe =
                matches!(item.probe.pixel_format.as_str(), "yuv420p" | "yuvj420p");
            let bit_depth_safe = item.probe.bit_depth == 0 || item.probe.bit_depth <= 8;
            let level_safe = item.probe.video_level == 0 || item.probe.video_level <= 51;
            codec.starts_with("avc1.")
                && profile_safe
                && pixel_format_safe
                && bit_depth_safe
                && level_safe
                && !rusty_dlna_transcode::browser_requires_sdr_tonemap(source.hdr)
        }
        rusty_dlna_transcode::VideoCodec::Hevc => {
            matches!(
                source.hdr,
                HdrKind::Sdr | HdrKind::Hdr10 | HdrKind::DolbyVisionProfile8
            )
        }
        _ => false,
    }
}

fn browser_audio_codec_string(codec: &str) -> Option<&'static str> {
    Some(match codec.trim().to_ascii_lowercase().as_str() {
        "aac" => "mp4a.40.2",
        "ac3" => "ac-3",
        "eac3" => "ec-3",
        "mp3" => "mp4a.6B",
        _ => return None,
    })
}

fn browser_audio_content_type(codec: &str) -> Option<String> {
    browser_audio_codec_string(codec).map(|codec| format!("audio/mp4; codecs=\"{codec}\""))
}

fn browser_can_remux_audio(codec: &str) -> bool {
    browser_audio_content_type(codec).is_some()
}

fn serve_original(
    app: &App,
    req: &HttpRequest,
    peer: SocketAddr,
    item: &MediaItem,
) -> HttpResponse {
    let mut original = req.clone();
    original.path = format!("/MediaItems/{}.{}", item.detail_id, item.ext);
    // Browser compatibility parameters describe a transcode plan and must not
    // leak into the original media route when transcoding is disabled.
    original.query.clear();
    original.target = original.path.clone();
    app.media(&original, false, peer)
}

fn download_content_disposition(file_name: &str, detail_id: i64, extension: &str) -> String {
    let file_name = safe_download_file_name(file_name, detail_id, extension);
    let ascii_name = file_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric()
                || matches!(character, ' ' | '.' | '-' | '_' | '(' | ')' | '[' | ']')
            {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let mut encoded_name = String::with_capacity(file_name.len());
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in file_name.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
        {
            encoded_name.push(char::from(byte));
        } else {
            encoded_name.push('%');
            encoded_name.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded_name.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    format!("attachment; filename=\"{ascii_name}\"; filename*=UTF-8''{encoded_name}")
}

fn safe_download_file_name(file_name: &str, detail_id: i64, extension: &str) -> String {
    const MAX_FILE_NAME_BYTES: usize = 240;
    let safe_extension = extension
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .take(16)
        .map(char::from)
        .collect::<String>();
    let safe_extension = if safe_extension.is_empty() {
        "bin"
    } else {
        safe_extension.as_str()
    };
    let fallback = format!("video-{detail_id}.{safe_extension}");
    let sanitized = file_name
        .chars()
        .map(|character| {
            if character.is_control() || matches!(character, '/' | '\\') {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    let sanitized = sanitized.trim();
    if sanitized.is_empty() || matches!(sanitized, "." | "..") {
        return fallback;
    }
    let candidate = sanitized.to_owned();
    if candidate.len() <= MAX_FILE_NAME_BYTES {
        return candidate;
    }

    let suffix = format!(".{safe_extension}");
    let stem = candidate
        .rsplit_once('.')
        .filter(|(_, current_extension)| current_extension.eq_ignore_ascii_case(safe_extension))
        .map_or(candidate.as_str(), |(stem, _)| stem);
    let stem_budget = MAX_FILE_NAME_BYTES.saturating_sub(suffix.len());
    let mut bounded = String::with_capacity(MAX_FILE_NAME_BYTES);
    for character in stem.chars() {
        if bounded.len() + character.len_utf8() > stem_budget {
            break;
        }
        bounded.push(character);
    }
    let bounded = bounded.trim_end_matches([' ', '.']);
    if bounded.is_empty() {
        fallback
    } else {
        format!("{bounded}{suffix}")
    }
}

fn web_media_id(path: &str) -> Option<i64> {
    let value = path
        .strip_prefix(WEB_MEDIA_PREFIX)?
        .strip_suffix(".mp4")
        .or_else(|| path.strip_prefix(WEB_MEDIA_PREFIX)?.strip_suffix(".m3u8"))
        .or_else(|| path.strip_prefix(WEB_MEDIA_PREFIX)?.strip_suffix(".m4s"))?;
    decimal_id(value)
}

fn web_download_id(path: &str) -> Option<i64> {
    decimal_id(path.strip_prefix(WEB_DOWNLOAD_PREFIX)?)
}

fn web_item_id(path: &str) -> Option<i64> {
    decimal_id(path.strip_prefix(WEB_ITEM_PREFIX)?)
}

fn web_transcode_status_id(path: &str) -> Option<i64> {
    decimal_id(path.strip_prefix(WEB_TRANSCODE_STATUS_PREFIX)?)
}

fn media_duration_seconds(value: &str) -> Option<usize> {
    let mut parts = value.split(':');
    let hours = parts.next()?.parse::<f64>().ok()?;
    let minutes = parts.next()?.parse::<f64>().ok()?;
    let seconds = parts.next()?.parse::<f64>().ok()?;
    if parts.next().is_some() || !hours.is_finite() || !minutes.is_finite() || !seconds.is_finite()
    {
        return None;
    }
    let total = hours * 3600.0 + minutes * 60.0 + seconds;
    (total.is_sign_positive() && total <= usize::MAX as f64).then_some(total.floor() as usize)
}

fn html_response(body: String) -> HttpResponse {
    let mut response = bytes_response("text/html; charset=utf-8", body.as_bytes(), "no-cache");
    response.set("X-Content-Type-Options", "nosniff");
    response
}

fn json_response(value: &(impl Serialize + ?Sized)) -> HttpResponse {
    json_response_with_status(200, value)
}

fn json_response_with_status(status_code: u16, value: &(impl Serialize + ?Sized)) -> HttpResponse {
    json_response_with_status_and_cache_control(status_code, value, "no-store")
}

fn json_response_with_status_and_cache_control(
    status_code: u16,
    value: &(impl Serialize + ?Sized),
    cache_control: &str,
) -> HttpResponse {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    let reason = match status_code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let mut response = HttpResponse::new(status_code, reason);
    response.set("Content-Type", "application/json; charset=utf-8");
    response.set("Cache-Control", cache_control);
    response.body = body;
    response.set("Content-Length", response.body.len());
    response.set("X-Content-Type-Options", "nosniff");
    response
}

fn bytes_response(mime: &str, body: &[u8], cache_control: &str) -> HttpResponse {
    let mut response = HttpResponse::new(200, "OK");
    response.set("Content-Type", mime);
    response.set("Cache-Control", cache_control);
    response.body = body.to_vec();
    response.set("Content-Length", response.body.len());
    response
}

fn not_found() -> HttpResponse {
    HttpResponse::html(404, "Not Found", "web player disabled or path not found")
}

#[derive(Default)]
struct QueryParams {
    entries: Vec<(String, String)>,
    invalid: bool,
}

impl QueryParams {
    fn parse(query: &str) -> Self {
        let mut params = Self::default();
        if query.is_empty() {
            return params;
        }
        for (index, part) in query.split('&').enumerate() {
            if index >= 16 || part.is_empty() {
                params.invalid = true;
                continue;
            }
            let Some((name, value)) = part.split_once('=') else {
                params.invalid = true;
                continue;
            };
            let (Ok(name), Ok(value)) = (percent_decode(name), percent_decode(value)) else {
                params.invalid = true;
                continue;
            };
            if name.is_empty() || params.entries.iter().any(|(key, _)| key == &name) {
                params.invalid = true;
                continue;
            }
            params.entries.push((name, value));
        }
        params
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn optional_usize(&self, name: &str) -> Result<Option<usize>, ()> {
        self.get(name)
            .map(|value| value.parse::<usize>().map_err(|_| ()))
            .transpose()
    }

    fn optional_u32(&self, name: &str) -> Result<Option<u32>, ()> {
        self.get(name)
            .map(|value| value.parse::<u32>().map_err(|_| ()))
            .transpose()
    }

    fn optional_u64(&self, name: &str) -> Result<Option<u64>, ()> {
        self.get(name)
            .map(|value| value.parse::<u64>().map_err(|_| ()))
            .transpose()
    }

    fn has_unknown(&self, allowed: &[&str]) -> bool {
        self.invalid
            || self
                .entries
                .iter()
                .any(|(name, _)| !allowed.contains(&name.as_str()))
    }
}

fn percent_decode(value: &str) -> Result<String, ()> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let high = hex(bytes[index + 1]);
                let low = hex(bytes[index + 2]);
                if let (Some(high), Some(low)) = (high, low) {
                    decoded.push((high << 4) | low);
                    index += 3;
                } else {
                    return Err(());
                }
            }
            b'%' => return Err(()),
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).map_err(|_| ())
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_item_ids_serialize_as_exact_decimal_strings() {
        let value = serde_json::to_value(WebItemId(9_007_199_254_740_993)).unwrap();
        assert_eq!(value, serde_json::Value::String("9007199254740993".into()));
    }

    #[test]
    fn trickplay_paths_and_jpeg_dimensions_are_strict() {
        assert!(matches!(
            trickplay_request("/api/web/preview/42"),
            Some(TrickplayRequest::Manifest(42))
        ));
        assert!(matches!(
            trickplay_request("/web/preview/42/0123456789abcdef/3.jpg"),
            Some(TrickplayRequest::Sheet {
                item_id: 42,
                revision: "0123456789abcdef",
                index: 3,
            })
        ));
        for invalid in [
            "/api/web/preview/0",
            "/api/web/preview/042",
            "/api/web/preview/42/extra",
            "/web/preview/042/0123456789abcdef/1.jpg",
            "/web/preview/42/0123456789abcdef/-1.jpg",
            "/web/preview/42/0123456789abcdef/1.png",
            "/web/preview/42/0123456789abcdef/1.jpg/extra",
        ] {
            assert!(trickplay_request(invalid).is_none(), "{invalid}");
        }
        assert!(valid_asset_revision("0123456789abcdef"));
        assert!(!valid_asset_revision("0123456789ABCDEF"));
        let jpeg = [
            0xff, 0xd8, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x0e, 0x10, 0x0c, 0x80, 0x01, 0x01, 0x11,
            0x00, 0xff, 0xd9,
        ];
        assert_eq!(jpeg_dimensions(&jpeg), Some((3200, 3600)));
        assert_eq!(jpeg_dimensions(&jpeg[..jpeg.len() - 2]), None);
    }

    #[test]
    fn browser_caption_eligibility_comes_from_protocol_conversion_policy() {
        for format in rusty_dlna_protocol::CAPTION_FORMATS {
            let expected = format.webvtt_conversion.is_some();
            assert_eq!(
                browser_caption_conversion(format.extension).is_some(),
                expected,
                "{} conversion",
                format.extension
            );
            assert_eq!(
                browser_caption_url(42, 3, format.extension).is_some(),
                expected,
                "{} URL",
                format.extension
            );
            assert_eq!(
                browser_caption_conversion(&format.extension.to_ascii_uppercase()).is_some(),
                expected,
                "{} ASCII case",
                format.extension
            );
        }
        assert_eq!(browser_caption_conversion("unknown"), None);
        assert_eq!(browser_caption_url(42, 3, "unknown"), None);
    }

    #[test]
    fn caption_language_uses_raw_sidecar_ownership() {
        assert_eq!(
            caption_language(Path::new("movie.mkv"), Path::new("movie.EN.srt")),
            Some("en".into())
        );
        assert_eq!(
            caption_language(Path::new("movie.mkv"), Path::new("movie.eng.forced.srt")),
            Some("eng".into())
        );
        assert_eq!(
            caption_language(Path::new("movie.mkv"), Path::new("movie.en-US.srt")),
            Some("en".into())
        );
        assert_eq!(
            caption_language(Path::new("movie.mkv"), Path::new("movie.eng_forced.srt")),
            Some("eng".into())
        );
        assert_eq!(
            caption_language(Path::new("movie.mkv"), Path::new("other.en.srt")),
            None
        );
        assert_eq!(
            caption_language(Path::new("movie.mkv"), Path::new("movie-en.srt")),
            None
        );
        assert_eq!(
            caption_language(Path::new("movie.mkv"), Path::new("movie_en.srt")),
            None
        );
    }

    #[test]
    fn query_params_decode_and_bound_values() {
        let params = QueryParams::parse("q=Blade+Runner%3A+2049&offset=20&limit=80");
        assert_eq!(params.get("q"), Some("Blade Runner: 2049"));
        assert_eq!(params.optional_usize("offset"), Ok(Some(20)));
        assert_eq!(params.optional_usize("limit"), Ok(Some(80)));
        assert!(!params.has_unknown(&["q", "offset", "limit"]));

        for query in [
            "q=one&q=two",
            "q=%GG",
            "q=%FF",
            "q",
            "q=one&&limit=1",
            "a=1&b=2&c=3&d=4&e=5&f=6&g=7&h=8&i=9&j=10&k=11&l=12&m=13&n=14&o=15&p=16&q=17",
        ] {
            assert!(QueryParams::parse(query).has_unknown(&["q"]), "{query}");
        }
    }

    #[test]
    fn continue_ids_are_canonical_distinct_and_bounded() {
        assert_eq!(continue_detail_ids(""), Ok(Vec::new()));
        assert_eq!(continue_detail_ids("9,42"), Ok(vec![9, 42]));
        for invalid in ["0", "01", "-1", "+1", "1,1", "1,,2", "x"] {
            assert_eq!(continue_detail_ids(invalid), Err(()), "{invalid}");
        }
        let hundred = (1..=100)
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(continue_detail_ids(&hundred).unwrap().len(), 100);
        assert_eq!(continue_detail_ids(&format!("{hundred},101")), Err(()));
        assert_eq!(
            continue_detail_ids("9223372036854775808"),
            Err(()),
            "IDs must remain inside the signed database key range"
        );
    }

    #[test]
    fn compact_audio_tracks_require_channels_and_preserve_order() {
        let metadata = CompactStreamMetadata::parse(concat!(
            "0:8:aac,",
            "1:3:ac3:6:eng:Dub%2C%20%D0%9A%D0%B8%D0%BD%D0%BE:1,",
            "2:3:eac3:2::bad%XX:0"
        ))
        .ok();
        let tracks = stored_audio_tracks(metadata, "truehd", 8);
        assert_eq!(tracks.len(), 2);
        assert_eq!(
            tracks.iter().map(|track| track.index).collect::<Vec<_>>(),
            [3, 3]
        );
        assert_eq!(tracks[0].codec, "ac3");
        assert_eq!(tracks[0].channels, 6);
        assert_eq!(tracks[0].language.as_deref(), Some("eng"));
        assert_eq!(tracks[0].title.as_deref(), Some("Dub, Кино"));
        assert!(tracks[0].default);
        assert_eq!(tracks[1].codec, "eac3");
        assert_eq!(tracks[1].title, None, "bad labels degrade to no label");

        let no_displayable_channels = CompactStreamMetadata::parse("0:9:aac").ok();
        let fallback = stored_audio_tracks(no_displayable_channels, "truehd,ac3", 8);
        assert_eq!(
            fallback
                .iter()
                .map(|track| (
                    track.index,
                    track.codec.as_str(),
                    track.channels,
                    track.default
                ))
                .collect::<Vec<_>>(),
            [(0, "truehd", 8, true), (1, "ac3", 8, false)],
            "three-field records remain selectable but are not browser display tracks"
        );
    }

    #[test]
    fn browser_audio_default_prefers_tagged_english_without_changing_original() {
        let metadata = CompactStreamMetadata::parse(concat!(
            "0:0:aac:2:jpn:Main:1,",
            "1:1:ac3:6:eng:English:0,",
            "2:2:aac:2:fra:French:0"
        ))
        .ok();
        let tracks = stored_audio_tracks(metadata, "aac,ac3,aac", 2);
        assert_eq!(default_audio_index(&tracks, metadata, "aac,ac3,aac"), 1);
        assert_eq!(original_audio_index(&tracks), 0);

        let no_english = CompactStreamMetadata::parse(concat!(
            "0:0:aac:2:jpn:Main:0,",
            "1:1:ac3:6:fra:French:1"
        ))
        .ok();
        let tracks = stored_audio_tracks(no_english, "aac,ac3", 2);
        assert_eq!(default_audio_index(&tracks, no_english, "aac,ac3"), 1);
    }

    #[test]
    fn english_audio_language_accepts_common_tags_and_rejects_lookalikes() {
        for language in ["en", "ENG", "en-US", "eng_GB", " English "] {
            assert!(is_english_audio_language(Some(language)), "{language}");
        }
        for language in ["", "und", "enm", "english-commentary", "jpn"] {
            assert!(!is_english_audio_language(Some(language)), "{language}");
        }
        assert!(!is_english_audio_language(None));
    }

    #[test]
    fn compact_chapters_keep_source_gaps_and_malformed_marker_presence() {
        let metadata = CompactStreamMetadata::parse(concat!(
            "@v:bad%XX,@t:,",
            "@c:bad|300:200:backwards|100:200:Good%20One|200:300:bad%XX|300:400:"
        ))
        .ok();
        assert!(stream_metadata_complete(
            metadata,
            "video/mp4",
            "mp4",
            "h264"
        ));
        let chapters = stored_chapters(metadata);
        assert_eq!(chapters.len(), 3);
        assert_eq!(
            chapters
                .iter()
                .map(|chapter| chapter.index)
                .collect::<Vec<_>>(),
            [2, 3, 4]
        );
        assert_eq!(chapters[0].title, "Good One");
        assert_eq!(chapters[1].title, "Chapter 4");
        assert_eq!(chapters[2].title, "Chapter 5");
    }

    #[test]
    fn oversized_compact_metadata_uses_safe_server_fallbacks() {
        let oversized = format!(
            "@v:any,@t:,{}",
            "x".repeat(rusty_dlna_protocol::MAX_COMPACT_STREAM_METADATA_BYTES)
        );
        let metadata = CompactStreamMetadata::parse(&oversized).ok();
        assert!(metadata.is_none());
        let tracks = stored_audio_tracks(metadata, "truehd,ac3", 8);
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].codec, "truehd");
        assert!(tracks[0].default);
        assert!(stored_chapters(metadata).is_empty());
        assert!(!stream_metadata_complete(
            metadata,
            "video/mp4",
            "mp4",
            "h264"
        ));
    }

    #[test]
    fn media_ids_are_strict() {
        assert_eq!(web_media_id("/web/media/42.mp4"), Some(42));
        assert_eq!(web_media_id("/web/media/42.m3u8"), Some(42));
        assert_eq!(web_media_id("/web/media/42.m4s"), Some(42));
        assert_eq!(web_media_id("/web/media/nope.mp4"), None);
        assert_eq!(web_item_id("/api/web/item/42"), Some(42));
        assert_eq!(web_item_id("/api/web/item/42junk"), None);
        assert_eq!(web_item_id("/api/web/item/0"), None);
        assert_eq!(web_item_id("/api/web/item/00"), None);
        assert_eq!(web_item_id("/api/web/item/042"), None);
        assert_eq!(
            web_item_id("/api/web/item/9223372036854775807"),
            Some(i64::MAX)
        );
        assert_eq!(web_item_id("/api/web/item/9223372036854775808"), None);
        assert_eq!(web_media_id("/web/media/42.mp4junk"), None);
        assert_eq!(web_download_id("/web/download/42"), Some(42));
        assert_eq!(web_download_id("/web/download/0"), None);
        assert_eq!(web_download_id("/web/download/042"), None);
        assert_eq!(web_download_id("/web/download/42/extra"), None);
    }

    #[test]
    fn download_disposition_is_safe_utf8_and_bounded() {
        let disposition = download_content_disposition("Кино \"night\"\r\n/clip.mkv", 42, "mkv");
        assert!(!disposition.contains(['\r', '\n', '/', '\\']));
        assert!(disposition.starts_with("attachment; filename=\""));
        assert!(disposition.contains("filename*=UTF-8''%D0%9A%D0%B8%D0%BD%D0%BE"));
        assert!(disposition.ends_with("clip.mkv"));

        let long_name = format!("{}.mkv", "К".repeat(300));
        let bounded = safe_download_file_name(&long_name, 42, "mkv");
        assert!(bounded.len() <= 240);
        assert!(bounded.ends_with(".mkv"));
    }

    #[test]
    fn media_duration_is_bounded_whole_seconds() {
        assert_eq!(media_duration_seconds("2:03:35.776"), Some(7_415));
        assert_eq!(media_duration_seconds("not-a-duration"), None);
    }
}
