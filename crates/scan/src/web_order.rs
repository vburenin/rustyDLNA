//! Collection-aware title ordering for the flat web library.

use std::path::Path;

/// An explicitly numbered movie's containing collection. No media is renamed.
#[derive(Debug, PartialEq, Eq)]
pub struct VideoCollection {
    /// Opaque directory identity; never expose the absolute path in the API.
    pub id: String,
    pub title: String,
    pub sequence: u32,
}

pub fn video_collection(path: &Path, mime: &str) -> Option<VideoCollection> {
    if !mime.starts_with("video/") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    let (number, movie) = stem.split_once(" - ")?;
    if number.is_empty() || number.len() > 3 || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let sequence = number.parse::<u32>().ok().filter(|number| *number > 0)?;
    // Require the normalized movie year so numbered episodes, workouts, and
    // music tracks do not accidentally become movie collections.
    let (title, suffix) = movie.split_once(" (")?;
    let year = suffix.as_bytes().get(..5)?;
    let year_number = std::str::from_utf8(&year[..4]).ok()?.parse::<u32>().ok()?;
    if title.is_empty()
        || !year[..4].iter().all(u8::is_ascii_digit)
        || year[4] != b')'
        || !(1800..=2099).contains(&year_number)
    {
        return None;
    }
    let parent = path.parent()?;
    let title = parent.file_name()?.to_str()?.to_owned();
    let id = crate::sha256_hex(parent.as_os_str().as_encoded_bytes());
    Some(VideoCollection {
        id,
        title,
        sequence,
    })
}

/// A single key keeps SQLite and memory pagination identical, including Unicode.
/// NUL separates fields; filesystem names cannot contain NUL. Group identity
/// precedes sequence so distinct same-named folders never interleave.
pub fn web_media_title_key(path: &Path, mime: &str, title: &str) -> String {
    match video_collection(path, mime) {
        Some(group) => format!(
            "{}\0{}\0{:03}\0{}",
            group.title.to_lowercase(),
            group.id,
            group.sequence,
            title.to_lowercase()
        ),
        None => title.to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn movies_sort_in_collection_sequence_among_standalones() {
        let mut paths = [
            "/movies/Briar Saga/03 - The Last Beacon (2025).mkv",
            "/movies/Copper Road (2006).mkv",
            "/movies/Briar Saga/01 - Briar Saga (2002).mkv",
            "/movies/Amber Road (1957).mkv",
            "/movies/Briar Saga/02 - Across the River (2007).mkv",
        ];
        paths.sort_by_cached_key(|path| {
            let path = Path::new(path);
            web_media_title_key(
                path,
                "video/x-matroska",
                path.file_stem().unwrap().to_str().unwrap(),
            )
        });
        assert!(paths[0].ends_with("Amber Road (1957).mkv"));
        assert!(paths[1].contains("01 -"));
        assert!(paths[2].contains("02 -"));
        assert!(paths[3].contains("03 -"));
        assert!(paths[4].ends_with("Copper Road (2006).mkv"));
    }

    #[test]
    fn only_explicitly_numbered_movies_form_groups() {
        for name in [
            "2001 - A Fictional Journey (1968).mkv",
            "Amber Road (1957).mkv",
            "01 - Show - S01E01.mkv",
            "1 - 01 Upper Body Circuit.mp4",
            "01 - Movie (abcd).mkv",
            "01 - Movie (2025x.mkv",
            "00 - Movie (2025).mkv",
        ] {
            assert_eq!(
                video_collection(&Path::new("/movies").join(name), "video/mp4"),
                None,
                "{name}"
            );
        }
        let path = Path::new("/anime/Example Studio/17 - The Lantern (2008).mkv");
        let group = video_collection(path, "video/x-matroska").unwrap();
        assert_eq!(group.title, "Example Studio");
        assert_eq!(group.sequence, 17);
        assert_eq!(group.id.len(), 64);
        assert_eq!(video_collection(path, "audio/flac"), None);
    }
}
