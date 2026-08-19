//! Structured NFO parse (MiniDLNA / Kodi tags) and `tvshow.nfo` inherit.

use std::path::{Path, PathBuf};

use rusty_dlna_protocol::w3c_normalize_date;

/// Skip sidecars larger than MiniDLNA's 64 KiB cap.
pub const NFO_MAX_BYTES: u64 = 64 * 1024;

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
    pub comment: Option<String>,
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
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.comment.is_none()
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

/// Split MiniDLNA-style joined genres (`Drama / Crime`).
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
            comment: self.plot,
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

fn parse_nfo_parts_bytes(bytes: &[u8]) -> Result<NfoParts, String> {
    use quick_xml::events::Event;

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
    // Entity references are separate events in quick-xml 0.41. Preserve the
    // spaces around them, then trim the completed element value.
    reader.config_mut().trim_text(false);
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<(String, String)> = Vec::new();
    let mut parts = NfoParts::default();
    let mut premiered = None;
    let mut aired = None;
    let mut year = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) => {
                let name =
                    String::from_utf8_lossy(start.local_name().as_ref()).to_ascii_lowercase();
                stack.push((name, String::new()));
            }
            Ok(Event::Text(text)) => {
                if let Some((_, value)) = stack.last_mut() {
                    let decoded = text.decode().map_err(|error| error.to_string())?;
                    let unescaped =
                        quick_xml::escape::unescape(&decoded).map_err(|error| error.to_string())?;
                    value.push_str(&unescaped);
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
                        let name = reference.decode().map_err(|error| error.to_string())?;
                        let entity = quick_xml::escape::resolve_xml_entity(&name)
                            .ok_or_else(|| format!("unrecognized XML entity '&{name};'"))?;
                        value.push_str(entity);
                    }
                }
            }
            Ok(Event::CData(text)) => {
                if let Some((_, value)) = stack.last_mut() {
                    value.push_str(&text.decode().map_err(|error| error.to_string())?);
                }
            }
            Ok(Event::End(end)) => {
                let end_name =
                    String::from_utf8_lossy(end.local_name().as_ref()).to_ascii_lowercase();
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
    let units = bytes.chunks_exact(2).map(|pair| {
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

/// `{stem}.nfo` then parent `tvshow.nfo` files up to (and including) a media root.
pub fn nfo_for_file(file: &Path, media_roots: &[PathBuf]) -> NfoMeta {
    nfo_for_file_with_policy(file, media_roots, false)
}

/// Root-aware NFO lookup used by the scanner. `wide_links` has exactly the
/// same meaning as it does for media, captions, and artwork.
pub fn nfo_for_file_with_policy(file: &Path, media_roots: &[PathBuf], wide_links: bool) -> NfoMeta {
    nfo_for_file_with_policy_result(file, media_roots, wide_links).unwrap_or_default()
}

/// Fallible scanner entry point. Missing, oversized, or jailed NFOs are
/// intentionally ignored; an NFO that is selected but cannot be read is an
/// observable scan error so its database mutation can be rolled back.
pub fn nfo_for_file_with_policy_result(
    file: &Path,
    media_roots: &[PathBuf],
    wide_links: bool,
) -> Result<NfoMeta, NfoError> {
    let mut parts = NfoParts::default();
    let file_nfo = file.with_extension("nfo");
    if let Some(bytes) = read_nfo_bytes(&file_nfo, media_roots, wide_links)? {
        parts = parse_nfo_parts_bytes(&bytes).map_err(|message| invalid_nfo(&file_nfo, message))?;
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
            let show =
                parse_nfo_parts_bytes(&bytes).map_err(|message| invalid_nfo(&tvshow, message))?;
            parts.inherit_tvshow(&show);
        }
        if lexical_root.is_some_and(|root| cur.as_path() == root.as_path())
            || (lexical_root.is_none() && is_media_root_dir(&cur, media_roots))
        {
            break;
        }
        dir = cur.parent().map(Path::to_path_buf);
    }
    Ok(parts.into_meta())
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
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(NfoError {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    if !crate::path_is_allowed_kind(path, roots, wide_links, |meta| meta.is_file()) {
        return Ok(None);
    }
    if !metadata.is_file() || nfo_too_large(metadata.len()) {
        return Ok(None);
    }
    std::fs::read(path).map(Some).map_err(|source| NfoError {
        path: path.to_path_buf(),
        source,
    })
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
            <k:plot>One &lt; two</k:plot>
            <k:genre>Drama</k:genre><k:genre>Crime</k:genre>
            <k:season>2</k:season><k:episode>7</k:episode>
            <k:premiered>2024-03-04</k:premiered>
          </k:episodedetails>"#;
        let parsed = parse_nfo_parts_bytes(bytes).unwrap().into_meta();
        assert_eq!(parsed.title.as_deref(), Some("Rock & Roll - Pilot ☃"));
        assert_eq!(parsed.comment.as_deref(), Some("One < two"));
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
}
