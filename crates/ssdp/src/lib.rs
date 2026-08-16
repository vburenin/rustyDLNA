//! SSDP packet builders. Sockets come later; the dialect is here.

use rusty_dlna_protocol::paths::ROOTDESC_PATH;
use rusty_dlna_protocol::ssdp::{
    known_service_types, notify_max_age, NTS_ALIVE, NTS_BYEBYE, SSDP_MCAST_ADDR, SSDP_PORT,
};

fn usn(uuid: &str, st: &str, index: usize) -> String {
    if index == 0 {
        uuid.to_string()
    } else {
        format!("{uuid}::{st}")
    }
}

/// Unsolicited NOTIFY (The dialect uses no space after `HOST:`, `NT:`, …).
pub fn notify_alive(
    uuid: &str,
    host: &str,
    port: u16,
    notify_interval: u32,
    server: &str,
) -> Vec<String> {
    let lifetime = notify_max_age(notify_interval);
    known_service_types(uuid)
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
        .collect()
}

pub fn notify_byebye(uuid: &str) -> Vec<String> {
    known_service_types(uuid)
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
        .collect()
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
    let types = known_service_types(uuid);
    let st = types[st_index];
    format!(
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
    )
}

pub fn man_is_discover(man: &str) -> bool {
    man.trim() == rusty_dlna_protocol::ssdp::MAN_DISCOVER
}

/// Parsed M-SEARCH. The dialect requires HTTP/1.1, a non-empty ST, MAN exactly
/// `"ssdp:discover"`, and MX as an integer >= 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MSearch {
    pub st: String,
    pub mx: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MSearchReject {
    NotMsearch,
    NotHttp11,
    MissingSt,
    BadMan,
    BadMx,
}

pub fn parse_msearch(packet: &str) -> Result<MSearch, MSearchReject> {
    let normalized = packet.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = normalized.split('\n');
    let first = lines.next().unwrap_or("");
    let first_u = first.to_ascii_uppercase();
    if !first_u.starts_with("M-SEARCH") {
        return Err(MSearchReject::NotMsearch);
    }
    if !first.contains("HTTP/1.1") {
        return Err(MSearchReject::NotHttp11);
    }
    let mut st = None;
    let mut man = None;
    let mut mx = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim();
        let val = v.trim();
        if key.eq_ignore_ascii_case("ST") {
            st = Some(val.to_string());
        } else if key.eq_ignore_ascii_case("MAN") {
            man = Some(val.to_string());
        } else if key.eq_ignore_ascii_case("MX") {
            mx = Some(val.to_string());
        }
    }
    let st = st.filter(|s| !s.is_empty()).ok_or(MSearchReject::MissingSt)?;
    match man {
        Some(m) if man_is_discover(&m) => {}
        _ => return Err(MSearchReject::BadMan),
    }
    let mx = match mx {
        None => 1,
        Some(s) => s.parse::<i32>().map_err(|_| MSearchReject::BadMx)?,
    };
    if mx < 0 {
        return Err(MSearchReject::BadMx);
    }
    Ok(MSearch { st, mx })
}

fn st_matches(known: &str, client_st: &str) -> bool {
    if client_st == known {
        return true;
    }
    if !client_st.starts_with(known) {
        return false;
    }
    let extra = &client_st[known.len()..];
    extra.trim().is_empty() || extra.trim() == "1" || extra.starts_with('1')
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
    msearch_reply_indices(uuid, st)
        .into_iter()
        .map(|i| msearch_ok(uuid, i, host, port, notify_interval, server, date))
        .collect()
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
        assert!(pkts.iter().all(|p| p.contains("NTS:ssdp:byebye")));
        assert!(pkts[0].contains("HOST:239.255.255.250:1900"));
    }

    #[test]
    fn man_must_be_quoted_ssdp_discover() {
        assert!(man_is_discover("\"ssdp:discover\""));
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
    }

    #[test]
    fn ssdp_all_is_six_specific_is_one() {
        let uuid = "uuid:00000000-0000-0000-0000-000000000000";
        assert_eq!(msearch_reply_indices(uuid, "ssdp:all").len(), 6);
        assert_eq!(
            msearch_reply_indices(uuid, "upnp:rootdevice").len(),
            1
        );
        assert!(msearch_reply_indices(uuid, "urn:foo:bar").is_empty());
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
}
