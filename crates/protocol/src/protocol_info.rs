use crate::media_format::wildcard_protocol_info_entries;

/// rustyDLNA's canonical profiled and compatibility protocol-info source values.
///
/// Entry order and bytes are compatibility-sensitive: some renderers select the
/// first profile they understand.
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
    "http-get:*:application/ogg:*,",
    "http-get:*:application/vnd.rn-realmedia:*,",
    "http-get:*:application/vnd.rn-realmedia-vbr:*,",
    "http-get:*:video/webm:*"
);

/// Profiled entries plus wildcard entries generated from the canonical
/// extension/MIME map. This prevents the scanner from serving a format that
/// ConnectionManager does not advertise.
pub fn protocol_info_source() -> String {
    let mut entries = PROTOCOL_INFO_SOURCE
        .split(',')
        .map(str::to_string)
        .collect::<Vec<_>>();
    for entry in wildcard_protocol_info_entries() {
        if !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    entries.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiled_source_bytes_and_order_are_stable() {
        let checksum = PROTOCOL_INFO_SOURCE
            .as_bytes()
            .iter()
            .fold(0x811c_9dc5_u32, |hash, byte| {
                (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
            });
        let entries = PROTOCOL_INFO_SOURCE.split(',').collect::<Vec<_>>();
        assert_eq!(PROTOCOL_INFO_SOURCE.len(), 4_721);
        assert_eq!(checksum, 0x576a_ef88);
        assert_eq!(entries.len(), 94);
        assert_eq!(
            entries.first(),
            Some(&"http-get:*:image/jpeg:DLNA.ORG_PN=JPEG_TN")
        );
        assert_eq!(entries.last(), Some(&"http-get:*:video/webm:*"));
    }

    #[test]
    fn generated_wildcards_append_in_canonical_order_without_duplicates() {
        let profiled = PROTOCOL_INFO_SOURCE.split(',').collect::<Vec<_>>();
        let expected_tail = wildcard_protocol_info_entries()
            .into_iter()
            .filter(|entry| !profiled.contains(&entry.as_str()))
            .collect::<Vec<_>>();
        let source = protocol_info_source();
        let entries = source.split(',').collect::<Vec<_>>();

        assert_eq!(&entries[..profiled.len()], profiled);
        assert_eq!(&entries[profiled.len()..], expected_tail);
    }
}
