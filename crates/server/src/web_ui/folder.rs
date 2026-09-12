//! Bounded, generation-owned physical-folder projections. Only IDs survive a
//! request; media metadata is copied for the requested page after sorting.
use super::*;
use std::collections::VecDeque;

const MAX_PROJECTIONS: usize = 8;
const MAX_PROJECTION_BYTES: usize = 32 * 1024 * 1024;
const MAX_CHILD_COUNTS: usize = 1024;
const MAX_CHILD_COUNT_BYTES: usize = 512 * 1024;

#[cfg(test)]
type ProjectionHook = (Arc<std::sync::Barrier>, Arc<std::sync::Barrier>);
#[cfg(test)]
static PROJECTION_HOOK: std::sync::LazyLock<Mutex<HashMap<usize, ProjectionHook>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
#[cfg(test)]
pub(crate) fn pause_projection(
    app: &App,
    reached: Arc<std::sync::Barrier>,
    release: Arc<std::sync::Barrier>,
) {
    lock_recover(&PROJECTION_HOOK).insert(app as *const App as usize, (reached, release));
}

struct Projection {
    generation: u32,
    folder: String,
    query: String,
    ids: Arc<[String]>,
    bytes: usize,
}

struct ChildCount {
    generation: u32,
    folder: String,
    count: usize,
    bytes: usize,
}

#[derive(Default)]
pub(crate) struct FolderProjectionCache {
    entries: VecDeque<Projection>,
    bytes: usize,
    child_counts: VecDeque<ChildCount>,
    child_count_bytes: usize,
    // Identity changes even when the public ui4 generation wraps.
    epoch: Arc<()>,
}

impl FolderProjectionCache {
    fn get(
        &mut self,
        generation: u32,
        folder: &str,
        query: &str,
    ) -> Option<(Arc<[String]>, usize)> {
        let position = self.entries.iter().position(|entry| {
            entry.generation == generation && entry.folder == folder && entry.query == query
        })?;
        let entry = self.entries.remove(position)?;
        let ids = (Arc::clone(&entry.ids), entry.bytes);
        self.entries.push_back(entry);
        Some(ids)
    }

    fn insert(
        &mut self,
        generation: u32,
        folder: &str,
        query: &str,
        ids: Arc<[String]>,
        bytes: usize,
    ) -> Vec<Projection> {
        let mut retired = Vec::new();
        if self.get(generation, folder, query).is_some() || bytes > MAX_PROJECTION_BYTES {
            return retired;
        }
        while self.entries.len() >= MAX_PROJECTIONS
            || self.bytes.saturating_add(bytes) > MAX_PROJECTION_BYTES
        {
            if let Some(old) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(old.bytes);
                retired.push(old);
            } else {
                break;
            }
        }
        self.bytes += bytes;
        self.entries.push_back(Projection {
            generation,
            folder: folder.into(),
            query: query.into(),
            ids,
            bytes,
        });
        retired
    }

    fn child_count(&self, generation: u32, folder: &str) -> Option<usize> {
        self.child_counts
            .iter()
            .find(|entry| entry.generation == generation && entry.folder == folder)
            .map(|entry| entry.count)
    }

    fn insert_child_count(
        &mut self,
        generation: u32,
        folder: String,
        count: usize,
    ) -> Vec<ChildCount> {
        let mut retired = Vec::new();
        let bytes = std::mem::size_of::<ChildCount>().saturating_add(folder.capacity());
        if bytes > MAX_CHILD_COUNT_BYTES || self.child_count(generation, &folder).is_some() {
            return retired;
        }
        while self.child_counts.len() >= MAX_CHILD_COUNTS
            || self.child_count_bytes.saturating_add(bytes) > MAX_CHILD_COUNT_BYTES
        {
            if let Some(old) = self.child_counts.pop_front() {
                self.child_count_bytes = self.child_count_bytes.saturating_sub(old.bytes);
                retired.push(old);
            } else {
                break;
            }
        }
        self.child_count_bytes += bytes;
        self.child_counts.push_back(ChildCount {
            generation,
            folder,
            count,
            bytes,
        });
        retired
    }
}

fn changed() -> HttpResponse {
    api_error(
        409,
        "catalog_changed",
        "The library changed while this list was loading. Refresh the list and try again.",
        true,
        Some("retry_library"),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn page(
    app: &App,
    req: &HttpRequest,
    folder_id: &str,
    query: &str,
    normalized_query: &str,
    requested_generation: Option<u32>,
    offset: usize,
    limit: usize,
    sort: &'static str,
) -> HttpResponse {
    let control = current_query_control();
    let catalog = match query_read_catalog(app, &control) {
        Ok(catalog) => catalog,
        Err(_) => return query_budget_error(),
    };
    let generation = app.update_id.load(Ordering::Acquire);
    if requested_generation.is_some_and(|expected| expected != generation) {
        return changed();
    }
    let Some(breadcrumbs) = physical_folder_chain(&catalog, folder_id) else {
        return api_error(
            404,
            "folder_missing",
            "That folder is no longer available.",
            true,
            Some("return_to_library"),
        );
    };
    // Validate the folder and generation before acknowledging an unchanged URL.
    if let Some(response) = generation_not_modified(req, generation) {
        return response;
    }
    let breadcrumb_dtos: Vec<_> = breadcrumbs
        .iter()
        .enumerate()
        .map(|(index, folder)| WebFolderRef {
            id: folder.object_id.clone(),
            title: if index == 0 {
                "Media".into()
            } else {
                folder.title.clone()
            },
        })
        .collect();
    let current = breadcrumbs.last().expect("validated physical folder chain");
    let folder = WebFolderRef {
        id: current.object_id.clone(),
        title: if current.object_id == rusty_dlna_protocol::object_id::BROWSEDIR_ID {
            "Media".into()
        } else {
            current.title.clone()
        },
    };
    let (cached, epoch) = {
        let mut cache = lock_recover(&app.folder_projection_cache);
        (
            cache.get(generation, folder_id, normalized_query),
            Arc::clone(&cache.epoch),
        )
    };
    let (ids, projection_bytes) = if let Some(ids) = cached {
        drop(catalog);
        ids
    } else {
        let child_count = current.children.len();
        drop(catalog);
        let mut key_bytes = child_count.saturating_mul(std::mem::size_of::<(u8, String, String)>());
        if control.check_work_items(child_count).is_err()
            || control.check_work_bytes(key_bytes).is_err()
        {
            return query_budget_error();
        }
        let mut keys = Vec::with_capacity(child_count);
        // Copy search/sort keys in bounded lock intervals. A waiting publisher
        // can proceed between batches even when several clients search a folder.
        for start in (0..child_count).step_by(256) {
            let catalog = match query_read_catalog(app, &control) {
                Ok(catalog) => catalog,
                Err(_) => return query_budget_error(),
            };
            if app.update_id.load(Ordering::Acquire) != generation {
                return changed();
            }
            let Some(current) = catalog.containers.get(folder_id) else {
                return changed();
            };
            let Some(children) = current
                .children
                .get(start..start.saturating_add(256).min(child_count))
            else {
                return changed();
            };
            for object_id in children {
                let entry = if let Some(folder) = catalog.containers.get(object_id) {
                    Some(WebEntry::Folder(folder))
                } else {
                    catalog
                        .items
                        .get(object_id)
                        .filter(|item| {
                            matches!(
                                media_kind_for_mime(&item.mime),
                                Some(MediaKind::Video | MediaKind::Audio)
                            )
                        })
                        .map(WebEntry::Media)
                };
                if let Some(entry) = entry
                    .filter(|entry| normalized_query.is_empty() || entry.matches(normalized_query))
                {
                    let title = entry.sort_title();
                    key_bytes = key_bytes
                        .saturating_add(title.capacity())
                        .saturating_add(entry.stable_id().len());
                    if control.check_work_bytes(key_bytes).is_err() {
                        return query_budget_error();
                    }
                    keys.push((entry.rank(), title, entry.stable_id().to_owned()));
                }
            }
            drop(catalog);
            std::thread::yield_now();
        }
        // Comparisons and sort scratch space never hold up catalog publication.
        #[cfg(test)]
        {
            let pause = lock_recover(&PROJECTION_HOOK).remove(&(app as *const App as usize));
            if let Some((reached, release)) = pause {
                reached.wait();
                release.wait();
            }
        }
        // Include sorting scratch and the temporary Vec-to-Arc ID transfer in
        // the request budget before making those allocations.
        let scratch_bytes = keys
            .len()
            .saturating_mul(3 * std::mem::size_of::<usize>() + 2 * std::mem::size_of::<String>());
        if control
            .check_work_bytes(key_bytes.saturating_add(scratch_bytes))
            .is_err()
        {
            return query_budget_error();
        }
        let mut order: Vec<usize> = (0..keys.len()).collect();
        if controlled_sort_by(&mut order, &control, |left, right| {
            keys[*left].cmp(&keys[*right])
        })
        .is_err()
        {
            return query_budget_error();
        }
        let mut ids = Vec::with_capacity(keys.len());
        let mut bytes = std::mem::size_of::<Projection>()
            .saturating_add(folder_id.len())
            .saturating_add(normalized_query.len());
        for (index, key) in order.into_iter().enumerate() {
            if index % 256 == 0 && control.check().is_err() {
                return query_budget_error();
            }
            bytes = bytes
                .saturating_add(std::mem::size_of::<String>())
                .saturating_add(keys[key].2.capacity());
            ids.push(std::mem::take(&mut keys[key].2));
        }
        (Arc::<[String]>::from(ids), bytes)
    };
    let catalog = match query_read_catalog(app, &control) {
        Ok(catalog) => catalog,
        Err(_) => return query_budget_error(),
    };
    if app.update_id.load(Ordering::Acquire) != generation {
        return changed();
    }
    enum PageEntry {
        Folder(String, String),
        Media(Box<MediaItem>),
    }
    let mut selected = Vec::with_capacity(limit);
    for object_id in ids.iter().skip(offset).take(limit) {
        if control.check().is_err() {
            return query_budget_error();
        }
        if let Some(folder) = catalog.containers.get(object_id) {
            selected.push(PageEntry::Folder(
                folder.object_id.clone(),
                folder.title.clone(),
            ));
        } else if let Some(item) = catalog.items.get(object_id) {
            selected.push(PageEntry::Media(Box::new(item.clone())));
        } else {
            return changed();
        }
    }
    drop(catalog);
    let mut entries = Vec::with_capacity(selected.len());
    let mut new_counts = Vec::new();
    for entry in selected {
        if control.check().is_err() {
            return query_budget_error();
        }
        entries.push(match entry {
            PageEntry::Folder(id, title) => {
                let cached =
                    lock_recover(&app.folder_projection_cache).child_count(generation, &id);
                let child_count = match cached {
                    Some(count) => count,
                    None => match count_folder_children(app, &id, generation, &control) {
                        Ok(count) => {
                            new_counts.push((id.clone(), count));
                            count
                        }
                        Err(ProjectionStopped::Changed) => return changed(),
                        Err(ProjectionStopped::Budget) => return query_budget_error(),
                    },
                };
                WebEntryDto::Folder {
                    id,
                    title,
                    child_count,
                }
            }
            PageEntry::Media(item) => WebEntryDto::Media(Box::new(media_dto(app, &item))),
        });
    }
    // Page metadata and counts came from one generation. Check again before
    // caching; the epoch also rejects a publication across ui4 wrap.
    let catalog = match query_read_catalog(app, &control) {
        Ok(catalog) => catalog,
        Err(_) => return query_budget_error(),
    };
    if app.update_id.load(Ordering::Acquire) != generation {
        return changed();
    }
    let (retired, retired_counts) = {
        let mut cache = lock_recover(&app.folder_projection_cache);
        if !Arc::ptr_eq(&cache.epoch, &epoch) {
            return changed();
        }
        let retired = cache.insert(
            generation,
            folder_id,
            normalized_query,
            Arc::clone(&ids),
            projection_bytes,
        );
        let mut retired_counts = Vec::new();
        for (id, count) in new_counts {
            retired_counts.extend(cache.insert_child_count(generation, id, count));
        }
        (retired, retired_counts)
    };
    drop(catalog);
    // Evicting a projection can free hundreds of thousands of strings. Never
    // do that while a publisher waits for either catalog or cache ownership.
    drop(retired);
    drop(retired_counts);
    if control.check().is_err() {
        return query_budget_error();
    }
    generation_json_response(
        req,
        generation,
        &WebLibraryPage {
            schema_version: WEB_SCHEMA_VERSION,
            generation,
            server_name: app.cfg.friendly_name.clone(),
            root_folder_id: rusty_dlna_protocol::object_id::BROWSEDIR_ID.into(),
            capabilities: web_capabilities(app),
            library_state: if ids.is_empty() { "empty" } else { "ready" },
            view: "folders",
            folder: Some(folder),
            breadcrumbs: breadcrumb_dtos,
            offset,
            limit,
            total: ids.len(),
            has_more: offset.saturating_add(entries.len()) < ids.len(),
            query: query.into(),
            sort,
            entries,
        },
    )
}

enum ProjectionStopped {
    Changed,
    Budget,
}

fn count_folder_children(
    app: &App,
    folder_id: &str,
    generation: u32,
    control: &QueryControl,
) -> Result<usize, ProjectionStopped> {
    let catalog = query_read_catalog(app, control).map_err(|_| ProjectionStopped::Budget)?;
    if app.update_id.load(Ordering::Acquire) != generation {
        return Err(ProjectionStopped::Changed);
    }
    let total = catalog
        .containers
        .get(folder_id)
        .ok_or(ProjectionStopped::Changed)?
        .children
        .len();
    drop(catalog);
    control
        .check_work_items(total)
        .map_err(|_| ProjectionStopped::Budget)?;
    let mut count = 0usize;
    for start in (0..total).step_by(256) {
        let catalog = query_read_catalog(app, control).map_err(|_| ProjectionStopped::Budget)?;
        if app.update_id.load(Ordering::Acquire) != generation {
            return Err(ProjectionStopped::Changed);
        }
        let current = catalog
            .containers
            .get(folder_id)
            .ok_or(ProjectionStopped::Changed)?;
        let children = current
            .children
            .get(start..start.saturating_add(256).min(total))
            .ok_or(ProjectionStopped::Changed)?;
        for id in children {
            if catalog.containers.contains_key(id)
                || catalog.items.get(id).is_some_and(|item| {
                    matches!(
                        media_kind_for_mime(&item.mime),
                        Some(MediaKind::Video | MediaKind::Audio)
                    )
                })
            {
                count = count.saturating_add(1);
            }
        }
        drop(catalog);
        std::thread::yield_now();
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_cache_bounds_distinct_queries_and_rejects_wrapped_generation() {
        let mut cache = FolderProjectionCache::default();
        let epoch = Arc::clone(&cache.epoch);
        for index in 0..100 {
            let id = format!("id-{index}");
            let bytes = "folder".len()
                + index.to_string().len()
                + std::mem::size_of::<String>()
                + id.capacity();
            cache.insert(0, "folder", &index.to_string(), Arc::from(vec![id]), bytes);
        }
        assert_eq!(cache.entries.len(), MAX_PROJECTIONS);
        assert!(cache.bytes <= MAX_PROJECTION_BYTES);
        assert!(cache.get(0, "folder", "0").is_none());
        assert_eq!(cache.get(0, "folder", "99").unwrap().0[0], "id-99");
        drop(std::mem::take(&mut cache));
        assert!(!Arc::ptr_eq(&cache.epoch, &epoch));
        assert!(cache.get(0, "folder", "99").is_none());
        assert_eq!(cache.bytes, 0);
        cache.insert(
            0,
            "folder",
            "oversized",
            Arc::from(vec!["x".repeat(MAX_PROJECTION_BYTES)]),
            MAX_PROJECTION_BYTES + 1,
        );
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn projection_eviction_defers_id_destruction_and_bounds_child_counts() {
        let mut cache = FolderProjectionCache::default();
        let first: Arc<[String]> = Arc::from(vec!["first".into()]);
        let observed = Arc::downgrade(&first);
        cache.insert(0, "folder", "first", first, MAX_PROJECTION_BYTES);
        let retired = cache.insert(
            0,
            "folder",
            "second",
            Arc::from(vec!["second".into()]),
            MAX_PROJECTION_BYTES,
        );
        assert!(observed.upgrade().is_some());
        assert_eq!(retired.len(), 1);
        drop(retired);
        assert!(observed.upgrade().is_none());
        for index in 0..MAX_CHILD_COUNTS + 5 {
            cache.insert_child_count(0, index.to_string(), index);
        }
        assert_eq!(cache.child_counts.len(), MAX_CHILD_COUNTS);
        assert!(cache.child_count_bytes <= MAX_CHILD_COUNT_BYTES);
        assert_eq!(cache.child_count(0, "0"), None);
        assert_eq!(cache.child_count(0, "1028"), Some(1028));
        assert_eq!(cache.child_count(1, "1028"), None);
        drop(std::mem::take(&mut cache));
        assert_eq!(cache.child_count(0, "1028"), None);
    }

    #[test]
    fn child_folder_counts_cache_only_playable_children_and_follow_publication() {
        let app = crate::tests::testdata_app();
        let root = rusty_dlna_protocol::object_id::BROWSEDIR_ID;
        let folder_id = "64$F";
        {
            let mut catalog = write_recover(&app.catalog);
            let template = catalog
                .items
                .values()
                .find(|item| item.mime.starts_with("video/"))
                .unwrap()
                .clone();
            *catalog = Catalog::new();
            for (id, parent, title) in [
                (folder_id, root, "Large folder"),
                ("64$F$FFFF", folder_id, "Nested folder"),
            ] {
                let mut folder = catalog.containers.get(root).unwrap().clone();
                folder.object_id = id.into();
                folder.parent_id = parent.into();
                folder.title = title.into();
                folder.children.clear();
                catalog.containers.insert(id.into(), folder);
                catalog
                    .containers
                    .get_mut(parent)
                    .unwrap()
                    .children
                    .push(id.into());
            }
            for index in 0..1001 {
                let mut item = template.clone();
                item.object_id = format!("{folder_id}${index:X}");
                item.parent_id = folder_id.into();
                item.detail_id = index + 1;
                if index == 1000 {
                    item.mime = "image/jpeg".into();
                }
                catalog
                    .containers
                    .get_mut(folder_id)
                    .unwrap()
                    .children
                    .push(item.object_id.clone());
                catalog.items.insert(item.object_id.clone(), item);
            }
        }
        let request = HttpRequest::parse_headers(
            "GET /api/web/library?view=folders HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        )
        .unwrap();
        let read_count = || {
            let response = app.handle(&request);
            assert_eq!(response.status, 200);
            let payload: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
            payload["entries"][0]["child_count"].as_u64().unwrap()
        };
        assert_eq!(read_count(), 1001);
        let generation = app.update_id.load(Ordering::Acquire);
        assert_eq!(
            lock_recover(&app.folder_projection_cache).child_count(generation, folder_id),
            Some(1001)
        );
        assert_eq!(read_count(), 1001);
        let retired;
        {
            let mut catalog = write_recover(&app.catalog);
            catalog.items.remove("64$F$0");
            retired = app.invalidate_catalog_query_cache();
            app.update_id
                .store(generation.wrapping_add(1), Ordering::Release);
        }
        drop(retired);
        assert!(lock_recover(&app.folder_projection_cache)
            .child_counts
            .is_empty());
        assert_eq!(read_count(), 1000);
    }
}
