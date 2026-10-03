//! Collection-aware title ordering for the flat web library.

use std::path::Path;

/// Browser substring matching uses Unicode lowercase only. It deliberately does
/// not fold accents, normalize combining marks, or alter SOAP search semantics.
pub fn web_search_normalize(value: &str) -> String {
    value.to_lowercase()
}

/// "Recently added" order key, compared newest first. Like the DLNA Recently
/// Added views it uses the stored file modification time, never NFO or
/// embedded dates; legacy second and current nanosecond stamps compare by
/// whole seconds before the raw value.
pub fn web_recently_added_key(mtime: i64) -> (i64, i64) {
    (crate::normalized_mtime_seconds(mtime), mtime)
}

pub fn web_media_file_name(path: &Path, title: &str) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| title.to_owned())
}

pub fn web_media_matches(item: &crate::MediaItem, normalized_query: &str) -> bool {
    web_media_fields_match(
        &item.path,
        &item.title,
        item.artist.as_deref(),
        item.album_artist.as_deref(),
        item.album.as_deref(),
        normalized_query,
    )
}

/// Distinct terms considered from one browser search. Each term adds one
/// substring scan per field and row, so longer queries ignore later words.
pub const WEB_SEARCH_MAX_TERMS: usize = 16;

/// Whitespace-separated terms of a normalized browser query, without
/// duplicates and capped at [`WEB_SEARCH_MAX_TERMS`]. Punctuation stays part of
/// its term, so `%`, `_`, and `\` remain literal characters.
pub fn web_search_terms(normalized_query: &str) -> Vec<&str> {
    let mut terms = Vec::new();
    for term in normalized_query.split_whitespace() {
        if terms.len() == WEB_SEARCH_MAX_TERMS {
            break;
        }
        if !terms.contains(&term) {
            terms.push(term);
        }
    }
    terms
}

/// A browser search matches when every term occurs in at least one of the
/// already-normalized fields; different terms may match different fields.
/// Any field containing the whole query therefore still matches.
pub fn web_search_fields_match(normalized_fields: &[&str], normalized_query: &str) -> bool {
    web_search_terms(normalized_query)
        .into_iter()
        .all(|term| normalized_fields.iter().any(|field| field.contains(term)))
}

pub(crate) fn web_media_fields_match(
    path: &Path,
    title: &str,
    artist: Option<&str>,
    album_artist: Option<&str>,
    album: Option<&str>,
    normalized_query: &str,
) -> bool {
    if normalized_query.is_empty() {
        return true;
    }
    // Normalize each field once per row, independent of the term count.
    let fields = [
        Some(web_media_file_name(path, title).as_str()),
        Some(title),
        artist,
        album_artist,
        album,
    ]
    .into_iter()
    .flatten()
    .map(web_search_normalize)
    .collect::<Vec<_>>();
    web_search_fields_match(
        &fields.iter().map(String::as_str).collect::<Vec<_>>(),
        normalized_query,
    )
}

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
            web_search_normalize(&group.title),
            group.id,
            group.sequence,
            web_search_normalize(title)
        ),
        None => web_search_normalize(title),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_case_normalization_preserves_accents_and_combining_forms() {
        assert_eq!(web_search_normalize("ÉTÉ ФИЛЬМ"), "été фильм");
        assert_eq!(web_search_normalize("E\u{301}"), "e\u{301}");
        assert_ne!(web_search_normalize("E\u{301}"), web_search_normalize("É"));
        assert_ne!(web_search_normalize("É"), "e");
        assert_eq!(web_search_normalize("Straße %_\\"), "straße %_\\");
    }

    #[test]
    fn search_terms_match_in_any_order_across_fields() {
        let path = Path::new("/movies/Blade Runner 2049 (2017).mkv");
        let matches = |title: &str, artist: Option<&str>, album: Option<&str>, query: &str| {
            web_media_fields_match(path, title, artist, None, album, query)
        };
        for query in [
            "",
            "blade 2049",
            "2049   blade",
            "runner: 2049",
            "(2017).mkv blade",
        ] {
            assert!(matches("Blade Runner: 2049", None, None, query), "{query}");
        }
        assert!(!matches("Blade Runner: 2049", None, None, "blade 1982"));
        assert!(matches(
            "Come Together",
            Some("the beatles"),
            Some("abbey road"),
            "beatles abbey"
        ));
        assert!(!matches(
            "Come Together",
            Some("the beatles"),
            Some("abbey road"),
            "beatles help"
        ));
        // Punctuation is literal: a symbol-only query never matches everything.
        assert!(!matches("Blade Runner: 2049", None, None, "%"));
        assert!(matches("100% Literal_Name", None, None, "% _name"));
        assert_eq!(web_search_terms("a  b a\tc"), ["a", "b", "c"]);
        let many = (0..40)
            .map(|index| format!("t{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(web_search_terms(&many).len(), WEB_SEARCH_MAX_TERMS);
        assert!(web_search_terms(" \t ").is_empty());
        assert!(web_search_fields_match(&["anything"], " "));
    }

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
