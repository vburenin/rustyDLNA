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
    msearch_reply_indices(uuid, st)
        .into_iter()
        .map(|i| msearch_ok(uuid, i, host, port, notify_interval, server, date))
        .collect()
}

/// Second NOTIFY-alive pass waits 150–250 ms (`replica.md` §1).
pub const ALIVE_DUP_DELAY_MS: std::ops::RangeInclusive<u64> = 150..=250;

/// M-SEARCH reply jitter: 13–30 ms for `ssdp:all`, 13–20 ms for a specific ST.
pub fn msearch_jitter_ms_range(ssdp_all: bool) -> std::ops::RangeInclusive<u64> {
    if ssdp_all {
        13..=30
    } else {
        13..=20
    }
}

/// Inbound renderer NOTIFY (`replica.md` §1). Only used to pre-fill the client cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundNotify {
    pub location: String,
    pub server: String,
    pub nt: String,
}

pub fn parse_inbound_notify(packet: &str) -> Option<InboundNotify> {
    let normalized = packet.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = normalized.split('\n');
    let first = lines.next().unwrap_or("").to_ascii_uppercase();
    if !first.starts_with("NOTIFY") {
        return None;
    }
    let mut nts = None;
    let mut nt = None;
    let mut location = None;
    let mut server = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim();
        let val = v.trim();
        if key.eq_ignore_ascii_case("NTS") {
            nts = Some(val.to_string());
        } else if key.eq_ignore_ascii_case("NT") {
            nt = Some(val.to_string());
        } else if key.eq_ignore_ascii_case("LOCATION") {
            location = Some(val.to_string());
        } else if key.eq_ignore_ascii_case("SERVER") {
            server = Some(val.to_string());
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
    })
}

pub fn jitter_ms(range: std::ops::RangeInclusive<u64>) -> u64 {
    let span = range.end().saturating_sub(*range.start()).saturating_add(1);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    range.start() + (now % span)
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
    fn jitter_ranges_match_replica() {
        assert_eq!(*ALIVE_DUP_DELAY_MS.start(), 150);
        assert_eq!(*ALIVE_DUP_DELAY_MS.end(), 250);
        assert_eq!(msearch_jitter_ms_range(true), 13..=30);
        assert_eq!(msearch_jitter_ms_range(false), 13..=20);
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
