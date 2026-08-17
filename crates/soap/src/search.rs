//! `SearchCriteria` parse + match (`replica.md` Search / GetSearchCapabilities).

/// A single dialect clause. Unknown properties match nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchClause {
    Contains { prop: SearchProp, needle: String },
    Equals { prop: SearchProp, value: String },
    DerivedFrom { prop: SearchProp, prefix: String },
    /// Parsed but not a known property — never matches.
    Unknown,
    /// Empty / `*` / missing → every row in scope.
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchProp {
    Title,
    Creator,
    Date,
    Class,
    Artist,
    Genre,
    Album,
    Actor,
    Id,
    ParentId,
    RefId,
}

#[derive(Clone, Debug, Default)]
pub struct SearchRow<'a> {
    pub title: &'a str,
    pub creator: &'a str,
    pub date: &'a str,
    pub class: &'a str,
    pub artist: &'a str,
    pub genre: &'a str,
    pub album: &'a str,
    pub actor: &'a str,
    pub id: &'a str,
    pub parent_id: &'a str,
    pub ref_id: Option<&'a str>,
    pub is_container: bool,
}

/// Parse MiniDLNA-style SearchCriteria. AND of clauses (spaces / `and`);
/// `or` is accepted as a separator but still evaluated as a list of
/// clauses — callers use [`row_matches`] which requires every
/// non-`All` clause to match (AND). Empty/`*`/missing → [`SearchClause::All`].
pub fn parse_search_criteria(raw: Option<&str>) -> Vec<SearchClause> {
    let s = raw.map(str::trim).unwrap_or("");
    if s.is_empty() || s == "*" || s == "1=1" {
        return vec![SearchClause::All];
    }
    let mut out = Vec::new();
    for piece in split_clauses(s) {
        out.push(parse_clause(piece));
    }
    if out.is_empty() {
        vec![SearchClause::All]
    } else {
        out
    }
}

fn split_clauses(s: &str) -> Vec<&str> {
    let lower = s.to_ascii_lowercase();
    let mut parts = Vec::new();
    let mut start = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 5 <= bytes.len() && lower[i..].starts_with(" and ") {
            parts.push(s[start..i].trim());
            i += 5;
            start = i;
            continue;
        }
        if i + 4 <= bytes.len() && lower[i..].starts_with(" or ") {
            parts.push(s[start..i].trim());
            i += 4;
            start = i;
            continue;
        }
        i += 1;
    }
    parts.push(s[start..].trim());
    parts.into_iter().filter(|p| !p.is_empty()).collect()
}

fn parse_clause(raw: &str) -> SearchClause {
    let s = raw.trim().trim_matches(|c| c == '(' || c == ')').trim();
    if s.is_empty() {
        return SearchClause::All;
    }
    if let Some(clause) = parse_op(s, " contains ") {
        return clause;
    }
    if let Some(clause) = parse_op(s, " derivedfrom ") {
        return clause;
    }
    if let Some(clause) = parse_op(s, " = ") {
        return clause;
    }
    // `prop=value` without spaces
    if let Some((p, v)) = s.split_once('=') {
        if !p.contains(' ') {
            return finish_clause(p.trim(), "=", unquote(v.trim()));
        }
    }
    SearchClause::Unknown
}

fn parse_op(s: &str, op: &str) -> Option<SearchClause> {
    let lower = s.to_ascii_lowercase();
    let idx = lower.find(op)?;
    let prop = s[..idx].trim();
    let val = unquote(s[idx + op.len()..].trim());
    Some(finish_clause(prop, op.trim(), val))
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn finish_clause(prop: &str, op: &str, val: String) -> SearchClause {
    let Some(prop) = parse_prop(prop) else {
        return SearchClause::Unknown;
    };
    match op {
        "contains" => SearchClause::Contains { prop, needle: val },
        "derivedfrom" => SearchClause::DerivedFrom { prop, prefix: val },
        "=" => SearchClause::Equals { prop, value: val },
        _ => SearchClause::Unknown,
    }
}

fn parse_prop(raw: &str) -> Option<SearchProp> {
    Some(match raw.trim() {
        "dc:title" => SearchProp::Title,
        "dc:creator" => SearchProp::Creator,
        "dc:date" => SearchProp::Date,
        "upnp:class" => SearchProp::Class,
        "upnp:artist" => SearchProp::Artist,
        "upnp:genre" => SearchProp::Genre,
        "upnp:album" => SearchProp::Album,
        "upnp:actor" => SearchProp::Actor,
        "@id" => SearchProp::Id,
        "@parentID" => SearchProp::ParentId,
        "@refID" => SearchProp::RefId,
        _ => return None,
    })
}

fn field<'a>(row: &SearchRow<'a>, prop: SearchProp) -> &'a str {
    match prop {
        SearchProp::Title => row.title,
        SearchProp::Creator => row.creator,
        SearchProp::Date => row.date,
        SearchProp::Class => row.class,
        SearchProp::Artist => row.artist,
        SearchProp::Genre => row.genre,
        SearchProp::Album => row.album,
        SearchProp::Actor => row.actor,
        SearchProp::Id => row.id,
        SearchProp::ParentId => row.parent_id,
        SearchProp::RefId => row.ref_id.unwrap_or(""),
    }
}

pub fn clause_matches(clause: &SearchClause, row: &SearchRow<'_>) -> bool {
    match clause {
        SearchClause::All => true,
        SearchClause::Unknown => false,
        SearchClause::Contains { prop, needle } => {
            let hay = if *prop == SearchProp::Class {
                class_full(field(row, *prop))
            } else {
                field(row, *prop).to_string()
            };
            hay.to_ascii_lowercase()
                .contains(&needle.to_ascii_lowercase())
        }
        SearchClause::Equals { prop, value } => {
            field(row, *prop).eq_ignore_ascii_case(value)
        }
        SearchClause::DerivedFrom { prop, prefix } => {
            let hay = field(row, *prop);
            class_derivedfrom(hay, prefix)
        }
    }
}

/// `object.item.videoItem` derivedfrom must not treat `object.container` as a hit.
/// Class values in the catalog are stored without the `object.` prefix
/// (`item.videoItem`); accept both forms.
fn class_full(hay: &str) -> String {
    if hay.starts_with("object.") {
        hay.to_string()
    } else {
        format!("object.{hay}")
    }
}

fn class_derivedfrom(hay: &str, prefix: &str) -> bool {
    let hay_full = class_full(hay);
    let pre = class_full(prefix);
    hay_full == pre || hay_full.starts_with(&format!("{pre}."))
}

pub fn row_matches(clauses: &[SearchClause], row: &SearchRow<'_>) -> bool {
    clauses.iter().all(|c| clause_matches(c, row))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(title: &str) -> SearchRow<'_> {
        SearchRow {
            title,
            class: "item.videoItem",
            is_container: false,
            ..SearchRow::default()
        }
    }

    fn folder(title: &str) -> SearchRow<'_> {
        SearchRow {
            title,
            class: "container.storageFolder",
            is_container: true,
            ..SearchRow::default()
        }
    }

    #[test]
    fn search_title_contains() {
        let clauses = parse_search_criteria(Some(r#"dc:title contains "Fixture""#));
        assert!(row_matches(&clauses, &video("Fixture Movie")));
        assert!(!row_matches(&clauses, &video("Other")));
        let unknown = parse_search_criteria(Some(r#"upnp:rating contains "pg""#));
        assert!(
            !row_matches(&unknown, &video("Fixture Movie")),
            "unknown clause matches nothing"
        );
        let all = parse_search_criteria(Some("*"));
        assert!(row_matches(&all, &video("x")));
        let empty = parse_search_criteria(None);
        assert!(row_matches(&empty, &folder("Video")));
    }

    #[test]
    fn search_class_derivedfrom_video() {
        let clauses =
            parse_search_criteria(Some(r#"upnp:class derivedfrom "object.item.videoItem""#));
        assert!(row_matches(&clauses, &video("a")));
        assert!(
            !row_matches(&clauses, &folder("Video")),
            "folders must not match videoItem derivedfrom"
        );
        let folders =
            parse_search_criteria(Some(r#"upnp:class derivedfrom "object.container""#));
        assert!(row_matches(&folders, &folder("Video")));
        assert!(!row_matches(&folders, &video("a")));
        let contains_c =
            parse_search_criteria(Some(r#"upnp:class contains "object.container""#));
        assert!(row_matches(&contains_c, &folder("Video")));
    }
}
