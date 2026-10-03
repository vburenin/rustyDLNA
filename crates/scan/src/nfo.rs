//! Structured Kodi-style NFO parsing and `tvshow.nfo` metadata application.

use std::path::{Path, PathBuf};

use rusty_dlna_helper::{read_to_end_bounded, BoundedReadError};
use rusty_dlna_protocol::w3c_normalize_date;

/// Skip sidecars larger than rustyDLNA's 64 KiB cap.
const NFO_READ_LIMIT: usize = 64 * 1024;
pub const NFO_MAX_BYTES: u64 = NFO_READ_LIMIT as u64;

const NFO_WITHOUT_TITLE_PREFIX: &str = "nfo-v1:without-title:";
const NFO_WITH_TITLE_PREFIX: &str = "nfo-v1:with-title:";

#[derive(Debug, thiserror::Error)]
#[error("read NFO {path}: {source}")]
pub struct NfoError {
    pub path: PathBuf,
    #[source]
    pub source: std::io::Error,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NfoMeta {
    pub title: Option<String>,
    /// Short, spoiler-safe movie description from Kodi `<outline>`.
    pub about: Option<String>,
    /// Full movie or episode description from Kodi `<plot>`.
    pub plot: Option<String>,
    pub genre: Option<String>,
    pub creator: Option<String>,
    pub artist: Option<String>,
    pub disc: Option<i64>,
    pub track: Option<i64>,
    pub date: Option<String>,
    /// `showtitle` (or inherited `tvshow` title). Stored as `DETAILS.ALBUM`.
    pub showtitle: Option<String>,
    /// Raw episode/movie `<title>` before the `Show - ` prefix.
    pub episode_title: Option<String>,
}

impl NfoMeta {
    /// Fingerprint the parsed effective override, including absent fields.
    /// Versioning makes a future change to this representation self-invalidating.
    pub(crate) fn fingerprint(&self) -> String {
        format!(
            "{}{}",
            if self.title.is_some() {
                NFO_WITH_TITLE_PREFIX
            } else {
                NFO_WITHOUT_TITLE_PREFIX
            },
            crate::sha256_hex(format!("{self:?}").as_bytes())
        )
    }

    pub(crate) fn fingerprint_has_no_title(fingerprint: &str) -> bool {
        fingerprint.starts_with(NFO_WITHOUT_TITLE_PREFIX)
    }

    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.about.is_none()
            && self.plot.is_none()
            && self.genre.is_none()
            && self.creator.is_none()
            && self.artist.is_none()
            && self.disc.is_none()
            && self.track.is_none()
            && self.date.is_none()
            && self.showtitle.is_none()
            && self.episode_title.is_none()
    }
}

/// Split slash-joined genres (`Drama / Crime`).
pub fn split_genres(genre: &str) -> Vec<String> {
    genre
        .split(" / ")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// Episode label under Series: strip `{show} - ` when present.
pub fn episode_display_title(title: &str, showtitle: Option<&str>) -> String {
    if let Some(show) = showtitle {
        let prefix = format!("{show} - ");
        if let Some(rest) = title.strip_prefix(&prefix) {
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    title.to_string()
}

pub fn nfo_too_large(len: u64) -> bool {
    len > NFO_MAX_BYTES
}

pub fn nfo_date_from_text(text: &str) -> Option<String> {
    parse_nfo_parts_bytes(text.as_bytes()).ok()?.date
}

#[derive(Clone, Debug, Default)]
struct NfoParts {
    title: Option<String>,
    showtitle: Option<String>,
    outline: Option<String>,
    plot: Option<String>,
    genres: Vec<String>,
    director: Option<String>,
    credits: Option<String>,
    studio: Option<String>,
    season: Option<i64>,
    episode: Option<i64>,
    date: Option<String>,
}

impl NfoParts {
    fn inherit_tvshow(&mut self, show: &NfoParts) {
        if self.showtitle.is_none() {
            self.showtitle = show.showtitle.clone().or_else(|| show.title.clone());
        }
        if self.plot.is_none() {
            self.plot = show.plot.clone();
        }
        if self.outline.is_none() {
            self.outline = show.outline.clone();
        }
        if self.genres.is_empty() {
            self.genres = show.genres.clone();
        }
        if self.studio.is_none() {
            self.studio = show.studio.clone();
        }
    }

    fn into_meta(self) -> NfoMeta {
        let title = match (self.showtitle.as_ref(), self.title.as_ref()) {
            (Some(show), Some(ep)) => Some(format!("{show} - {ep}")),
            (Some(show), None) => Some(show.clone()),
            (None, Some(ep)) => Some(ep.clone()),
            (None, None) => None,
        };
        let genre = if self.genres.is_empty() {
            None
        } else {
            Some(self.genres.join(" / "))
        };
        let creator = self
            .director
            .or(self.credits)
            .or_else(|| self.studio.clone());
        let artist = self.studio.clone().or(self.showtitle.clone());
        NfoMeta {
            episode_title: self.title.clone(),
            showtitle: self.showtitle,
            title,
            about: self.outline,
            plot: self.plot,
            genre,
            creator,
            artist,
            disc: self.season,
            track: self.episode,
            date: self.date,
        }
    }
}

pub fn parse_nfo_text(text: &str) -> NfoMeta {
    parse_nfo_parts_bytes(text.as_bytes())
        .map(NfoParts::into_meta)
        .unwrap_or_default()
}

/// Kodi accepts a plain-text NFO that only names a scraper URL, and scene
/// releases ship ASCII-art `.nfo` files beside media. Neither is an XML
/// document, so neither carries metadata overrides. A document whose first
/// significant byte is `<` is parsed; a trailing URL after the XML root is
/// ignored by the parser.
fn is_xml_document(bytes: &[u8]) -> bool {
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        return true;
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    bytes
        .iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| *byte == b'<')
}

/// HTML line breaks are common in scraped `<plot>` text and are never closed.
fn is_html_line_break(name: &str) -> bool {
    name == "br"
}

fn parse_nfo_parts_bytes(bytes: &[u8]) -> Result<NfoParts, String> {
    use quick_xml::events::Event;

    if !is_xml_document(bytes) {
        return Ok(NfoParts::default());
    }

    // quick-xml's event scanner requires ASCII-compatible markup even when
    // its decoder feature is enabled. Normalize UTF-16 documents first so
    // element boundaries cannot be mistaken for NUL-delimited text.
    let normalized;
    let bytes = if bytes.starts_with(&[0xff, 0xfe]) {
        normalized = decode_utf16_xml(&bytes[2..], true)?;
        normalized.as_bytes()
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        normalized = decode_utf16_xml(&bytes[2..], false)?;
        normalized.as_bytes()
    } else {
        bytes
    };
    let mut reader = quick_xml::Reader::from_reader(bytes);
    // Entity references are separate events in quick-xml. Preserve the
    // spaces around them, then trim the completed element value.
    reader.config_mut().trim_text(false);
    reader.config_mut().expand_empty_elements = true;
    // A bare `&` in scraped text (`Tom & Jerry`) is kept literally rather
    // than rejecting an otherwise well-formed document.
    reader.config_mut().allow_dangling_amp = true;
    // `<br>` is matched below, so end-name checks happen here instead.
    reader.config_mut().check_end_names = false;
    let mut stack: Vec<(String, String)> = Vec::new();
    let mut parts = NfoParts::default();
    let mut premiered = None;
    let mut aired = None;
    let mut year = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) => {
                let name = start.local_name().as_ref().to_ascii_lowercase();
                if is_html_line_break(&name) {
                    if let Some((_, value)) = stack.last_mut() {
                        value.push('\n');
                    }
                    continue;
                }
                stack.push((name, String::new()));
            }
            Ok(Event::Text(text)) => {
                if let Some((_, value)) = stack.last_mut() {
                    // References arrive as separate events, so text only
                    // contains `&` when it is a tolerated dangling ampersand.
                    value.push_str(&text.into_inner());
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                if let Some((_, value)) = stack.last_mut() {
                    if let Some(character) = reference
                        .resolve_char_ref()
                        .map_err(|error| error.to_string())?
                    {
                        value.push(character);
                    } else {
                        let name = reference.into_inner();
                        match quick_xml::escape::resolve_xml_entity(&name) {
                            Some(entity) => value.push_str(entity),
                            // HTML entities are common in scraped text; keep
                            // a readable space or the literal reference.
                            None if name.as_ref() == "nbsp" => value.push(' '),
                            None => {
                                value.push('&');
                                value.push_str(&name);
                                value.push(';');
                            }
                        }
                    }
                }
            }
            Ok(Event::CData(text)) => {
                if let Some((_, value)) = stack.last_mut() {
                    value.push_str(&text.into_inner());
                }
            }
            Ok(Event::End(end)) => {
                let end_name = end.local_name().as_ref().to_ascii_lowercase();
                if is_html_line_break(&end_name) {
                    continue;
                }
                let Some((name, value)) = stack.pop() else {
                    return Err("unexpected closing element".into());
                };
                if name != end_name {
                    return Err(format!("closing element {end_name} does not match {name}"));
                }
                let value = value.trim();
                if value.is_empty() {
                    continue;
                }
                let value = value.to_string();
                match name.as_str() {
                    "title" if parts.title.is_none() => parts.title = Some(value),
                    "episodetitle" if parts.title.is_none() => parts.title = Some(value),
                    "showtitle" if parts.showtitle.is_none() => parts.showtitle = Some(value),
                    "outline" if parts.outline.is_none() => parts.outline = Some(value),
                    "plot" if parts.plot.is_none() => parts.plot = Some(value),
                    "genre" => parts.genres.push(value),
                    "director" if parts.director.is_none() => parts.director = Some(value),
                    "credits" if parts.credits.is_none() => parts.credits = Some(value),
                    "studio" if parts.studio.is_none() => parts.studio = Some(value),
                    "season" if parts.season.is_none() => parts.season = value.parse().ok(),
                    "episode" if parts.episode.is_none() => parts.episode = value.parse().ok(),
                    "premiered" if premiered.is_none() => premiered = Some(value),
                    "aired" if aired.is_none() => aired = Some(value),
                    "year" if year.is_none() => year = Some(value),
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    if !stack.is_empty() {
        return Err("unclosed element".into());
    }
    parts.date = premiered
        .or(aired)
        .or(year)
        .map(|value| w3c_normalize_date(&value));
    Ok(parts)
}

fn decode_utf16_xml(bytes: &[u8], little_endian: bool) -> Result<String, String> {
    if bytes.len() & 1 != 0 {
        return Err("UTF-16 XML has an odd byte count".into());
    }
    let units = bytes.as_chunks::<2>().0.iter().map(|pair| {
        if little_endian {
            u16::from_le_bytes([pair[0], pair[1]])
        } else {
            u16::from_be_bytes([pair[0], pair[1]])
        }
    });
    let decoded = std::char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .map_err(|error| format!("invalid UTF-16 XML: {error}"))?;
    // The bytes below are UTF-8 now; retaining `encoding="UTF-16"` would
    // make the XML decoder reinterpret text payload bytes a second time.
    let trimmed = decoded.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with("<?xml") {
        let end = trimmed
            .find("?>")
            .ok_or_else(|| "unterminated XML declaration".to_string())?;
        Ok(trimmed[end + 2..].to_string())
    } else {
        Ok(decoded)
    }
}

/// `{stem}.nfo` then parent `tvshow.nfo` files up to (and including) a media
/// root. Folder-level `movie.nfo` needs the scanner's admission context and is
/// only applied by [`nfo_lookup_with_policy`].
pub fn nfo_for_file(file: &Path, media_roots: &[PathBuf]) -> NfoMeta {
    nfo_for_file_with_policy(file, media_roots, false)
}

/// Root-aware NFO lookup used by the scanner. `wide_links` has exactly the
/// same meaning as it does for media, captions, and artwork.
pub fn nfo_for_file_with_policy(file: &Path, media_roots: &[PathBuf], wide_links: bool) -> NfoMeta {
    nfo_for_file_with_policy_result(file, media_roots, wide_links).unwrap_or_default()
}

/// Strict lookup. Missing, oversized, or jailed NFOs are intentionally
/// ignored; an NFO that is selected but cannot be read or parsed is an error.
pub fn nfo_for_file_with_policy_result(
    file: &Path,
    media_roots: &[PathBuf],
    wide_links: bool,
) -> Result<NfoMeta, NfoError> {
    let lookup = nfo_lookup_with_policy(file, media_roots, wide_links, || false)?;
    match lookup.invalid {
        Some(invalid) => Err(invalid_nfo(&invalid.path, invalid.message)),
        None => Ok(lookup.meta),
    }
}

/// A selected sidecar that was read successfully but is not usable XML.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidNfo {
    pub path: PathBuf,
    pub message: String,
}

/// Effective NFO metadata for one media path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NfoLookup {
    /// Metadata from every selected sidecar that parsed.
    pub meta: NfoMeta,
    /// The first selected sidecar that could not be parsed. Scanner callers
    /// keep an existing item's stored presentation and provenance in that
    /// case, because the file may be mid-write; a new item indexes with the
    /// metadata that did parse.
    pub invalid: Option<InvalidNfo>,
}

/// Kodi's folder-level sidecar for the "movies in separate folders" layout.
pub const FOLDER_MOVIE_NFO: &str = "movie.nfo";

/// Scanner entry point. Real I/O failures stay errors so the surrounding
/// staged mutation rolls back; malformed XML is reported in the result and
/// logged once per path and message, so it can never abort a whole scan.
///
/// Precedence: `{stem}.nfo`, otherwise the same directory's `movie.nfo` when
/// `folder_movie_nfo_applies` confirms that the folder holds this one movie,
/// then inherited `tvshow.nfo` for fields the local sidecar leaves empty.
pub fn nfo_lookup_with_policy(
    file: &Path,
    media_roots: &[PathBuf],
    wide_links: bool,
    folder_movie_nfo_applies: impl FnOnce() -> bool,
) -> Result<NfoLookup, NfoError> {
    let mut invalid = None;
    let mut parse = |path: &Path, bytes: &[u8]| match parse_nfo_parts_bytes(bytes) {
        Ok(parts) => Some(parts),
        Err(message) => {
            warn_invalid_nfo(path, &message);
            invalid.get_or_insert_with(|| InvalidNfo {
                path: path.to_path_buf(),
                message,
            });
            None
        }
    };
    let mut parts = NfoParts::default();
    let file_nfo = file.with_extension("nfo");
    let local = match read_nfo_bytes(&file_nfo, media_roots, wide_links)? {
        Some(bytes) => Some((file_nfo, bytes)),
        None => match file.parent().map(|dir| dir.join(FOLDER_MOVIE_NFO)) {
            // A folder `movie.nfo` that would not apply to this file (several
            // videos share the folder) cannot fail its indexing either.
            Some(folder_nfo) => match read_nfo_bytes(&folder_nfo, media_roots, wide_links) {
                Ok(Some(bytes)) => folder_movie_nfo_applies().then_some((folder_nfo, bytes)),
                Ok(None) => None,
                Err(error) => {
                    if folder_movie_nfo_applies() {
                        return Err(error);
                    }
                    None
                }
            },
            None => None,
        },
    };
    if let Some((path, bytes)) = local {
        if let Some(parsed) = parse(&path, &bytes) {
            parts = parsed;
        }
    }
    // Scanner paths normally retain their lexical root even when a component
    // is a directory symlink. Walk those lexical parents cheaply; any NFO that
    // actually exists is still canonicalized and jailed by `read_nfo_bytes`.
    let lexical_root = media_roots
        .iter()
        .filter(|root| file.starts_with(root))
        .max_by_key(|root| root.components().count());
    let mut dir = file.parent().map(Path::to_path_buf);
    while let Some(cur) = dir {
        let inside = lexical_root.is_some_and(|root| cur.starts_with(root))
            || (lexical_root.is_none() && dir_is_inside_roots(&cur, media_roots, wide_links));
        if !inside {
            break;
        }
        let tvshow = cur.join("tvshow.nfo");
        if let Some(bytes) = read_nfo_bytes(&tvshow, media_roots, wide_links)? {
            if let Some(show) = parse(&tvshow, &bytes) {
                parts.inherit_tvshow(&show);
            }
        }
        if lexical_root.is_some_and(|root| cur.as_path() == root.as_path())
            || (lexical_root.is_none() && is_media_root_dir(&cur, media_roots))
        {
            break;
        }
        dir = cur.parent().map(Path::to_path_buf);
    }
    Ok(NfoLookup {
        meta: parts.into_meta(),
        invalid,
    })
}

/// Periodic reconciliation revisits a permanently malformed sidecar on every
/// pass. Warn once per path and parser message; bound the remembered set so
/// a pathological library cannot grow it without limit.
fn warn_invalid_nfo(path: &Path, message: &str) {
    const REMEMBERED_LIMIT: usize = 4096;
    type Warned = std::collections::HashSet<(PathBuf, String)>;
    static WARNED: std::sync::Mutex<Option<Warned>> = std::sync::Mutex::new(None);
    let key = (path.to_path_buf(), message.to_string());
    let first = {
        let mut warned = WARNED.lock().unwrap_or_else(|error| error.into_inner());
        let warned = warned.get_or_insert_with(Default::default);
        if warned.len() >= REMEMBERED_LIMIT && !warned.contains(&key) {
            warned.clear();
        }
        warned.insert(key)
    };
    if first {
        tracing::warn!(
            target: "rusty_dlna",
            path = %path.display(),
            error = message,
            "ignoring NFO that is not well-formed XML"
        );
    }
}

fn invalid_nfo(path: &Path, message: String) -> NfoError {
    NfoError {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, message),
    }
}

fn read_nfo_bytes(
    path: &Path,
    roots: &[PathBuf],
    wide_links: bool,
) -> Result<Option<Vec<u8>>, NfoError> {
    let mut opened = match crate::open_file_under_roots(path, roots, wide_links) {
        Ok(opened) => opened,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::InvalidInput
            ) =>
        {
            return Ok(None);
        }
        Err(source) => {
            return Err(NfoError {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let metadata = opened.file.metadata().map_err(|source| NfoError {
        path: path.to_path_buf(),
        source,
    })?;
    if nfo_too_large(metadata.len()) {
        return Ok(None);
    }
    let bytes = match read_to_end_bounded(&mut opened.file, NFO_READ_LIMIT) {
        Ok(bytes) => bytes,
        Err(BoundedReadError::LimitExceeded { .. }) => return Ok(None),
        Err(BoundedReadError::Io(source)) => {
            return Err(NfoError {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    Ok(Some(bytes))
}

fn is_media_root_dir(dir: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| same_dir(dir, root))
}

fn dir_is_inside_roots(dir: &Path, roots: &[PathBuf], wide_links: bool) -> bool {
    if roots.is_empty() {
        return true;
    }
    roots.iter().any(|root| {
        if dir.starts_with(root) && wide_links {
            return true;
        }
        match (dir.canonicalize(), root.canonicalize()) {
            (Ok(d), Ok(r)) => d.starts_with(&r),
            _ => false,
        }
    })
}

fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(aa), Ok(bb)) => aa == bb,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfo_unescapes_xml_entities() {
        let m = parse_nfo_text("<movie><title>Foo &amp; Bar</title></movie>");
        assert_eq!(m.title.as_deref(), Some("Foo & Bar"));
    }

    #[test]
    fn nfo_stream_parser_handles_namespaces_cdata_attributes_and_repeated_tags() {
        let bytes = br#"<?xml version="1.0"?>
          <k:episodedetails xmlns:k="urn:kodi" source="fixture">
            <k:showtitle><![CDATA[Rock & Roll]]></k:showtitle>
            <k:title lang="en">Pilot &#x2603;</k:title>
            <k:outline>A safe outline</k:outline>
            <k:plot>One &lt; two</k:plot>
            <k:genre>Drama</k:genre><k:genre>Crime</k:genre>
            <k:season>2</k:season><k:episode>7</k:episode>
            <k:premiered>2024-03-04</k:premiered>
          </k:episodedetails>"#;
        let parsed = parse_nfo_parts_bytes(bytes).unwrap().into_meta();
        assert_eq!(parsed.title.as_deref(), Some("Rock & Roll - Pilot ☃"));
        assert_eq!(parsed.about.as_deref(), Some("A safe outline"));
        assert_eq!(parsed.plot.as_deref(), Some("One < two"));
        assert_eq!(parsed.genre.as_deref(), Some("Drama / Crime"));
        assert_eq!(parsed.disc, Some(2));
        assert_eq!(parsed.track, Some(7));
        assert_eq!(parsed.date.as_deref(), Some("2024-03-04"));
    }

    #[test]
    fn nfo_stream_parser_decodes_utf16_and_rejects_malformed_xml() {
        let xml =
            "<?xml version=\"1.0\" encoding=\"UTF-16\"?><movie><title>Crème ☃</title></movie>";
        let mut utf16le = vec![0xff, 0xfe];
        for unit in xml.encode_utf16() {
            utf16le.extend_from_slice(&unit.to_le_bytes());
        }
        let parsed = parse_nfo_parts_bytes(&utf16le).unwrap().into_meta();
        assert_eq!(parsed.title.as_deref(), Some("Crème ☃"));
        assert!(parse_nfo_parts_bytes(b"<movie><title>broken</movie>").is_err());
    }

    #[test]
    fn nfo_parser_tolerates_scene_text_html_breaks_and_entities() {
        // Scene ASCII art (CP437 bytes, `<`, `&`) and URL-only scraper NFOs
        // are not XML documents and carry no overrides.
        let art = b"  \xdb\xdb\xb2 GRP <presents> Movie & more \xb0\xb1\r\n";
        assert!(parse_nfo_parts_bytes(art).unwrap().into_meta().is_empty());
        let url = b"https://www.themoviedb.org/movie/603?a=1&b=2\n";
        assert!(parse_nfo_parts_bytes(url).unwrap().into_meta().is_empty());
        let bom_xml = b"\xef\xbb\xbf\n<movie><title>Bom</title></movie>";
        assert_eq!(
            parse_nfo_parts_bytes(bom_xml).unwrap().into_meta().title,
            Some("Bom".into())
        );
        // Kodi's XML-plus-trailing-URL form keeps the XML overrides.
        let trailing = b"<movie><title>Kodi</title></movie>\nhttps://x.test/?a=1&b=2";
        assert_eq!(
            parse_nfo_parts_bytes(trailing).unwrap().into_meta().title,
            Some("Kodi".into())
        );

        let parsed = parse_nfo_text(
            "<movie><title>Tom & Jerry</title>\
             <plot>First line<br>second&nbsp;line<br/>third &eacute; &amp; done</plot></movie>",
        );
        assert_eq!(parsed.title.as_deref(), Some("Tom & Jerry"));
        assert_eq!(
            parsed.plot.as_deref(),
            Some("First line\nsecond line\nthird &eacute; & done")
        );

        // Structurally broken XML is still reported, so the scanner can keep
        // an item's previous presentation while a sidecar is mid-write.
        assert!(parse_nfo_parts_bytes(b"<movie><title>half").is_err());
        assert!(parse_nfo_parts_bytes(b"<movie><title>x</plot></movie>").is_err());
    }
}
