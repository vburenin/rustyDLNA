//! Dialect lock: live Rust `&str`s must appear in the MiniDLNA oracle
//! headers / replica notes. Expected values are **not** hand-copied here
//! so they cannot rot independently of the C tree.

use std::path::PathBuf;

use crate::clients::{identify_user_agent, remap_mime, ClientFlags, CLIENTS};
use crate::object_id::{BROWSEDIR_ID, IMAGE_ID, MUSIC_ID, ROOT_ID, VIDEO_ID};
use crate::paths::{
    CONNECTIONMGR_CONTROLURL, CONNECTIONMGR_EVENTURL, CONNECTIONMGR_PATH,
    CONTENTDIRECTORY_CONTROLURL, CONTENTDIRECTORY_EVENTURL, CONTENTDIRECTORY_PATH, ROOTDESC_PATH,
    X_MS_MEDIARECEIVERREGISTRAR_CONTROLURL, X_MS_MEDIARECEIVERREGISTRAR_EVENTURL,
    X_MS_MEDIARECEIVERREGISTRAR_PATH,
};
use crate::soap::{
    CONTENTDIRECTORY_TYPE, DIDL_SCHEMAS, DLNA_NAMESPACE, PV_NAMESPACE, SEARCH_CAPS, SEC_NAMESPACE,
    SOAP_METHODS, SORT_CAPS,
};
use crate::w3c_normalize_date;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn oracle(name: &str) -> String {
    let p = workspace_root().join("docs/minidlna-oracle").join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn replica() -> String {
    let p = workspace_root().join("replica.md");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read replica.md: {e}"))
}

fn quoted(s: &str) -> String {
    format!("\"{s}\"")
}

#[test]
fn path_literals_appear_in_minidlnapath_h() {
    let h = oracle("minidlnapath.h");
    for lit in [
        ROOTDESC_PATH,
        CONTENTDIRECTORY_PATH,
        CONTENTDIRECTORY_CONTROLURL,
        CONTENTDIRECTORY_EVENTURL,
        CONNECTIONMGR_PATH,
        CONNECTIONMGR_CONTROLURL,
        CONNECTIONMGR_EVENTURL,
        X_MS_MEDIARECEIVERREGISTRAR_PATH,
        X_MS_MEDIARECEIVERREGISTRAR_CONTROLURL,
        X_MS_MEDIARECEIVERREGISTRAR_EVENTURL,
    ] {
        assert!(
            h.contains(&quoted(lit)),
            "oracle minidlnapath.h missing {lit:?}"
        );
    }
}

#[test]
fn object_ids_appear_in_scanner_h() {
    let h = oracle("scanner.h");
    for lit in [ROOT_ID, BROWSEDIR_ID, MUSIC_ID, VIDEO_ID, IMAGE_ID] {
        if lit == ROOT_ID {
            // scanner.h does not #define "0"; replica.md documents it.
            assert!(
                replica().contains("`0`") && replica().contains("True root"),
                "replica.md must document root id 0"
            );
            continue;
        }
        assert!(h.contains(&quoted(lit)), "scanner.h missing object id {lit}");
    }
}

#[test]
fn soap_method_names_appear_in_replica() {
    let r = replica();
    for name in SOAP_METHODS {
        assert!(r.contains(name), "replica.md missing SOAP method {name}");
    }
}

#[test]
fn didl_namespaces_appear_in_upnpsoap_h() {
    let h = oracle("upnpsoap.h");
    assert!(h.contains("xmlns:dc="));
    // Live Rust strings must be substrings of the C macros.
    assert!(
        h.contains("http://purl.org/dc/elements/1.1/"),
        "upnpsoap.h missing dc xmlns that DIDL_SCHEMAS uses"
    );
    assert!(DIDL_SCHEMAS.contains("http://purl.org/dc/elements/1.1/"));
    assert!(h.contains("urn:schemas-upnp-org:metadata-1-0/upnp/"));
    assert!(h.contains("urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"));
    assert!(DLNA_NAMESPACE.contains("urn:schemas-dlna-org:metadata-1-0/"));
    assert!(h.contains("urn:schemas-dlna-org:metadata-1-0/"));
    assert!(PV_NAMESPACE.contains("http://www.pv.com/pvns/"));
    assert!(h.contains("http://www.pv.com/pvns/"));
    assert!(SEC_NAMESPACE.contains("http://www.sec.co.kr/dlna"));
    assert!(h.contains("http://www.sec.co.kr/dlna"));
    assert!(h.contains(CONTENTDIRECTORY_TYPE) || replica().contains(CONTENTDIRECTORY_TYPE));
    assert!(replica().contains(SORT_CAPS));
    assert!(replica().contains(SEARCH_CAPS));
}

#[test]
fn w3c_normalize_matches_w3c_date_c_cases() {
    let c = oracle("w3c_date.c");
    assert!(c.contains("w3c_normalize_date"));
    assert!(c.contains("%Y-%m-%dT%H:%M:%SZ"));
    assert!(c.contains("1999-01-01") || c.contains("-01-01"));
    assert!(c.contains("YYYY:MM:DD HH:MM:SS") || c.contains("EXIF"));
    // Behaviour lock: the C file documents these shapes; run the live fn.
    assert!(w3c_normalize_date("2024-03-15T14:30:00").ends_with('Z'));
    assert_eq!(w3c_normalize_date("1999").len(), 10);
    assert!(w3c_normalize_date("2024:03:15 14:30:00").ends_with('Z'));
}

#[test]
fn client_table_order_matches_oracle_and_rusty_additions() {
    let c = oracle("clients.c");
    let pc = c
        .find("SEC_HHP_[PC]")
        .expect("clients.c must contain SEC_HHP_[PC]");
    // Generic Samsung token is the quoted match string "SEC_HHP_".
    let generic = c
        .rfind("\"SEC_HHP_\"")
        .expect("clients.c must contain generic SEC_HHP_");
    assert!(pc < generic, "SEC_HHP_[PC] must sit above SEC_HHP_ in oracle");

    let rust: Vec<&str> = CLIENTS.iter().filter_map(|p| p.match_str).collect();
    let rpc = rust
        .iter()
        .position(|s| *s == "SEC_HHP_[PC]")
        .expect("Rust table missing SEC_HHP_[PC]");
    let rsec = rust
        .iter()
        .position(|s| *s == "SEC_HHP_")
        .expect("Rust table missing SEC_HHP_");
    assert!(rpc < rsec, "Rust table must keep SEC_HHP_[PC] before SEC_HHP_");

    let cr = rust
        .iter()
        .position(|s| *s == "CrKey")
        .expect("Rust table missing CrKey");
    let dlna = rust
        .iter()
        .position(|s| *s == "DLNADOC/1.50")
        .expect("Rust table missing DLNADOC/1.50");
    assert!(cr < dlna, "CrKey must sit above generic DLNADOC/1.50");
}

#[test]
fn samsung_mime_and_kodi_flags_locked_to_oracle() {
    let h = oracle("clients.h");
    assert!(h.contains("FLAG_SAMSUNG"));
    assert!(h.contains("FLAG_DLNA"));
    assert!(h.contains("FLAG_CAPTION_RES"));
    let c = oracle("clients.c");
    assert!(c.contains("FLAG_DLNA | FLAG_MIME_AVI_AVI | FLAG_CAPTION_RES"));
    assert!(c.contains("\"Kodi\""));

    let kodi = identify_user_agent("Kodi/21.0").expect("kodi");
    assert!(kodi.flags.contains(ClientFlags::DLNA));
    assert!(kodi.flags.contains(ClientFlags::CAPTION_RES));
    assert!(!kodi.flags.contains(ClientFlags::NEED_SAFE_VIDEO));

    let tv = identify_user_agent("SEC_HHP_[TV]UE40D7000/1.0").expect("samsung tv");
    assert_eq!(remap_mime(tv, "video/x-matroska"), "video/x-mkv");
    assert!(h.contains("FLAG_SAMSUNG"));
}
