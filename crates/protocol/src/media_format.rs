//! Canonical media extension, MIME, UPnP class, and protocol-capability map.
//!
//! Keep admission and representation derived from this table. Containers that
//! MiniDLNA accepts as either audio or video expose both MIME types and are
//! resolved from the streams found by the scanner.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Video,
    Audio,
    Image,
}

impl MediaKind {
    pub const fn upnp_class(self) -> &'static str {
        match self {
            Self::Video => "item.videoItem",
            Self::Audio => "item.audioItem.musicTrack",
            Self::Image => "item.imageItem.photo",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaFormat {
    pub extension: &'static str,
    pub video_mime: Option<&'static str>,
    pub audio_mime: Option<&'static str>,
    pub image_mime: Option<&'static str>,
}

impl MediaFormat {
    pub const fn is_ambiguous(self) -> bool {
        self.video_mime.is_some() && self.audio_mime.is_some()
    }

    pub const fn allows(self, kind: MediaKind) -> bool {
        match kind {
            MediaKind::Video => self.video_mime.is_some(),
            MediaKind::Audio => self.audio_mime.is_some(),
            MediaKind::Image => self.image_mime.is_some(),
        }
    }

    pub fn resolve(self, detected: Option<MediaKind>) -> ResolvedMediaFormat {
        if let Some(kind) = detected {
            let mime = match kind {
                MediaKind::Video => self.video_mime,
                MediaKind::Audio => self.audio_mime,
                MediaKind::Image => self.image_mime,
            };
            if let Some(mime) = mime {
                return ResolvedMediaFormat {
                    extension: self.extension,
                    mime,
                    kind,
                };
            }
        }
        if let Some(mime) = self.video_mime {
            return ResolvedMediaFormat {
                extension: self.extension,
                mime,
                kind: MediaKind::Video,
            };
        }
        if let Some(mime) = self.audio_mime {
            return ResolvedMediaFormat {
                extension: self.extension,
                mime,
                kind: MediaKind::Audio,
            };
        }
        ResolvedMediaFormat {
            extension: self.extension,
            mime: self.image_mime.unwrap_or("application/octet-stream"),
            kind: MediaKind::Image,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedMediaFormat {
    pub extension: &'static str,
    pub mime: &'static str,
    pub kind: MediaKind,
}

impl ResolvedMediaFormat {
    pub const fn upnp_class(self) -> &'static str {
        self.kind.upnp_class()
    }
}

const fn video(extension: &'static str, mime: &'static str) -> MediaFormat {
    MediaFormat {
        extension,
        video_mime: Some(mime),
        audio_mime: None,
        image_mime: None,
    }
}

const fn audio(extension: &'static str, mime: &'static str) -> MediaFormat {
    MediaFormat {
        extension,
        video_mime: None,
        audio_mime: Some(mime),
        image_mime: None,
    }
}

/// Every extension admitted by the scanner, with no octet-stream fallback.
pub const MEDIA_FORMATS: &[MediaFormat] = &[
    video("mpg", "video/mpeg"),
    video("mpeg", "video/mpeg"),
    video("avi", "video/x-msvideo"),
    video("divx", "video/divx"),
    MediaFormat {
        extension: "asf",
        video_mime: Some("video/x-ms-wmv"),
        audio_mime: Some("audio/x-ms-wma"),
        image_mime: None,
    },
    video("wmv", "video/x-ms-wmv"),
    MediaFormat {
        extension: "mp4",
        video_mime: Some("video/mp4"),
        audio_mime: Some("audio/mp4"),
        image_mime: None,
    },
    video("m4v", "video/mp4"),
    video("mts", "video/vnd.dlna.mpeg-tts"),
    video("m2ts", "video/vnd.dlna.mpeg-tts"),
    video("m2t", "video/vnd.dlna.mpeg-tts"),
    video("mkv", "video/x-matroska"),
    video("vob", "video/mpeg"),
    video("ts", "video/vnd.dlna.mpeg-tts"),
    video("flv", "video/x-flv"),
    video("xvid", "video/x-msvideo"),
    video("mov", "video/quicktime"),
    MediaFormat {
        extension: "3gp",
        video_mime: Some("video/3gpp"),
        audio_mime: Some("audio/3gpp"),
        image_mime: None,
    },
    video("rm", "application/vnd.rn-realmedia"),
    video("rmvb", "application/vnd.rn-realmedia-vbr"),
    video("webm", "video/webm"),
    audio("mp3", "audio/mpeg"),
    audio("flac", "audio/x-flac"),
    audio("wma", "audio/x-ms-wma"),
    audio("fla", "audio/x-flac"),
    audio("flc", "audio/x-flac"),
    audio("m4a", "audio/mp4"),
    audio("aac", "audio/aac"),
    audio("m4p", "audio/mp4"),
    audio("wav", "audio/x-wav"),
    audio("ogg", "application/ogg"),
    audio("pcm", "audio/L16"),
    audio("dsf", "audio/x-dsd"),
    audio("dff", "audio/x-dsd"),
    MediaFormat {
        extension: "jpg",
        video_mime: None,
        audio_mime: None,
        image_mime: Some("image/jpeg"),
    },
    MediaFormat {
        extension: "jpeg",
        video_mime: None,
        audio_mime: None,
        image_mime: Some("image/jpeg"),
    },
];

pub fn media_format_for_extension(extension: &str) -> Option<MediaFormat> {
    MEDIA_FORMATS
        .iter()
        .copied()
        .find(|format| format.extension.eq_ignore_ascii_case(extension))
}

pub fn media_format_for_name(name: &str) -> Option<MediaFormat> {
    let extension = name.rsplit_once('.')?.1;
    media_format_for_extension(extension)
}

/// Wildcard HTTP capabilities derived from the same MIME map as scan and GET.
pub fn wildcard_protocol_info_entries() -> Vec<String> {
    let mut mimes = Vec::new();
    for format in MEDIA_FORMATS {
        for mime in [format.video_mime, format.audio_mime, format.image_mime]
            .into_iter()
            .flatten()
        {
            if !mimes.contains(&mime) {
                mimes.push(mime);
            }
        }
    }
    mimes
        .into_iter()
        .map(|mime| format!("http-get:*:{mime}:*"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn oracle(name: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("docs/oracle")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    }

    #[test]
    fn every_entry_resolves_without_octet_stream() {
        for format in MEDIA_FORMATS {
            for kind in [MediaKind::Video, MediaKind::Audio, MediaKind::Image] {
                if format.allows(kind) {
                    let resolved = format.resolve(Some(kind));
                    assert_ne!(resolved.mime, "application/octet-stream");
                    assert!(resolved.upnp_class().starts_with("item."));
                }
            }
        }
    }

    #[test]
    fn admitted_extensions_and_classes_match_scanner_oracle() {
        let utils = oracle("utils-media.c");
        for format in MEDIA_FORMATS {
            assert!(
                utils.contains(&format!("\".{}\"", format.extension)),
                "MiniDLNA scanner oracle does not admit .{}",
                format.extension
            );
        }

        let scanner = oracle("scanner-classification.c");
        for class in [
            MediaKind::Video.upnp_class(),
            MediaKind::Audio.upnp_class(),
            MediaKind::Image.upnp_class(),
        ] {
            assert!(scanner.contains(class), "scanner oracle missing {class}");
        }
        assert!(scanner.contains("TYPE_PLAYLIST"));
        assert!(scanner.contains("Fall back to audio"));
    }

    #[test]
    fn advertised_wildcards_cover_reference_protocol_info_core() {
        let reference = oracle("upnpglobalvars.h");
        let generated = wildcard_protocol_info_entries();
        for mime in [
            "image/jpeg",
            "video/mpeg",
            "video/mp4",
            "video/x-matroska",
            "video/x-ms-wmv",
            "video/x-msvideo",
            "video/x-flv",
            "video/quicktime",
            "audio/mpeg",
            "audio/mp4",
            "audio/x-wav",
            "audio/x-flac",
            "audio/x-dsd",
            "application/ogg",
            "application/vnd.rn-realmedia",
            "application/vnd.rn-realmedia-vbr",
            "video/webm",
        ] {
            let entry = format!("http-get:*:{mime}:*");
            assert!(
                reference.contains(&format!("http-get:*:{mime}:")),
                "reference missing protocol-info MIME {mime}"
            );
            assert!(generated.contains(&entry), "generated list missing {entry}");
        }
    }
}
