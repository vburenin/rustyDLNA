//! Embedded, optional browser player.
//!
//! The UI assets, paginated library API, and compatibility media endpoint
//! live together here so disabling `[web]` removes the entire browser-facing
//! surface without changing DLNA behavior.

use super::*;
use serde_json::json;
use std::collections::HashSet;
use std::ffi::OsString;

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_CSS: &str = include_str!("../web/app.css");
const APP_JS: &str = include_str!("../web/app.js");
const WEB_MEDIA_PREFIX: &str = "/web/media/";
const WEB_ITEM_PREFIX: &str = "/api/web/item/";
const DEFAULT_PAGE_SIZE: usize = 60;
const MAX_PAGE_SIZE: usize = 200;
const SDR_TONEMAP_CACHE_REVISION: &str = "sdr-tonemap-libplacebo-v1";

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
        _ => return not_found(),
    };
    bytes_response(mime, body.as_bytes(), "no-cache")
}

pub(crate) fn library(app: &App, req: &HttpRequest) -> HttpResponse {
    if !app.cfg.web.enable {
        return not_found();
    }
    let params = QueryParams::parse(&req.query);
    let offset = params.usize("offset").unwrap_or(0);
    let limit = params
        .usize("limit")
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);
    let kind = params.get("kind").unwrap_or("all");
    let query = params.get("q").unwrap_or("").trim().to_lowercase();
    let folder_view = params.get("view") == Some("folders") || params.get("folder").is_some();

    let catalog = read_recover(&app.catalog);
    if folder_view {
        let folder_id = params
            .get("folder")
            .filter(|value| !value.is_empty())
            .unwrap_or(rusty_dlna_protocol::object_id::BROWSEDIR_ID);
        let Some(breadcrumbs) = physical_folder_chain(&catalog, folder_id) else {
            return HttpResponse::html(404, "Not Found", "no such media folder");
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
            .filter(|entry| query.is_empty() || entry.matches(&query))
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
                    json!({
                        "entry_type": "folder",
                        "id": folder.object_id,
                        "title": folder.title,
                        "child_count": child_count,
                    })
                }
                WebEntry::Media(item) => media_json(app, item),
            })
            .collect::<Vec<_>>();
        let breadcrumb_json = breadcrumbs
            .iter()
            .enumerate()
            .map(|(index, folder)| {
                json!({
                    "id": folder.object_id,
                    "title": if index == 0 { "Media" } else { folder.title.as_str() },
                })
            })
            .collect::<Vec<_>>();

        return json_response(json!({
            "server_name": app.cfg.friendly_name,
            "transcoding_enabled": app.cfg.transcode.enable,
            "view": "folders",
            "folder": {
                "id": current.object_id,
                "title": if current.object_id == rusty_dlna_protocol::object_id::BROWSEDIR_ID {
                    "Media"
                } else {
                    current.title.as_str()
                },
            },
            "breadcrumbs": breadcrumb_json,
            "offset": offset,
            "limit": limit,
            "total": total,
            "entries": page,
        }));
    }

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
        .filter(|item| query.is_empty() || media_matches(item, &query))
        .collect::<Vec<_>>();
    items.sort_by(|left, right| {
        media_file_name(left)
            .to_lowercase()
            .cmp(&media_file_name(right).to_lowercase())
            .then_with(|| left.detail_id.cmp(&right.detail_id))
    });
    let mut physical_files = HashSet::new();
    items.retain(|item| item.inode == 0 || physical_files.insert((item.device, item.inode)));
    let total = items.len();
    let page = items
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|item| media_json(app, item))
        .collect::<Vec<_>>();
    drop(catalog);

    json_response(json!({
        "server_name": app.cfg.friendly_name,
        "transcoding_enabled": app.cfg.transcode.enable,
        "view": "library",
        "offset": offset,
        "limit": limit,
        "total": total,
        "items": page,
    }))
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
            Some(StoredAudioTrack {
                index,
                codec: codec.to_owned(),
                channels,
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
                }),
        );
    }
    tracks
}

fn audio_track_json(
    index: usize,
    codec: &str,
    channels: u32,
    language: Option<&str>,
    title: Option<&str>,
) -> serde_json::Value {
    json!({
        "index": index,
        "codec": codec,
        "channels": channels,
        "language": language,
        "title": title,
    })
}

fn media_json(app: &App, item: &MediaItem) -> serde_json::Value {
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
        .map(|track| audio_track_json(track.index, &track.codec, track.channels, None, None))
        .collect::<Vec<_>>();
    let default_audio_index =
        pick_audio_index_from_streams(&item.probe.audio_streams, &item.probe.audio);
    let file_name = media_file_name(item);
    json!({
        "entry_type": "media",
        "id": item.detail_id,
        "title": file_name,
        "metadata_title": item.title,
        "kind": media_kind,
        "mime": item.mime,
        "ext": item.ext,
        "date": item.date,
        "duration": item.duration,
        "duration_seconds": item.duration.as_deref().and_then(media_duration_seconds),
        "resolution": item.resolution,
        "artist": item.artist,
        "album": item.album,
        "container": item.probe.container,
        "video_codec": item.probe.video,
        "audio_codec": item.probe.audio,
        "audio_tracks": audio_tracks,
        "default_audio_index": default_audio_index,
        "art_url": art_url,
        "source_url": format!("/MediaItems/{}.{}", item.detail_id, item.ext),
        "fallback_url": format!("{WEB_MEDIA_PREFIX}{}.mp4", item.detail_id),
        "transcode_likely": !likely_browser_native(item, &source),
    })
}

pub(crate) fn item(app: &App, req: &HttpRequest) -> HttpResponse {
    if !app.cfg.web.enable {
        return not_found();
    }
    let Some(id) = web_item_id(&req.path) else {
        return HttpResponse::html(404, "Not Found", "bad web item id");
    };
    let Some(item) = read_recover(&app.catalog).get_item_by_detail(id).cloned() else {
        return HttpResponse::html(404, "Not Found", "no such media item");
    };
    if !(item.mime.starts_with("video/") || item.mime.starts_with("audio/")) {
        return HttpResponse::html(415, "Unsupported Media Type", "not audio or video");
    }
    let source_path = rusty_dlna_scan::rebase_media_path_for_config(&item.path, &app.scan_cfg);
    let opened = match rusty_dlna_scan::open_allowed_file(&source_path, &app.scan_cfg) {
        Ok(opened) => opened,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return HttpResponse::html(403, "Forbidden", "path escaped media directory");
        }
        Err(_) => return HttpResponse::html(404, "Not Found", "missing media file"),
    };
    let _helper_permit = match app.helpers.acquire_timeout_cancelled(
        Duration::from_secs(app.cfg.helper_queue_timeout_secs),
        &app.scan_cfg.cancellation,
    ) {
        Ok(permit) => permit,
        Err(error) => {
            tracing::warn!(%error, detail_id = id, "web stream probe admission rejected");
            let mut response = HttpResponse::html(
                503,
                "Service Unavailable",
                "media helper capacity exhausted",
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
        return HttpResponse::html(422, "Unprocessable Content", "cannot inspect media streams");
    };
    let audio_tracks = probe
        .audio_tracks
        .iter()
        .map(|track| {
            audio_track_json(
                track.index,
                &track.codec,
                track.channels,
                track.language.as_deref(),
                track.title.as_deref(),
            )
        })
        .collect::<Vec<_>>();
    json_response(json!({
        "id": id,
        "audio_tracks": audio_tracks,
    }))
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

fn apply_browser_sdr_tonemap(args: &mut Vec<OsString>, hardware_decode: HardwareDecode) {
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
    if let Some(vf) = args.iter().position(|argument| argument == "-vf") {
        args[vf + 1] = OsString::from(filter);
    } else {
        let video_codec = args
            .iter()
            .position(|argument| argument == "-c:v")
            .expect("ffmpeg browser video command must include an encoder");
        args.splice(
            video_codec..video_codec,
            [OsString::from("-vf"), OsString::from(filter)],
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
        return HttpResponse::html(404, "Not Found", "bad web media id");
    };
    let Some(item) = read_recover(&app.catalog).get_item_by_detail(id).cloned() else {
        return HttpResponse::html(404, "Not Found", "no such media item");
    };
    if !(item.mime.starts_with("video/") || item.mime.starts_with("audio/")) {
        return HttpResponse::html(415, "Unsupported Media Type", "not audio or video");
    }

    // The player remains useful when transcoding is disabled: its fallback
    // endpoint becomes an alias for the validated original media route.
    if !app.cfg.transcode.enable {
        return serve_original(app, req, peer, &item);
    }

    let tracks = stored_audio_tracks(&item);
    let default_audio_index =
        pick_audio_index_from_streams(&item.probe.audio_streams, &item.probe.audio);
    let params = QueryParams::parse(&req.query);
    let audio_index = match params.usize("audio") {
        Some(index) if tracks.iter().any(|track| track.index == index) => index,
        Some(0) if tracks.is_empty() => 0,
        Some(_) => return HttpResponse::html(400, "Bad Request", "invalid audio track"),
        None => default_audio_index,
    };
    let start_seconds = params.usize("start").unwrap_or(0);
    if item
        .duration
        .as_deref()
        .and_then(media_duration_seconds)
        .is_some_and(|duration| start_seconds >= duration)
    {
        return HttpResponse::html(400, "Bad Request", "start is past media duration");
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
    };
    // Audio files have no video stream; retaining an explicit copy plan keeps
    // the optional video map harmless while producing an AAC-only MP4.

    let source_path = rusty_dlna_scan::rebase_media_path_for_config(&item.path, &app.scan_cfg);
    let opened = match rusty_dlna_scan::open_allowed_file(&source_path, &app.scan_cfg) {
        Ok(opened) => opened,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return HttpResponse::html(403, "Forbidden", "path escaped media directory");
        }
        Err(error) => {
            tracing::warn!(path = %source_path.display(), %error, "web media missing");
            return HttpResponse::html(404, "Not Found", "missing media file");
        }
    };
    let Some(mut cache_key) =
        transcode_cache_key_file(&opened.file, &opened.resolved_path, &plan, false)
    else {
        return HttpResponse::html(
            500,
            "Internal Server Error",
            "cannot fingerprint browser transcode source",
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
            apply_browser_sdr_tonemap(&mut args, selected_plan.hardware_decode);
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
        "web compatibility transcode GET"
    );
    let mut response = live_transcode_response(output_mime);
    response.remux_job = Some(RemuxJobSpec {
        detail_id: item.detail_id,
        mime: output_mime,
        job_key,
        cache_key,
        src: opened.resolved_path.clone(),
        source_file: Some(Arc::new(opened.file)),
        dest: destination,
        args,
        fallback_args,
        continue_after_disconnect: false,
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
    original.target = if req.query.is_empty() {
        original.path.clone()
    } else {
        format!("{}?{}", original.path, req.query)
    };
    app.media(&original, false, peer)
}

fn web_media_id(path: &str) -> Option<i64> {
    let value = path.strip_prefix(WEB_MEDIA_PREFIX)?;
    rusty_dlna_protocol::paths::strtoll_prefix(value)
}

fn web_item_id(path: &str) -> Option<i64> {
    let value = path.strip_prefix(WEB_ITEM_PREFIX)?;
    rusty_dlna_protocol::paths::strtoll_prefix(value)
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

fn json_response(value: serde_json::Value) -> HttpResponse {
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    let mut response = bytes_response("application/json; charset=utf-8", &body, "no-store");
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
struct QueryParams(Vec<(String, String)>);

impl QueryParams {
    fn parse(query: &str) -> Self {
        Self(
            query
                .split('&')
                .filter(|part| !part.is_empty())
                .take(16)
                .map(|part| {
                    let (name, value) = part.split_once('=').unwrap_or((part, ""));
                    (percent_decode(name), percent_decode(value))
                })
                .collect(),
        )
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn usize(&self, name: &str) -> Option<usize> {
        self.get(name)?.parse().ok()
    }
}

fn percent_decode(value: &str) -> String {
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
                    decoded.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
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
        assert_eq!(params.usize("offset"), Some(20));
        assert_eq!(params.usize("limit"), Some(80));
    }

    #[test]
    fn media_id_accepts_decorative_extension() {
        assert_eq!(web_media_id("/web/media/42.mp4"), Some(42));
        assert_eq!(web_media_id("/web/media/nope.mp4"), None);
        assert_eq!(web_item_id("/api/web/item/42"), Some(42));
    }

    #[test]
    fn media_duration_is_bounded_whole_seconds() {
        assert_eq!(media_duration_seconds("2:03:35.776"), Some(7_415));
        assert_eq!(media_duration_seconds("not-a-duration"), None);
    }
}
