//! Browser trick-play preview sidecar grammar.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Reserved hidden container within each video's parent directory.
pub const TRICKPLAY_DIRECTORY_NAME: &str = ".rusty_previews";
/// Previous adjacent-directory suffix, retained so scanners ignore old output.
pub const TRICKPLAY_LEGACY_DIRECTORY_SUFFIX: &str = ".rustydlna-previews";
/// Compatibility name for the previous adjacent-directory suffix.
pub const TRICKPLAY_DIRECTORY_SUFFIX: &str = TRICKPLAY_LEGACY_DIRECTORY_SUFFIX;
/// Manifest published last after every referenced sprite sheet is complete.
pub const TRICKPLAY_MANIFEST_NAME: &str = "manifest.json";
/// Current on-disk and browser API schema.
pub const TRICKPLAY_SCHEMA_VERSION: u8 = 1;
/// Default offline-generator frame width when no resolution option is supplied.
pub const TRICKPLAY_FRAME_WIDTH: u32 = 640;
/// Default offline-generator frame height when no resolution option is supplied.
pub const TRICKPLAY_FRAME_HEIGHT: u32 = 360;
/// Default sprite-sheet columns for 640×360 frames.
pub const TRICKPLAY_SHEET_COLUMNS: u32 = 5;
/// Default sprite-sheet rows for 640×360 frames.
pub const TRICKPLAY_SHEET_ROWS: u32 = 10;
pub const TRICKPLAY_TARGET_FRAMES: u32 = 2_400;
pub const TRICKPLAY_FRAMES_PER_SHEET: u32 = TRICKPLAY_SHEET_COLUMNS * TRICKPLAY_SHEET_ROWS;
pub const TRICKPLAY_MAX_SHEETS: u32 = 256;
pub const TRICKPLAY_MIN_FRAME_DIMENSION: u32 = 16;
pub const TRICKPLAY_MAX_FRAME_DIMENSION: u32 = 4_096;
pub const TRICKPLAY_MAX_FRAME_PIXELS: u32 = 4_194_304;
pub const TRICKPLAY_MAX_LAYOUT_AXIS: u32 = 10;
pub const TRICKPLAY_MAX_SHEET_DIMENSION: u32 = 4_096;
pub const TRICKPLAY_MAX_SHEET_PIXELS: u32 = 12_000_000;
pub const TRICKPLAY_MAX_DURATION_SECONDS: f64 = 7.0 * 24.0 * 60.0 * 60.0;
pub const TRICKPLAY_MAX_MANIFEST_BYTES: u64 = 16 * 1024;
pub const TRICKPLAY_MAX_SHEET_BYTES: u64 = 16 * 1024 * 1024;
pub const TRICKPLAY_ASSET_REVISION_HEX_LEN: usize = 16;

/// Return whether a raw directory name is reserved for trick-play output.
pub fn is_trickplay_directory_name(name: &OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    bytes == TRICKPLAY_DIRECTORY_NAME.as_bytes()
        || bytes.ends_with(TRICKPLAY_LEGACY_DIRECTORY_SUFFIX.as_bytes())
}

/// Hidden generated directory for one media path, preserving raw Unix names.
pub fn trickplay_directory_for_media(media_path: &Path) -> Option<PathBuf> {
    let name = OsString::from(media_path.file_stem()?);
    if name.is_empty() {
        return None;
    }
    Some(
        media_path
            .parent()?
            .join(TRICKPLAY_DIRECTORY_NAME)
            .join(name),
    )
}

/// Adaptive whole-second interval targeting at most 2,400 preview frames.
pub fn trickplay_interval_seconds(duration_seconds: f64) -> Option<u32> {
    trickplay_interval_seconds_for_target(duration_seconds, TRICKPLAY_TARGET_FRAMES)
}

fn trickplay_interval_seconds_for_target(duration_seconds: f64, target_frames: u32) -> Option<u32> {
    if !duration_seconds.is_finite()
        || duration_seconds <= 0.0
        || duration_seconds > TRICKPLAY_MAX_DURATION_SECONDS
        || target_frames == 0
        || target_frames > TRICKPLAY_TARGET_FRAMES
    {
        return None;
    }
    Some(
        (duration_seconds / f64::from(target_frames))
            .ceil()
            .max(1.0) as u32,
    )
}

/// Adaptive interval honoring both the global frame bound and one layout's
/// maximum 256-sheet capacity.
pub fn trickplay_interval_seconds_for_layout(
    duration_seconds: f64,
    columns: u32,
    rows: u32,
) -> Option<u32> {
    let frames_per_sheet = columns.checked_mul(rows)?;
    if frames_per_sheet == 0
        || columns > TRICKPLAY_MAX_LAYOUT_AXIS
        || rows > TRICKPLAY_MAX_LAYOUT_AXIS
    {
        return None;
    }
    let capacity = frames_per_sheet
        .checked_mul(TRICKPLAY_MAX_SHEETS)?
        .min(TRICKPLAY_TARGET_FRAMES);
    trickplay_interval_seconds_for_target(duration_seconds, capacity)
}

pub fn trickplay_frame_count(duration_seconds: f64, interval_seconds: u32) -> Option<u32> {
    if interval_seconds == 0
        || !duration_seconds.is_finite()
        || duration_seconds <= 0.0
        || duration_seconds > TRICKPLAY_MAX_DURATION_SECONDS
    {
        return None;
    }
    let count = (duration_seconds / f64::from(interval_seconds)).ceil() as u32;
    (count > 0 && count <= TRICKPLAY_TARGET_FRAMES).then_some(count)
}

pub fn trickplay_layout_is_valid(
    frame_width: u32,
    frame_height: u32,
    columns: u32,
    rows: u32,
) -> bool {
    let Some(frame_pixels) = frame_width.checked_mul(frame_height) else {
        return false;
    };
    let Some(sheet_width) = frame_width.checked_mul(columns) else {
        return false;
    };
    let Some(sheet_height) = frame_height.checked_mul(rows) else {
        return false;
    };
    let Some(sheet_pixels) = sheet_width.checked_mul(sheet_height) else {
        return false;
    };
    (TRICKPLAY_MIN_FRAME_DIMENSION..=TRICKPLAY_MAX_FRAME_DIMENSION).contains(&frame_width)
        && (TRICKPLAY_MIN_FRAME_DIMENSION..=TRICKPLAY_MAX_FRAME_DIMENSION).contains(&frame_height)
        && frame_pixels <= TRICKPLAY_MAX_FRAME_PIXELS
        && (1..=TRICKPLAY_MAX_LAYOUT_AXIS).contains(&columns)
        && (1..=TRICKPLAY_MAX_LAYOUT_AXIS).contains(&rows)
        && sheet_width <= TRICKPLAY_MAX_SHEET_DIMENSION
        && sheet_height <= TRICKPLAY_MAX_SHEET_DIMENSION
        && sheet_pixels <= TRICKPLAY_MAX_SHEET_PIXELS
}

pub fn trickplay_sheet_count_for_layout(frame_count: u32, columns: u32, rows: u32) -> Option<u32> {
    let frames_per_sheet = columns.checked_mul(rows)?;
    (frame_count > 0
        && frame_count <= TRICKPLAY_TARGET_FRAMES
        && frames_per_sheet > 0
        && columns <= TRICKPLAY_MAX_LAYOUT_AXIS
        && rows <= TRICKPLAY_MAX_LAYOUT_AXIS)
        .then(|| frame_count.div_ceil(frames_per_sheet))
        .filter(|count| *count <= TRICKPLAY_MAX_SHEETS)
}

/// Compatibility helper using the default 640×360 layout.
pub fn trickplay_sheet_count(frame_count: u32) -> Option<u32> {
    trickplay_sheet_count_for_layout(frame_count, TRICKPLAY_SHEET_COLUMNS, TRICKPLAY_SHEET_ROWS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_target_at_most_the_default_sheet_count() {
        assert_eq!(TRICKPLAY_FRAMES_PER_SHEET, 50);
        assert_eq!(trickplay_sheet_count(TRICKPLAY_TARGET_FRAMES), Some(48));
        for (duration, interval) in [
            (20.0 * 60.0, 1),
            (45.0 * 60.0, 2),
            (90.0 * 60.0, 3),
            (2.0 * 60.0 * 60.0, 3),
            (3.0 * 60.0 * 60.0, 5),
        ] {
            assert_eq!(trickplay_interval_seconds(duration), Some(interval));
            let frames = trickplay_frame_count(duration, interval).unwrap();
            assert!(frames <= TRICKPLAY_TARGET_FRAMES);
            assert!(trickplay_sheet_count(frames).unwrap() <= TRICKPLAY_MAX_SHEETS);
        }
    }

    #[test]
    fn manifest_driven_layouts_are_bounded() {
        assert!(trickplay_layout_is_valid(640, 360, 5, 10));
        assert!(trickplay_layout_is_valid(960, 540, 3, 7));
        assert_eq!(trickplay_sheet_count_for_layout(2_400, 3, 7), Some(115));
        assert!(!trickplay_layout_is_valid(4_096, 2_160, 1, 1));
        assert!(!trickplay_layout_is_valid(1_920, 1_080, 2, 3));
        assert!(!trickplay_layout_is_valid(640, 360, 0, 10));
        assert_eq!(trickplay_sheet_count_for_layout(2_400, 1, 1), None);
    }

    #[test]
    fn large_frames_use_a_layout_aware_interval() {
        let duration = 2.0 * 60.0 * 60.0;
        assert_eq!(
            trickplay_interval_seconds_for_layout(duration, 3, 7),
            Some(3)
        );
        assert_eq!(
            trickplay_interval_seconds_for_layout(duration, 3, 2),
            Some(5)
        );
        let frames = trickplay_frame_count(duration, 5).unwrap();
        assert_eq!(frames, 1_440);
        assert_eq!(trickplay_sheet_count_for_layout(frames, 3, 2), Some(240));
        assert_eq!(trickplay_interval_seconds_for_layout(duration, 0, 2), None);
        assert_eq!(trickplay_interval_seconds_for_layout(duration, 11, 1), None);
    }

    #[test]
    fn directory_grammar_preserves_raw_stems() {
        assert_eq!(
            trickplay_directory_for_media(Path::new("Film.Name.mkv")),
            Some(PathBuf::from(".rusty_previews/Film.Name"))
        );
        assert!(is_trickplay_directory_name(OsStr::new(".rusty_previews")));
        assert!(is_trickplay_directory_name(OsStr::new(
            "Film.Name.rustydlna-previews"
        )));
        assert!(!is_trickplay_directory_name(OsStr::new(
            "ordinary-previews"
        )));
    }

    #[cfg(unix)]
    #[test]
    fn directory_grammar_preserves_non_utf8_stems() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let path = PathBuf::from(OsString::from_vec(b"film-\xff.mkv".to_vec()));
        let directory = trickplay_directory_for_media(&path).unwrap();
        assert_eq!(directory.file_name().unwrap().as_bytes(), b"film-\xff");
        assert_eq!(
            directory.parent().unwrap().file_name().unwrap().as_bytes(),
            b".rusty_previews"
        );
    }
}
