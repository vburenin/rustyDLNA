//! Embedded, optional browser player.
//!
//! The UI assets, paginated library API, and compatibility media endpoint
//! live together here so disabling `[web]` removes the entire browser-facing
//! surface without changing DLNA behavior.

use super::*;
use serde::Serialize;
use std::collections::HashSet;
use std::ffi::OsString;
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
const WEB_MEDIA_PREFIX: &str = "/web/media/";
const WEB_ITEM_PREFIX: &str = "/api/web/item/";
const WEB_TRANSCODE_STATUS_PREFIX: &str = "/api/web/transcode/";
const DEFAULT_PAGE_SIZE: usize = 60;
const MAX_PAGE_SIZE: usize = 200;
const SDR_TONEMAP_CACHE_REVISION: &str = "sdr-tonemap-libplacebo-v1";
const WEB_SCHEMA_VERSION: u8 = 1;

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
    id: i64,
    item: WebMediaItem,
    audio_tracks: Vec<WebAudioTrack>,
    chapters: Vec<WebChapter>,
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
    item_id: i64,
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
struct WebMediaItem {
    id: i64,
    title: String,
    file_name: String,
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
    audio_codec: String,
    audio_layout: Option<String>,
    codec_string: Option<String>,
    hdr: String,
    audio_tracks: Vec<WebAudioTrack>,
    default_audio_index: usize,
    captions: Vec<WebCaption>,
    chapters: Vec<WebChapter>,
    stream_metadata_complete: bool,
    art_url: Option<String>,
    source_url: String,
    fallback_url: String,
    transcode_likely: bool,
}

#[derive(Clone, Serialize)]
struct WebAudioTrack {
    index: usize,
    codec: String,
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
    };
    WebCapabilities {
        transcoding: app.cfg.transcode.enable,
        captions: app.scan_cfg.subtitles,
        // Web progress is intentionally browser-local. It must not overwrite
        // the accountless DLNA/Kodi bookmark identity stored in the database.
        resume: "browser_local",
        queue: true,
        media_session: true,
        quality_profiles: vec![
            quality(
                rusty_dlna_transcode::BrowserQuality::Auto,
                "Auto · up to 4K",
            ),
            quality(
                rusty_dlna_transcode::BrowserQuality::FullHd,
                "1080p",
            ),
            quality(
                rusty_dlna_transcode::BrowserQuality::DataSaver,
                "720p",
            ),
        ],
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
    let etag = format!("W/\"web-v1-{generation}\"");
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
    let db_page = query_db_web_media(
        app.db_pool.as_deref(),
        app.scan_cfg.db_path.as_deref(),
        db_kind,
        query,
        db_sort,
        offset,
        limit,
    );
    let catalog = read_recover(&app.catalog);
    let db_materialized = db_page.as_ref().and_then(|page| {
        materialize_db_page(&catalog, page).map(|entries| {
            let items = entries
                .into_iter()
                .filter_map(|entry| match entry {
                    CatalogChildRef::Item(item) => Some(item),
                    CatalogChildRef::Container(_) => None,
                })
                .collect::<Vec<_>>();
            (items, page.total as usize)
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
    items.sort_by(|left, right| {
        let ordering = match sort {
            "date_desc" => right.date.cmp(&left.date),
            "episode" => left
                .disc
                .unwrap_or(0)
                .cmp(&right.disc.unwrap_or(0))
                .then_with(|| left.track.unwrap_or(0).cmp(&right.track.unwrap_or(0))),
            _ => left.title.to_lowercase().cmp(&right.title.to_lowercase()),
        };
        ordering.then_with(|| left.detail_id.cmp(&right.detail_id))
    });
    let mut physical_files = HashSet::new();
    items.retain(|item| item.inode == 0 || physical_files.insert((item.device, item.inode)));
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

#[derive(Clone)]
struct StoredAudioTrack {
    index: usize,
    codec: String,
    channels: u32,
    language: Option<String>,
    title: Option<String>,
    default: bool,
}

fn decode_stream_field(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded)
        .ok()
        .filter(|value| !value.is_empty())
}

fn stored_audio_tracks(item: &MediaItem) -> Vec<StoredAudioTrack> {
    let mut tracks = item
        .probe
        .audio_streams
        .split(',')
        .filter_map(|record| {
            let mut fields = record.split(':');
            let _global_index = fields.next()?.parse::<usize>().ok()?;
            let index = fields.next()?.parse::<usize>().ok()?;
            let codec = fields.next()?.trim();
            let channels = fields.next()?.parse::<u32>().ok()?;
            let language = fields.next().and_then(decode_stream_field);
            let title = fields.next().and_then(decode_stream_field);
            let default = fields.next() == Some("1");
            Some(StoredAudioTrack {
                index,
                codec: codec.to_owned(),
                channels,
                language,
                title,
                default,
            })
        })
        .collect::<Vec<_>>();
    if tracks.is_empty() {
        tracks.extend(
            item.probe
                .audio
                .split(',')
                .map(str::trim)
                .filter(|codec| !codec.is_empty())
                .enumerate()
                .map(|(index, codec)| StoredAudioTrack {
                    index,
                    codec: codec.to_owned(),
                    channels: u32::try_from(item.channels.unwrap_or(0).max(0)).unwrap_or(0),
                    language: None,
                    title: None,
                    default: index == 0,
                }),
        );
    }
    tracks
}

fn stored_chapters(item: &MediaItem) -> Vec<WebChapter> {
    let Some(record) = item
        .probe
        .audio_streams
        .split(',')
        .find_map(|record| record.strip_prefix("@c:"))
    else {
        return Vec::new();
    };
    record
        .split('|')
        .take(512)
        .enumerate()
        .filter_map(|(index, chapter)| {
            let mut fields = chapter.splitn(3, ':');
            let start_millis = fields.next()?.parse::<u64>().ok()?;
            let end_millis = fields.next()?.parse::<u64>().ok()?;
            if end_millis < start_millis {
                return None;
            }
            let title = fields.next().and_then(decode_stream_field);
            Some(WebChapter {
                index,
                title: title.unwrap_or_else(|| format!("Chapter {}", index + 1)),
                start_seconds: start_millis as f64 / 1000.0,
                end_seconds: end_millis as f64 / 1000.0,
            })
        })
        .collect()
}

fn stream_metadata_complete(item: &MediaItem) -> bool {
    item.probe
        .audio_streams
        .split(',')
        .any(|record| record.starts_with("@v:"))
}

fn audio_track_dto(
    index: usize,
    codec: &str,
    channels: u32,
    language: Option<&str>,
    title: Option<&str>,
    default: bool,
) -> WebAudioTrack {
    WebAudioTrack {
        index,
        codec: codec.to_owned(),
        channels,
        language: language.map(str::to_owned),
        title: title.map(str::to_owned),
        default,
    }
}

fn caption_language(item: &MediaItem, caption: &rusty_dlna_scan::Caption) -> Option<String> {
    let media_stem = item.path.file_stem()?.to_string_lossy();
    let caption_stem = caption.path.file_stem()?.to_string_lossy();
    let suffix = caption_stem.strip_prefix(media_stem.as_ref())?;
    let candidate = suffix
        .trim_start_matches(['.', '-', '_'])
        .split(['.', '-', '_'])
        .next()?
        .to_ascii_lowercase();
    ((2..=3).contains(&candidate.len()) && candidate.bytes().all(|byte| byte.is_ascii_alphabetic()))
        .then_some(candidate)
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
            let language = caption_language(item, caption);
            let browser_supported =
                matches!(caption.ext.as_str(), "vtt" | "srt" | "ass" | "ssa" | "smi");
            WebCaption {
                index: caption.index,
                label: caption_label(caption.index, language.as_deref()),
                language,
                // Sidecar filenames do not carry a standards-defined default
                // disposition. Keep captions opt-in unless that fact is
                // persisted explicitly in a future schema.
                default: false,
                source_format: caption.ext.clone(),
                browser_supported,
                url: browser_supported.then(|| {
                    format!(
                        "/Captions/{}/{}.vtt?format=webvtt",
                        item.detail_id, caption.index
                    )
                }),
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
    let audio_tracks = stored_audio_tracks(item)
        .iter()
        .map(|track| {
            audio_track_dto(
                track.index,
                &track.codec,
                track.channels,
                track.language.as_deref(),
                track.title.as_deref(),
                track.default,
            )
        })
        .collect::<Vec<_>>();
    let default_audio_index = audio_tracks
        .iter()
        .find(|track| track.default)
        .map(|track| track.index)
        .unwrap_or_else(|| {
            pick_audio_index_from_streams(&item.probe.audio_streams, &item.probe.audio)
        });
    let file_name = media_file_name(item);
    WebMediaItem {
        id: item.detail_id,
        title: item.title.clone(),
        file_name,
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
        summary: item.comment.clone(),
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
        audio_codec: item.probe.audio.clone(),
        audio_layout: (!item.probe.audio_layout.is_empty())
            .then(|| item.probe.audio_layout.clone()),
        codec_string: (!item.probe.codec_string.is_empty())
            .then(|| item.probe.codec_string.clone()),
        hdr: item.probe.hdr.clone(),
        audio_tracks,
        default_audio_index,
        captions: caption_dtos(item),
        chapters: stored_chapters(item),
        stream_metadata_complete: stream_metadata_complete(item),
        art_url,
        source_url: format!("{WEB_MEDIA_PREFIX}{}.mp4?mode=direct", item.detail_id),
        fallback_url: format!("{WEB_MEDIA_PREFIX}{}.mp4", item.detail_id),
        transcode_likely: !likely_browser_native(item, &source),
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
    let Some(item) = read_recover(&app.catalog).get_item_by_detail(id).cloned() else {
        return api_error(
            404,
            "media_missing",
            "This title is no longer in the library.",
            true,
            Some("return_to_library"),
        );
    };
    let generation = app.update_id.load(Ordering::Acquire);
    let etag = format!(
        "W/\"web-v1-{generation}-item-{id}{}\"",
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
        audio_tracks = probe
            .audio_tracks
            .iter()
            .map(|track| {
                audio_track_dto(
                    track.index,
                    &track.codec,
                    track.channels,
                    track.language.as_deref(),
                    track.title.as_deref(),
                    track.default,
                )
            })
            .collect();
        chapters = probe
            .chapters
            .iter()
            .enumerate()
            .map(|(index, chapter)| WebChapter {
                index,
                title: chapter
                    .title
                    .clone()
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| format!("Chapter {}", index + 1)),
                start_seconds: chapter.start_seconds,
                end_seconds: chapter.end_seconds,
            })
            .collect();
        item_dto.audio_tracks.clone_from(&audio_tracks);
        item_dto.chapters.clone_from(&chapters);
        item_dto.stream_metadata_complete = true;
        item_dto.default_audio_index = audio_tracks
            .iter()
            .find(|track| track.default)
            .map(|track| track.index)
            .unwrap_or(item_dto.default_audio_index);
    }
    let mut response = json_response_with_status_and_cache_control(
        200,
        &WebItemDetails {
            schema_version: WEB_SCHEMA_VERSION,
            id,
            item: item_dto,
            audio_tracks,
            chapters,
        },
        "private, max-age=0, must-revalidate",
    );
    response.set("ETag", etag);
    response
}

pub(crate) fn transcode_status(app: &App, req: &HttpRequest) -> HttpResponse {
    if !app.cfg.web.enable {
        return not_found();
    }
    let params = QueryParams::parse(&req.query);
    if params.has_unknown(&["request"]) {
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
    let (state, retry_after_seconds) = crate::remux::web_job_state(app, id, request_id);
    let mut response = json_response(&WebTranscodeStatus {
        schema_version: WEB_SCHEMA_VERSION,
        item_id: id,
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
    match caption_to_webvtt(ext, body) {
        Ok(vtt) => bytes_response("text/vtt; charset=utf-8", &vtt, "no-store"),
        Err(BrowserCaptionError::Unsupported) => api_error(
            415,
            "caption_unsupported",
            "This caption format cannot be shown in the browser.",
            false,
            None,
        ),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BrowserCaptionError {
    Unsupported,
    Encoding,
    Malformed,
}

fn caption_to_webvtt(ext: &str, body: &[u8]) -> Result<Vec<u8>, BrowserCaptionError> {
    let text = std::str::from_utf8(body)
        .map_err(|_| BrowserCaptionError::Encoding)?
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let output = match ext {
        "vtt" => validate_webvtt(&text)?,
        "srt" => srt_to_webvtt(&text)?,
        "ass" | "ssa" => ass_to_webvtt(&text)?,
        "smi" => smi_to_webvtt(&text)?,
        _ => return Err(BrowserCaptionError::Unsupported),
    };
    Ok(output.into_bytes())
}

fn validate_webvtt(text: &str) -> Result<String, BrowserCaptionError> {
    let Some(first) = text.lines().next() else {
        return Err(BrowserCaptionError::Malformed);
    };
    if !first.trim_end().starts_with("WEBVTT") {
        return Err(BrowserCaptionError::Malformed);
    }
    let mut saw_cue = false;
    for line in text.lines().filter(|line| line.contains("-->")) {
        let (start, rest) = line
            .split_once("-->")
            .ok_or(BrowserCaptionError::Malformed)?;
        let end = rest
            .split_whitespace()
            .next()
            .ok_or(BrowserCaptionError::Malformed)?;
        if parse_caption_time(start.trim()).is_none() || parse_caption_time(end).is_none() {
            return Err(BrowserCaptionError::Malformed);
        }
        saw_cue = true;
    }
    if !saw_cue {
        return Err(BrowserCaptionError::Malformed);
    }
    Ok(format!("{}\n", text.trim_end()))
}

fn srt_to_webvtt(text: &str) -> Result<String, BrowserCaptionError> {
    let mut output = String::from("WEBVTT\n\n");
    let mut cues = 0usize;
    for block in text.split("\n\n").filter(|block| !block.trim().is_empty()) {
        let mut lines = block.lines();
        let first = lines.next().ok_or(BrowserCaptionError::Malformed)?;
        let timing = if first.contains("-->") {
            first
        } else {
            if !first.trim().bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(BrowserCaptionError::Malformed);
            }
            lines.next().ok_or(BrowserCaptionError::Malformed)?
        };
        let (start, end_and_settings) = timing
            .split_once("-->")
            .ok_or(BrowserCaptionError::Malformed)?;
        let mut end_parts = end_and_settings.split_whitespace();
        let end = end_parts.next().ok_or(BrowserCaptionError::Malformed)?;
        let start_seconds =
            parse_caption_time(start.trim()).ok_or(BrowserCaptionError::Malformed)?;
        let end_seconds = parse_caption_time(end).ok_or(BrowserCaptionError::Malformed)?;
        if end_seconds < start_seconds {
            return Err(BrowserCaptionError::Malformed);
        }
        let payload = lines.collect::<Vec<_>>();
        if payload.is_empty() {
            return Err(BrowserCaptionError::Malformed);
        }
        output.push_str(&start.trim().replace(',', "."));
        output.push_str(" --> ");
        output.push_str(&end.replace(',', "."));
        for setting in end_parts {
            output.push(' ');
            output.push_str(setting);
        }
        output.push('\n');
        output.push_str(&payload.join("\n"));
        output.push_str("\n\n");
        cues += 1;
    }
    (cues > 0)
        .then_some(output)
        .ok_or(BrowserCaptionError::Malformed)
}

fn ass_to_webvtt(text: &str) -> Result<String, BrowserCaptionError> {
    let mut in_events = false;
    let mut columns = Vec::<String>::new();
    let mut output = String::from("WEBVTT\n\n");
    let mut cues = 0usize;
    for raw in text.lines() {
        let line = raw.trim();
        if line.eq_ignore_ascii_case("[events]") {
            in_events = true;
            continue;
        }
        if line.starts_with('[') && !line.eq_ignore_ascii_case("[events]") {
            in_events = false;
        }
        if !in_events {
            continue;
        }
        if let Some(format) = line
            .strip_prefix("Format:")
            .or_else(|| line.strip_prefix("format:"))
        {
            columns = format
                .split(',')
                .map(|field| field.trim().to_ascii_lowercase())
                .collect();
            continue;
        }
        let Some(dialogue) = line
            .strip_prefix("Dialogue:")
            .or_else(|| line.strip_prefix("dialogue:"))
        else {
            continue;
        };
        if columns.is_empty() {
            return Err(BrowserCaptionError::Malformed);
        }
        let fields = dialogue
            .splitn(columns.len(), ',')
            .map(str::trim)
            .collect::<Vec<_>>();
        if fields.len() != columns.len() {
            return Err(BrowserCaptionError::Malformed);
        }
        let field = |name: &str| {
            columns
                .iter()
                .position(|column| column == name)
                .and_then(|index| fields.get(index).copied())
        };
        let start = ass_time(field("start").ok_or(BrowserCaptionError::Malformed)?)
            .ok_or(BrowserCaptionError::Malformed)?;
        let end = ass_time(field("end").ok_or(BrowserCaptionError::Malformed)?)
            .ok_or(BrowserCaptionError::Malformed)?;
        if parse_caption_time(&end).unwrap_or(0.0) < parse_caption_time(&start).unwrap_or(0.0) {
            return Err(BrowserCaptionError::Malformed);
        }
        let cue = strip_ass_overrides(field("text").ok_or(BrowserCaptionError::Malformed)?);
        if cue.trim().is_empty() {
            continue;
        }
        output.push_str(&format!("{start} --> {end}\n{cue}\n\n"));
        cues += 1;
    }
    (cues > 0)
        .then_some(output)
        .ok_or(BrowserCaptionError::Malformed)
}

fn ass_time(value: &str) -> Option<String> {
    let mut fields = value.trim().split(':');
    let hours = fields.next()?.parse::<u32>().ok()?;
    let minutes = fields.next()?.parse::<u32>().ok()?;
    let seconds = fields.next()?.parse::<f64>().ok()?;
    if fields.next().is_some() || minutes > 59 || !(0.0..60.0).contains(&seconds) {
        return None;
    }
    let millis = (seconds * 1000.0).round() as u32;
    Some(format!(
        "{hours:02}:{minutes:02}:{:02}.{:03}",
        millis / 1000,
        millis % 1000
    ))
}

fn strip_ass_overrides(value: &str) -> String {
    let mut output = String::new();
    let mut in_override = false;
    for character in value.chars() {
        match character {
            '{' => in_override = true,
            '}' => in_override = false,
            _ if !in_override => output.push(character),
            _ => {}
        }
    }
    output
        .replace("\\N", "\n")
        .replace("\\n", "\n")
        .replace("\\h", " ")
}

fn smi_to_webvtt(text: &str) -> Result<String, BrowserCaptionError> {
    let lower = text.to_ascii_lowercase();
    let mut cues = Vec::<(u64, String)>::new();
    let mut cursor = 0usize;
    while let Some(relative) = lower[cursor..].find("<sync") {
        let tag_start = cursor + relative;
        let tag_end = lower[tag_start..]
            .find('>')
            .map(|index| tag_start + index + 1)
            .ok_or(BrowserCaptionError::Malformed)?;
        let tag = &lower[tag_start..tag_end];
        let start = tag
            .find("start")
            .and_then(|index| {
                tag[index + 5..]
                    .find('=')
                    .map(|offset| index + 5 + offset + 1)
            })
            .and_then(|index| {
                tag[index..]
                    .trim_start_matches([' ', '\'', '"'])
                    .split(|character: char| !character.is_ascii_digit())
                    .next()
            })
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(BrowserCaptionError::Malformed)?;
        let next = lower[tag_end..]
            .find("<sync")
            .map(|index| tag_end + index)
            .unwrap_or(text.len());
        let cue = strip_smi_markup(&text[tag_end..next]);
        if !cue.trim().is_empty() && !cue.trim().eq_ignore_ascii_case("&nbsp;") {
            cues.push((start, cue));
        }
        cursor = next;
        if cursor >= text.len() {
            break;
        }
    }
    if cues.is_empty() {
        return Err(BrowserCaptionError::Malformed);
    }
    let mut output = String::from("WEBVTT\n\n");
    for (index, (start, cue)) in cues.iter().enumerate() {
        let end = cues
            .get(index + 1)
            .map(|next| next.0)
            .unwrap_or_else(|| start.saturating_add(5_000));
        output.push_str(&format!(
            "{} --> {}\n{}\n\n",
            millis_vtt(*start),
            millis_vtt(end.max(start.saturating_add(1))),
            cue
        ));
    }
    Ok(output)
}

fn strip_smi_markup(value: &str) -> String {
    let normalized = value
        .replace("<br>", "\n")
        .replace("<BR>", "\n")
        .replace("<br/>", "\n")
        .replace("<BR/>", "\n");
    let mut output = String::new();
    let mut in_tag = false;
    for character in normalized.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(character),
            _ => {}
        }
    }
    output
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_owned()
}

fn millis_vtt(value: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        value / 3_600_000,
        (value / 60_000) % 60,
        (value / 1000) % 60,
        value % 1000
    )
}

fn parse_caption_time(value: &str) -> Option<f64> {
    let normalized = value.replace(',', ".");
    let mut parts = normalized.split(':').collect::<Vec<_>>();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let seconds = parts.pop()?.parse::<f64>().ok()?;
    let minutes = parts.pop()?.parse::<u32>().ok()?;
    let hours = parts
        .pop()
        .map(str::parse::<u32>)
        .transpose()
        .ok()?
        .unwrap_or(0);
    (minutes <= 59 && (0.0..60.0).contains(&seconds))
        .then_some(hours as f64 * 3600.0 + minutes as f64 * 60.0 + seconds)
}

fn browser_requires_sdr_tonemap(hdr: HdrKind) -> bool {
    matches!(
        hdr,
        HdrKind::Hdr10
            | HdrKind::DolbyVisionProfile5
            | HdrKind::DolbyVisionProfile7
            | HdrKind::DolbyVisionProfile8
            | HdrKind::DolbyVisionOther
    )
}

fn apply_browser_sdr_tonemap(
    args: &mut Vec<OsString>,
    hardware_decode: HardwareDecode,
    quality: rusty_dlna_transcode::BrowserQuality,
) {
    let input = args
        .iter()
        .position(|argument| argument == "-i")
        .expect("ffmpeg browser command must include an input");
    args.splice(
        input..input,
        [
            OsString::from("-init_hw_device"),
            OsString::from("vulkan=vk:0"),
            OsString::from("-filter_hw_device"),
            OsString::from("vk"),
        ],
    );

    // libplacebo consumes the Dolby Vision RPU side data and maps the result
    // to ordinary BT.709 SDR. Profile 5 has no HDR10-compatible base-layer
    // colors, so merely dropping its metadata produces the familiar purple
    // browser image. NVDEC frames stay GPU-backed apart from the CUDA/Vulkan
    // interop transfers; encoding returns to NVENC after the Vulkan filter.
    let filter = match hardware_decode {
        HardwareDecode::Cuda => concat!(
            "hwdownload,format=p010le,hwupload,",
            "libplacebo=apply_dolbyvision=true:colorspace=bt709:",
            "color_primaries=bt709:color_trc=bt709:range=tv:",
            "tonemapping=bt.2390:format=yuv420p,",
            "hwdownload,format=yuv420p"
        ),
        HardwareDecode::None => concat!(
            "format=yuv420p10le,hwupload,",
            "libplacebo=apply_dolbyvision=true:colorspace=bt709:",
            "color_primaries=bt709:color_trc=bt709:range=tv:",
            "tonemapping=bt.2390:format=yuv420p,",
            "hwdownload,format=yuv420p"
        ),
    };
    let scale = format!(
        "scale=w='min(iw,{})':h='min(ih,{})':force_original_aspect_ratio=decrease:force_divisible_by=2:flags=fast_bilinear",
        quality.max_width(),
        quality.max_height()
    );
    let filter = format!("{filter},{scale}");
    if let Some(vf) = args.iter().position(|argument| argument == "-vf") {
        args[vf + 1] = OsString::from(&filter);
    } else {
        let video_codec = args
            .iter()
            .position(|argument| argument == "-c:v")
            .expect("ffmpeg browser video command must include an encoder");
        args.splice(
            video_codec..video_codec,
            [OsString::from("-vf"), OsString::from(&filter)],
        );
    }

    let output = args
        .len()
        .checked_sub(1)
        .expect("ffmpeg browser command must include an output");
    args.splice(
        output..output,
        [
            OsString::from("-color_primaries"),
            OsString::from("bt709"),
            OsString::from("-color_trc"),
            OsString::from("bt709"),
            OsString::from("-colorspace"),
            OsString::from("bt709"),
            OsString::from("-color_range"),
            OsString::from("tv"),
        ],
    );
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
    if params.has_unknown(&["audio", "start", "quality", "reason", "request", "mode"]) {
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
    let quality = match params.get("quality").unwrap_or("auto") {
        "auto" => rusty_dlna_transcode::BrowserQuality::Auto,
        "full_hd" => rusty_dlna_transcode::BrowserQuality::FullHd,
        "data_saver" => rusty_dlna_transcode::BrowserQuality::DataSaver,
        _ => {
            return api_error(
                400,
                "invalid_quality",
                "Choose Auto, 1080p, or Data saver quality.",
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
        if ["audio", "start", "quality"]
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

    let tracks = stored_audio_tracks(&item);
    let default_audio_index = tracks
        .iter()
        .find(|track| track.default)
        .map(|track| track.index)
        .unwrap_or_else(|| {
            pick_audio_index_from_streams(&item.probe.audio_streams, &item.probe.audio)
        });
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
        .is_some_and(|duration| start_seconds >= duration)
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
    let tone_map_to_sdr = is_video && browser_requires_sdr_tonemap(source.hdr);
    let video_encoder = if !is_video {
        "copy"
    } else {
        rusty_dlna_transcode::browser_video_encoder(&app.cfg.web.encoder)
    };
    let hardware_decode =
        if video_encoder == "h264_nvenc" && matches!(item.probe.video.as_str(), "h264" | "hevc") {
            HardwareDecode::Cuda
        } else {
            HardwareDecode::None
        };
    let plan = TranscodePlan {
        decision: Decision::Recode,
        action: RecodeAction::Browser,
        rule: Some("embedded-web-player".into()),
        keep_hdr10: false,
        drop_dolby_vision: true,
        video_encoder: video_encoder.into(),
        hardware_decode,
        audio: AudioAction::ToAac,
        container: "mp4",
        audio_index,
        browser_quality: Some(quality),
    };
    // Audio files have no video stream; retaining an explicit copy plan keeps
    // the optional video map harmless while producing an AAC-only MP4.

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
        Err(error) => {
            tracing::warn!(path = %source_path.display(), %error, "web media missing");
            return api_error(
                404,
                "media_missing",
                "The media file is not available.",
                true,
                Some("retry_media"),
            );
        }
    };
    let Some(mut cache_key) =
        transcode_cache_key_file(&opened.file, &opened.resolved_path, &plan, false)
    else {
        return api_error(
            500,
            "transcode_failed",
            "The server could not prepare this title.",
            true,
            Some("retry_media"),
        );
    };
    if tone_map_to_sdr {
        cache_key.push('-');
        cache_key.push_str(SDR_TONEMAP_CACHE_REVISION);
    }
    if start_seconds > 0 {
        cache_key.push_str(&format!("-start-{start_seconds}"));
    }
    let destination = cache_dest_for_key(
        &app.cache_dir,
        item.detail_id,
        RecodeAction::Browser,
        &cache_key,
    );
    let part = cache_part(&destination);
    let transcode_args = |selected_plan: &TranscodePlan| {
        let mut args = ffmpeg_grow_os_args(Path::new("/proc/self/fd/3"), &part, selected_plan);
        if tone_map_to_sdr {
            apply_browser_sdr_tonemap(&mut args, selected_plan.hardware_decode, quality);
        }
        if start_seconds > 0 {
            let input = args
                .iter()
                .position(|argument| argument == "-i")
                .expect("ffmpeg browser command must include an input");
            args.splice(
                input..input,
                [
                    OsString::from("-ss"),
                    OsString::from(start_seconds.to_string()),
                ],
            );
        }
        args
    };
    let args = transcode_args(&plan);
    let fallback_args = if is_video && plan.video_encoder != "libx264" {
        let mut fallback = plan.clone();
        fallback.video_encoder = "libx264".into();
        fallback.hardware_decode = HardwareDecode::None;
        Some(transcode_args(&fallback))
    } else {
        None
    };
    let job_key = format!("web:{}:{cache_key}:{args:?}", item.detail_id);
    let output_mime = if is_video { "video/mp4" } else { "audio/mp4" };

    tracing::info!(
        title = %item.title,
        input_mime = %item.mime,
        output_mime,
        video_encoder = %plan.video_encoder,
        hardware_decode = ?plan.hardware_decode,
        source_hdr = ?source.hdr,
        tone_map_to_sdr,
        audio_index = plan.audio_index,
        start_seconds,
        source_mode = "compatible",
        fallback_reason,
        quality = quality.id(),
        "web compatibility transcode GET"
    );
    let mut response = live_transcode_response(output_mime);
    response.remux_job = Some(RemuxJobSpec {
        detail_id: item.detail_id,
        web_request_id: request_id,
        mime: output_mime,
        job_key,
        cache_key,
        src: opened.resolved_path.clone(),
        source_file: Some(Arc::new(opened.file)),
        dest: destination,
        args,
        fallback_args,
        continue_after_disconnect: false,
        cacheable: start_seconds == 0,
        remux_p8: false,
        audio_index: plan.audio_index,
        audio: RemuxAudio::Aac,
    });
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

fn web_media_id(path: &str) -> Option<i64> {
    let value = path.strip_prefix(WEB_MEDIA_PREFIX)?.strip_suffix(".mp4")?;
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse::<i64>().ok())
        .flatten()
        .filter(|id| *id >= 0)
}

fn web_item_id(path: &str) -> Option<i64> {
    let value = path.strip_prefix(WEB_ITEM_PREFIX)?;
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse::<i64>().ok())
        .flatten()
        .filter(|id| *id >= 0)
}

fn web_transcode_status_id(path: &str) -> Option<i64> {
    let value = path.strip_prefix(WEB_TRANSCODE_STATUS_PREFIX)?;
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse::<i64>().ok())
        .flatten()
        .filter(|id| *id >= 0)
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
    fn persisted_audio_labels_decode_without_exposing_delimiters() {
        assert_eq!(
            decode_stream_field("Dub%2C %D0%9A%D0%B8%D0%BD%D0%BE"),
            Some("Dub, Кино".into())
        );
        assert_eq!(decode_stream_field(""), None);
        assert_eq!(decode_stream_field("bad%XX"), None);
    }

    #[test]
    fn media_ids_are_strict() {
        assert_eq!(web_media_id("/web/media/42.mp4"), Some(42));
        assert_eq!(web_media_id("/web/media/nope.mp4"), None);
        assert_eq!(web_item_id("/api/web/item/42"), Some(42));
        assert_eq!(web_item_id("/api/web/item/42junk"), None);
        assert_eq!(web_media_id("/web/media/42.mp4junk"), None);
    }

    #[test]
    fn media_duration_is_bounded_whole_seconds() {
        assert_eq!(media_duration_seconds("2:03:35.776"), Some(7_415));
        assert_eq!(media_duration_seconds("not-a-duration"), None);
    }

    #[test]
    fn srt_conversion_preserves_multiline_unicode_and_overlap() {
        let input = "1\r\n00:00:01,250 --> 00:00:04,000\r\nHello\r\n世界\r\n\r\n2\r\n00:00:03,500 --> 00:00:05,000\r\nOverlap\r\n";
        let output =
            String::from_utf8(caption_to_webvtt("srt", input.as_bytes()).unwrap()).unwrap();
        assert!(output.starts_with("WEBVTT\n\n"));
        assert!(output.contains("00:00:01.250 --> 00:00:04.000\nHello\n世界"));
        assert!(output.contains("00:00:03.500 --> 00:00:05.000\nOverlap"));
    }

    #[test]
    fn ass_conversion_uses_declared_columns_and_removes_override_commands() {
        let input = "[Events]\nFormat: Layer, Start, End, Style, Text\nDialogue: 0,0:00:01.20,0:00:03.40,Default,{\\i1}Hello\\Nworld";
        let output =
            String::from_utf8(caption_to_webvtt("ass", input.as_bytes()).unwrap()).unwrap();
        assert!(output.contains("00:00:01.200 --> 00:00:03.400"));
        assert!(output.contains("Hello\nworld"));
        assert!(!output.contains("\\i1"));
    }

    #[test]
    fn caption_conversion_rejects_invalid_or_bitmap_input() {
        assert_eq!(
            caption_to_webvtt("srt", b"not a cue"),
            Err(BrowserCaptionError::Malformed)
        );
        assert_eq!(
            caption_to_webvtt("sub", b"bitmap"),
            Err(BrowserCaptionError::Unsupported)
        );
        assert_eq!(
            caption_to_webvtt("vtt", &[0xff, 0xfe]),
            Err(BrowserCaptionError::Encoding)
        );
    }
}
