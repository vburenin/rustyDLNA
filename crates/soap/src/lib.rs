//! SOAP envelope, DIDL emit, and ContentDirectory dispatch.

use rusty_dlna_protocol::object_id::{BROWSEDIR_ID, IMAGE_ID, MUSIC_ID, ROOT_ID, VIDEO_ID};
use rusty_dlna_protocol::soap::{
    soap_action_method, CONNECTIONMANAGER_TYPE, CONTENTDIRECTORY_TYPE, DIDL_SCHEMAS,
    MS_REGISTRAR_TYPE, SEARCH_CAPS, SORT_CAPS,
};
use rusty_dlna_protocol::w3c_normalize_date;
use rusty_dlna_protocol::{ClientFlags, ClientProfile};

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
pub fn browse_response(didl_inner: &str, returned: u32, total: u32, update_id: u32) -> String {
    let didl = format!("<DIDL-Lite{DIDL_SCHEMAS}>\n{didl_inner}</DIDL-Lite>");
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

pub fn search_response(didl_inner: &str, returned: u32, total: u32, update_id: u32) -> String {
    let didl = format!("<DIDL-Lite{DIDL_SCHEMAS}>\n{didl_inner}</DIDL-Lite>");
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
}

pub fn emit_didl_object(o: &DidlObject) -> String {
    if o.is_container {
        let mut s = format!(
            "<container id=\"{}\" parentID=\"{}\" restricted=\"{}\"",
            xml_escape(&o.id),
            xml_escape(&o.parent_id),
            if o.restricted { "1" } else { "0" }
        );
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
        s.push_str("</container>");
        s
    } else {
        let mut s = format!(
            "<item id=\"{}\" parentID=\"{}\" restricted=\"{}\">",
            xml_escape(&o.id),
            xml_escape(&o.parent_id),
            if o.restricted { "1" } else { "0" }
        );
        s.push_str(&format!(
            "<dc:title>{}</dc:title><upnp:class>object.{}</upnp:class>",
            xml_escape(&o.title),
            o.class
        ));
        if let Some(d) = &o.date {
            let nd = w3c_normalize_date(d);
            if !nd.is_empty() {
                s.push_str(&format!("<dc:date>{}</dc:date>", xml_escape(&nd)));
            }
        }
        for r in &o.resources {
            s.push_str("<res protocolInfo=\"");
            s.push_str(&xml_escape(&r.protocol_info));
            s.push('"');
            if let Some(sz) = r.size {
                s.push_str(&format!(" size=\"{sz}\""));
            }
            if let Some(d) = &r.duration {
                s.push_str(&format!(" duration=\"{}\"", xml_escape(d)));
            }
            if let Some(b) = r.bitrate {
                s.push_str(&format!(" bitrate=\"{b}\""));
            }
            if let Some(sf) = r.sample_frequency {
                s.push_str(&format!(" sampleFrequency=\"{sf}\""));
            }
            if let Some(ch) = r.nr_audio_channels {
                s.push_str(&format!(" nrAudioChannels=\"{ch}\""));
            }
            if let Some(res) = &r.resolution {
                s.push_str(&format!(" resolution=\"{}\"", xml_escape(res)));
            }
            s.push('>');
            s.push_str(&xml_escape(&r.url));
            s.push_str("</res>");
        }
        s.push_str("</item>");
        s
    }
}

pub fn emit_didl(objects: &[DidlObject]) -> String {
    let mut s = String::with_capacity(objects.len().saturating_mul(384));
    for o in objects {
        s.push_str(&emit_didl_object(o));
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

pub fn feature_list_ids(client: &ClientProfile, root_container: Option<&str>) -> [&'static str; 3] {
    if let Some(rc) = root_container {
        if rc != BROWSEDIR_ID && rc != "64" {
            // single container override — caller maps; default below
        }
    }
    if client.flags.contains(ClientFlags::SAMSUNG_DCM10)
        && root_container.map(|s| s == BROWSEDIR_ID || s == "64" || s.is_empty()) != Some(false)
        && root_container != Some("2")
        && root_container != Some("1")
    {
        if root_container == Some(BROWSEDIR_ID) || root_container == Some("64") {
            return ["1$14", "2$15", "3$16"];
        }
        return ["A", "V", "I"];
    }
    if root_container == Some(BROWSEDIR_ID) || root_container == Some("64") {
        return ["1$14", "2$15", "3$16"];
    }
    [MUSIC_ID, VIDEO_ID, IMAGE_ID]
}

pub fn feature_list_xml(ids: [&str; 3]) -> String {
    format!(
        "<Features xmlns=\"urn:schemas-upnp-org:av:avs\" \
         xmlns:sec=\"http://www.sec.co.kr/dlna\">\
         <Feature name=\"samsung.com_BASICVIEW\" version=\"1\">\
         <container id=\"{}\" type=\"object.item.audioItem\"/>\
         <container id=\"{}\" type=\"object.item.videoItem\"/>\
         <container id=\"{}\" type=\"object.item.imageItem\"/>\
         </Feature></Features>",
        ids[0], ids[1], ids[2]
    )
}

pub const PROTOCOL_INFO_SOURCE: &str = concat!(
    "http-get:*:video/mpeg:DLNA.ORG_PN=MPEG_PS_NTSC,",
    "http-get:*:video/mpeg:DLNA.ORG_PN=MPEG_PS_PAL,",
    "http-get:*:video/x-matroska:*,",
    "http-get:*:video/mp4:*,",
    "http-get:*:video/x-mkv:*,",
    "http-get:*:audio/mpeg:*,",
    "http-get:*:image/jpeg:*"
);

fn ok_tag(method: &str, xmlns: &str, inner: &str) -> String {
    wrap_soap_success(&format!(
        "<u:{method}Response xmlns:u=\"{xmlns}\">{inner}</u:{method}Response>"
    ))
}

/// Catalog-independent SOAP methods. Browse/Search are built by the caller
/// via [`build_browse`].
pub fn dispatch_simple(
    call: &SoapCall,
    client: &ClientProfile,
    uuid: &str,
    update_id: u32,
    root_container: Option<&str>,
    bookmarks: Option<&mut std::collections::HashMap<String, i64>>,
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
        "X_SetBookmark" => {
            let Some(oid) = &call.object_id else {
                return Some(SoapOutcome::fault402());
            };
            let pos = call.pos_second.unwrap_or(0);
            let sec = bookmark_seconds(pos, client.flags.contains(ClientFlags::CONVERT_MS));
            if let Some(map) = bookmarks {
                map.insert(oid.clone(), sec);
            }
            Some(SoapOutcome::Ok(ok_tag(method, CONTENTDIRECTORY_TYPE, "")))
        }
        "QueryStateVariable" | "UpdateObject" => {
            Some(SoapOutcome::Ok(ok_tag(method, CONTENTDIRECTORY_TYPE, "")))
        }
        "Browse" | "Search" => None,
        _ => Some(SoapOutcome::fault401()),
    }
}

pub fn build_browse(
    is_search: bool,
    objects: &[DidlObject],
    returned: u32,
    total: u32,
    update_id: u32,
) -> String {
    let inner = emit_didl(objects);
    if is_search {
        search_response(&inner, returned, total, update_id)
    } else {
        browse_response(&inner, returned, total, update_id)
    }
}

pub fn magic_object_id(id: &str, client: &ClientProfile) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_is_escaped_didl() {
        let xml = browse_response("<container id=\"0\"/>", 1, 1, 28);
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
    }

    #[test]
    fn dcm10_feature_ids_are_avi() {
        let tv = rusty_dlna_protocol::identify_user_agent("SEC_HHP_[TV]UE40D7000/1.0").unwrap();
        assert_eq!(feature_list_ids(tv, None), ["A", "V", "I"]);
        let pc = rusty_dlna_protocol::identify_user_agent("SEC_HHP_[PC]LPC001/1.0").unwrap();
        assert_eq!(feature_list_ids(pc, None), ["1", "2", "3"]);
        assert!(!pc.flags.contains(ClientFlags::SAMSUNG_DCM10));
    }

    #[test]
    fn container_didl_is_storage_folder() {
        let xml = emit_didl_object(&DidlObject {
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
        });
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
        let wrapped = browse_response(&xml, 1, 1, 1);
        assert!(
            wrapped.contains("xmlns:dlna=\"urn:schemas-dlna-org:metadata-1-0/\""),
            "{wrapped}"
        );
    }
}
