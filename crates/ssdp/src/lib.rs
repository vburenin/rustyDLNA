//! rustyDLNA SSDP packet builders. Socket ownership lives in the server crate.

use rusty_dlna_protocol::paths::ROOTDESC_PATH;
use rusty_dlna_protocol::ssdp::{
    known_service_types, notify_max_age, NTS_ALIVE, NTS_BYEBYE, SSDP_MCAST_ADDR, SSDP_PORT,
};
use rusty_dlna_protocol::{is_http_field_value, is_http_token, trim_http_ows};

/// An outbound SSDP packet cannot be represented safely.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SsdpBuildError {
    /// The UUID is empty or cannot safely appear in an SSDP line.
    InvalidUuid,
    /// The advertised host is empty or cannot safely appear in `LOCATION`.
    InvalidHost,
    /// The `SERVER` field is empty or contains a forbidden byte.
    InvalidServer,
    /// The `DATE` field is empty or contains a forbidden byte.
    InvalidDate,
    /// The requested search target contains a forbidden byte.
    InvalidSearchTarget,
    /// The selected service type does not exist.
    ServiceTypeIndexOutOfRange { index: usize, available: usize },
}

impl std::fmt::Display for SsdpBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUuid => f.write_str("invalid SSDP UUID"),
            Self::InvalidHost => f.write_str("invalid SSDP host"),
            Self::InvalidServer => f.write_str("invalid SSDP SERVER value"),
            Self::InvalidDate => f.write_str("invalid SSDP DATE value"),
            Self::InvalidSearchTarget => f.write_str("invalid SSDP search target"),
            Self::ServiceTypeIndexOutOfRange { index, available } => write!(
                f,
                "SSDP service type index {index} is outside {available} available types"
            ),
        }
    }
}

impl std::error::Error for SsdpBuildError {}

fn valid_line_component(value: &str) -> bool {
    !value.is_empty()
        && is_http_field_value(value)
        && !value.bytes().any(|byte| matches!(byte, b' ' | b'\t'))
}

fn valid_required_field(value: &str) -> bool {
    !trim_http_ows(value).is_empty() && is_http_field_value(value)
}

fn usn(uuid: &str, st: &str, index: usize) -> String {
    if index == 0 {
        uuid.to_string()
    } else {
        format!("{uuid}::{st}")
    }
}

/// Unsolicited NOTIFY (rustyDLNA uses no space after `HOST:`, `NT:`, …).
pub fn notify_alive(
    uuid: &str,
    host: &str,
    port: u16,
    notify_interval: u32,
    server: &str,
) -> Vec<String> {
    try_notify_alive(uuid, host, port, notify_interval, server).unwrap_or_default()
}

/// Fallible form of [`notify_alive`] that rejects unsafe line values.
pub fn try_notify_alive(
    uuid: &str,
    host: &str,
    port: u16,
    notify_interval: u32,
    server: &str,
) -> Result<Vec<String>, SsdpBuildError> {
    if !valid_line_component(uuid) {
        return Err(SsdpBuildError::InvalidUuid);
    }
    if !valid_line_component(host) {
        return Err(SsdpBuildError::InvalidHost);
    }
    if !valid_required_field(server) {
        return Err(SsdpBuildError::InvalidServer);
    }
    let lifetime = notify_max_age(notify_interval);
    Ok(known_service_types(uuid)
        .into_iter()
        .enumerate()
        .map(|(i, st)| {
            format!(
                "NOTIFY * HTTP/1.1\r\n\
                 HOST:{SSDP_MCAST_ADDR}:{SSDP_PORT}\r\n\
                 CACHE-CONTROL:max-age={lifetime}\r\n\
                 LOCATION:http://{host}:{port}{ROOTDESC_PATH}\r\n\
                 SERVER: {server}\r\n\
                 NT:{st}\r\n\
                 USN:{}\r\n\
                 NTS:{NTS_ALIVE}\r\n\
                 \r\n",
                usn(uuid, st, i)
            )
        })
        .collect())
}

pub fn notify_byebye(uuid: &str) -> Vec<String> {
    try_notify_byebye(uuid).unwrap_or_default()
}

/// Fallible form of [`notify_byebye`] that rejects unsafe UUIDs.
pub fn try_notify_byebye(uuid: &str) -> Result<Vec<String>, SsdpBuildError> {
    if !valid_line_component(uuid) {
        return Err(SsdpBuildError::InvalidUuid);
    }
    Ok(known_service_types(uuid)
        .into_iter()
        .enumerate()
        .map(|(i, st)| {
            format!(
                "NOTIFY * HTTP/1.1\r\n\
                 HOST:{SSDP_MCAST_ADDR}:{SSDP_PORT}\r\n\
                 NT:{st}\r\n\
                 USN:{}\r\n\
                 NTS:{NTS_BYEBYE}\r\n\
                 \r\n",
                usn(uuid, st, i)
            )
        })
        .collect())
}

pub fn msearch_ok(
    uuid: &str,
    st_index: usize,
    host: &str,
    port: u16,
    notify_interval: u32,
    server: &str,
    date: &str,
) -> String {
    try_msearch_ok(uuid, st_index, host, port, notify_interval, server, date).unwrap_or_default()
}

/// Fallible form of [`msearch_ok`] that rejects unsafe values and invalid
/// service-type indices.
pub fn try_msearch_ok(
    uuid: &str,
    st_index: usize,
    host: &str,
    port: u16,
    notify_interval: u32,
    server: &str,
    date: &str,
) -> Result<String, SsdpBuildError> {
    if !valid_line_component(uuid) {
        return Err(SsdpBuildError::InvalidUuid);
    }
    if !valid_line_component(host) {
        return Err(SsdpBuildError::InvalidHost);
    }
    if !valid_required_field(server) {
        return Err(SsdpBuildError::InvalidServer);
    }
    if !valid_required_field(date) {
        return Err(SsdpBuildError::InvalidDate);
    }
    let types = known_service_types(uuid);
    let st = *types
        .get(st_index)
        .ok_or(SsdpBuildError::ServiceTypeIndexOutOfRange {
            index: st_index,
            available: types.len(),
        })?;
    Ok(format!(
        "HTTP/1.1 200 OK\r\n\
         CACHE-CONTROL: max-age={}\r\n\
         DATE: {date}\r\n\
         ST: {st}\r\n\
         USN: {}\r\n\
         EXT:\r\n\
         SERVER: {server}\r\n\
         LOCATION: http://{host}:{port}{ROOTDESC_PATH}\r\n\
         Content-Length: 0\r\n\
         \r\n",
        notify_max_age(notify_interval),
        usn(uuid, st, st_index)
    ))
}

pub fn man_is_discover(man: &str) -> bool {
    man.trim() == rusty_dlna_protocol::ssdp::MAN_DISCOVER
}

/// Parsed M-SEARCH. rustyDLNA requires the exact standard request line, a
/// non-empty ST, MAN exactly `"ssdp:discover"`, and MX as an integer >= 0.
/// Repeated relevant headers must carry identical trimmed values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MSearch {
    pub st: String,
    pub mx: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MSearchReject {
    NotMsearch,
    NotHttp11,
    MissingSt,
    BadMan,
    BadMx,
}

fn has_lone_carriage_return(packet: &str) -> bool {
    packet
        .as_bytes()
        .iter()
        .enumerate()
        .any(|(index, byte)| *byte == b'\r' && packet.as_bytes().get(index + 1) != Some(&b'\n'))
}

fn set_unique_header(slot: &mut Option<String>, value: &str) -> bool {
    match slot {
        Some(existing) => existing == value,
        None => {
            *slot = Some(value.to_string());
            true
        }
    }
}

fn request_line_matches(line: &str, method: &str) -> bool {
    if line.starts_with([' ', '\t']) || line.ends_with([' ', '\t']) {
        return false;
    }
    let mut tokens = line.split([' ', '\t']).filter(|token| !token.is_empty());
    tokens.next() == Some(method)
        && tokens.next() == Some("*")
        && tokens.next() == Some("HTTP/1.1")
        && tokens.next().is_none()
}

fn request_line_method(line: &str) -> Option<&str> {
    line.split([' ', '\t']).find(|token| !token.is_empty())
}

fn parse_header_line(line: &str) -> Option<(&str, &str)> {
    let (name, value) = line.split_once(':')?;
    if !is_http_token(name) {
        return None;
    }
    if !is_http_field_value(value) {
        return None;
    }
    Some((name, trim_http_ows(value)))
}

fn is_blank_header_line(line: &str) -> bool {
    line.bytes().all(|byte| matches!(byte, b' ' | b'\t'))
}

#[derive(Clone, Copy)]
enum MSearchHeader {
    St,
    Man,
    Mx,
}

impl MSearchHeader {
    fn from_name(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("ST") {
            Some(Self::St)
        } else if name.eq_ignore_ascii_case("MAN") {
            Some(Self::Man)
        } else if name.eq_ignore_ascii_case("MX") {
            Some(Self::Mx)
        } else {
            None
        }
    }

    fn from_apparent_line(line: &str) -> Option<Self> {
        let name = line
            .split(|character: char| {
                character == ':' || character == '=' || character == ' ' || character == '\t'
            })
            .find(|token| !token.is_empty())?;
        Self::from_name(name)
    }

    fn rejection(self) -> MSearchReject {
        match self {
            Self::St => MSearchReject::MissingSt,
            Self::Man => MSearchReject::BadMan,
            Self::Mx => MSearchReject::BadMx,
        }
    }
}

pub fn parse_msearch(packet: &str) -> Result<MSearch, MSearchReject> {
    if has_lone_carriage_return(packet) {
        return Err(MSearchReject::NotHttp11);
    }
    let normalized = packet.replace("\r\n", "\n");
    let mut lines = normalized.split('\n');
    let first = lines.next().unwrap_or("");
    if !request_line_matches(first, "M-SEARCH") {
        return if request_line_method(first) == Some("M-SEARCH") {
            Err(MSearchReject::NotHttp11)
        } else {
            Err(MSearchReject::NotMsearch)
        };
    }
    let mut st = None;
    let mut man = None;
    let mut mx = None;
    for line in lines {
        if is_blank_header_line(line) {
            break;
        }
        let Some((key, val)) = parse_header_line(line) else {
            return Err(MSearchHeader::from_apparent_line(line)
                .map(MSearchHeader::rejection)
                .unwrap_or(MSearchReject::NotHttp11));
        };
        let Some(header) = MSearchHeader::from_name(key) else {
            continue;
        };
        let accepted = match header {
            MSearchHeader::St => set_unique_header(&mut st, val),
            MSearchHeader::Man => set_unique_header(&mut man, val),
            MSearchHeader::Mx => set_unique_header(&mut mx, val),
        };
        if !accepted {
            return Err(header.rejection());
        }
    }
    let st = st
        .filter(|s| !s.is_empty())
        .ok_or(MSearchReject::MissingSt)?;
    match man {
        Some(m) if man_is_discover(&m) => {}
        _ => return Err(MSearchReject::BadMan),
    }
    let mx = match mx {
        None => return Err(MSearchReject::BadMx),
        Some(s) => s.parse::<u32>().map_err(|_| MSearchReject::BadMx)?,
    };
    Ok(MSearch { st, mx: mx.min(5) })
}

fn st_matches(known: &str, client_st: &str) -> bool {
    if client_st == known {
        return true;
    }
    if !client_st.starts_with(known) {
        return false;
    }
    let extra = client_st[known.len()..].trim();
    extra.is_empty() || extra == "1"
}

/// Which `known_service_types` indices to reply with. `ssdp:all` → all six;
/// a specific ST → at most one.
pub fn msearch_reply_indices(uuid: &str, st: &str) -> Vec<usize> {
    use rusty_dlna_protocol::ssdp::ST_ALL;
    let types = known_service_types(uuid);
    if st == ST_ALL {
        return (0..types.len()).collect();
    }
    types
        .iter()
        .enumerate()
        .find(|(_, k)| st_matches(k, st))
        .map(|(i, _)| vec![i])
        .unwrap_or_default()
}

pub fn msearch_replies(
    uuid: &str,
    st: &str,
    host: &str,
    port: u16,
    notify_interval: u32,
    server: &str,
    date: &str,
) -> Vec<String> {
    try_msearch_replies(uuid, st, host, port, notify_interval, server, date).unwrap_or_default()
}

/// Fallible form of [`msearch_replies`] that validates every value before
/// generating a response packet.
pub fn try_msearch_replies(
    uuid: &str,
    st: &str,
    host: &str,
    port: u16,
    notify_interval: u32,
    server: &str,
    date: &str,
) -> Result<Vec<String>, SsdpBuildError> {
    if !valid_line_component(uuid) {
        return Err(SsdpBuildError::InvalidUuid);
    }
    if !valid_line_component(host) {
        return Err(SsdpBuildError::InvalidHost);
    }
    if !valid_required_field(server) {
        return Err(SsdpBuildError::InvalidServer);
    }
    if !valid_required_field(date) {
        return Err(SsdpBuildError::InvalidDate);
    }
    if !is_http_field_value(st) {
        return Err(SsdpBuildError::InvalidSearchTarget);
    }
    msearch_reply_indices(uuid, st)
        .into_iter()
        .map(|index| try_msearch_ok(uuid, index, host, port, notify_interval, server, date))
        .collect()
}

/// The second NOTIFY-alive pass waits 150–250 ms per the protocol contract.
pub const ALIVE_DUP_DELAY_MS: std::ops::RangeInclusive<u64> = 150..=250;

/// M-SEARCH reply jitter: 13–30 ms for `ssdp:all`, 13–20 ms for a specific ST.
pub fn msearch_jitter_ms_range(ssdp_all: bool) -> std::ops::RangeInclusive<u64> {
    if ssdp_all {
        13..=30
    } else {
        13..=20
    }
}

/// M-SEARCH reply jitter constrained by the request's parsed `MX` window.
/// The established short jitter remains for positive windows; `MX: 0`
/// schedules immediately instead of sleeping outside the advertised window.
pub fn msearch_jitter_ms_range_for_mx(
    ssdp_all: bool,
    mx_seconds: u32,
) -> std::ops::RangeInclusive<u64> {
    if mx_seconds == 0 {
        return 0..=0;
    }
    let baseline = msearch_jitter_ms_range(ssdp_all);
    let maximum = (*baseline.end()).min(u64::from(mx_seconds).saturating_mul(1_000));
    *baseline.start()..=maximum
}

/// Inbound renderer NOTIFY. Only used to pre-fill the client cache. The
/// standard request line must match exactly and repeated relevant headers must
/// carry identical trimmed values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundNotify {
    pub location: String,
    pub server: String,
    pub nt: String,
    pub usn: Option<String>,
}

pub fn parse_inbound_notify(packet: &str) -> Option<InboundNotify> {
    if has_lone_carriage_return(packet) {
        return None;
    }
    let normalized = packet.replace("\r\n", "\n");
    let mut lines = normalized.split('\n');
    if !request_line_matches(lines.next().unwrap_or(""), "NOTIFY") {
        return None;
    }
    let mut nts = None;
    let mut nt = None;
    let mut location = None;
    let mut server = None;
    let mut usn = None;
    for line in lines {
        if is_blank_header_line(line) {
            break;
        }
        let (key, val) = parse_header_line(line)?;
        let accepted = if key.eq_ignore_ascii_case("NTS") {
            set_unique_header(&mut nts, val)
        } else if key.eq_ignore_ascii_case("NT") {
            set_unique_header(&mut nt, val)
        } else if key.eq_ignore_ascii_case("LOCATION") {
            set_unique_header(&mut location, val)
        } else if key.eq_ignore_ascii_case("SERVER") {
            set_unique_header(&mut server, val)
        } else if key.eq_ignore_ascii_case("USN") {
            set_unique_header(&mut usn, val)
        } else {
            true
        };
        if !accepted {
            return None;
        }
    }
    if nts.as_deref() != Some("ssdp:alive") {
        return None;
    }
    let nt = nt?;
    if !nt.starts_with("urn:schemas-upnp-org:device:MediaRenderer") {
        return None;
    }
    let location = location.filter(|s| !s.is_empty())?;
    let server = server.unwrap_or_default();
    let usn = usn.filter(|value| !value.is_empty());
    let sniff = server.contains("Allegro-Software-RomPlug")
        || location.contains("SamsungMRDesc.xml")
        || server.contains("DigiOn DiXiM");
    if !sniff {
        return None;
    }
    Some(InboundNotify {
        location,
        server,
        nt,
        usn,
    })
}

pub fn jitter_ms(range: std::ops::RangeInclusive<u64>) -> u64 {
    let span = range.end().saturating_sub(*range.start()).saturating_add(1);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let offset = u64::try_from(now % u128::from(span)).unwrap_or(0);
    range.start().saturating_add(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alive_has_six_notifies_and_rootdesc() {
        let pkts = notify_alive(
            "uuid:00000000-0000-0000-0000-000000000000",
            "192.0.2.1",
            8200,
            895,
            "Linux DLNADOC/1.50 UPnP/1.0 rustyDLNA/0.1.0",
        );
        assert_eq!(pkts.len(), 6);
        assert!(pkts[0].contains("NTS:ssdp:alive"));
        assert!(pkts[0].contains("LOCATION:http://192.0.2.1:8200/rootDesc.xml"));
        assert!(pkts[0].contains("HOST:239.255.255.250:1900"));
        assert!(pkts.iter().any(|p| p.contains("MediaServer:1")));
    }

    #[test]
    fn msearch_response_has_spaces() {
        let s = msearch_ok(
            "uuid:x",
            1,
            "192.0.2.1",
            8200,
            895,
            "Linux DLNADOC/1.50 UPnP/1.0 rustyDLNA/0.1.0",
            "Sun, 16 Aug 2026 00:00:00 GMT",
        );
        assert!(s.starts_with("HTTP/1.1 200 OK"));
        assert!(s.contains("LOCATION: http://192.0.2.1:8200/rootDesc.xml"));
        assert!(s.contains("max-age=1800"));
    }

    #[test]
    fn byebye_has_six_no_location() {
        let pkts = notify_byebye("uuid:x");
        assert_eq!(pkts.len(), 6);
        assert!(pkts.iter().all(|p| !p.contains("LOCATION")));
        assert!(pkts.iter().all(|p| !p.contains("SERVER")));
        assert!(pkts.iter().all(|p| !p.contains("CACHE-CONTROL")));
        assert!(pkts.iter().all(|p| p.contains("NTS:ssdp:byebye")));
        assert!(pkts[0].contains("HOST:239.255.255.250:1900"));
    }

    #[test]
    fn all_ssdp_wire_variants_match_contract_shapes() {
        let uuid = "uuid:00000000-0000-4000-8000-000000000001";
        let types = rusty_dlna_protocol::ssdp::known_service_types(uuid);
        assert_eq!(types.len(), 6);
        let alive = notify_alive(uuid, "192.0.2.10", 8200, 895, "rustydlna-test");
        let byebye = notify_byebye(uuid);
        let search = (0..types.len())
            .map(|index| {
                msearch_ok(
                    uuid,
                    index,
                    "192.0.2.10",
                    8200,
                    895,
                    "rustydlna-test",
                    "Tue, 18 Aug 2026 00:00:00 GMT",
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(alive.len(), 6);
        assert_eq!(byebye.len(), 6);
        assert_eq!(search.len(), 6);

        for (needle, generated) in [
            ("NOTIFY * HTTP/1.1", &alive[0]),
            ("NTS:ssdp:alive", &alive[0]),
            ("NTS:ssdp:byebye", &byebye[0]),
            ("HTTP/1.1 200 OK", &search[0]),
            ("CACHE-CONTROL: max-age=", &search[0]),
            ("LOCATION: http://", &search[0]),
            ("Content-Length: 0", &search[0]),
        ] {
            assert!(
                generated.contains(needle),
                "generated packet missing {needle}"
            );
        }
        assert_eq!(ALIVE_DUP_DELAY_MS, 150..=250);
    }

    #[test]
    fn fallible_builders_preserve_the_exact_legacy_wire() {
        let uuid = "uuid:00000000-0000-4000-8000-000000000001";
        let alive = "NOTIFY * HTTP/1.1\r\nHOST:239.255.255.250:1900\r\nCACHE-CONTROL:max-age=1800\r\nLOCATION:http://192.0.2.10:8200/rootDesc.xml\r\nSERVER: rustydlna-test\r\nNT:uuid:00000000-0000-4000-8000-000000000001\r\nUSN:uuid:00000000-0000-4000-8000-000000000001\r\nNTS:ssdp:alive\r\n\r\n";
        let byebye = "NOTIFY * HTTP/1.1\r\nHOST:239.255.255.250:1900\r\nNT:uuid:00000000-0000-4000-8000-000000000001\r\nUSN:uuid:00000000-0000-4000-8000-000000000001\r\nNTS:ssdp:byebye\r\n\r\n";
        let search = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nDATE: Tue, 18 Aug 2026 00:00:00 GMT\r\nST: upnp:rootdevice\r\nUSN: uuid:00000000-0000-4000-8000-000000000001::upnp:rootdevice\r\nEXT:\r\nSERVER: rustydlna-test\r\nLOCATION: http://192.0.2.10:8200/rootDesc.xml\r\nContent-Length: 0\r\n\r\n";

        let alive_packets =
            try_notify_alive(uuid, "192.0.2.10", 8200, 895, "rustydlna-test").unwrap();
        assert_eq!(alive_packets[0], alive);
        assert_eq!(
            notify_alive(uuid, "192.0.2.10", 8200, 895, "rustydlna-test"),
            alive_packets
        );
        let byebye_packets = try_notify_byebye(uuid).unwrap();
        assert_eq!(byebye_packets[0], byebye);
        assert_eq!(notify_byebye(uuid), byebye_packets);
        assert_eq!(
            try_msearch_ok(
                uuid,
                1,
                "192.0.2.10",
                8200,
                895,
                "rustydlna-test",
                "Tue, 18 Aug 2026 00:00:00 GMT",
            )
            .unwrap(),
            search
        );
        assert_eq!(
            msearch_ok(
                uuid,
                1,
                "192.0.2.10",
                8200,
                895,
                "rustydlna-test",
                "Tue, 18 Aug 2026 00:00:00 GMT",
            ),
            search
        );
    }

    #[test]
    fn fallible_builders_reject_unsafe_values_and_bad_indices() {
        let uuid = "uuid:00000000-0000-4000-8000-000000000001";
        assert_eq!(
            try_notify_alive(
                "uuid:x\r\nNTS:ssdp:byebye",
                "192.0.2.10",
                8200,
                895,
                "server"
            ),
            Err(SsdpBuildError::InvalidUuid)
        );
        assert_eq!(
            try_notify_alive(uuid, "192.0.2.10\0bad", 8200, 895, "server"),
            Err(SsdpBuildError::InvalidHost)
        );
        assert_eq!(
            try_notify_alive(uuid, "192.0.2.10", 8200, 895, "server\r\nX: y"),
            Err(SsdpBuildError::InvalidServer)
        );
        assert_eq!(
            try_notify_byebye("uuid:x\nUSN:other"),
            Err(SsdpBuildError::InvalidUuid)
        );
        assert_eq!(
            try_msearch_ok(uuid, 6, "192.0.2.10", 8200, 895, "server", "date"),
            Err(SsdpBuildError::ServiceTypeIndexOutOfRange {
                index: 6,
                available: 6
            })
        );
        assert_eq!(
            try_msearch_ok(uuid, 0, "192.0.2.10", 8200, 895, "server", "date\r\nX: y"),
            Err(SsdpBuildError::InvalidDate)
        );

        assert!(notify_alive("uuid:x\nX:y", "host", 8200, 895, "server").is_empty());
        assert!(notify_byebye("uuid:x\nX:y").is_empty());
        assert_eq!(msearch_ok(uuid, 6, "host", 8200, 895, "server", "date"), "");
    }

    #[test]
    fn reply_builder_validates_every_argument_even_for_unknown_targets() {
        let uuid = "uuid:00000000-0000-4000-8000-000000000001";
        let common = |uuid, st, host, server, date| {
            try_msearch_replies(uuid, st, host, 8200, 895, server, date)
        };
        for invalid in ["bad\r\nX:y", "bad\nX:y", "bad\0X:y"] {
            assert_eq!(
                common(invalid, "urn:unknown", "host", "server", "date"),
                Err(SsdpBuildError::InvalidUuid),
                "accepted UUID {invalid:?}"
            );
            assert_eq!(
                common(uuid, "urn:unknown", invalid, "server", "date"),
                Err(SsdpBuildError::InvalidHost),
                "accepted host {invalid:?}"
            );
            assert_eq!(
                common(uuid, "urn:unknown", "host", invalid, "date"),
                Err(SsdpBuildError::InvalidServer),
                "accepted SERVER {invalid:?}"
            );
            assert_eq!(
                common(uuid, "urn:unknown", "host", "server", invalid),
                Err(SsdpBuildError::InvalidDate),
                "accepted DATE {invalid:?}"
            );
            assert_eq!(
                common(uuid, invalid, "host", "server", "date"),
                Err(SsdpBuildError::InvalidSearchTarget),
                "accepted ST {invalid:?}"
            );
        }
        assert_eq!(
            common(uuid, "urn:unknown", "host", "server", "date"),
            Ok(Vec::new())
        );
        assert!(msearch_replies(
            uuid,
            "urn:unknown\r\nST:ssdp:all",
            "host",
            8200,
            895,
            "server",
            "date"
        )
        .is_empty());
    }

    #[test]
    fn jitter_ranges_match_protocol_contract() {
        assert_eq!(*ALIVE_DUP_DELAY_MS.start(), 150);
        assert_eq!(*ALIVE_DUP_DELAY_MS.end(), 250);
        assert_eq!(msearch_jitter_ms_range(true), 13..=30);
        assert_eq!(msearch_jitter_ms_range(false), 13..=20);
        assert_eq!(msearch_jitter_ms_range_for_mx(true, 0), 0..=0);
        assert_eq!(msearch_jitter_ms_range_for_mx(true, 1), 13..=30);
        assert_eq!(msearch_jitter_ms_range_for_mx(false, 5), 13..=20);
        let all = jitter_ms(msearch_jitter_ms_range(true));
        assert!((13..=30).contains(&all), "ssdp:all jitter {all}");
        let one = jitter_ms(msearch_jitter_ms_range(false));
        assert!((13..=20).contains(&one), "specific ST jitter {one}");
        let dup = jitter_ms(ALIVE_DUP_DELAY_MS);
        assert!((150..=250).contains(&dup), "alive dup delay {dup}");
    }

    #[test]
    fn man_must_be_quoted_ssdp_discover() {
        assert!(man_is_discover("\"ssdp:discover\""));
        assert!(man_is_discover(" \t\"ssdp:discover\"\t "));
        assert!(man_is_discover("\u{a0}\"ssdp:discover\"\u{a0}"));
        assert!(!man_is_discover("ssdp:discover"));
        assert!(!man_is_discover("\"ssdp:alive\""));
    }

    #[test]
    fn parse_msearch_rejects_bad_man() {
        let pkt = "M-SEARCH * HTTP/1.1\r\nHOST:239.255.255.250:1900\r\nMAN: ssdp:discover\r\nMX: 1\r\nST: ssdp:all\r\n\r\n";
        assert_eq!(parse_msearch(pkt), Err(MSearchReject::BadMan));
        let ok = "M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: ssdp:all\r\n\r\n";
        assert_eq!(parse_msearch(ok).unwrap().st, "ssdp:all");
        let kodi = "M-SEARCH * HTTP/1.1\nHOST: 239.255.255.250:1900\nMAN: \"ssdp:discover\"\nMX: 5\nST: upnp:rootdevice\nUser-Agent: UPnP/1.0 DLNADOC/1.50 Platinum/1.0.5.13\n\n";
        assert_eq!(parse_msearch(kodi).unwrap().st, "upnp:rootdevice");
        let missing_mx = "M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nST: ssdp:all\r\n\r\n";
        assert_eq!(parse_msearch(missing_mx), Err(MSearchReject::BadMx));
        let huge_mx =
            "M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nMX: 999999\r\nST: ssdp:all\r\n\r\n";
        assert_eq!(parse_msearch(huge_mx).unwrap().mx, 5);
    }

    #[test]
    fn parsed_msearch_retains_unicode_whitespace_semantic_compatibility() {
        let service = "urn:schemas-upnp-org:service:ContentDirectory:1";
        let packet = format!(
            "M-SEARCH * HTTP/1.1\r\nMAN: \u{a0}\"ssdp:discover\"\u{a0}\r\nMX: 1\r\nST: {service}\u{a0}\r\n\r\n"
        );
        let parsed = parse_msearch(&packet).expect("NBSP-padded semantic values remain compatible");

        assert_eq!(parsed.st, format!("{service}\u{a0}"));
        let uuid = "uuid:00000000-0000-0000-0000-000000000000";
        let replies = try_msearch_replies(
            uuid,
            &parsed.st,
            "192.0.2.10",
            8200,
            895,
            "rustydlna-test",
            "Tue, 18 Aug 2026 00:00:00 GMT",
        )
        .expect("parsed specific ST remains replyable");
        assert_eq!(replies.len(), 1);
        assert!(replies[0].contains(&format!("ST: {service}\r\n")));
        assert!(replies[0].contains(&format!("USN: {uuid}::{service}\r\n")));
    }

    #[test]
    fn inbound_request_lines_must_match_the_ssdp_shape_exactly() {
        let headers = "MAN: \"ssdp:discover\"\r\nMX: 1\r\nST: ssdp:all\r\n\r\n";
        assert!(parse_msearch(&format!("M-SEARCH * HTTP/1.1\r\n{headers}")).is_ok());
        assert_eq!(
            parse_msearch(&format!("m-search * HTTP/1.1\r\n{headers}")),
            Err(MSearchReject::NotMsearch)
        );
        for line in [
            "M-SEARCH / HTTP/1.1",
            "M-SEARCH * HTTP/1.0",
            "M-SEARCH * http/1.1",
            "M-SEARCH * HTTP/1.1 extra",
            " M-SEARCH * HTTP/1.1",
            "M-SEARCH * HTTP/1.1 ",
        ] {
            assert_eq!(
                parse_msearch(&format!("{line}\r\n{headers}")),
                Err(MSearchReject::NotHttp11),
                "accepted malformed M-SEARCH line: {line}"
            );
        }
        for line in ["M-SEARCH  * HTTP/1.1", "M-SEARCH\t*\tHTTP/1.1"] {
            assert!(
                parse_msearch(&format!("{line}\r\n{headers}")).is_ok(),
                "rejected compatible request-line spacing: {line:?}"
            );
        }

        let renderer = "urn:schemas-upnp-org:device:MediaRenderer:1";
        let valid = notify(
            "Allegro-Software-RomPlug/1.0",
            "http://192.0.2.10/desc.xml",
            renderer,
            "ssdp:alive",
        );
        assert!(parse_inbound_notify(&valid).is_some());
        for line in [
            "notify * HTTP/1.1",
            "NOTIFY / HTTP/1.1",
            "NOTIFY * HTTP/1.0",
            "NOTIFY * HTTP/1.1 extra",
        ] {
            let malformed = valid.replacen("NOTIFY * HTTP/1.1", line, 1);
            assert!(
                parse_inbound_notify(&malformed).is_none(),
                "accepted malformed NOTIFY line: {line}"
            );
        }
        let compatible = valid.replacen("NOTIFY * HTTP/1.1", "NOTIFY\t  *\tHTTP/1.1", 1);
        assert!(parse_inbound_notify(&compatible).is_some());
    }

    #[test]
    fn conflicting_relevant_ssdp_headers_are_rejected() {
        let base = "M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: ssdp:all\r\n";
        for (duplicate, expected) in [
            ("ST: upnp:rootdevice\r\n", MSearchReject::MissingSt),
            ("MAN: \"ssdp:alive\"\r\n", MSearchReject::BadMan),
            ("MX: 2\r\n", MSearchReject::BadMx),
        ] {
            assert_eq!(
                parse_msearch(&format!("{base}{duplicate}\r\n")),
                Err(expected),
                "accepted conflicting header: {duplicate:?}"
            );
        }
        let identical = format!("{base}st: ssdp:all\r\nman: \"ssdp:discover\"\r\nmx: 1\r\n\r\n");
        assert_eq!(
            parse_msearch(&identical),
            Ok(MSearch {
                st: "ssdp:all".into(),
                mx: 1
            })
        );

        let renderer = "urn:schemas-upnp-org:device:MediaRenderer:1";
        let valid = notify(
            "Allegro-Software-RomPlug/1.0",
            "http://192.0.2.10/desc.xml",
            renderer,
            "ssdp:alive",
        );
        for header in [
            "NTS:ssdp:byebye\r\n",
            "NT:urn:schemas-upnp-org:device:MediaServer:1\r\n",
            "LOCATION:http://192.0.2.99/desc.xml\r\n",
            "SERVER: other\r\n",
        ] {
            let conflicting = valid.replacen("\r\n\r\n", &format!("\r\n{header}\r\n"), 1);
            assert!(
                parse_inbound_notify(&conflicting).is_none(),
                "accepted conflicting NOTIFY header: {header:?}"
            );
        }
        let conflicting_usn = valid.replacen(
            "\r\n\r\n",
            "\r\nUSN: uuid:one\r\nUSN: uuid:other\r\n\r\n",
            1,
        );
        assert!(parse_inbound_notify(&conflicting_usn).is_none());

        let identical = valid.replacen(
            "\r\n\r\n",
            &format!(
                "\r\nnts:ssdp:alive\r\nnt:{renderer}\r\nlocation:http://192.0.2.10/desc.xml\r\nserver: Allegro-Software-RomPlug/1.0\r\n\r\n"
            ),
            1,
        );
        assert!(parse_inbound_notify(&identical).is_some());
    }

    #[test]
    fn malformed_relevant_headers_and_lone_cr_lines_are_rejected() {
        for malformed in [
            "M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: ssdp:all\r\nST upnp:rootdevice\r\n\r\n",
            "M-SEARCH * HTTP/1.1\rMAN: \"ssdp:discover\"\rMX: 1\rST: ssdp:all\r\r",
            "M-SEARCH * HTTP/1.1\0\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: ssdp:all\r\n\r\n",
        ] {
            assert!(parse_msearch(malformed).is_err(), "accepted {malformed:?}");
        }

        let malformed_notify = "NOTIFY * HTTP/1.1\r\nNTS: ssdp:alive\r\nNT: urn:schemas-upnp-org:device:MediaRenderer:1\r\nNT urn:schemas-upnp-org:device:MediaServer:1\r\nLOCATION: http://192.0.2.10/desc.xml\r\nSERVER: Allegro-Software-RomPlug/1.0\r\n\r\n";
        assert!(parse_inbound_notify(malformed_notify).is_none());
    }

    #[test]
    fn every_ssdp_header_line_must_have_valid_http_field_syntax() {
        let base = "M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: ssdp:all\r\n";
        for (line, expected) in [
            ("ST = upnp:rootdevice", MSearchReject::MissingSt),
            ("ST : upnp:rootdevice", MSearchReject::MissingSt),
            ("MAN = \"ssdp:discover\"", MSearchReject::BadMan),
            ("MX = 2", MSearchReject::BadMx),
            ("Unknown extension", MSearchReject::NotHttp11),
            ("X-Bad : value", MSearchReject::NotHttp11),
            ("X-Bad:\0value", MSearchReject::NotHttp11),
        ] {
            assert_eq!(
                parse_msearch(&format!("{base}{line}\r\n\r\n")),
                Err(expected),
                "accepted malformed header line: {line:?}"
            );
        }
        let extension = format!("{base}X-Vendor_Extension: value:with:colons\r\n\r\n");
        assert!(parse_msearch(&extension).is_ok());

        let renderer = "urn:schemas-upnp-org:device:MediaRenderer:1";
        let valid = notify(
            "Allegro-Software-RomPlug/1.0",
            "http://192.0.2.10/desc.xml",
            renderer,
            "ssdp:alive",
        );
        for line in ["Unknown extension", "X-Bad : value", "X-Bad:\0value"] {
            let malformed = valid.replacen("\r\n\r\n", &format!("\r\n{line}\r\n\r\n"), 1);
            assert!(
                parse_inbound_notify(&malformed).is_none(),
                "accepted malformed NOTIFY header: {line:?}"
            );
        }
        let extension = valid.replacen(
            "\r\n\r\n",
            "\r\nX-Vendor_Extension: value:with:colons\r\n\r\n",
            1,
        );
        assert!(parse_inbound_notify(&extension).is_some());
    }

    #[test]
    fn lf_only_ssdp_and_header_boundaries_remain_compatible() {
        let search =
            "M-SEARCH * HTTP/1.1\nMAN: \"ssdp:discover\"\nMX: 5\nST: upnp:rootdevice\n\nMX: 1\n";
        assert_eq!(parse_msearch(search).unwrap().mx, 5);

        let renderer = "urn:schemas-upnp-org:device:MediaRenderer:1";
        let packet = notify(
            "Allegro-Software-RomPlug/1.0",
            "http://192.0.2.10/desc.xml",
            renderer,
            "ssdp:alive",
        )
        .replace("\r\n", "\n");
        assert!(parse_inbound_notify(&packet).is_some());
    }

    #[test]
    fn ssdp_all_is_six_specific_is_one() {
        let uuid = "uuid:00000000-0000-0000-0000-000000000000";
        assert_eq!(msearch_reply_indices(uuid, "ssdp:all").len(), 6);
        assert_eq!(msearch_reply_indices(uuid, "upnp:rootdevice").len(), 1);
        assert!(msearch_reply_indices(uuid, "urn:foo:bar").is_empty());
        let cd = "urn:schemas-upnp-org:service:ContentDirectory:1";
        assert_eq!(msearch_reply_indices(uuid, cd).len(), 1);
        assert!(
            msearch_reply_indices(uuid, "urn:schemas-upnp-org:service:ContentDirectory:10")
                .is_empty(),
            "ST leftover must be version 1, not 10"
        );
        assert!(
            msearch_reply_indices(uuid, "urn:schemas-upnp-org:service:ContentDirectory:1foo")
                .is_empty()
        );
        assert_eq!(
            msearch_reply_indices(
                uuid,
                "urn:schemas-upnp-org:service:ContentDirectory:1\u{a0}"
            )
            .len(),
            1,
            "line-safe Unicode whitespace remains compatible in ST semantics"
        );
        let replies = msearch_replies(
            uuid,
            "ssdp:all",
            "127.0.0.1",
            18200,
            895,
            "Linux DLNADOC/1.50 UPnP/1.0 rustyDLNA/0.1.0",
            "Sun, 16 Aug 2026 00:00:00 GMT",
        );
        assert_eq!(replies.len(), 6);
        assert!(replies[0].contains("LOCATION: http://127.0.0.1:18200/rootDesc.xml"));
        assert!(replies[0].contains("max-age=1800"));
    }

    fn notify(server: &str, location: &str, nt: &str, nts: &str) -> String {
        format!(
            "NOTIFY * HTTP/1.1\r\nHOST:239.255.255.250:1900\r\nNTS:{nts}\r\nNT:{nt}\r\nLOCATION:{location}\r\nSERVER: {server}\r\n\r\n"
        )
    }

    #[test]
    fn parse_inbound_notify_roku_samsung_dixim_vs_ignore() {
        let renderer = "urn:schemas-upnp-org:device:MediaRenderer:1";
        let roku = parse_inbound_notify(&notify(
            "Allegro-Software-RomPlug/1.0",
            "http://192.0.2.10/desc.xml",
            renderer,
            "ssdp:alive",
        ))
        .expect("Roku");
        assert_eq!(roku.location, "http://192.0.2.10/desc.xml");
        assert!(roku.server.contains("Allegro-Software-RomPlug"));

        let samsung = parse_inbound_notify(&notify(
            "Linux UPnP/1.0 Samsung",
            "http://192.0.2.11/SamsungMRDesc.xml",
            renderer,
            "ssdp:alive",
        ))
        .expect("Samsung");
        assert!(samsung.location.contains("SamsungMRDesc.xml"));

        let dixim = parse_inbound_notify(&notify(
            "DigiOn DiXiM/1.0",
            "http://192.0.2.12/desc.xml",
            renderer,
            "ssdp:alive",
        ))
        .expect("DiXiM");
        assert!(dixim.server.contains("DigiOn DiXiM"));

        assert!(
            parse_inbound_notify(&notify(
                "Kodi/21.0",
                "http://192.0.2.13/desc.xml",
                renderer,
                "ssdp:alive",
            ))
            .is_none(),
            "generic renderer must be ignored"
        );
        assert!(
            parse_inbound_notify(&notify(
                "Allegro-Software-RomPlug/1.0",
                "http://192.0.2.10/desc.xml",
                "urn:schemas-upnp-org:device:MediaServer:1",
                "ssdp:alive",
            ))
            .is_none(),
            "MediaServer NOTIFY must be ignored"
        );
        assert!(
            parse_inbound_notify(&notify(
                "Allegro-Software-RomPlug/1.0",
                "http://192.0.2.10/desc.xml",
                renderer,
                "ssdp:byebye",
            ))
            .is_none(),
            "byebye must be ignored"
        );
    }
}
