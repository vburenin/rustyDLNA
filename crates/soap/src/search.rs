//! `SearchCriteria` parsing and matching.

/// A single rustyDLNA search clause. Unknown properties match nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchClause {
    Contains {
        prop: SearchProp,
        needle: String,
    },
    DoesNotContain {
        prop: SearchProp,
        needle: String,
    },
    Equals {
        prop: SearchProp,
        value: String,
    },
    NotEquals {
        prop: SearchProp,
        value: String,
    },
    LessThan {
        prop: SearchProp,
        value: String,
        inclusive: bool,
    },
    GreaterThan {
        prop: SearchProp,
        value: String,
        inclusive: bool,
    },
    DerivedFrom {
        prop: SearchProp,
        prefix: String,
    },
    /// `exists true/false` maps to IS NOT NULL / IS NULL.
    Exists {
        prop: SearchProp,
        want: bool,
    },
    /// Parsed but not a known property — never matches.
    Unknown,
    /// Empty / `*` / missing → every row in scope.
    All,
}

/// OR of AND-groups. SQL-style: `and` binds tighter than `or`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchQuery {
    pub groups: Vec<Vec<SearchClause>>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid SearchCriteria: {0}")]
pub struct SearchParseError(String);

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

/// Parse rustyDLNA `SearchCriteria`. `and` / `or` use SQL precedence
/// (`and` tighter). Empty/`*`/`1=1` → one `All` group.
pub fn parse_search_criteria(raw: Option<&str>) -> SearchQuery {
    try_parse_search_criteria(raw).unwrap_or(SearchQuery {
        groups: vec![vec![SearchClause::Unknown]],
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Value(String),
    Operator(String),
    Left,
    Right,
}

fn tokenize(input: &str) -> Result<Vec<Token>, SearchParseError> {
    let chars: Vec<char> = input.chars().collect();
    let mut output = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index].is_whitespace() {
            index += 1;
            continue;
        }
        match chars[index] {
            '(' => {
                output.push(Token::Left);
                index += 1;
            }
            ')' => {
                output.push(Token::Right);
                index += 1;
            }
            '=' | '<' | '>' | '!' => {
                let mut operator = chars[index].to_string();
                index += 1;
                if index < chars.len() && chars[index] == '=' {
                    operator.push('=');
                    index += 1;
                }
                output.push(Token::Operator(operator));
            }
            quote @ ('"' | '\'') => {
                index += 1;
                let mut value = String::new();
                let mut closed = false;
                while index < chars.len() {
                    let current = chars[index];
                    index += 1;
                    if current == quote {
                        closed = true;
                        break;
                    }
                    if current == '\\' && index < chars.len() {
                        value.push(chars[index]);
                        index += 1;
                    } else {
                        value.push(current);
                    }
                }
                if !closed {
                    return Err(SearchParseError("unterminated quoted value".into()));
                }
                output.push(Token::Value(value));
            }
            _ => {
                let start = index;
                while index < chars.len()
                    && !chars[index].is_whitespace()
                    && !matches!(chars[index], '(' | ')' | '=' | '<' | '>' | '!')
                {
                    index += 1;
                }
                if start == index {
                    return Err(SearchParseError(format!(
                        "unexpected character {}",
                        chars[index]
                    )));
                }
                output.push(Token::Word(chars[start..index].iter().collect()));
            }
        }
        if output.len() > 512 {
            return Err(SearchParseError("criteria has too many tokens".into()));
        }
    }
    Ok(output)
}

#[derive(Clone, Debug)]
enum Expression {
    Clause(SearchClause),
    And(Box<Expression>, Box<Expression>),
    Or(Box<Expression>, Box<Expression>),
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn take(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position).cloned();
        self.position += usize::from(token.is_some());
        token
    }

    fn word_is(&self, wanted: &str) -> bool {
        matches!(self.peek(), Some(Token::Word(word)) if word.eq_ignore_ascii_case(wanted))
    }

    fn parse_or(&mut self) -> Result<Expression, SearchParseError> {
        let mut expression = self.parse_and()?;
        while self.word_is("or") {
            self.take();
            expression = Expression::Or(Box::new(expression), Box::new(self.parse_and()?));
        }
        Ok(expression)
    }

    fn parse_and(&mut self) -> Result<Expression, SearchParseError> {
        let mut expression = self.parse_primary()?;
        while self.word_is("and") {
            self.take();
            expression = Expression::And(Box::new(expression), Box::new(self.parse_primary()?));
        }
        Ok(expression)
    }

    fn parse_primary(&mut self) -> Result<Expression, SearchParseError> {
        if matches!(self.peek(), Some(Token::Left)) {
            self.take();
            let expression = self.parse_or()?;
            if !matches!(self.take(), Some(Token::Right)) {
                return Err(SearchParseError("missing closing parenthesis".into()));
            }
            return Ok(expression);
        }
        self.parse_clause().map(Expression::Clause)
    }

    fn parse_clause(&mut self) -> Result<SearchClause, SearchParseError> {
        let Some(Token::Word(property)) = self.take() else {
            return Err(SearchParseError("expected a property".into()));
        };
        let prop = parse_prop(&property)
            .ok_or_else(|| SearchParseError(format!("unsupported property {property}")))?;
        let operator = match self.take() {
            Some(Token::Word(operator)) | Some(Token::Operator(operator)) => {
                operator.to_ascii_lowercase()
            }
            _ => return Err(SearchParseError("expected an operator".into())),
        };
        let value = match self.take() {
            Some(Token::Word(value)) | Some(Token::Value(value)) => value,
            _ => return Err(SearchParseError("expected an operator value".into())),
        };
        Ok(match operator.as_str() {
            "contains" => SearchClause::Contains {
                prop,
                needle: value,
            },
            "doesnotcontain" => SearchClause::DoesNotContain {
                prop,
                needle: value,
            },
            "=" => SearchClause::Equals { prop, value },
            "!=" => SearchClause::NotEquals { prop, value },
            "<" => SearchClause::LessThan {
                prop,
                value,
                inclusive: false,
            },
            "<=" => SearchClause::LessThan {
                prop,
                value,
                inclusive: true,
            },
            ">" => SearchClause::GreaterThan {
                prop,
                value,
                inclusive: false,
            },
            ">=" => SearchClause::GreaterThan {
                prop,
                value,
                inclusive: true,
            },
            "derivedfrom" => SearchClause::DerivedFrom {
                prop,
                prefix: value,
            },
            "exists" => SearchClause::Exists {
                prop,
                want: match value.to_ascii_lowercase().as_str() {
                    "true" => true,
                    "false" => false,
                    _ => return Err(SearchParseError("exists expects true or false".into())),
                },
            },
            _ => return Err(SearchParseError(format!("unsupported operator {operator}"))),
        })
    }
}

fn to_groups(expression: Expression) -> Result<Vec<Vec<SearchClause>>, SearchParseError> {
    match expression {
        Expression::Clause(clause) => Ok(vec![vec![clause]]),
        Expression::Or(left, right) => {
            let mut groups = to_groups(*left)?;
            groups.extend(to_groups(*right)?);
            if groups.len() > 256 {
                return Err(SearchParseError(
                    "criteria expands to too many groups".into(),
                ));
            }
            Ok(groups)
        }
        Expression::And(left, right) => {
            let left = to_groups(*left)?;
            let right = to_groups(*right)?;
            let mut output = Vec::new();
            for left_group in &left {
                for right_group in &right {
                    let mut group = left_group.clone();
                    group.extend(right_group.clone());
                    if group.len() > 64 || output.len() >= 256 {
                        return Err(SearchParseError("criteria is too complex".into()));
                    }
                    output.push(group);
                }
            }
            Ok(output)
        }
    }
}

pub fn try_parse_search_criteria(raw: Option<&str>) -> Result<SearchQuery, SearchParseError> {
    let s = raw.map(str::trim).unwrap_or("");
    if s.is_empty() || s == "*" || s == "1=1" {
        return Ok(SearchQuery {
            groups: vec![vec![SearchClause::All]],
        });
    }
    let tokens = tokenize(s)?;
    if tokens.is_empty() {
        return Err(SearchParseError("criteria is empty".into()));
    }
    let mut parser = Parser {
        tokens,
        position: 0,
    };
    let expression = parser.parse_or()?;
    if parser.position != parser.tokens.len() {
        return Err(SearchParseError("unexpected trailing token".into()));
    }
    Ok(SearchQuery {
        groups: to_groups(expression)?,
    })
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
        SearchClause::DoesNotContain { prop, needle } => !field(row, *prop)
            .to_ascii_lowercase()
            .contains(&needle.to_ascii_lowercase()),
        SearchClause::Equals { prop, value } => field(row, *prop).eq_ignore_ascii_case(value),
        SearchClause::NotEquals { prop, value } => !field(row, *prop).eq_ignore_ascii_case(value),
        SearchClause::LessThan {
            prop,
            value,
            inclusive,
        } => {
            let order = field(row, *prop)
                .to_ascii_lowercase()
                .cmp(&value.to_ascii_lowercase());
            order.is_lt() || (*inclusive && order.is_eq())
        }
        SearchClause::GreaterThan {
            prop,
            value,
            inclusive,
        } => {
            let order = field(row, *prop)
                .to_ascii_lowercase()
                .cmp(&value.to_ascii_lowercase());
            order.is_gt() || (*inclusive && order.is_eq())
        }
        SearchClause::DerivedFrom { prop, prefix } => {
            let hay = field(row, *prop);
            class_derivedfrom(hay, prefix)
        }
        SearchClause::Exists { prop, want } => {
            let present = !field(row, *prop).is_empty();
            present == *want
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

pub fn row_matches(query: &SearchQuery, row: &SearchRow<'_>) -> bool {
    query
        .groups
        .iter()
        .any(|ands| ands.iter().all(|c| clause_matches(c, row)))
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
        let folders = parse_search_criteria(Some(r#"upnp:class derivedfrom "object.container""#));
        assert!(row_matches(&folders, &folder("Video")));
        assert!(!row_matches(&folders, &video("a")));
        let contains_c = parse_search_criteria(Some(r#"upnp:class contains "object.container""#));
        assert!(row_matches(&contains_c, &folder("Video")));
    }

    #[test]
    fn xbox_exists_false_skips_aliases() {
        let q = parse_search_criteria(Some(
            r#"upnp:class derivedfrom "object.item.videoItem" and @refID exists false"#,
        ));
        let original = SearchRow {
            title: "Fixture Movie",
            class: "item.videoItem",
            ref_id: None,
            ..SearchRow::default()
        };
        let alias = SearchRow {
            title: "Fixture Movie",
            class: "item.videoItem",
            ref_id: Some("64$ABC"),
            ..SearchRow::default()
        };
        assert!(row_matches(&q, &original));
        assert!(!row_matches(&q, &alias));
        let want = parse_search_criteria(Some(r#"@refID exists true"#));
        assert!(row_matches(&want, &alias));
        assert!(!row_matches(&want, &original));
    }

    #[test]
    fn or_is_not_and() {
        let q = parse_search_criteria(Some(
            r#"(upnp:class derivedfrom "object.item.videoItem") or (upnp:class derivedfrom "object.item.audioItem")"#,
        ));
        assert_eq!(q.groups.len(), 2, "{q:?}");
        assert!(row_matches(&q, &video("a")));
        let audio = SearchRow {
            title: "song",
            class: "item.audioItem.musicTrack",
            ..SearchRow::default()
        };
        assert!(row_matches(&q, &audio));
        assert!(!row_matches(&q, &folder("Video")));
    }

    #[test]
    fn tokenizer_respects_quotes_parentheses_precedence_and_all_operators() {
        let quoted = try_parse_search_criteria(Some(
            r#"dc:title contains "rock or roll" or (dc:title = "Other" and dc:date >= "2024-01-01")"#,
        ))
        .unwrap();
        assert_eq!(quoted.groups.len(), 2);
        assert!(row_matches(&quoted, &video("Rock or Roll Collection")));
        let dated = SearchRow {
            title: "Other",
            date: "2024-06-01",
            class: "item.videoItem",
            ..SearchRow::default()
        };
        assert!(row_matches(&quoted, &dated));

        for (criteria, matches) in [
            (r#"dc:title doesNotContain "other""#, true),
            (r#"dc:title != "Other""#, true),
            (r#"dc:title < "Zulu""#, true),
            (r#"dc:title <= "Fixture Movie""#, true),
            (r#"dc:title > "Alpha""#, true),
            (r#"dc:title >= "Fixture Movie""#, true),
        ] {
            let query = try_parse_search_criteria(Some(criteria)).unwrap();
            assert_eq!(
                row_matches(&query, &video("Fixture Movie")),
                matches,
                "{criteria}"
            );
        }
    }

    #[test]
    fn invalid_or_unknown_criteria_are_explicit_errors() {
        for criteria in [
            r#"upnp:rating contains "PG""#,
            r#"dc:title approximately "x""#,
            r#"dc:title contains "unterminated"#,
            r#"(dc:title = "x""#,
            r#"dc:title exists maybe"#,
        ] {
            assert!(
                try_parse_search_criteria(Some(criteria)).is_err(),
                "{criteria}"
            );
        }
    }
}
