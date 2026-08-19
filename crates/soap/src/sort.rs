//! `SortCriteria` parse (`replica.md` Sort / GetSortCapabilities).

use rusty_dlna_protocol::ClientFlags;
use rusty_dlna_protocol::ClientProfile;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Title,
    Date,
    Class,
    Album,
    EpisodeNumber,
    Track,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortSpec {
    pub key: SortKey,
    pub descending: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortError {
    Unparseable,
}

/// Parse `+dc:title,-dc:date`. Empty → `Ok(vec![])` (caller applies client default).
pub fn parse_sort_criteria(raw: Option<&str>) -> Result<Vec<SortSpec>, SortError> {
    let s = raw.map(str::trim).unwrap_or("");
    if s.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for piece in s.split(',') {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let (descending, name) = if let Some(rest) = piece.strip_prefix('-') {
            (true, rest)
        } else if let Some(rest) = piece.strip_prefix('+') {
            (false, rest)
        } else {
            (false, piece)
        };
        let key = match name.trim() {
            "dc:title" => SortKey::Title,
            "dc:date" => SortKey::Date,
            "upnp:class" => SortKey::Class,
            "upnp:album" => SortKey::Album,
            "upnp:episodeNumber" => SortKey::EpisodeNumber,
            "upnp:originalTrackNumber" => SortKey::Track,
            _ => return Err(SortError::Unparseable),
        };
        out.push(SortSpec { key, descending });
    }
    if out.is_empty() {
        Ok(Vec::new())
    } else {
        Ok(out)
    }
}

/// Unparseable + FLAG_DLNA → SOAP 709.
pub fn sort_or_709(raw: Option<&str>, client: &ClientProfile) -> Result<Vec<SortSpec>, u16> {
    match parse_sort_criteria(raw) {
        Ok(v) => Ok(v),
        Err(SortError::Unparseable) if client.flags.contains(ClientFlags::DLNA) => Err(709),
        Err(SortError::Unparseable) => Ok(Vec::new()),
    }
}

/// Client default when SortCriteria is empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultOrder {
    /// FLAG_FORCE_SORT: CLASS, DISC, TRACK, TITLE
    ForceSort,
    /// LG: CLASS, TITLE
    Lg,
    /// folders-first + title
    FoldersFirst,
}

pub fn default_order(client: &ClientProfile) -> DefaultOrder {
    if client.flags.contains(ClientFlags::FORCE_SORT) {
        DefaultOrder::ForceSort
    } else if matches!(
        client.kind,
        rusty_dlna_protocol::ClientKind::Lg | rusty_dlna_protocol::ClientKind::LgNetCast
    ) {
        DefaultOrder::Lg
    } else {
        DefaultOrder::FoldersFirst
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_dlna_protocol::identify_user_agent;

    #[test]
    fn bad_sort_is_709_for_dlna() {
        let dlna = identify_user_agent("DLNADOC/1.50").expect("generic dlna");
        assert!(dlna.flags.contains(ClientFlags::DLNA));
        assert_eq!(sort_or_709(Some("+notAField"), dlna), Err(709));
        assert_eq!(sort_or_709(Some("-nope"), dlna), Err(709));
        let ok = sort_or_709(Some("+dc:title,-dc:date"), dlna).unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!(ok[0].key, SortKey::Title);
        assert!(!ok[0].descending);
        assert_eq!(ok[1].key, SortKey::Date);
        assert!(ok[1].descending);
        assert!(sort_or_709(Some(""), dlna).unwrap().is_empty());
    }

    #[test]
    fn force_sort_default_is_class_disc_track_title() {
        let p = identify_user_agent("Panasonic").expect("panasonic");
        assert_eq!(default_order(p), DefaultOrder::ForceSort);
        let lg = identify_user_agent("LGE_DLNA_SDK").expect("lg");
        assert_eq!(default_order(lg), DefaultOrder::Lg);
    }
}
