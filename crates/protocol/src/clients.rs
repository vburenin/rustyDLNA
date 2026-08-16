//! Client table from `src/clients.c` / `clients.h`.
//! First matching row wins. Order is load-bearing (Samsung `SEC_HHP_[PC]`
//! sits above generic `SEC_HHP_`).

use bitflags::bitflags;

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ClientFlags: u32 {
        const DLNA            = 0x0000_0001;
        const MIME_AVI_DIVX   = 0x0000_0002;
        const MIME_AVI_AVI    = 0x0000_0004;
        const MIME_FLAC_FLAC  = 0x0000_0008;
        const MIME_WAV_WAV    = 0x0000_0010;
        const RESIZE_THUMBS   = 0x0000_0020;
        const NO_RESIZE       = 0x0000_0040;
        const MS_PFS          = 0x0000_0080;
        const SAMSUNG         = 0x0000_0100;
        const SAMSUNG_DCM10   = 0x0000_0200;
        const AUDIO_ONLY      = 0x0000_0400;
        const FORCE_SORT      = 0x0000_0800;
        const CAPTION_RES     = 0x0000_1000;
        const SKIP_DLNA_PN    = 0x0000_2000;
        const CONVERT_MS      = 0x0000_4000;
        /// rustyDLNA: client cannot play typical remuxes (DV P7, TrueHD, MKV).
        const NEED_SAFE_VIDEO = 0x0000_8000;
        const HAS_CAPTIONS    = 0x1000_0000;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientKind {
    Unknown,
    Xbox,
    Ps3,
    Cling,
    AllShare,
    SamsungBdJ5500,
    SamsungSeriesCdeBdp,
    SamsungSeriesQ,
    SamsungSeriesCde,
    SamsungSeriesA,
    SamsungSeriesB,
    Panasonic,
    NetFrontLivingConnect,
    DenonReceiver,
    FreeBox,
    PopcornHour,
    SonyBdp,
    LgNetCast,
    Lg,
    SonyBravia,
    SonyInternetTv,
    NetgearEva2000,
    DirecTv,
    ToshibaTv,
    HyundaiTv,
    RokuSoundBridge,
    MarantzDmp,
    MediaRoom,
    LifeTab,
    AsusOPlay,
    BubbleUpnp,
    Movian,
    Kodi,
    Windows,
    Tivo,
    StandardDlna150,
    StandardUpnp,
    /// New: Google Cast / Streamer / Chromecast (not in the dialect).
    GoogleCast,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchKind {
    None,
    UserAgent,
    XAvClientInfo,
    FriendlyName,
    ModelName,
    FriendlyNameSsdp,
}

#[derive(Clone, Copy, Debug)]
pub struct ClientProfile {
    pub kind: ClientKind,
    pub flags: ClientFlags,
    pub name: &'static str,
    pub match_str: Option<&'static str>,
    pub match_kind: MatchKind,
}

const fn f(bits: u32) -> ClientFlags {
    ClientFlags::from_bits_truncate(bits)
}

/// rustyDLNA table, then rustyDLNA additions. Do not reorder Samsung rows.
pub static CLIENTS: &[ClientProfile] = &[
    ClientProfile {
        kind: ClientKind::Unknown,
        flags: ClientFlags::empty(),
        name: "Unknown",
        match_str: None,
        match_kind: MatchKind::None,
    },
    ClientProfile {
        kind: ClientKind::Xbox,
        flags: f(ClientFlags::MIME_AVI_AVI.bits() | ClientFlags::MS_PFS.bits()),
        name: "Xbox 360",
        match_str: Some("Xbox/"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::Ps3,
        flags: f(ClientFlags::DLNA.bits() | ClientFlags::MIME_AVI_DIVX.bits()),
        name: "PLAYSTATION 3",
        match_str: Some("PLAYSTATION"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::Ps3,
        flags: f(ClientFlags::DLNA.bits() | ClientFlags::MIME_AVI_DIVX.bits()),
        name: "PLAYSTATION 3",
        match_str: Some("PLAYSTATION 3"),
        match_kind: MatchKind::XAvClientInfo,
    },
    ClientProfile {
        kind: ClientKind::Cling,
        flags: ClientFlags::MS_PFS,
        name: "Cling",
        match_str: Some("Cling/"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::AllShare,
        flags: ClientFlags::DLNA,
        name: "AllShare",
        match_str: Some("SEC_HHP_[PC]"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::SamsungBdJ5500,
        flags: f(ClientFlags::SAMSUNG.bits()
            | ClientFlags::DLNA.bits()
            | ClientFlags::NO_RESIZE.bits()
            | ClientFlags::CAPTION_RES.bits()
            | ClientFlags::SKIP_DLNA_PN.bits()),
        name: "Samsung BD J5500",
        match_str: Some("[BD]J5500"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::SamsungSeriesCdeBdp,
        flags: f(ClientFlags::SAMSUNG.bits()
            | ClientFlags::DLNA.bits()
            | ClientFlags::NO_RESIZE.bits()),
        name: "Samsung Series [CDEF] BDP",
        match_str: Some("SEC_HHP_BD"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::SamsungSeriesQ,
        flags: f(ClientFlags::SAMSUNG.bits()
            | ClientFlags::DLNA.bits()
            | ClientFlags::NO_RESIZE.bits()
            | ClientFlags::SAMSUNG_DCM10.bits()
            | ClientFlags::CAPTION_RES.bits()
            | ClientFlags::CONVERT_MS.bits()),
        name: "Samsung Series [Q]",
        match_str: Some("SEC_HHP_[TV] Samsung Q"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::SamsungSeriesQ,
        flags: f(ClientFlags::SAMSUNG.bits()
            | ClientFlags::DLNA.bits()
            | ClientFlags::NO_RESIZE.bits()
            | ClientFlags::SAMSUNG_DCM10.bits()
            | ClientFlags::CAPTION_RES.bits()
            | ClientFlags::CONVERT_MS.bits()),
        name: "Samsung Series [QN]",
        match_str: Some("SEC_HHP_Samsung QN"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::SamsungSeriesCde,
        flags: f(ClientFlags::SAMSUNG.bits()
            | ClientFlags::DLNA.bits()
            | ClientFlags::NO_RESIZE.bits()
            | ClientFlags::SAMSUNG_DCM10.bits()
            | ClientFlags::CAPTION_RES.bits()),
        name: "Samsung Series [CDEFJ]",
        match_str: Some("SEC_HHP_"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::SamsungSeriesA,
        flags: f(ClientFlags::SAMSUNG.bits()
            | ClientFlags::DLNA.bits()
            | ClientFlags::NO_RESIZE.bits()),
        name: "Samsung Series A",
        match_str: Some("SamsungWiselinkPro"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::SamsungSeriesB,
        flags: f(ClientFlags::SAMSUNG.bits()
            | ClientFlags::DLNA.bits()
            | ClientFlags::NO_RESIZE.bits()),
        name: "Samsung Series B",
        match_str: Some("Samsung DTV DMR"),
        match_kind: MatchKind::ModelName,
    },
    ClientProfile {
        kind: ClientKind::Panasonic,
        flags: f(ClientFlags::DLNA.bits() | ClientFlags::FORCE_SORT.bits()),
        name: "Panasonic",
        match_str: Some("Panasonic"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::NetFrontLivingConnect,
        flags: f(ClientFlags::DLNA.bits()
            | ClientFlags::FORCE_SORT.bits()
            | ClientFlags::CAPTION_RES.bits()),
        name: "NetFront Living Connect",
        match_str: Some("IPI/1"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::DenonReceiver,
        flags: ClientFlags::DLNA,
        name: "Denon Receiver",
        match_str: Some("bridgeCo-DMP/3"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::FreeBox,
        flags: ClientFlags::RESIZE_THUMBS,
        name: "FreeBox",
        match_str: Some("fbxupnpav/"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::PopcornHour,
        flags: ClientFlags::MIME_FLAC_FLAC,
        name: "Popcorn Hour",
        match_str: Some("SMP8634"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::SonyBdp,
        flags: ClientFlags::DLNA,
        name: "Sony BDP",
        match_str: Some("mv=\"2.0\""),
        match_kind: MatchKind::XAvClientInfo,
    },
    ClientProfile {
        kind: ClientKind::LgNetCast,
        flags: f(ClientFlags::DLNA.bits()
            | ClientFlags::CAPTION_RES.bits()
            | ClientFlags::MIME_FLAC_FLAC.bits()),
        name: "LG",
        match_str: Some("LGE_DLNA_SDK/1.6.0"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::Lg,
        flags: f(ClientFlags::DLNA.bits()
            | ClientFlags::CAPTION_RES.bits()
            | ClientFlags::MIME_FLAC_FLAC.bits()),
        name: "LG",
        match_str: Some("LGE_DLNA_SDK"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::SonyBravia,
        flags: ClientFlags::DLNA,
        name: "Sony Bravia",
        match_str: Some("BRAVIA"),
        match_kind: MatchKind::XAvClientInfo,
    },
    ClientProfile {
        kind: ClientKind::SonyInternetTv,
        flags: ClientFlags::DLNA,
        name: "Sony Internet TV",
        match_str: Some("INTERNET TV"),
        match_kind: MatchKind::XAvClientInfo,
    },
    ClientProfile {
        kind: ClientKind::NetgearEva2000,
        flags: f(ClientFlags::MS_PFS.bits() | ClientFlags::RESIZE_THUMBS.bits()),
        name: "EVA2000",
        match_str: Some("Verismo,"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::DirecTv,
        flags: ClientFlags::RESIZE_THUMBS,
        name: "DirecTV",
        match_str: Some("DIRECTV "),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::ToshibaTv,
        flags: ClientFlags::DLNA,
        name: "Toshiba TV",
        match_str: Some("UPnP/1.0 DLNADOC/1.50 Intel_SDK_for_UPnP_devices/1.2"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::HyundaiTv,
        flags: ClientFlags::DLNA,
        name: "Hyundai TV",
        match_str: Some("HYUNDAITV"),
        match_kind: MatchKind::FriendlyName,
    },
    ClientProfile {
        kind: ClientKind::RokuSoundBridge,
        flags: f(ClientFlags::MS_PFS.bits()
            | ClientFlags::AUDIO_ONLY.bits()
            | ClientFlags::MIME_WAV_WAV.bits()
            | ClientFlags::FORCE_SORT.bits()),
        name: "Roku SoundBridge",
        match_str: Some("Roku SoundBridge"),
        match_kind: MatchKind::ModelName,
    },
    ClientProfile {
        kind: ClientKind::MarantzDmp,
        flags: f(ClientFlags::DLNA.bits() | ClientFlags::MIME_WAV_WAV.bits()),
        name: "marantz DMP",
        match_str: Some("marantz DMP"),
        match_kind: MatchKind::FriendlyNameSsdp,
    },
    ClientProfile {
        kind: ClientKind::MediaRoom,
        flags: ClientFlags::MS_PFS,
        name: "MS MediaRoom",
        match_str: Some("Microsoft-IPTV-Client"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::LifeTab,
        flags: ClientFlags::MS_PFS,
        name: "LIFETAB",
        match_str: Some("LIFETAB"),
        match_kind: MatchKind::FriendlyName,
    },
    ClientProfile {
        kind: ClientKind::AsusOPlay,
        flags: f(ClientFlags::DLNA.bits()
            | ClientFlags::MIME_AVI_AVI.bits()
            | ClientFlags::CAPTION_RES.bits()),
        name: "Asus OPlay Mini/Mini+",
        match_str: Some("O!Play"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::BubbleUpnp,
        flags: ClientFlags::CAPTION_RES,
        name: "BubbleUPnP",
        match_str: Some("BubbleUPnP"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::Movian,
        flags: ClientFlags::CAPTION_RES,
        name: "Movian",
        match_str: Some("Movian"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::Kodi,
        flags: f(ClientFlags::DLNA.bits()
            | ClientFlags::MIME_AVI_AVI.bits()
            | ClientFlags::CAPTION_RES.bits()),
        name: "Kodi",
        match_str: Some("Kodi"),
        match_kind: MatchKind::UserAgent,
    },
    // Kodi's UPnP stack (Platinum) often omits "Kodi" from User-Agent.
    ClientProfile {
        kind: ClientKind::Kodi,
        flags: f(ClientFlags::DLNA.bits()
            | ClientFlags::MIME_AVI_AVI.bits()
            | ClientFlags::CAPTION_RES.bits()),
        name: "Kodi",
        match_str: Some("Platinum/"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::Windows,
        flags: f(ClientFlags::DLNA.bits() | ClientFlags::MIME_AVI_AVI.bits()),
        name: "Windows",
        match_str: Some("FDSSDP"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::Tivo,
        flags: ClientFlags::empty(),
        name: "TiVo",
        match_str: Some("TvHttpClient"),
        match_kind: MatchKind::UserAgent,
    },
    // rustyDLNA: must sit above generic DLNADOC/1.50
    ClientProfile {
        kind: ClientKind::GoogleCast,
        flags: f(ClientFlags::DLNA.bits() | ClientFlags::NEED_SAFE_VIDEO.bits()),
        name: "Google Cast / Streamer",
        match_str: Some("CrKey"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::StandardDlna150,
        flags: f(ClientFlags::DLNA.bits() | ClientFlags::MIME_AVI_AVI.bits()),
        name: "Generic DLNA 1.5",
        match_str: Some("DLNADOC/1.50"),
        match_kind: MatchKind::UserAgent,
    },
    ClientProfile {
        kind: ClientKind::StandardUpnp,
        flags: ClientFlags::empty(),
        name: "Generic UPnP 1.0",
        match_str: Some("UPnP/1.0"),
        match_kind: MatchKind::UserAgent,
    },
];

fn first_match(kind: MatchKind, hay: &str) -> Option<&'static ClientProfile> {
    CLIENTS
        .iter()
        .find(|c| c.match_kind == kind && c.match_str.is_some_and(|m| hay.contains(m)))
}

pub fn identify_user_agent(ua: &str) -> Option<&'static ClientProfile> {
    first_match(MatchKind::UserAgent, ua)
}

pub fn identify_x_av_client_info(hdr: &str) -> Option<&'static ClientProfile> {
    first_match(MatchKind::XAvClientInfo, hdr)
}

pub fn identify_friendly_name(name: &str) -> Option<&'static ClientProfile> {
    first_match(MatchKind::FriendlyName, name)
}

/// Samsung `video/x-matroska` → `video/x-mkv` (`strcpy(mime+8, "mkv")`).
pub fn remap_mime(profile: &ClientProfile, mime: &str) -> String {
    if profile.flags.contains(ClientFlags::SAMSUNG) && mime == "video/x-matroska" {
        return "video/x-mkv".into();
    }
    if profile.kind == ClientKind::SonyBdp && (mime == "video/x-matroska" || mime == "video/mpeg") {
        return "video/divx".into();
    }
    if profile.flags.contains(ClientFlags::MIME_AVI_AVI) && mime == "video/x-msvideo" {
        return "video/avi".into();
    }
    if profile.flags.contains(ClientFlags::MIME_FLAC_FLAC) && mime == "audio/x-flac" {
        return "audio/flac".into();
    }
    if profile.flags.contains(ClientFlags::MIME_WAV_WAV) && mime == "audio/x-wav" {
        return "audio/wav".into();
    }
    mime.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kodi_from_ua() {
        let c = identify_user_agent("Kodi/21.0 (Linux; Android 12)").unwrap();
        assert_eq!(c.kind, ClientKind::Kodi);
        assert!(c.flags.contains(ClientFlags::DLNA));
        assert!(c.flags.contains(ClientFlags::CAPTION_RES));
        assert!(!c.flags.contains(ClientFlags::NEED_SAFE_VIDEO));
        let plat = identify_user_agent("UPnP/1.0 DLNADOC/1.50 Platinum/1.0.5.13").unwrap();
        assert_eq!(plat.kind, ClientKind::Kodi);
        assert_eq!(plat.name, "Kodi");
    }

    #[test]
    fn allshare_is_not_samsung_tv() {
        let c = identify_user_agent("DLNADOC/1.50 SEC_HHP_[PC]LPC001/1.0").unwrap();
        assert_eq!(c.kind, ClientKind::AllShare);
        assert!(!c.flags.contains(ClientFlags::SAMSUNG));
    }

    #[test]
    fn samsung_tv_after_allshare() {
        let c = identify_user_agent("DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0").unwrap();
        assert_eq!(c.kind, ClientKind::SamsungSeriesCde);
        assert!(c.flags.contains(ClientFlags::SAMSUNG_DCM10));
    }

    #[test]
    fn j5500_skips_pn() {
        let c = identify_user_agent("DLNADOC/1.50 [BD]J5500").unwrap();
        assert!(c.flags.contains(ClientFlags::SKIP_DLNA_PN));
    }

    #[test]
    fn crkey_is_cast_not_generic_dlna() {
        let c = identify_user_agent("CrKey/1.54.384650 DLNADOC/1.50").unwrap();
        assert_eq!(c.kind, ClientKind::GoogleCast);
        assert!(c.flags.contains(ClientFlags::NEED_SAFE_VIDEO));
    }

    #[test]
    fn samsung_mkv_mime() {
        let c = identify_user_agent("SEC_HHP_[TV]x").unwrap();
        assert_eq!(remap_mime(c, "video/x-matroska"), "video/x-mkv");
    }
}
