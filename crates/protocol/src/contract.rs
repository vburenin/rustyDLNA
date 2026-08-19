//! Self-contained locks for the inherited MiniDLNA wire and object-ID contract.

use std::collections::HashSet;

use crate::clients::{identify_user_agent, remap_mime, ClientFlags, CLIENTS};
use crate::object_id::{
    BROWSEDIR_ID, IMAGE_ALBUM_ID, IMAGE_ALL_ID, IMAGE_CAMERA_ID, IMAGE_DATE_ID, IMAGE_DIR_ID,
    IMAGE_ID, IMAGE_PLIST_ID, IMAGE_RATING_ID, IMAGE_RECENT_ID, MUSIC_ALBUM_ARTIST_ID,
    MUSIC_ALBUM_ID, MUSIC_ALL_ID, MUSIC_ARTIST_ID, MUSIC_COMPOSER_ID, MUSIC_CONTRIB_ARTIST_ID,
    MUSIC_DIR_ID, MUSIC_GENRE_ID, MUSIC_ID, MUSIC_PLIST_ID, MUSIC_RATING_ID, MUSIC_RECENT_ID,
    ROOT_ID, VIDEO_ACTOR_ID, VIDEO_ALL_ID, VIDEO_DIR_ID, VIDEO_GENRE_ID, VIDEO_ID, VIDEO_PLIST_ID,
    VIDEO_RATING_ID, VIDEO_RECENT_ID, VIDEO_SERIES_ID,
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

#[test]
fn inherited_paths_remain_exact_and_unique() {
    let actual = [
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
    ];
    assert_eq!(
        actual,
        [
            "/rootDesc.xml",
            "/ContentDir.xml",
            "/ctl/ContentDir",
            "/evt/ContentDir",
            "/ConnectionMgr.xml",
            "/ctl/ConnectionMgr",
            "/evt/ConnectionMgr",
            "/X_MS_MediaReceiverRegistrar.xml",
            "/ctl/X_MS_MediaReceiverRegistrar",
            "/evt/X_MS_MediaReceiverRegistrar",
        ]
    );
    assert_eq!(
        actual.into_iter().collect::<HashSet<_>>().len(),
        actual.len()
    );
}

#[test]
fn inherited_object_ids_remain_exact_and_unique() {
    let actual = [
        ROOT_ID,
        BROWSEDIR_ID,
        MUSIC_ID,
        MUSIC_ALL_ID,
        MUSIC_GENRE_ID,
        MUSIC_ARTIST_ID,
        MUSIC_ALBUM_ID,
        MUSIC_PLIST_ID,
        MUSIC_DIR_ID,
        MUSIC_CONTRIB_ARTIST_ID,
        MUSIC_ALBUM_ARTIST_ID,
        MUSIC_COMPOSER_ID,
        MUSIC_RATING_ID,
        MUSIC_RECENT_ID,
        VIDEO_ID,
        VIDEO_ALL_ID,
        VIDEO_GENRE_ID,
        VIDEO_ACTOR_ID,
        VIDEO_SERIES_ID,
        VIDEO_PLIST_ID,
        VIDEO_DIR_ID,
        VIDEO_RATING_ID,
        VIDEO_RECENT_ID,
        IMAGE_ID,
        IMAGE_ALL_ID,
        IMAGE_DATE_ID,
        IMAGE_ALBUM_ID,
        IMAGE_CAMERA_ID,
        IMAGE_PLIST_ID,
        IMAGE_DIR_ID,
        IMAGE_RATING_ID,
        IMAGE_RECENT_ID,
    ];
    assert_eq!(ROOT_ID, "0");
    assert_eq!(BROWSEDIR_ID, "64");
    assert_eq!(MUSIC_ID, "1");
    assert_eq!(VIDEO_ID, "2");
    assert_eq!(IMAGE_ID, "3");
    assert!(actual.contains(&"1$FF0"));
    assert!(actual.contains(&"2$FF0"));
    assert!(actual.contains(&"3$FF0"));
    assert_eq!(
        actual.into_iter().collect::<HashSet<_>>().len(),
        actual.len()
    );
}

#[test]
fn soap_and_namespace_contract_is_complete() {
    assert!(SOAP_METHODS.contains(&"Browse"));
    assert!(SOAP_METHODS.contains(&"Search"));
    assert!(SOAP_METHODS.contains(&"GetProtocolInfo"));
    assert!(DIDL_SCHEMAS.contains("http://purl.org/dc/elements/1.1/"));
    assert!(DIDL_SCHEMAS.contains("urn:schemas-upnp-org:metadata-1-0/upnp/"));
    assert!(DIDL_SCHEMAS.contains("urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"));
    assert!(DLNA_NAMESPACE.contains("urn:schemas-dlna-org:metadata-1-0/"));
    assert!(PV_NAMESPACE.contains("http://www.pv.com/pvns/"));
    assert!(SEC_NAMESPACE.contains("http://www.sec.co.kr/dlna"));
    assert_eq!(
        CONTENTDIRECTORY_TYPE,
        "urn:schemas-upnp-org:service:ContentDirectory:1"
    );
    assert!(!SORT_CAPS.is_empty());
    assert!(!SEARCH_CAPS.is_empty());
}

#[test]
fn inherited_date_shapes_are_normalized() {
    assert_eq!(
        w3c_normalize_date("2024-03-15T14:30:00"),
        "2024-03-15T14:30:00Z"
    );
    assert_eq!(w3c_normalize_date("1999"), "1999-01-01");
    assert_eq!(
        w3c_normalize_date("2024:03:15 14:30:00"),
        "2024-03-15T14:30:00Z"
    );
}

#[test]
fn specific_client_profiles_precede_generic_profiles() {
    let matches = CLIENTS
        .iter()
        .filter_map(|profile| profile.match_str)
        .collect::<Vec<_>>();
    let position = |needle: &str| {
        matches
            .iter()
            .position(|candidate| *candidate == needle)
            .unwrap_or_else(|| panic!("client table missing {needle}"))
    };
    assert!(position("SEC_HHP_[PC]") < position("SEC_HHP_"));
    assert!(position("CrKey") < position("DLNADOC/1.50"));
}

#[test]
fn samsung_and_kodi_quirks_remain_locked() {
    let kodi = identify_user_agent("Kodi/21.0").expect("Kodi profile");
    assert!(kodi.flags.contains(ClientFlags::DLNA));
    assert!(kodi.flags.contains(ClientFlags::CAPTION_RES));
    assert!(!kodi.flags.contains(ClientFlags::NEED_SAFE_VIDEO));

    let television = identify_user_agent("SEC_HHP_[TV]UE40D7000/1.0").expect("Samsung TV profile");
    assert_eq!(remap_mime(television, "video/x-matroska"), "video/x-mkv");
}
