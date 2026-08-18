//! SOAP envelope, DIDL emit, and ContentDirectory dispatch.

mod filter;
mod search;
mod sort;

use rusty_dlna_protocol::object_id::{
    BROWSEDIR_ID, IMAGE_ALL_ID, IMAGE_CAMERA_ID, IMAGE_DATE_ID, IMAGE_DIR_ID, IMAGE_ID,
    MUSIC_ALBUM_ID, MUSIC_ALL_ID, MUSIC_ARTIST_ID, MUSIC_DIR_ID, MUSIC_GENRE_ID, MUSIC_ID,
    MUSIC_PLIST_ID, ROOT_ID, VIDEO_ALL_ID, VIDEO_DIR_ID, VIDEO_ID,
};
use rusty_dlna_protocol::soap::{
    soap_action_method, CONNECTIONMANAGER_TYPE, CONTENTDIRECTORY_TYPE, MS_REGISTRAR_TYPE,
    SEARCH_CAPS, SORT_CAPS,
};
use rusty_dlna_protocol::w3c_normalize_date;
use rusty_dlna_protocol::{ClientFlags, ClientKind, ClientProfile};

pub use filter::{didl_xmlns, parse_filter, FilterBits};
pub use search::{
    parse_search_criteria, row_matches, SearchClause, SearchQuery, SearchRow,
};
pub use sort::{default_order, parse_sort_criteria, sort_or_709, DefaultOrder, SortKey, SortSpec};

pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    xml_escape_into(s, &mut out);
    out
}

fn xml_escape_into(s: &str, out: &mut String) {
    if !s.bytes().any(|b| matches!(b, b'&' | b'<' | b'>' | b'"' | b'\'')) {
        out.push_str(s);
        return;
    }
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
}

/// rustyDLNA DIDL-inside-`<Result>`: escape `&` `<` `>` only. Attribute
/// quotes stay raw (`id="2$8"`). VLC's ixml / some Windows stacks treat
/// `childCount=&quot;…&quot;` as a file, not an expandable folder.
pub fn xml_escape_didl_result(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 8);
    xml_escape_didl_into(s, &mut out);
    out
}

fn xml_escape_didl_into(s: &str, out: &mut String) {
    if !s.bytes().any(|b| matches!(b, b'&' | b'<' | b'>')) {
        out.push_str(s);
        return;
    }
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

/// Text of `<tag>`, `<u:tag>`, or `<ns:tag>` (first match, case-insensitive).
pub fn xml_tag_text(hay: &str, tag: &str) -> Option<String> {
    let mut search = 0;
    while let Some(rel) = hay[search..].find('<') {
        let abs = search + rel;
        let after = &hay[abs + 1..];
        if after.starts_with('/') || after.starts_with('!') || after.starts_with('?') {
            search = abs + 1;
            continue;
        }
        let name_end = after
            .find(|c: char| c == '>' || c == ' ' || c == '/' || c == '\t')
            .unwrap_or(0);
        if name_end == 0 {
            search = abs + 1;
            continue;
        }
        let raw_name = &after[..name_end];
        let local = raw_name.rsplit(':').next().unwrap_or(raw_name);
        if !local.eq_ignore_ascii_case(tag) {
            search = abs + 1;
            continue;
        }
        let gt = after.find('>')?;
        if after.as_bytes().get(gt.saturating_sub(1)) == Some(&b'/') {
            return Some(String::new());
        }
        let content_start = abs + 1 + gt + 1;
        let rest = &hay[content_start..];
        let rel_end = find_close_tag(rest, raw_name)
            .or_else(|| find_close_tag(rest, tag))
            .or_else(|| find_close_tag(rest, local))?;
        return Some(hay[content_start..content_start + rel_end].to_string());
    }
    None
}

fn find_close_tag(hay: &str, name: &str) -> Option<usize> {
    let mut i = 0;
    while let Some(rel) = hay[i..].find("</") {
        let abs = i + rel;
        let after = &hay[abs + 2..];
        if after.len() >= name.len()
            && after[..name.len()].eq_ignore_ascii_case(name)
            && after.as_bytes().get(name.len()) == Some(&b'>')
        {
            return Some(abs);
        }
        i = abs + 2;
    }
    None
}

pub fn wrap_soap_success(body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body>{body}</s:Body></s:Envelope>\r\n"
    )
}

pub fn soap_fault(code: u16, desc: &str) -> String {
    format!(
        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><s:Fault>\
         <faultcode>s:Client</faultcode>\
         <faultstring>UPnPError</faultstring>\
         <detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
         <errorCode>{code}</errorCode>\
         <errorDescription>{desc}</errorDescription>\
         </UPnPError></detail></s:Fault></s:Body></s:Envelope>"
    )
}

/// `didl_inner` is raw (unescaped) DIDL children. Result is XML-escaped.
pub fn browse_response(
    didl_inner: &str,
    returned: u32,
    total: u32,
    update_id: u32,
    bits: &FilterBits,
) -> String {
    let xmlns = didl_xmlns(bits);
    let didl = format!("<DIDL-Lite{xmlns}>\n{didl_inner}</DIDL-Lite>");
    let body = format!(
        "<u:BrowseResponse xmlns:u=\"{CONTENTDIRECTORY_TYPE}\">\
         <Result>{}</Result>\n\
         <NumberReturned>{returned}</NumberReturned>\n\
         <TotalMatches>{total}</TotalMatches>\n\
         <UpdateID>{update_id}</UpdateID>\
         </u:BrowseResponse>",
        xml_escape_didl_result(&didl)
    );
    wrap_soap_success(&body)
}

pub fn search_response(
    didl_inner: &str,
    returned: u32,
    total: u32,
    update_id: u32,
    bits: &FilterBits,
) -> String {
    let xmlns = didl_xmlns(bits);
    let didl = format!("<DIDL-Lite{xmlns}>\n{didl_inner}</DIDL-Lite>");
    let body = format!(
        "<u:SearchResponse xmlns:u=\"{CONTENTDIRECTORY_TYPE}\">\
         <Result>{}</Result>\n\
         <NumberReturned>{returned}</NumberReturned>\n\
         <TotalMatches>{total}</TotalMatches>\n\
         <UpdateID>{update_id}</UpdateID>\
         </u:SearchResponse>",
        xml_escape_didl_result(&didl)
    );
    wrap_soap_success(&body)
}

pub fn method_from_header(action: &str) -> Option<&'static str> {
    soap_action_method(action)
}

#[derive(Clone, Debug)]
pub struct DidlRes {
    pub url: String,
    pub protocol_info: String,
    pub size: Option<u64>,
    pub duration: Option<String>,
    pub bitrate: Option<i64>,
    pub resolution: Option<String>,
    pub sample_frequency: Option<i64>,
    pub nr_audio_channels: Option<i64>,
    /// Filter `pv:subtitleFileType` — always `SRT` when set.
    pub pv_subtitle_type: Option<String>,
    /// Filter `pv:subtitleFileUri` — `/Captions/{id}.srt` (first caption).
    pub pv_subtitle_uri: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DidlCaption {
    pub ext: String,
    pub url: String,
}

#[derive(Clone, Debug)]
pub struct DidlObject {
    pub id: String,
    pub parent_id: String,
    pub title: String,
    pub class: String,
    pub date: Option<String>,
    pub restricted: bool,
    pub searchable: Option<bool>,
    pub child_count: Option<u32>,
    pub child_container_count: Option<u32>,
    pub is_container: bool,
    pub resources: Vec<DidlRes>,
    pub album_art_uri: Option<String>,
    /// Samsung `dlna:profileID="JPEG_TN"` on `upnp:albumArtURI`.
    pub album_art_profile: bool,
    pub creator: Option<String>,
    pub description: Option<String>,
    pub artist: Option<String>,
    pub actor: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub track: Option<i64>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    /// Indexed caption URLs for `sec:CaptionInfoEx` (`/Captions/{id}/{n}.{ext}`).
    pub captions: Vec<DidlCaption>,
    /// Raw seconds, or CONVERT_MS milliseconds. Emit when `Some`.
    pub last_playback_position: Option<i64>,
    /// `upnp:playbackCount` when watch count > 0.
    pub playback_count: Option<i64>,
    /// Samsung `sec:dcmInfo` (`CREATIONDATE=0,FOLDER={title},BM={sec}`).
    pub dcm_info: Option<String>,
    /// Virtual-view alias (`REF_ID`).
    pub ref_id: Option<String>,
    /// BrowseMetadata of `0`: `upnp:searchClass includeDerived="1"`.
    pub search_classes: Vec<String>,
    /// Sony `av:mediaClass` — `M` / `V` / `P`.
    pub av_media_class: Option<char>,
}

pub fn emit_didl_object(o: &DidlObject, bits: &FilterBits) -> String {
    if o.is_container {
        let mut s = format!(
            "<container id=\"{}\" parentID=\"{}\" restricted=\"{}\"",
            xml_escape(&o.id),
            xml_escape(&o.parent_id),
            if o.restricted { "1" } else { "0" }
        );
        if let Some(r) = o.ref_id.as_deref().filter(|v| !v.is_empty()) {
            s.push_str(&format!(" refID=\"{}\"", xml_escape(r)));
        }
        if let Some(sc) = o.searchable {
            s.push_str(&format!(" searchable=\"{}\"", if sc { "1" } else { "0" }));
        }
        if let Some(n) = o.child_count {
            s.push_str(&format!(" childCount=\"{n}\""));
        }
        if let Some(n) = o.child_container_count {
            s.push_str(&format!(" childContainerCount=\"{n}\""));
        }
        s.push('>');
        s.push_str(&format!(
            "<dc:title>{}</dc:title><upnp:class>object.{}</upnp:class>",
            xml_escape(&o.title),
            o.class
        ));
        // The dialect always emits this for storageFolder (upnpsoap.c
        // `strcmp(class+10, "storageFolder")`). Windows UPnP / VLC use it
        // with `<container>` + childCount as the folder expand marker.
        if o.class.contains("storageFolder") {
            s.push_str("<upnp:storageUsed>-1</upnp:storageUsed>");
        }
        for sc in &o.search_classes {
            s.push_str("<upnp:searchClass includeDerived=\"1\">");
            s.push_str(&xml_escape(sc));
            s.push_str("</upnp:searchClass>");
        }
        if let Some(c) = o.av_media_class {
            s.push_str(
                "<av:mediaClass xmlns:av=\"urn:schemas-sony-com:av\">",
            );
            s.push(c);
            s.push_str("</av:mediaClass>");
        }
        emit_album_art_uri(&mut s, o);
        s.push_str("</container>");
        s
    } else {
        let mut s = format!(
            "<item id=\"{}\" parentID=\"{}\" restricted=\"{}\"",
            xml_escape(&o.id),
            xml_escape(&o.parent_id),
            if o.restricted { "1" } else { "0" }
        );
        if let Some(r) = o.ref_id.as_deref().filter(|v| !v.is_empty()) {
            s.push_str(&format!(" refID=\"{}\"", xml_escape(r)));
        }
        s.push('>');
        s.push_str(&format!(
            "<dc:title>{}</dc:title><upnp:class>object.{}</upnp:class>",
            xml_escape(&o.title),
            o.class
        ));
        if bits.dc_date {
            if let Some(d) = &o.date {
                let nd = w3c_normalize_date(d);
                if !nd.is_empty() {
                    s.push_str(&format!("<dc:date>{}</dc:date>", xml_escape(&nd)));
                }
            }
        }
        emit_opt_tag(&mut s, "dc:creator", o.creator.as_deref());
        if let Some(desc) = o.description.as_deref().filter(|v| !v.is_empty()) {
            let cut = truncate_chars(desc, 384);
            emit_opt_tag(&mut s, "dc:description", Some(cut));
        }
        emit_opt_tag(&mut s, "upnp:artist", o.artist.as_deref());
        emit_opt_tag(&mut s, "upnp:actor", o.actor.as_deref());
        emit_opt_tag(&mut s, "upnp:album", o.album.as_deref());
        emit_opt_tag(&mut s, "upnp:genre", o.genre.as_deref());
        if o.class.contains("audio") {
            if let Some(n) = o.track {
                s.push_str(&format!(
                    "<upnp:originalTrackNumber>{n}</upnp:originalTrackNumber>"
                ));
            }
        }
        if let Some(n) = o.season {
            s.push_str(&format!("<upnp:episodeSeason>{n}</upnp:episodeSeason>"));
        }
        if let Some(n) = o.episode {
            s.push_str(&format!("<upnp:episodeNumber>{n}</upnp:episodeNumber>"));
        }
        if let Some(pos) = o.last_playback_position {
            s.push_str(&format!(
                "<upnp:lastPlaybackPosition>{pos}</upnp:lastPlaybackPosition>"
            ));
        }
        if bits.sec {
            if let Some(dcm) = o.dcm_info.as_deref().filter(|v| !v.is_empty()) {
                s.push_str("<sec:dcmInfo>");
                s.push_str(&xml_escape(dcm));
                s.push_str("</sec:dcmInfo>");
            }
        }
        if let Some(n) = o.playback_count.filter(|n| *n > 0) {
            s.push_str(&format!("<upnp:playbackCount>{n}</upnp:playbackCount>"));
        }
        for r in &o.resources {
            if !bits.res && !bits.pv {
                continue;
            }
            s.push_str("<res protocolInfo=\"");
            s.push_str(&xml_escape(&r.protocol_info));
            s.push('"');
            if bits.res_size {
                if let Some(sz) = r.size {
                    s.push_str(&format!(" size=\"{sz}\""));
                }
            }
            if bits.res_duration {
                if let Some(d) = &r.duration {
                    s.push_str(&format!(" duration=\"{}\"", xml_escape(d)));
                }
            }
            if bits.res_bitrate {
                if let Some(b) = r.bitrate {
                    s.push_str(&format!(" bitrate=\"{b}\""));
                }
            }
            if bits.res_sample {
                if let Some(sf) = r.sample_frequency {
                    s.push_str(&format!(" sampleFrequency=\"{sf}\""));
                }
            }
            if bits.res_channels {
                if let Some(ch) = r.nr_audio_channels {
                    s.push_str(&format!(" nrAudioChannels=\"{ch}\""));
                }
            }
            if bits.res_resolution {
                if let Some(res) = &r.resolution {
                    s.push_str(&format!(" resolution=\"{}\"", xml_escape(res)));
                }
            }
            if bits.pv {
                if let Some(t) = &r.pv_subtitle_type {
                    s.push_str(" pv:subtitleFileType=\"");
                    s.push_str(&xml_escape(t));
                    s.push('"');
                }
                if let Some(u) = &r.pv_subtitle_uri {
                    s.push_str(" pv:subtitleFileUri=\"");
                    s.push_str(&xml_escape(u));
                    s.push('"');
                }
            }
            s.push('>');
            s.push_str(&xml_escape(&r.url));
            s.push_str("</res>");
        }
        if bits.sec {
            for cap in &o.captions {
                s.push_str("<sec:CaptionInfoEx sec:type=\"");
                s.push_str(&xml_escape(&cap.ext));
                s.push_str("\">");
                s.push_str(&xml_escape(&cap.url));
                s.push_str("</sec:CaptionInfoEx>");
            }
        }
        emit_album_art_uri(&mut s, o);
        s.push_str("</item>");
        s
    }
}

fn emit_opt_tag(s: &mut String, tag: &str, val: Option<&str>) {
    let Some(v) = val.filter(|v| !v.is_empty()) else {
        return;
    };
    s.push('<');
    s.push_str(tag);
    s.push('>');
    s.push_str(&xml_escape(v));
    s.push_str("</");
    s.push_str(tag);
    s.push('>');
}

fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn emit_album_art_uri(s: &mut String, o: &DidlObject) {
    let Some(uri) = &o.album_art_uri else {
        return;
    };
    if o.album_art_profile {
        s.push_str("<upnp:albumArtURI dlna:profileID=\"JPEG_TN\">");
    } else {
        s.push_str("<upnp:albumArtURI>");
    }
    s.push_str(&xml_escape(uri));
    s.push_str("</upnp:albumArtURI>");
}

pub fn emit_didl(objects: &[DidlObject], bits: &FilterBits) -> String {
    let mut s = String::with_capacity(objects.len().saturating_mul(384));
    for o in objects {
        s.push_str(&emit_didl_object(o, bits));
    }
    s
}

#[derive(Clone, Debug)]
pub struct SoapCall {
    pub method: Option<&'static str>,
    pub object_id: Option<String>,
    pub browse_flag: Option<String>,
    pub starting_index: i32,
    pub requested_count: i32,
    pub search_criteria: Option<String>,
    pub pos_second: Option<i64>,
    pub connection_id: Option<String>,
    pub filter: Option<String>,
    pub current_tag_value: Option<String>,
    pub new_tag_value: Option<String>,
    pub sort_criteria: Option<String>,
    pub var_name: Option<String>,
}

pub fn parse_soap_call(action: &str, body: &str) -> SoapCall {
    let object_id = xml_tag_text(body, "ObjectID").or_else(|| xml_tag_text(body, "ContainerID"));
    let starting_index = xml_tag_text(body, "StartingIndex")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let requested_count = xml_tag_text(body, "RequestedCount")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    SoapCall {
        method: method_from_header(action),
        object_id,
        browse_flag: xml_tag_text(body, "BrowseFlag"),
        starting_index,
        requested_count,
        search_criteria: xml_tag_text(body, "SearchCriteria"),
        pos_second: xml_tag_text(body, "PosSecond").and_then(|s| s.trim().parse().ok()),
        connection_id: xml_tag_text(body, "ConnectionID"),
        filter: xml_tag_text(body, "Filter"),
        current_tag_value: xml_tag_text(body, "CurrentTagValue"),
        new_tag_value: xml_tag_text(body, "NewTagValue"),
        sort_criteria: xml_tag_text(body, "SortCriteria"),
        var_name: xml_tag_text(body, "varName"),
    }
}

#[derive(Clone, Debug)]
pub enum SoapOutcome {
    Ok(String),
    Fault {
        http: u16,
        code: u16,
        desc: &'static str,
        persist: bool,
    },
}

impl SoapOutcome {
    pub fn fault401() -> Self {
        Self::Fault {
            http: 500,
            code: 401,
            desc: "Invalid Action",
            persist: false,
        }
    }
    pub fn fault402() -> Self {
        Self::Fault {
            http: 500,
            code: 402,
            desc: "Invalid Args",
            persist: false,
        }
    }
    pub fn fault701() -> Self {
        Self::Fault {
            http: 500,
            code: 701,
            desc: "No such object error",
            persist: false,
        }
    }
    pub fn fault709() -> Self {
        Self::Fault {
            http: 500,
            code: 709,
            desc: "Unsupported or invalid sort criteria",
            persist: false,
        }
    }
    pub fn fault404() -> Self {
        Self::Fault {
            http: 500,
            code: 404,
            desc: "Invalid Var",
            persist: false,
        }
    }
}

/// Title hacks from `callback()` in `upnpsoap.c`.
pub fn apply_title_hack(title: &str, ext: &str, client: &ClientProfile, has_captions: bool) -> String {
    match client.kind {
        ClientKind::Lg | ClientKind::LgNetCast if has_captions => format!("{title}."),
        ClientKind::AsusOPlay if has_captions => {
            title.chars().take(23).collect()
        }
        ClientKind::HyundaiTv => format!("{title}.{ext}"),
        _ => title.to_string(),
    }
}

/// Toshiba / Sony BDP / Bravia extra `<res>` rows that still point at the original file.
pub fn extra_ci1_protocol_infos(
    kind: ClientKind,
    mime: &str,
    pn: Option<&str>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    match kind {
        ClientKind::ToshibaTv => {
            if let Some(pn) = pn {
                if pn.starts_with("MPEG_TS_HD_NA")
                    || pn.starts_with("MPEG_TS_SD_NA")
                    || pn.starts_with("AVC_TS_MP_HD_AC3")
                    || pn.starts_with("AVC_TS_HP_HD_AC3")
                {
                    out.push((
                        mime.to_string(),
                        "DLNA.ORG_PN=MPEG_PS_NTSC;DLNA.ORG_OP=01;DLNA.ORG_CI=1".into(),
                    ));
                }
            }
        }
        ClientKind::SonyBdp => {
            if let Some(pn) = pn {
                if pn.starts_with("AVC_TS") || pn.starts_with("MPEG_TS") {
                    if !pn.starts_with("MPEG_TS_SD_NA") {
                        out.push((
                            mime.to_string(),
                            "DLNA.ORG_PN=MPEG_TS_SD_NA;DLNA.ORG_OP=01;DLNA.ORG_CI=1".into(),
                        ));
                    }
                    if !pn.starts_with("MPEG_TS_SD_EU") {
                        out.push((
                            mime.to_string(),
                            "DLNA.ORG_PN=MPEG_TS_SD_EU;DLNA.ORG_OP=01;DLNA.ORG_CI=1".into(),
                        ));
                    }
                    return out;
                }
            }
            let rest = mime.strip_prefix("video/").unwrap_or(mime);
            if pn.is_some_and(|p| p.starts_with("AVC_MP4") || p.starts_with("MPEG4_P2_MP4"))
                || matches!(rest, "x-matroska" | "x-mkv" | "x-msvideo" | "mpeg")
            {
                if !pn.is_some_and(|p| p.starts_with("MPEG_PS_NTSC")) {
                    out.push((
                        "video/avi".into(),
                        "DLNA.ORG_PN=MPEG_PS_NTSC;DLNA.ORG_OP=01;DLNA.ORG_CI=1".into(),
                    ));
                }
                if !pn.is_some_and(|p| p.starts_with("MPEG_PS_PAL")) {
                    out.push((
                        "video/avi".into(),
                        "DLNA.ORG_PN=MPEG_PS_PAL;DLNA.ORG_OP=01;DLNA.ORG_CI=1".into(),
                    ));
                }
            }
        }
        ClientKind::SonyBravia => {
            if let Some(pn) = pn {
                if pn.starts_with("AVC_TS_MP_SD_AC3")
                    || pn.starts_with("AVC_TS_MP_HD_AC3")
                    || pn.starts_with("AVC_TS_HP_HD_AC3")
                {
                    let suffix = if pn.len() > 16 { &pn[16..] } else { "" };
                    out.push((
                        mime.to_string(),
                        format!("DLNA.ORG_PN=AVC_TS_HD_50_AC3{suffix}"),
                    ));
                }
            }
        }
        _ => {}
    }
    out
}

/// dialect `X_SetBookmark`: CONVERT_MS divides by 1000; values < 30 store as 0.
pub fn bookmark_seconds(pos: i64, convert_ms: bool) -> i64 {
    let sec = if convert_ms { pos / 1000 } else { pos };
    if sec < 30 {
        0
    } else {
        sec
    }
}

/// Tags parsed from UpdateObject `CurrentTagValue` / `NewTagValue`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpdateObjectTags {
    pub last_playback_position: Option<i64>,
    pub playback_count: Option<i64>,
}

/// MiniDLNA-style tag list: `<upnp:lastPlaybackPosition>N</…>` or `name=N`.
/// NewTagValue wins over CurrentTagValue. Unknown tags ignored.
pub fn parse_update_object_tags(current: Option<&str>, new: Option<&str>) -> UpdateObjectTags {
    let new = new.unwrap_or("");
    let current = current.unwrap_or("");
    let pick = |names: &[&str]| update_tag_i64(new, names).or_else(|| update_tag_i64(current, names));
    UpdateObjectTags {
        last_playback_position: pick(&["upnp:lastPlaybackPosition", "lastPlaybackPosition"]),
        playback_count: pick(&[
            "upnp:playbackCount",
            "upnp:playCount",
            "playbackCount",
            "playCount",
        ]),
    }
}

fn unescape_xml_light(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn parse_pos_or_count(v: &str) -> Option<i64> {
    let v = v.trim();
    if v.contains(':') {
        let parts: Vec<&str> = v.split(':').collect();
        if parts.len() == 3 {
            let h: i64 = parts[0].parse().ok()?;
            let m: i64 = parts[1].parse().ok()?;
            let s: i64 = parts[2].split('.').next()?.parse().ok()?;
            return Some(h.saturating_mul(3600) + m.saturating_mul(60) + s);
        }
    }
    v.parse().ok()
}

fn update_tag_i64(hay: &str, names: &[&str]) -> Option<i64> {
    if hay.is_empty() {
        return None;
    }
    let decoded = unescape_xml_light(hay);
    for tag in names {
        let local = tag.rsplit(':').next().unwrap_or(tag);
        if let Some(v) = xml_tag_text(&decoded, local) {
            if let Some(n) = parse_pos_or_count(&v) {
                return Some(n);
            }
        }
        for key in [*tag, local] {
            let pat = format!("{key}=");
            if let Some(i) = decoded.find(&pat) {
                let rest = decoded[i + pat.len()..].trim_start();
                let num: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '-')
                    .collect();
                if let Some(n) = parse_pos_or_count(&num) {
                    return Some(n);
                }
            }
        }
    }
    None
}

pub fn empty_cd_response(method: &str) -> String {
    ok_tag(method, CONTENTDIRECTORY_TYPE, "")
}

pub fn feature_list_ids(client: &ClientProfile, root_container: Option<&str>) -> [String; 3] {
    let rc = root_container.map(str::trim).filter(|s| !s.is_empty());
    if let Some(rc) = rc {
        if rc != BROWSEDIR_ID && rc != "64" {
            let one = match rc {
                "V" | "v" | "2" => VIDEO_ID,
                "A" | "1" => MUSIC_ID,
                "I" | "3" => IMAGE_ID,
                other => other,
            };
            return [one.into(), one.into(), one.into()];
        }
    }
    if client.flags.contains(ClientFlags::SAMSUNG_DCM10)
        && rc.map(|s| s == BROWSEDIR_ID || s == "64").unwrap_or(true)
    {
        if rc == Some(BROWSEDIR_ID) || rc == Some("64") {
            return ["1$14".into(), "2$15".into(), "3$16".into()];
        }
        return ["A".into(), "V".into(), "I".into()];
    }
    if rc == Some(BROWSEDIR_ID) || rc == Some("64") {
        return ["1$14".into(), "2$15".into(), "3$16".into()];
    }
    [MUSIC_ID.into(), VIDEO_ID.into(), IMAGE_ID.into()]
}

pub fn feature_list_xml(ids: [impl AsRef<str>; 3]) -> String {
    format!(
        "<Features xmlns=\"urn:schemas-upnp-org:av:avs\" \
         xmlns:sec=\"http://www.sec.co.kr/dlna\">\
         <Feature name=\"samsung.com_BASICVIEW\" version=\"1\">\
         <container id=\"{}\" type=\"object.item.audioItem\"/>\
         <container id=\"{}\" type=\"object.item.videoItem\"/>\
         <container id=\"{}\" type=\"object.item.imageItem\"/>\
         </Feature></Features>",
        ids[0].as_ref(),
        ids[1].as_ref(),
        ids[2].as_ref()
    )
}

/// MiniDLNA `RESOURCE_PROTOCOL_INFO_VALUES` (`upnpglobalvars.h`).
pub const PROTOCOL_INFO_SOURCE: &str = concat!(
    "http-get:*:image/jpeg:DLNA.ORG_PN=JPEG_TN,",
    "http-get:*:image/jpeg:DLNA.ORG_PN=JPEG_SM,",
    "http-get:*:image/jpeg:DLNA.ORG_PN=JPEG_MED,",
    "http-get:*:image/jpeg:DLNA.ORG_PN=JPEG_LRG,",
    "http-get:*:video/mpeg:DLNA.ORG_PN=AVC_TS_HD_50_AC3_ISO,",
    "http-get:*:video/mpeg:DLNA.ORG_PN=AVC_TS_HD_60_AC3_ISO,",
    "http-get:*:video/mpeg:DLNA.ORG_PN=AVC_TS_HP_HD_AC3_ISO,",
    "http-get:*:video/mpeg:DLNA.ORG_PN=AVC_TS_MP_HD_AAC_MULT5_ISO,",
    "http-get:*:video/mpeg:DLNA.ORG_PN=AVC_TS_MP_HD_AC3_ISO,",
    "http-get:*:video/mpeg:DLNA.ORG_PN=AVC_TS_MP_HD_MPEG1_L3_ISO,",
    "http-get:*:video/mpeg:DLNA.ORG_PN=AVC_TS_MP_SD_AAC_MULT5_ISO,",
    "http-get:*:video/mpeg:DLNA.ORG_PN=AVC_TS_MP_SD_AC3_ISO,",
    "http-get:*:video/mpeg:DLNA.ORG_PN=AVC_TS_MP_SD_MPEG1_L3_ISO,",
    "http-get:*:video/mpeg:DLNA.ORG_PN=MPEG_PS_NTSC,",
    "http-get:*:video/mpeg:DLNA.ORG_PN=MPEG_PS_PAL,",
    "http-get:*:video/mpeg:DLNA.ORG_PN=MPEG_TS_HD_NA_ISO,",
    "http-get:*:video/mpeg:DLNA.ORG_PN=MPEG_TS_SD_NA_ISO,",
    "http-get:*:video/mpeg:DLNA.ORG_PN=MPEG_TS_SD_EU_ISO,",
    "http-get:*:video/mpeg:DLNA.ORG_PN=MPEG1,",
    "http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_MP_SD_AAC_MULT5,",
    "http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_MP_SD_AC3,",
    "http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_BL_CIF15_AAC_520,",
    "http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_BL_CIF30_AAC_940,",
    "http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_BL_L31_HD_AAC,",
    "http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_BL_L32_HD_AAC,",
    "http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_BL_L3L_SD_AAC,",
    "http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_HP_HD_AAC,",
    "http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_MP_HD_1080i_AAC,",
    "http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_MP_HD_720p_AAC,",
    "http-get:*:video/mp4:DLNA.ORG_PN=MPEG4_P2_MP4_ASP_AAC,",
    "http-get:*:video/mp4:DLNA.ORG_PN=MPEG4_P2_MP4_SP_VGA_AAC,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=AVC_TS_HD_50_AC3,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=AVC_TS_HD_50_AC3_T,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=AVC_TS_HD_60_AC3,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=AVC_TS_HD_60_AC3_T,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=AVC_TS_HP_HD_AC3_T,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=AVC_TS_MP_HD_AAC_MULT5,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=AVC_TS_MP_HD_AAC_MULT5_T,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=AVC_TS_MP_HD_AC3,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=AVC_TS_MP_HD_AC3_T,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=AVC_TS_MP_HD_MPEG1_L3,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=AVC_TS_MP_HD_MPEG1_L3_T,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=AVC_TS_MP_SD_AAC_MULT5,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=AVC_TS_MP_SD_AAC_MULT5_T,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=AVC_TS_MP_SD_AC3,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=AVC_TS_MP_SD_AC3_T,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=AVC_TS_MP_SD_MPEG1_L3,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=AVC_TS_MP_SD_MPEG1_L3_T,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=MPEG_TS_HD_NA,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=MPEG_TS_HD_NA_T,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=MPEG_TS_SD_EU,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=MPEG_TS_SD_EU_T,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=MPEG_TS_SD_NA,",
    "http-get:*:video/vnd.dlna.mpeg-tts:DLNA.ORG_PN=MPEG_TS_SD_NA_T,",
    "http-get:*:video/x-ms-wmv:DLNA.ORG_PN=WMVSPLL_BASE,",
    "http-get:*:video/x-ms-wmv:DLNA.ORG_PN=WMVSPML_BASE,",
    "http-get:*:video/x-ms-wmv:DLNA.ORG_PN=WMVSPML_MP3,",
    "http-get:*:video/x-ms-wmv:DLNA.ORG_PN=WMVMED_BASE,",
    "http-get:*:video/x-ms-wmv:DLNA.ORG_PN=WMVMED_FULL,",
    "http-get:*:video/x-ms-wmv:DLNA.ORG_PN=WMVMED_PRO,",
    "http-get:*:video/x-ms-wmv:DLNA.ORG_PN=WMVHIGH_FULL,",
    "http-get:*:video/x-ms-wmv:DLNA.ORG_PN=WMVHIGH_PRO,",
    "http-get:*:video/3gpp:DLNA.ORG_PN=MPEG4_P2_3GPP_SP_L0B_AAC,",
    "http-get:*:video/3gpp:DLNA.ORG_PN=MPEG4_P2_3GPP_SP_L0B_AMR,",
    "http-get:*:audio/mpeg:DLNA.ORG_PN=MP3,",
    "http-get:*:audio/x-ms-wma:DLNA.ORG_PN=WMABASE,",
    "http-get:*:audio/x-ms-wma:DLNA.ORG_PN=WMAFULL,",
    "http-get:*:audio/x-ms-wma:DLNA.ORG_PN=WMAPRO,",
    "http-get:*:audio/x-ms-wma:DLNA.ORG_PN=WMALSL,",
    "http-get:*:audio/x-ms-wma:DLNA.ORG_PN=WMALSL_MULT5,",
    "http-get:*:audio/mp4:DLNA.ORG_PN=AAC_ISO_320,",
    "http-get:*:audio/3gpp:DLNA.ORG_PN=AAC_ISO_320,",
    "http-get:*:audio/mp4:DLNA.ORG_PN=AAC_ISO,",
    "http-get:*:audio/mp4:DLNA.ORG_PN=AAC_MULT5_ISO,",
    "http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM,",
    "http-get:*:image/jpeg:*,",
    "http-get:*:video/avi:*,",
    "http-get:*:video/divx:*,",
    "http-get:*:video/x-matroska:*,",
    "http-get:*:video/mpeg:*,",
    "http-get:*:video/mp4:*,",
    "http-get:*:video/x-ms-wmv:*,",
    "http-get:*:video/x-msvideo:*,",
    "http-get:*:video/x-flv:*,",
    "http-get:*:video/x-tivo-mpeg:*,",
    "http-get:*:video/quicktime:*,",
    "http-get:*:audio/mp4:*,",
    "http-get:*:audio/x-wav:*,",
    "http-get:*:audio/x-flac:*,",
    "http-get:*:audio/x-dsd:*,",
    "http-get:*:application/ogg:*",
    "http-get:*:application/vnd.rn-realmedia:*",
    "http-get:*:application/vnd.rn-realmedia-vbr:*",
    "http-get:*:video/webm:*"
);

fn ok_tag(method: &str, xmlns: &str, inner: &str) -> String {
    wrap_soap_success(&format!(
        "<u:{method}Response xmlns:u=\"{xmlns}\">{inner}</u:{method}Response>"
    ))
}

/// Catalog-independent SOAP methods. Browse/Search, `X_SetBookmark`, and
/// `UpdateObject` are built by the caller (need catalog / `BOOKMARKS`).
pub fn dispatch_simple(
    call: &SoapCall,
    client: &ClientProfile,
    uuid: &str,
    update_id: u32,
    root_container: Option<&str>,
) -> Option<SoapOutcome> {
    let method = call.method?;
    match method {
        "GetSearchCapabilities" => Some(SoapOutcome::Ok(ok_tag(
            method,
            CONTENTDIRECTORY_TYPE,
            &format!("<SearchCaps>{SEARCH_CAPS}</SearchCaps>"),
        ))),
        "GetSortCapabilities" => Some(SoapOutcome::Ok(ok_tag(
            method,
            CONTENTDIRECTORY_TYPE,
            &format!("<SortCaps>{SORT_CAPS}</SortCaps>"),
        ))),
        "GetSystemUpdateID" => Some(SoapOutcome::Ok(ok_tag(
            method,
            CONTENTDIRECTORY_TYPE,
            &format!("<Id>{update_id}</Id>"),
        ))),
        "GetProtocolInfo" => Some(SoapOutcome::Ok(ok_tag(
            method,
            CONNECTIONMANAGER_TYPE,
            &format!("<Source>{PROTOCOL_INFO_SOURCE}</Source><Sink></Sink>"),
        ))),
        "GetCurrentConnectionIDs" => Some(SoapOutcome::Ok(ok_tag(
            method,
            CONNECTIONMANAGER_TYPE,
            "<ConnectionIDs>0</ConnectionIDs>",
        ))),
        "GetCurrentConnectionInfo" => {
            let id = call.connection_id.as_deref().unwrap_or("");
            if id != "0" && !id.is_empty() {
                return Some(SoapOutcome::fault701());
            }
            Some(SoapOutcome::Ok(ok_tag(
                method,
                CONNECTIONMANAGER_TYPE,
                "<RcsID>-1</RcsID><AVTransportID>-1</AVTransportID>\
                 <ProtocolInfo></ProtocolInfo><PeerConnectionManager></PeerConnectionManager>\
                 <PeerConnectionID>-1</PeerConnectionID><Direction>Output</Direction>\
                 <Status>Unknown</Status>",
            )))
        }
        "IsAuthorized" | "IsValidated" => Some(SoapOutcome::Ok(ok_tag(
            method,
            MS_REGISTRAR_TYPE,
            "<Result>1</Result>",
        ))),
        "RegisterDevice" => Some(SoapOutcome::Ok(ok_tag(
            method,
            MS_REGISTRAR_TYPE,
            &format!("<RegistrationRespMsg>{}</RegistrationRespMsg>", xml_escape(uuid)),
        ))),
        "X_GetFeatureList" => {
            let ids = feature_list_ids(client, root_container);
            let feat = feature_list_xml(ids);
            Some(SoapOutcome::Ok(ok_tag(
                method,
                CONTENTDIRECTORY_TYPE,
                &format!("<FeatureList>{}</FeatureList>", xml_escape(&feat)),
            )))
        }
        "QueryStateVariable" => match call.var_name.as_deref() {
            None => Some(SoapOutcome::fault402()),
            Some("ConnectionStatus") => Some(SoapOutcome::Ok(ok_tag(
                method,
                "urn:schemas-upnp-org:control-1-0",
                "<return>Connected</return>",
            ))),
            Some(_) => Some(SoapOutcome::fault404()),
        },
        // Persist via the server catalog / `BOOKMARKS` path.
        "X_SetBookmark" | "UpdateObject" | "Browse" | "Search" => None,
        _ => Some(SoapOutcome::fault401()),
    }
}

pub fn build_browse(
    is_search: bool,
    objects: &[DidlObject],
    returned: u32,
    total: u32,
    update_id: u32,
    bits: &FilterBits,
) -> String {
    let inner = emit_didl(objects, bits);
    if is_search {
        search_response(&inner, returned, total, update_id, bits)
    } else {
        browse_response(&inner, returned, total, update_id, bits)
    }
}

pub fn magic_object_id(id: &str, client: &ClientProfile) -> String {
    if client.flags.contains(ClientFlags::MS_PFS) {
        if let Some(real) = rewrite_pfs_child(id) {
            return real;
        }
    }
    if client.flags.contains(ClientFlags::SAMSUNG_DCM10) {
        return match id {
            "A" => MUSIC_ID.to_string(),
            "V" => VIDEO_ID.to_string(),
            "I" => IMAGE_ID.to_string(),
            other => other.to_string(),
        };
    }
    if client.flags.contains(ClientFlags::AUDIO_ONLY) && id == ROOT_ID {
        return MUSIC_ID.to_string();
    }
    id.to_string()
}

/// Rewrite PFS short ids (`8` → `2$8`, `8$HEX` → `2$8$HEX`).
pub fn rewrite_pfs_child(id: &str) -> Option<String> {
    const MAP: &[(&str, &str)] = &[
        ("D2", IMAGE_CAMERA_ID),
        ("14", MUSIC_DIR_ID),
        ("15", VIDEO_DIR_ID),
        ("16", IMAGE_DIR_ID),
        ("4", MUSIC_ALL_ID),
        ("5", MUSIC_GENRE_ID),
        ("6", MUSIC_ARTIST_ID),
        ("7", MUSIC_ALBUM_ID),
        ("8", VIDEO_ALL_ID),
        ("B", IMAGE_ALL_ID),
        ("C", IMAGE_DATE_ID),
        ("F", MUSIC_PLIST_ID),
    ];
    for (short, real) in MAP {
        if id == *short {
            return Some((*real).to_string());
        }
        let prefix = format!("{short}$");
        if let Some(tail) = id.strip_prefix(&prefix) {
            return Some(format!("{real}${tail}"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_is_escaped_didl() {
        let xml = browse_response(
            "<container id=\"0\"/>",
            1,
            1,
            28,
            &FilterBits::standard(),
        );
        assert!(xml.contains("&lt;DIDL-Lite"));
        assert!(
            xml.contains("&lt;container id=\"0\"/&gt;"),
            "The dialect leaves attribute quotes raw in Result: {xml}"
        );
        assert!(xml.contains("xmlns:dc=\"http://purl.org/dc/elements/1.1/\""));
        assert!(xml.contains("<NumberReturned>1</NumberReturned>"));
        assert!(xml.contains("ContentDirectory:1"));
    }

    #[test]
    fn unknown_action() {
        assert_eq!(method_from_header("urn:x#Nope"), None);
        assert_eq!(
            method_from_header(r#""urn:schemas-upnp-org:service:ContentDirectory:1#Search""#),
            Some("Search")
        );
    }

    #[test]
    fn xml_tag_strips_prefix() {
        let body = r#"<u:Browse><ObjectID>0</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag></u:Browse>"#;
        assert_eq!(xml_tag_text(body, "ObjectID").as_deref(), Some("0"));
        assert_eq!(
            xml_tag_text(body, "BrowseFlag").as_deref(),
            Some("BrowseDirectChildren")
        );
    }

    #[test]
    fn bookmark_convert_ms() {
        assert_eq!(bookmark_seconds(120_000, true), 120);
        assert_eq!(bookmark_seconds(10, false), 0);
        assert_eq!(bookmark_seconds(45, false), 45);
        assert_eq!(bookmark_seconds(-1, false), 0);
        assert_eq!(bookmark_seconds(-1, true), 0);
    }

    #[test]
    fn dcm10_feature_ids_are_avi() {
        let tv = rusty_dlna_protocol::identify_user_agent("SEC_HHP_[TV]UE40D7000/1.0").unwrap();
        assert_eq!(
            feature_list_ids(tv, None),
            ["A".to_string(), "V".to_string(), "I".to_string()]
        );
        let pc = rusty_dlna_protocol::identify_user_agent("SEC_HHP_[PC]LPC001/1.0").unwrap();
        assert_eq!(
            feature_list_ids(pc, None),
            ["1".to_string(), "2".to_string(), "3".to_string()]
        );
        assert!(!pc.flags.contains(ClientFlags::SAMSUNG_DCM10));
    }

    #[test]
    fn container_didl_is_storage_folder() {
        let xml = emit_didl_object(
            &DidlObject {
                id: "2$15".into(),
                parent_id: "0".into(),
                title: "Folders".into(),
                class: "container.storageFolder".into(),
                date: None,
                restricted: true,
                searchable: Some(true),
                child_count: Some(1),
                child_container_count: Some(1),
                is_container: true,
                resources: vec![],
                album_art_uri: None,
                album_art_profile: false,
                creator: None,
                description: None,
                artist: None,
                actor: None,
                album: None,
                genre: None,
                track: None,
                season: None,
                episode: None,
                captions: vec![],
                last_playback_position: None,
                playback_count: None,
                dcm_info: None,
                ref_id: None,
                search_classes: vec![],
                av_media_class: None,
            },
            &FilterBits::standard(),
        );
        assert!(
            xml.starts_with("<container "),
            "folders must be DIDL containers, not items: {xml}"
        );
        assert!(xml.contains("id=\"2$15\""));
        assert!(xml.contains("parentID=\"0\""));
        assert!(xml.contains("restricted=\"1\""));
        assert!(xml.contains("searchable=\"1\""));
        assert!(xml.contains("childCount=\"1\""));
        assert!(xml.contains("<upnp:class>object.container.storageFolder</upnp:class>"));
        assert!(xml.contains("<upnp:storageUsed>-1</upnp:storageUsed>"));
        assert!(xml.ends_with("</container>"));
        assert!(!xml.contains("<item"));
        let wrapped = browse_response(&xml, 1, 1, 1, &FilterBits::standard());
        assert!(
            wrapped.contains("xmlns:dc=\"http://purl.org/dc/elements/1.1/\""),
            "{wrapped}"
        );
        assert!(
            !wrapped.contains("xmlns:dlna="),
            "standard Filter omits xmlns:dlna: {wrapped}"
        );
    }

    #[test]
    fn didl_namespaces_follow_filter_bits() {
        let samsung = parse_filter(Some("*"), true);
        let xml = browse_response("<item/>", 1, 1, 1, &samsung);
        assert!(xml.contains("xmlns:sec=\"http://www.sec.co.kr/dlna\""), "{xml}");
        assert!(
            xml.contains("xmlns:dlna=\"urn:schemas-dlna-org:metadata-1-0/\""),
            "{xml}"
        );
        assert!(!xml.contains("xmlns:pv="), "{xml}");

        let kodi = parse_filter(Some("*"), false);
        let xml = browse_response("<item/>", 1, 1, 1, &kodi);
        assert!(!xml.contains("xmlns:sec="), "{xml}");
        assert!(!xml.contains("xmlns:pv="), "{xml}");
        assert!(!xml.contains("xmlns:dlna="), "{xml}");

        let pv = parse_filter(Some("pv:subtitleFileUri"), false);
        let xml = browse_response("<item/>", 1, 1, 1, &pv);
        assert!(xml.contains("xmlns:pv=\"http://www.pv.com/pvns/\""), "{xml}");
    }

    #[test]
    fn parse_soap_call_reads_update_tags() {
        let body = r#"<u:UpdateObject><ObjectID>64$1</ObjectID><CurrentTagValue>old</CurrentTagValue><NewTagValue>&lt;upnp:playCount&gt;3&lt;/upnp:playCount&gt;</NewTagValue></u:UpdateObject>"#;
        let call = parse_soap_call("urn:x#UpdateObject", body);
        assert_eq!(call.object_id.as_deref(), Some("64$1"));
        assert_eq!(call.current_tag_value.as_deref(), Some("old"));
        assert!(
            call.new_tag_value
                .as_deref()
                .unwrap_or("")
                .contains("playCount")
        );
        let tags = parse_update_object_tags(call.current_tag_value.as_deref(), call.new_tag_value.as_deref());
        assert_eq!(tags.playback_count, Some(3));
        assert_eq!(
            parse_update_object_tags(
                None,
                Some("<upnp:lastPlaybackPosition>90</upnp:lastPlaybackPosition>")
            )
            .last_playback_position,
            Some(90)
        );
        assert_eq!(
            parse_update_object_tags(None, Some("upnp:lastPlaybackPosition=90,upnp:playCount=3"))
                .last_playback_position,
            Some(90)
        );
    }

    #[test]
    fn parse_soap_call_reads_filter() {
        let body = r#"<u:Browse><ObjectID>0</ObjectID><Filter>*</Filter></u:Browse>"#;
        let call = parse_soap_call(
            r#""urn:schemas-upnp-org:service:ContentDirectory:1#Browse""#,
            body,
        );
        assert_eq!(call.filter.as_deref(), Some("*"));
        let listed = r#"<Browse><Filter>dc:title,sec:CaptionInfoEx</Filter></Browse>"#;
        let call = parse_soap_call("urn:x#Browse", listed);
        assert_eq!(
            call.filter.as_deref(),
            Some("dc:title,sec:CaptionInfoEx")
        );
    }

    #[test]
    fn emit_sec_and_pv_follow_bits() {
        let obj = DidlObject {
            id: "64$1".into(),
            parent_id: "64".into(),
            title: "movie".into(),
            class: "item.videoItem".into(),
            date: Some("1999-01-01".into()),
            restricted: true,
            searchable: None,
            child_count: None,
            child_container_count: None,
            is_container: false,
            resources: vec![DidlRes {
                url: "http://127.0.0.1:18200/MediaItems/9.mkv".into(),
                protocol_info: "http-get:*:video/x-matroska:*".into(),
                size: Some(1),
                duration: None,
                bitrate: None,
                resolution: None,
                sample_frequency: None,
                nr_audio_channels: None,
                pv_subtitle_type: Some("SRT".into()),
                pv_subtitle_uri: Some("http://127.0.0.1:18200/Captions/9.srt".into()),
            }],
            album_art_uri: None,
            album_art_profile: false,
            creator: None,
            description: None,
            artist: None,
            actor: None,
            album: None,
            genre: None,
            track: None,
            season: None,
            episode: None,
            captions: vec![DidlCaption {
                ext: "srt".into(),
                url: "http://127.0.0.1:18200/Captions/9/0.srt".into(),
            }],
            last_playback_position: Some(120),
            playback_count: Some(3),
            dcm_info: Some("CREATIONDATE=0,FOLDER=movie,BM=120".into()),
            ref_id: None,
            search_classes: vec![],
            av_media_class: None,
        };
        let star = parse_filter(Some("*"), false);
        let kodi = emit_didl_object(&obj, &star);
        assert!(kodi.contains("<dc:date>1999-01-01</dc:date>"));
        assert!(!kodi.contains("sec:CaptionInfoEx"));
        assert!(!kodi.contains("pv:subtitle"));
        assert!(kodi.contains("<upnp:lastPlaybackPosition>120</upnp:lastPlaybackPosition>"));
        assert!(kodi.contains("<upnp:playbackCount>3</upnp:playbackCount>"));
        assert!(!kodi.contains("sec:dcmInfo"));

        let tv = emit_didl_object(&obj, &parse_filter(Some("*"), true));
        assert!(tv.contains("<sec:CaptionInfoEx sec:type=\"srt\">"));
        assert!(tv.contains("/Captions/9/0.srt"));
        assert!(!tv.contains("pv:subtitle"));
        assert!(tv.contains("<sec:dcmInfo>CREATIONDATE=0,FOLDER=movie,BM=120</sec:dcmInfo>"));

        let pv = emit_didl_object(&obj, &parse_filter(Some("pv:subtitleFileType"), false));
        assert!(pv.contains("pv:subtitleFileType=\"SRT\""));
        assert!(pv.contains("pv:subtitleFileUri=\"http://127.0.0.1:18200/Captions/9.srt\""));
        assert!(!pv.contains("sec:CaptionInfoEx"));
        assert!(!pv.contains("<dc:date>"));
    }

    fn dummy_item(ref_id: Option<&str>, search: Vec<String>) -> DidlObject {
        DidlObject {
            id: "2$9$1".into(),
            parent_id: "2$9".into(),
            title: "alias".into(),
            class: "item.videoItem".into(),
            date: None,
            restricted: true,
            searchable: None,
            child_count: None,
            child_container_count: None,
            is_container: false,
            resources: vec![],
            album_art_uri: None,
            album_art_profile: false,
            creator: None,
            description: None,
            artist: None,
            actor: None,
            album: None,
            genre: None,
            track: None,
            season: None,
            episode: None,
            captions: vec![],
            last_playback_position: None,
            playback_count: None,
            dcm_info: None,
            ref_id: ref_id.map(str::to_string),
            search_classes: search,
            av_media_class: None,
        }
    }

    #[test]
    fn pfs_xbox_eight_rewrites_to_video_all() {
        let xbox = rusty_dlna_protocol::identify_user_agent("Xbox/360").unwrap();
        assert!(xbox.flags.contains(ClientFlags::MS_PFS));
        assert_eq!(rewrite_pfs_child("8").as_deref(), Some("2$8"));
        assert_eq!(rewrite_pfs_child("8$ABC").as_deref(), Some("2$8$ABC"));
        assert_eq!(magic_object_id("8", xbox), "2$8");
        let kodi = rusty_dlna_protocol::identify_user_agent("Kodi/21.0").unwrap();
        assert_eq!(magic_object_id("8", kodi), "8");
    }

    #[test]
    fn feature_list_dcm10_avi_and_non64_collapse() {
        let tv = rusty_dlna_protocol::identify_user_agent("SEC_HHP_[TV]UE40D7000/1.0").unwrap();
        assert_eq!(
            feature_list_ids(tv, None),
            ["A".to_string(), "V".to_string(), "I".to_string()]
        );
        let collapsed = feature_list_ids(tv, Some("V"));
        assert_eq!(collapsed[0], collapsed[1]);
        assert_eq!(collapsed[1], collapsed[2]);
        assert!(
            collapsed[0] == "2" || collapsed[0] == "V",
            "non-64 root_container collapses FeatureList: {collapsed:?}"
        );
        assert_eq!(
            feature_list_ids(tv, Some("64")),
            ["1$14".to_string(), "2$15".to_string(), "3$16".to_string()]
        );
        let generic = rusty_dlna_protocol::identify_user_agent("DLNADOC/1.50").unwrap();
        let gen = feature_list_ids(generic, Some("V"));
        assert_eq!(gen[0], gen[1]);
        assert_eq!(gen[1], gen[2]);
    }

    #[test]
    fn extra_ci1_protocol_infos_toshiba_sony_bdp_bravia() {
        let toshiba = extra_ci1_protocol_infos(
            ClientKind::ToshibaTv,
            "video/mpeg",
            Some("MPEG_TS_HD_NA"),
        );
        assert!(
            toshiba.iter().any(|(_, info)| {
                info.contains("DLNA.ORG_PN=MPEG_PS_NTSC") && info.contains("DLNA.ORG_CI=1")
            }),
            "{toshiba:?}"
        );
        let none = extra_ci1_protocol_infos(ClientKind::ToshibaTv, "video/mpeg", Some("MPEG_PS_NTSC"));
        assert!(none.is_empty(), "{none:?}");

        let bdp_ts = extra_ci1_protocol_infos(
            ClientKind::SonyBdp,
            "video/mpeg",
            Some("AVC_TS_MP_HD_AC3"),
        );
        assert!(
            bdp_ts.iter().any(|(_, info)| info.contains("MPEG_TS_SD_NA")
                && info.contains("DLNA.ORG_CI=1")),
            "{bdp_ts:?}"
        );
        assert!(
            bdp_ts.iter().any(|(_, info)| info.contains("MPEG_TS_SD_EU")
                && info.contains("DLNA.ORG_CI=1")),
            "{bdp_ts:?}"
        );
        let bdp_mkv = extra_ci1_protocol_infos(
            ClientKind::SonyBdp,
            "video/x-matroska",
            None,
        );
        assert!(
            bdp_mkv.iter().any(|(m, info)| {
                m == "video/avi" && info.contains("MPEG_PS_NTSC") && info.contains("CI=1")
            }),
            "{bdp_mkv:?}"
        );
        assert!(
            bdp_mkv.iter().any(|(m, info)| {
                m == "video/avi" && info.contains("MPEG_PS_PAL") && info.contains("CI=1")
            }),
            "{bdp_mkv:?}"
        );

        let bravia = extra_ci1_protocol_infos(
            ClientKind::SonyBravia,
            "video/mpeg",
            Some("AVC_TS_MP_HD_AC3_T"),
        );
        assert!(
            bravia
                .iter()
                .any(|(_, info)| info.contains("DLNA.ORG_PN=AVC_TS_HD_50_AC3_T")),
            "{bravia:?}"
        );
    }

    #[test]
    fn apply_title_hack_lg_asus_hyundai() {
        let lg = rusty_dlna_protocol::identify_user_agent("LGE_DLNA_SDK/1.6.0").unwrap();
        assert_eq!(apply_title_hack("Fixture", "mkv", lg, true), "Fixture.");
        assert_eq!(apply_title_hack("Fixture", "mkv", lg, false), "Fixture");

        let asus = rusty_dlna_protocol::identify_user_agent("O!Play Mini").unwrap();
        let long = "012345678901234567890123456789";
        assert_eq!(apply_title_hack(long, "mkv", asus, true), &long[..23]);
        assert_eq!(apply_title_hack(long, "mkv", asus, false), long);

        let hyundai = rusty_dlna_protocol::identify_friendly_name("HYUNDAITV").unwrap();
        assert_eq!(
            apply_title_hack("Fixture", "mkv", hyundai, false),
            "Fixture.mkv"
        );
    }

    #[test]
    fn query_state_variable_connection_status_missing_unknown() {
        let client = rusty_dlna_protocol::identify_user_agent("Kodi/21.0").unwrap();
        let connected = parse_soap_call(
            "urn:schemas-upnp-org:control-1-0#QueryStateVariable",
            "<QueryStateVariable><varName>ConnectionStatus</varName></QueryStateVariable>",
        );
        match dispatch_simple(&connected, client, "uuid:x", 1, None) {
            Some(SoapOutcome::Ok(xml)) => {
                assert!(xml.contains("<return>Connected</return>"), "{xml}");
            }
            other => panic!("ConnectionStatus: {other:?}"),
        }
        let missing = parse_soap_call(
            "urn:schemas-upnp-org:control-1-0#QueryStateVariable",
            "<QueryStateVariable></QueryStateVariable>",
        );
        match dispatch_simple(&missing, client, "uuid:x", 1, None) {
            Some(SoapOutcome::Fault { code: 402, .. }) => {}
            other => panic!("missing varName: {other:?}"),
        }
        let unknown = parse_soap_call(
            "urn:schemas-upnp-org:control-1-0#QueryStateVariable",
            "<QueryStateVariable><varName>Nope</varName></QueryStateVariable>",
        );
        match dispatch_simple(&unknown, client, "uuid:x", 1, None) {
            Some(SoapOutcome::Fault { code: 404, .. }) => {}
            other => panic!("unknown var: {other:?}"),
        }
    }

    #[test]
    fn didl_emits_search_class_and_refid() {
        let root = DidlObject {
            id: "0".into(),
            parent_id: "-1".into(),
            title: "root".into(),
            class: "container.storageFolder".into(),
            date: None,
            restricted: true,
            searchable: Some(true),
            child_count: Some(3),
            child_container_count: Some(3),
            is_container: true,
            resources: vec![],
            album_art_uri: None,
            album_art_profile: false,
            creator: None,
            description: None,
            artist: None,
            actor: None,
            album: None,
            genre: None,
            track: None,
            season: None,
            episode: None,
            captions: vec![],
            last_playback_position: None,
            playback_count: None,
            dcm_info: None,
            ref_id: None,
            search_classes: vec![
                "object.item.audioItem".into(),
                "object.item.imageItem".into(),
                "object.item.videoItem".into(),
            ],
            av_media_class: None,
        };
        let xml = emit_didl_object(&root, &FilterBits::standard());
        assert!(
            xml.contains("<upnp:searchClass includeDerived=\"1\">object.item.audioItem</upnp:searchClass>"),
            "{xml}"
        );
        assert!(xml.contains("object.item.imageItem"), "{xml}");
        assert!(xml.contains("object.item.videoItem"), "{xml}");

        let alias = dummy_item(Some("64$1"), vec![]);
        let xml = emit_didl_object(&alias, &FilterBits::standard());
        assert!(xml.contains("refID=\"64$1\""), "{xml}");
    }
}
