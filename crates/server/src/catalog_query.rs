//! SQLite-backed and in-memory catalog query, pagination, and stable sorting.

use std::path::Path;
use std::sync::atomic::Ordering;

#[cfg(test)]
static WEB_MEDIA_QUERY_COUNTS: std::sync::LazyLock<
    std::sync::Mutex<
        std::collections::HashMap<
            std::path::PathBuf,
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
        >,
    >,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
static CATALOG_QUERY_COUNTS: std::sync::LazyLock<
    std::sync::Mutex<
        std::collections::HashMap<
            std::path::PathBuf,
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
        >,
    >,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
pub(crate) fn count_catalog_queries_for_test(
    path: &Path,
    count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    CATALOG_QUERY_COUNTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(path.to_path_buf(), count);
}

#[cfg(test)]
pub(crate) fn stop_counting_catalog_queries_for_test(path: &Path) {
    CATALOG_QUERY_COUNTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(path);
}

#[cfg(test)]
fn record_catalog_query(path: &Path) {
    let count = CATALOG_QUERY_COUNTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(path)
        .cloned();
    if let Some(count) = count {
        count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
pub(crate) fn count_web_media_queries_for_test(
    path: &Path,
    count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    WEB_MEDIA_QUERY_COUNTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(path.to_path_buf(), count);
}

#[cfg(test)]
pub(crate) fn stop_counting_web_media_queries_for_test(path: &Path) {
    WEB_MEDIA_QUERY_COUNTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(path);
}

#[cfg(test)]
fn record_web_media_query(path: &Path) {
    let count = WEB_MEDIA_QUERY_COUNTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(path)
        .cloned();
    if let Some(count) = count {
        count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

use rusty_dlna_protocol::ClientProfile;
use rusty_dlna_scan::{
    Catalog, CatalogChild, CatalogDefaultOrder, CatalogQuery, CatalogQueryClause,
    CatalogQueryField, CatalogQueryOp, CatalogQueryPage, CatalogQuerySort, LibraryDb, MediaItem,
    WebMediaKind, WebMediaSort,
};
use rusty_dlna_soap::{
    row_matches, DefaultOrder, DidlObject, FilterBits, SearchClause, SearchProp, SearchQuery,
    SearchRow, SortKey, SortSpec,
};

use super::{App, CatalogChildRef, DbPool, MAX_SOAP_PAGE_OBJECTS};

/// Hash the canonical structured query as it is formatted, without allocating
/// an expanded Debug string (or a cache key proportional to request size).
pub(super) fn search_cache_key(
    scope: &str,
    query: &CatalogQuery,
    start: usize,
    take: usize,
) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    struct DigestWriter(Sha256);
    impl std::fmt::Write for DigestWriter {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            self.0.update(text.as_bytes());
            Ok(())
        }
    }
    let mut writer = DigestWriter(Sha256::new());
    // DigestWriter is infallible; Debug for these owned query types only writes.
    let _ = write!(writer, "{:?}", ("search", scope, query, start, take));
    let mut key = String::from("search:");
    for byte in writer.0.finalize() {
        let _ = write!(key, "{byte:02x}");
    }
    key
}

pub(super) fn container_search_row(c: &rusty_dlna_scan::Container) -> SearchRow<'_> {
    SearchRow {
        title: &c.title,
        class: &c.class,
        id: &c.object_id,
        parent_id: &c.parent_id,
        is_container: true,
        ..SearchRow::default()
    }
}

pub(super) fn item_search_row(it: &MediaItem) -> SearchRow<'_> {
    SearchRow {
        title: &it.title,
        creator: it.creator.as_deref().unwrap_or(""),
        date: &it.date,
        class: &it.class,
        artist: it.artist.as_deref().unwrap_or(""),
        genre: it.genre.as_deref().unwrap_or(""),
        album: it.album.as_deref().unwrap_or(""),
        id: &it.object_id,
        parent_id: &it.parent_id,
        ref_id: it.ref_id.as_deref(),
        is_container: false,
        ..SearchRow::default()
    }
}

pub(super) fn query_field(prop: SearchProp) -> CatalogQueryField {
    match prop {
        SearchProp::Title => CatalogQueryField::Title,
        SearchProp::Creator => CatalogQueryField::Creator,
        SearchProp::Date => CatalogQueryField::Date,
        SearchProp::Class => CatalogQueryField::Class,
        SearchProp::Artist => CatalogQueryField::Artist,
        SearchProp::Genre => CatalogQueryField::Genre,
        SearchProp::Album => CatalogQueryField::Album,
        SearchProp::Actor => CatalogQueryField::Actor,
        SearchProp::Id => CatalogQueryField::Id,
        SearchProp::ParentId => CatalogQueryField::ParentId,
        SearchProp::RefId => CatalogQueryField::RefId,
    }
}

pub(super) fn query_clause(clause: &SearchClause) -> CatalogQueryClause {
    match clause {
        SearchClause::Contains { prop, needle } => CatalogQueryClause {
            field: query_field(*prop),
            op: CatalogQueryOp::Contains(needle.clone()),
        },
        SearchClause::DoesNotContain { prop, needle } => CatalogQueryClause {
            field: query_field(*prop),
            op: CatalogQueryOp::DoesNotContain(needle.clone()),
        },
        SearchClause::Equals { prop, value } => CatalogQueryClause {
            field: query_field(*prop),
            op: CatalogQueryOp::Equals(value.clone()),
        },
        SearchClause::NotEquals { prop, value } => CatalogQueryClause {
            field: query_field(*prop),
            op: CatalogQueryOp::NotEquals(value.clone()),
        },
        SearchClause::LessThan {
            prop,
            value,
            inclusive,
        } => CatalogQueryClause {
            field: query_field(*prop),
            op: CatalogQueryOp::LessThan {
                value: value.clone(),
                inclusive: *inclusive,
            },
        },
        SearchClause::GreaterThan {
            prop,
            value,
            inclusive,
        } => CatalogQueryClause {
            field: query_field(*prop),
            op: CatalogQueryOp::GreaterThan {
                value: value.clone(),
                inclusive: *inclusive,
            },
        },
        SearchClause::DerivedFrom { prop, prefix } => CatalogQueryClause {
            field: query_field(*prop),
            op: CatalogQueryOp::DerivedFrom(prefix.clone()),
        },
        SearchClause::Exists { prop, want } => CatalogQueryClause {
            field: query_field(*prop),
            op: CatalogQueryOp::Exists(*want),
        },
        SearchClause::Unknown => CatalogQueryClause {
            field: CatalogQueryField::Id,
            op: CatalogQueryOp::Never,
        },
        SearchClause::All => CatalogQueryClause {
            field: CatalogQueryField::Id,
            op: CatalogQueryOp::All,
        },
    }
}

pub(super) fn query_sort(spec: &SortSpec) -> CatalogQuerySort {
    CatalogQuerySort {
        field: match spec.key {
            SortKey::Title => CatalogQueryField::Title,
            SortKey::Date => CatalogQueryField::Date,
            SortKey::Class => CatalogQueryField::Class,
            SortKey::Album => CatalogQueryField::Album,
            SortKey::EpisodeNumber | SortKey::Track => CatalogQueryField::Track,
        },
        descending: spec.descending,
    }
}

pub(super) fn query_default(default: DefaultOrder) -> CatalogDefaultOrder {
    match default {
        DefaultOrder::FoldersFirst => CatalogDefaultOrder::FoldersFirst,
        DefaultOrder::Lg => CatalogDefaultOrder::ClassTitle,
        DefaultOrder::ForceSort => CatalogDefaultOrder::ClassDiscTrackTitle,
    }
}

pub(super) fn catalog_query(
    clauses: &SearchQuery,
    sort: &[SortSpec],
    default: DefaultOrder,
) -> CatalogQuery {
    CatalogQuery {
        groups: clauses
            .groups
            .iter()
            .map(|group| group.iter().map(query_clause).collect())
            .collect(),
        sort: sort.iter().map(query_sort).collect(),
        default_order: query_default(default),
    }
}

pub(super) fn query_db_children(
    pool: Option<&DbPool>,
    db_path: Option<&Path>,
    parent: &str,
    sort: &[SortSpec],
    default: DefaultOrder,
    start: usize,
    take: usize,
) -> Option<CatalogQuerySnapshot> {
    let path = db_path?;
    #[cfg(test)]
    record_catalog_query(path);
    let query = |db: &LibraryDb| {
        let transaction = db.transaction()?;
        let generation = db.get_update_id()?;
        let sort: Vec<_> = sort.iter().map(query_sort).collect();
        let page = db.query_children_page(parent, &sort, query_default(default), start, take)?;
        transaction.commit()?;
        Ok(CatalogQuerySnapshot { generation, page })
    };
    let result = match pool {
        Some(pool) => pool.read(query),
        None => LibraryDb::open_read_only(path).and_then(|db| query(&db)),
    };
    match result {
        Ok(page) => Some(page),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "catalog Browse query fell back to memory");
            None
        }
    }
}

pub(super) fn query_db_search(
    pool: Option<&DbPool>,
    db_path: Option<&Path>,
    root: &str,
    query: &CatalogQuery,
    start: usize,
    take: usize,
) -> Option<CatalogQuerySnapshot> {
    let path = db_path?;
    #[cfg(test)]
    record_catalog_query(path);
    let result = match pool {
        Some(pool) => pool.read(|db| {
            let transaction = db.transaction()?;
            let generation = db.get_update_id()?;
            let page = db.query_search_page(root, query, start, take)?;
            transaction.commit()?;
            Ok(CatalogQuerySnapshot { generation, page })
        }),
        None => LibraryDb::open_read_only(path).and_then(|db| {
            let transaction = db.transaction()?;
            let generation = db.get_update_id()?;
            let page = db.query_search_page(root, query, start, take)?;
            transaction.commit()?;
            Ok(CatalogQuerySnapshot { generation, page })
        }),
    };
    match result {
        Ok(page) => Some(page),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "catalog Search query fell back to memory");
            None
        }
    }
}

pub(super) fn query_db_web_media(
    pool: Option<&DbPool>,
    db_path: Option<&Path>,
    kind: WebMediaKind,
    query: &str,
    sort: WebMediaSort,
    start: usize,
    take: usize,
) -> Option<CatalogQuerySnapshot> {
    let path = db_path?;
    #[cfg(test)]
    record_web_media_query(path);
    let query_page = |database: &LibraryDb| {
        let transaction = database.transaction()?;
        let generation = database.get_update_id()?;
        let page = database.query_web_media_page(kind, query, sort, start, take)?;
        transaction.commit()?;
        Ok(CatalogQuerySnapshot { generation, page })
    };
    let result = match pool {
        Some(pool) => pool.read(query_page),
        None => LibraryDb::open_read_only(path).and_then(|database| query_page(&database)),
    };
    match result {
        Ok(page) => Some(page),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "web catalog query fell back to memory");
            None
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct CatalogQuerySnapshot {
    pub(super) generation: u32,
    pub(super) page: CatalogQueryPage,
}

pub(super) fn query_stable_catalog_snapshot(
    app: &App,
    mut query: impl FnMut() -> Option<CatalogQuerySnapshot>,
) -> Result<Option<CatalogQuerySnapshot>, ()> {
    const MAX_ATTEMPTS: usize = 3;
    for attempt in 0..MAX_ATTEMPTS {
        if let Ok(snapshot) = query_catalog_snapshot_once(app, &mut query) {
            return Ok(snapshot);
        }
        if attempt + 1 < MAX_ATTEMPTS {
            std::thread::yield_now();
        }
    }
    Err(())
}

pub(super) fn query_catalog_snapshot_once(
    app: &App,
    query: impl FnOnce() -> Option<CatalogQuerySnapshot>,
) -> Result<Option<CatalogQuerySnapshot>, ()> {
    let before = app.update_id.load(Ordering::Acquire);
    let Some(snapshot) = query() else {
        return Ok(None);
    };
    let after = app.update_id.load(Ordering::Acquire);
    (before == snapshot.generation && snapshot.generation == after)
        .then_some(Some(snapshot))
        .ok_or(())
}

pub(super) fn catalog_population(cat: &Catalog) -> u32 {
    let count = cat
        .containers
        .len()
        .saturating_add(cat.items.len())
        .saturating_sub(1);
    u32::try_from(count).unwrap_or(u32::MAX)
}

pub(super) fn materialize_db_page<'a>(
    cat: &'a Catalog,
    page: &CatalogQueryPage,
) -> Option<Vec<CatalogChildRef<'a>>> {
    if page.population != catalog_population(cat) {
        return None;
    }
    page.object_ids
        .iter()
        .map(|id| child_ref_by_id(cat, id))
        .collect()
}

enum SearchScope<'a> {
    All,
    Ids(std::collections::HashSet<&'a str>),
}

fn search_scope<'a>(cat: &'a Catalog, root: &str) -> SearchScope<'a> {
    if root.is_empty() || root == rusty_dlna_protocol::object_id::ROOT_ID {
        return SearchScope::All;
    }
    let mut out = std::collections::HashSet::new();
    let Some((root_id, _)) = cat.containers.get_key_value(root) else {
        return SearchScope::Ids(out);
    };
    let mut stack = vec![root_id.as_str()];
    while let Some(id) = stack.pop() {
        if !out.insert(id) {
            continue;
        }
        if let Some(c) = cat.containers.get(id) {
            for ch in &c.children {
                stack.push(ch.as_str());
            }
        }
    }
    SearchScope::Ids(out)
}

fn scope_contains(scope: &SearchScope<'_>, id: &str) -> bool {
    match scope {
        SearchScope::All => true,
        SearchScope::Ids(ids) => ids.contains(id),
    }
}

fn item_in_scope(it: &MediaItem, scope: &SearchScope<'_>) -> bool {
    scope_contains(scope, &it.object_id)
        || scope_contains(scope, &it.parent_id)
        || it
            .ref_id
            .as_ref()
            .is_some_and(|value| scope_contains(scope, value))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn search_memory_page(
    app: &App,
    cat: &Catalog,
    scope: &str,
    clauses: &SearchQuery,
    sort: &[SortSpec],
    order: DefaultOrder,
    start: usize,
    take: usize,
    client: &ClientProfile,
    ua: Option<&str>,
    bits: &FilterBits,
) -> (Vec<DidlObject>, u32) {
    let scoped = search_scope(cat, scope);
    let mut hits: Vec<CatalogChildRef<'_>> = Vec::new();
    for container in cat.containers.values() {
        if container.object_id == rusty_dlna_protocol::object_id::ROOT_ID
            || !scope_contains(&scoped, &container.object_id)
        {
            continue;
        }
        if row_matches(clauses, &container_search_row(container)) {
            hits.push(CatalogChildRef::Container(container));
        }
    }
    for item in cat.items.values() {
        if item_in_scope(item, &scoped) && row_matches(clauses, &item_search_row(item)) {
            hits.push(CatalogChildRef::Item(item));
        }
    }
    sort_catalog_child_refs(&mut hits, sort, order);
    let total = u32::try_from(hits.len()).unwrap_or(u32::MAX);
    let page = hits
        .into_iter()
        .skip(start)
        .take(take)
        .map(|child| app.to_didl_ref(child, cat, client, ua, bits))
        .collect();
    (page, total)
}

pub(super) fn child_ref_by_id<'a>(cat: &'a Catalog, id: &str) -> Option<CatalogChildRef<'a>> {
    cat.containers
        .get(id)
        .map(CatalogChildRef::Container)
        .or_else(|| cat.items.get(id).map(CatalogChildRef::Item))
}

pub(super) fn catalog_child_as_ref(child: &CatalogChild) -> CatalogChildRef<'_> {
    match child {
        CatalogChild::Container(value) => CatalogChildRef::Container(value),
        CatalogChild::Item(value) => CatalogChildRef::Item(value),
    }
}

/// Sort lightweight references and clone only the requested page.  Recent
/// containers synthesize object IDs and are already capped by the scanner, so
/// they retain their specialized materialization path.
pub(super) fn sorted_child_page(
    cat: &Catalog,
    id: &str,
    start: usize,
    take: usize,
    specs: &[SortSpec],
    default: DefaultOrder,
) -> Option<(Vec<CatalogChild>, u32)> {
    let container = cat.containers.get(id)?;
    let displayed_total = cat.displayed_child_count(id);
    if usize::try_from(displayed_total).ok() != Some(container.children.len()) {
        let (mut recent, total) = cat.page_children(id, 0, MAX_SOAP_PAGE_OBJECTS)?;
        sort_catalog_children(&mut recent, specs, default);
        return Some((recent.into_iter().skip(start).take(take).collect(), total));
    }
    let mut refs: Vec<_> = container
        .children
        .iter()
        .filter_map(|child| child_ref_by_id(cat, child))
        .collect();
    sort_catalog_child_refs(&mut refs, specs, default);
    let total = u32::try_from(refs.len()).unwrap_or(u32::MAX);
    let page = refs
        .into_iter()
        .skip(start)
        .take(take)
        .map(CatalogChildRef::to_owned)
        .collect();
    Some((page, total))
}

pub(super) fn sort_catalog_children(
    children: &mut [CatalogChild],
    specs: &[SortSpec],
    default: DefaultOrder,
) {
    children.sort_by(|a, b| cmp_children(a, b, specs, default));
}

pub(super) fn sort_catalog_child_refs(
    children: &mut [CatalogChildRef<'_>],
    specs: &[SortSpec],
    default: DefaultOrder,
) {
    children.sort_by(|a, b| cmp_child_refs(*a, *b, specs, default));
}

pub(super) fn cmp_children(
    a: &CatalogChild,
    b: &CatalogChild,
    specs: &[SortSpec],
    default: DefaultOrder,
) -> std::cmp::Ordering {
    let a = match a {
        CatalogChild::Container(value) => CatalogChildRef::Container(value),
        CatalogChild::Item(value) => CatalogChildRef::Item(value),
    };
    let b = match b {
        CatalogChild::Container(value) => CatalogChildRef::Container(value),
        CatalogChild::Item(value) => CatalogChildRef::Item(value),
    };
    cmp_child_refs(a, b, specs, default)
}

pub(super) fn cmp_child_refs(
    a: CatalogChildRef<'_>,
    b: CatalogChildRef<'_>,
    specs: &[SortSpec],
    default: DefaultOrder,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let primary = if !specs.is_empty() {
        for spec in specs {
            let ord = cmp_sort_key_ref(a, b, spec.key);
            let ord = if spec.descending { ord.reverse() } else { ord };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    } else {
        match default {
            DefaultOrder::FoldersFirst => match (is_folder_ref(a), is_folder_ref(b)) {
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                _ => cmp_ci(child_title_ref(a), child_title_ref(b)),
            },
            DefaultOrder::Lg => {
                let c = cmp_ci(child_class_ref(a), child_class_ref(b));
                if c != Ordering::Equal {
                    return c;
                }
                cmp_ci(child_title_ref(a), child_title_ref(b))
            }
            DefaultOrder::ForceSort => {
                let c = cmp_ci(child_class_ref(a), child_class_ref(b));
                if c != Ordering::Equal {
                    return c;
                }
                let c = child_disc_ref(a).cmp(&child_disc_ref(b));
                if c != Ordering::Equal {
                    return c;
                }
                let c = child_track_ref(a).cmp(&child_track_ref(b));
                if c != Ordering::Equal {
                    return c;
                }
                cmp_ci(child_title_ref(a), child_title_ref(b))
            }
        }
    };
    if primary == Ordering::Equal {
        // Match `catalog_order_sql`: the stable identity tie-breaker is always
        // ascending, even when the final requested key is descending.
        child_object_id_ref(a).cmp(child_object_id_ref(b))
    } else {
        primary
    }
}

pub(super) fn child_object_id_ref(ch: CatalogChildRef<'_>) -> &str {
    match ch {
        CatalogChildRef::Container(value) => &value.object_id,
        CatalogChildRef::Item(value) => &value.object_id,
    }
}

pub(super) fn is_folder_ref(ch: CatalogChildRef<'_>) -> bool {
    matches!(ch, CatalogChildRef::Container(_))
}

pub(super) fn child_title_ref(ch: CatalogChildRef<'_>) -> &str {
    match ch {
        CatalogChildRef::Container(value) => &value.title,
        CatalogChildRef::Item(value) => &value.title,
    }
}

pub(super) fn child_class_ref(ch: CatalogChildRef<'_>) -> &str {
    match ch {
        CatalogChildRef::Container(value) => &value.class,
        CatalogChildRef::Item(value) => &value.class,
    }
}

pub(super) fn child_date_ref(ch: CatalogChildRef<'_>) -> &str {
    match ch {
        CatalogChildRef::Container(_) => "",
        CatalogChildRef::Item(value) => &value.date,
    }
}

pub(super) fn child_album_ref(ch: CatalogChildRef<'_>) -> &str {
    match ch {
        CatalogChildRef::Container(_) => "",
        CatalogChildRef::Item(value) => value.album.as_deref().unwrap_or(""),
    }
}

pub(super) fn child_disc_ref(ch: CatalogChildRef<'_>) -> i64 {
    match ch {
        CatalogChildRef::Container(_) => 0,
        CatalogChildRef::Item(value) => value.disc.unwrap_or(0),
    }
}

pub(super) fn child_track_ref(ch: CatalogChildRef<'_>) -> i64 {
    match ch {
        CatalogChildRef::Container(_) => 0,
        CatalogChildRef::Item(value) => value.track.unwrap_or(0),
    }
}

pub(super) fn cmp_ci(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase())
}

pub(super) fn cmp_sort_key_ref(
    a: CatalogChildRef<'_>,
    b: CatalogChildRef<'_>,
    key: SortKey,
) -> std::cmp::Ordering {
    match key {
        SortKey::Title => cmp_ci(child_title_ref(a), child_title_ref(b)),
        SortKey::Date => child_date_ref(a).cmp(child_date_ref(b)),
        SortKey::Class => cmp_ci(child_class_ref(a), child_class_ref(b)),
        SortKey::Album => cmp_ci(child_album_ref(a), child_album_ref(b)),
        SortKey::EpisodeNumber | SortKey::Track => child_track_ref(a).cmp(&child_track_ref(b)),
    }
}
