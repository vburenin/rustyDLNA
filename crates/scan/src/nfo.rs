//! Structured NFO parse (MiniDLNA / Kodi tags) and `tvshow.nfo` inherit.

use std::path::{Path, PathBuf};

use rusty_dlna_protocol::w3c_normalize_date;

/// Skip sidecars larger than MiniDLNA's 64 KiB cap.
pub const NFO_MAX_BYTES: u64 = 64 * 1024;

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
    for tag in ["premiered", "aired", "year"] {
        if let Some(start) = text.find(&format!("<{tag}>")) {
            let rest = &text[start + tag.len() + 2..];
            if let Some(end) = rest.find(&format!("</{tag}>")) {
                let raw = rest[..end].trim();
                if !raw.is_empty() {
                    return Some(w3c_normalize_date(raw));
                }
            }
        }
    }
    None
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
    parse_nfo_parts(text).into_meta()
}

fn parse_nfo_parts(text: &str) -> NfoParts {
    let episode_title = first_tag(text, "title").or_else(|| first_tag(text, "episodetitle"));
    NfoParts {
        title: episode_title,
        showtitle: first_tag(text, "showtitle"),
        plot: first_tag(text, "plot"),
        genres: all_tags(text, "genre"),
        director: first_tag(text, "director"),
        credits: first_tag(text, "credits"),
        studio: first_tag(text, "studio"),
        season: first_i64(text, "season"),
        episode: first_i64(text, "episode"),
        date: nfo_date_from_text(text),
    }
}

/// `{stem}.nfo` then parent `tvshow.nfo` files up to (and including) a media root.
pub fn nfo_for_file(file: &Path, media_roots: &[PathBuf]) -> NfoMeta {
    let mut parts = NfoParts::default();
    if let Some(text) = read_nfo_text(&file.with_extension("nfo")) {
        parts = parse_nfo_parts(&text);
    }
    let mut dir = file.parent().map(Path::to_path_buf);
    while let Some(cur) = dir {
        if !dir_is_inside_roots(&cur, media_roots) {
            break;
        }
        if let Some(text) = read_nfo_text(&cur.join("tvshow.nfo")) {
            parts.inherit_tvshow(&parse_nfo_parts(&text));
        }
        if is_media_root_dir(&cur, media_roots) {
            break;
        }
        dir = cur.parent().map(Path::to_path_buf);
    }
    parts.into_meta()
}

fn read_nfo_text(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || nfo_too_large(meta.len()) {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn first_tag(text: &str, tag: &str) -> Option<String> {
    all_tags(text, tag).into_iter().next()
}

fn first_i64(text: &str, tag: &str) -> Option<i64> {
    first_tag(text, tag)?.parse().ok()
}

fn all_tags(text: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else {
            break;
        };
        let raw = after[..end].trim();
        if !raw.is_empty() {
            out.push(raw.to_string());
        }
        rest = &after[end + close.len()..];
    }
    out
}

fn is_media_root_dir(dir: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| same_dir(dir, root))
}

fn dir_is_inside_roots(dir: &Path, roots: &[PathBuf]) -> bool {
    if roots.is_empty() {
        return true;
    }
    roots.iter().any(|root| {
        if dir.starts_with(root) {
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
