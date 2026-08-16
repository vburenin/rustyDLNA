//! PlaysForSure-style object IDs from `src/scanner.h`.

pub const ROOT_ID: &str = "0";
pub const BROWSEDIR_ID: &str = "64";
pub const MUSIC_ID: &str = "1";
pub const MUSIC_ALL_ID: &str = "1$4";
pub const MUSIC_GENRE_ID: &str = "1$5";
pub const MUSIC_ARTIST_ID: &str = "1$6";
pub const MUSIC_ALBUM_ID: &str = "1$7";
pub const MUSIC_PLIST_ID: &str = "1$F";
pub const MUSIC_DIR_ID: &str = "1$14";
pub const MUSIC_CONTRIB_ARTIST_ID: &str = "1$100";
pub const MUSIC_ALBUM_ARTIST_ID: &str = "1$107";
pub const MUSIC_COMPOSER_ID: &str = "1$108";
pub const MUSIC_RATING_ID: &str = "1$101";
pub const MUSIC_RECENT_ID: &str = "1$FF0";

pub const VIDEO_ID: &str = "2";
pub const VIDEO_ALL_ID: &str = "2$8";
pub const VIDEO_GENRE_ID: &str = "2$9";
pub const VIDEO_ACTOR_ID: &str = "2$A";
pub const VIDEO_SERIES_ID: &str = "2$E";
pub const VIDEO_PLIST_ID: &str = "2$10";
pub const VIDEO_DIR_ID: &str = "2$15";
pub const VIDEO_RATING_ID: &str = "2$200";
pub const VIDEO_RECENT_ID: &str = "2$FF0";
/// Recently Added lists the newest unique videos. No time window.
pub const RECENT_MAX: usize = 200;

pub const IMAGE_ID: &str = "3";
pub const IMAGE_ALL_ID: &str = "3$B";
pub const IMAGE_DATE_ID: &str = "3$C";
pub const IMAGE_ALBUM_ID: &str = "3$D";
pub const IMAGE_CAMERA_ID: &str = "3$D2";
pub const IMAGE_PLIST_ID: &str = "3$11";
pub const IMAGE_DIR_ID: &str = "3$16";
pub const IMAGE_RATING_ID: &str = "3$300";
pub const IMAGE_RECENT_ID: &str = "3$FF0";

/// Samsung DCM10 BASICVIEW aliases (`src/containers.c`).
pub const SAMSUNG_AUDIO: &str = "A";
pub const SAMSUNG_VIDEO: &str = "V";
pub const SAMSUNG_IMAGE: &str = "I";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_match_minidlna() {
        assert_eq!(BROWSEDIR_ID, "64");
        assert_eq!(MUSIC_ID, "1");
        assert_eq!(VIDEO_ID, "2");
        assert_eq!(VIDEO_ALL_ID, "2$8");
        assert_eq!(VIDEO_DIR_ID, "2$15");
        assert_eq!(VIDEO_RECENT_ID, "2$FF0");
        assert_eq!(IMAGE_ID, "3");
        assert_eq!(ROOT_ID, "0");
        assert_eq!(RECENT_MAX, 200);
    }
}
