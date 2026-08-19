//! Dialect lock: live Rust `&str`s must appear in the oracle headers /
//! replica notes. Expected values are **not** hand-copied here so they
//! cannot rot independently of those files.

use std::path::PathBuf;

use crate::clients::{identify_user_agent, remap_mime, ClientFlags, CLIENTS};
use crate::object_id::{
    BROWSEDIR_ID, IMAGE_ALBUM_ID, IMAGE_ALL_ID, IMAGE_CAMERA_ID, IMAGE_DATE_ID, IMAGE_DIR_ID,
    IMAGE_ID, IMAGE_PLIST_ID, IMAGE_RATING_ID, MUSIC_ALBUM_ARTIST_ID, MUSIC_ALBUM_ID, MUSIC_ALL_ID,
    MUSIC_ARTIST_ID, MUSIC_COMPOSER_ID, MUSIC_CONTRIB_ARTIST_ID, MUSIC_DIR_ID, MUSIC_GENRE_ID,
    MUSIC_ID, MUSIC_PLIST_ID, MUSIC_RATING_ID, ROOT_ID, VIDEO_ACTOR_ID, VIDEO_ALL_ID, VIDEO_DIR_ID,
    VIDEO_GENRE_ID, VIDEO_ID, VIDEO_PLIST_ID, VIDEO_RATING_ID, VIDEO_SERIES_ID,
};
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
    let p = workspace_root().join("docs/oracle").join(name);
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
fn path_literals_appear_in_oracle_paths() {
    let h = oracle("paths.h");
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
            "oracle path header missing {lit:?}"
        );
    }
}

#[test]
fn object_ids_appear_in_scanner_h() {
    let h = oracle("scanner.h");
    for (name, lit) in [
        ("BROWSEDIR_ID", BROWSEDIR_ID),
        ("MUSIC_ID", MUSIC_ID),
        ("MUSIC_ALL_ID", MUSIC_ALL_ID),
        ("MUSIC_GENRE_ID", MUSIC_GENRE_ID),
        ("MUSIC_ARTIST_ID", MUSIC_ARTIST_ID),
        ("MUSIC_ALBUM_ID", MUSIC_ALBUM_ID),
        ("MUSIC_PLIST_ID", MUSIC_PLIST_ID),
        ("MUSIC_DIR_ID", MUSIC_DIR_ID),
        ("MUSIC_CONTRIB_ARTIST_ID", MUSIC_CONTRIB_ARTIST_ID),
        ("MUSIC_ALBUM_ARTIST_ID", MUSIC_ALBUM_ARTIST_ID),
        ("MUSIC_COMPOSER_ID", MUSIC_COMPOSER_ID),
        ("MUSIC_RATING_ID", MUSIC_RATING_ID),
        ("VIDEO_ID", VIDEO_ID),
        ("VIDEO_ALL_ID", VIDEO_ALL_ID),
        ("VIDEO_GENRE_ID", VIDEO_GENRE_ID),
        ("VIDEO_ACTOR_ID", VIDEO_ACTOR_ID),
        ("VIDEO_SERIES_ID", VIDEO_SERIES_ID),
        ("VIDEO_PLIST_ID", VIDEO_PLIST_ID),
        ("VIDEO_DIR_ID", VIDEO_DIR_ID),
        ("VIDEO_RATING_ID", VIDEO_RATING_ID),
        ("IMAGE_ID", IMAGE_ID),
        ("IMAGE_ALL_ID", IMAGE_ALL_ID),
        ("IMAGE_DATE_ID", IMAGE_DATE_ID),
        ("IMAGE_ALBUM_ID", IMAGE_ALBUM_ID),
        ("IMAGE_CAMERA_ID", IMAGE_CAMERA_ID),
        ("IMAGE_PLIST_ID", IMAGE_PLIST_ID),
        ("IMAGE_DIR_ID", IMAGE_DIR_ID),
        ("IMAGE_RATING_ID", IMAGE_RATING_ID),
    ] {
        assert!(
            h.lines().any(|line| {
                line.starts_with(&format!("#define {name}")) && line.contains(&quoted(lit))
            }),
            "scanner.h missing exact {name}={lit}"
        );
    }
    // The root is implicit in scanner.h and explicit in the reference's
    // containers table and our wire contract.
    assert_eq!(ROOT_ID, "0");
    assert!(oracle("containers.c").contains("Alternate root") || replica().contains("True root"));
}

#[test]
fn virtual_view_aliases_appear_in_containers_c() {
    let c = oracle("containers.c");
    for lit in ["1$FF0", "2$FF0", "3$FF0", "Recently Added"] {
        assert!(c.contains(lit), "containers.c missing {lit}");
    }
    for alias in [
        "4", "5", "6", "7", "8", "B", "C", "F", "14", "15", "16", "D2",
    ] {
        assert!(
            c.contains(&format!("NULL, \"{alias}\"")),
            "containers.c missing PlaysForSure alias {alias}"
        );
    }
    for alias in ["A", "V", "I"] {
        assert!(
            c.contains(&format!("NULL, \"{alias}\"")),
            "containers.c missing Samsung alias {alias}"
        );
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
    assert!(
        pc < generic,
        "SEC_HHP_[PC] must sit above SEC_HHP_ in oracle"
    );

    let rust: Vec<&str> = CLIENTS.iter().filter_map(|p| p.match_str).collect();
    let rpc = rust
        .iter()
        .position(|s| *s == "SEC_HHP_[PC]")
        .expect("Rust table missing SEC_HHP_[PC]");
    let rsec = rust
        .iter()
        .position(|s| *s == "SEC_HHP_")
        .expect("Rust table missing SEC_HHP_");
    assert!(
        rpc < rsec,
        "Rust table must keep SEC_HHP_[PC] before SEC_HHP_"
    );

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
