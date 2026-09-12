//! The single-file demuxers supported by scanner and media helpers.
//! Manifest/playlist demuxers and image sequences are deliberately excluded.

/// Names used by libavformat's `format_whitelist` on FFmpeg 6 through 8.
/// Raw HEVC is included for the server-owned Dolby Vision intermediate.
pub const MEDIA_DEMUXER_WHITELIST: &str = "matroska,webm,mov,mp4,m4a,3gp,3g2,mj2,avi,mpegts,mpeg,mpegvideo,flv,asf,rm,mp3,flac,aac,wav,ogg,dsf,iff,s16be,s16le,hevc,jpeg_pipe,mjpeg,png_pipe,bmp_pipe,tiff_pipe,webp_pipe";

/// Resolve exact libav demuxer names; never relabel an unknown input.
pub fn media_container_for_demuxer(names: &str) -> Option<&'static str> {
    let mut container = None;
    for name in names.split(',') {
        let detected = match name {
            "matroska" | "webm" => "mkv",
            "mov" | "mp4" | "m4a" | "3gp" | "3g2" | "mj2" => "mp4",
            "avi" => "avi",
            "mpegts" => "mpeg-ts",
            "mpeg" | "mpegvideo" => "mpeg",
            "flv" => "flv",
            "asf" => "asf",
            "rm" => "rm",
            "mp3" => "mp3",
            "flac" => "flac",
            "aac" => "aac",
            "wav" => "wav",
            "ogg" => "ogg",
            "dsf" => "dsf",
            "iff" => "dff",
            "s16be" | "s16le" => "pcm",
            "hevc" => "hevc",
            "jpeg_pipe" | "mjpeg" => "jpeg",
            "png_pipe" => "png",
            "bmp_pipe" => "bmp",
            "tiff_pipe" => "tiff",
            "webp_pipe" => "webp",
            _ => return None,
        };
        if container.is_some_and(|container| container != detected) {
            return None;
        }
        container = Some(detected);
    }
    container
}

/// Input-only CLI options. `fd` is seekable for regular files on FFmpeg 6+.
/// No pathname or network protocol can be opened, even by a nested demuxer.
pub fn inherited_media_input_options(fd: i32) -> Vec<std::ffi::OsString> {
    [
        "-protocol_whitelist".into(),
        "fd".into(),
        "-format_whitelist".into(),
        MEDIA_DEMUXER_WHITELIST.into(),
        "-fd".into(),
        fd.to_string().into(),
    ]
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demuxer_names_are_exact_and_unknown_formats_fail_closed() {
        assert_eq!(
            media_container_for_demuxer("mov,mp4,m4a,3gp,3g2,mj2"),
            Some("mp4")
        );
        assert_eq!(media_container_for_demuxer("matroska,webm"), Some("mkv"));
        for name in ["dash", "hls", "concat", "image2", "not-mp4", "", "mov,dash"] {
            assert_eq!(media_container_for_demuxer(name), None, "{name}");
        }
        for name in MEDIA_DEMUXER_WHITELIST.split(',') {
            assert!(media_container_for_demuxer(name).is_some(), "{name}");
        }
    }
}
