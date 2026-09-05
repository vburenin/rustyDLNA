//! Request-time catalog data model and bounded browse views.
#![warn(missing_docs)]

use super::*;

#[allow(missing_docs)]
#[derive(Clone, Debug)]
pub struct Container {
    pub object_id: String,
    pub parent_id: String,
    pub title: String,
    pub class: String,
    pub children: Vec<String>,
    pub searchable: bool,
}

#[allow(missing_docs)]
#[derive(Clone, Debug)]
pub struct Catalog {
    pub containers: HashMap<String, Container>,
    pub items: HashMap<String, MediaItem>,
    pub by_detail: HashMap<i64, String>,
    pub next_detail: i64,
    /// Unique browse-folder videos, capped at `RECENT_MAX`.
    pub recent_count: u32,
    /// Newest unique browse-folder item ids (already sorted).
    pub recent_ids: Vec<String>,
    /// Bounded overflow behind `recent_ids` so ordinary incremental deletes
    /// can refill the hot Recently Added view without a full catalog walk.
    recent_candidate_ids: Vec<String>,
    /// `ALBUM_ART.ID` → stored JPEG path.
    pub album_art_paths: HashMap<i64, PathBuf>,
    recent_limit: usize,
    recent_cutoff_unix: Option<i64>,
}

#[allow(missing_docs)]
#[derive(Debug, Default)]
pub struct CatalogPatch {
    pub(crate) changed_object_ids: Vec<String>,
    pub(crate) changed_detail_ids: Vec<i64>,
    pub(crate) changed_album_art_ids: Vec<i64>,
    pub(crate) containers: Vec<Container>,
    pub(crate) items: Vec<MediaItem>,
    pub(crate) album_art_paths: HashMap<i64, PathBuf>,
}

fn physical_identity_key(item: &MediaItem) -> (u64, u64) {
    if item.inode != 0 {
        (item.device, item.inode)
    } else {
        (0, u64_from_sqlite_i64_bits(item.detail_id))
    }
}

fn catalog_count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[allow(missing_docs)]
impl Catalog {
    pub fn new() -> Self {
        let mut c = Self {
            containers: HashMap::new(),
            items: HashMap::new(),
            by_detail: HashMap::new(),
            next_detail: 1,
            recent_count: 0,
            recent_ids: Vec::new(),
            recent_candidate_ids: Vec::new(),
            album_art_paths: HashMap::new(),
            recent_limit: RECENT_MAX,
            recent_cutoff_unix: None,
        };
        c.add_container(ROOT_ID, "-1", "root", "container.storageFolder", true);
        c.add_container(
            BROWSEDIR_ID,
            ROOT_ID,
            "Browse Folders",
            "container.storageFolder",
            true,
        );
        c.add_container(MUSIC_ID, ROOT_ID, "Music", "container.storageFolder", true);
        c.add_container(VIDEO_ID, ROOT_ID, "Video", "container.storageFolder", true);
        c.add_container(
            IMAGE_ID,
            ROOT_ID,
            "Pictures",
            "container.storageFolder",
            true,
        );
        c.add_container(
            VIDEO_ALL_ID,
            VIDEO_ID,
            "All Video",
            "container.storageFolder",
            true,
        );
        c.add_container(
            VIDEO_DIR_ID,
            VIDEO_ID,
            "Folders",
            "container.storageFolder",
            true,
        );
        c.add_container(
            VIDEO_RECENT_ID,
            VIDEO_ID,
            "Recently Added",
            "container.storageFolder",
            false,
        );
        c.add_container(
            VIDEO_SERIES_ID,
            VIDEO_ID,
            "Series",
            "container.storageFolder",
            true,
        );
        c.add_container(
            VIDEO_GENRE_ID,
            VIDEO_ID,
            "Genre",
            "container.storageFolder",
            true,
        );
        c.add_container(
            VIDEO_ACTOR_ID,
            VIDEO_ID,
            "Actor",
            "container.storageFolder",
            true,
        );
        c.add_container(
            VIDEO_PLIST_ID,
            VIDEO_ID,
            "Playlists",
            "container.storageFolder",
            true,
        );
        c.add_container(
            VIDEO_RATING_ID,
            VIDEO_ID,
            "Rating",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_ALL_ID,
            MUSIC_ID,
            "All Music",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_GENRE_ID,
            MUSIC_ID,
            "Genre",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_ARTIST_ID,
            MUSIC_ID,
            "Artist",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_ALBUM_ID,
            MUSIC_ID,
            "Album",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_DIR_ID,
            MUSIC_ID,
            "Folders",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_PLIST_ID,
            MUSIC_ID,
            "Playlists",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_CONTRIB_ARTIST_ID,
            MUSIC_ID,
            "Contributing Artists",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_ALBUM_ARTIST_ID,
            MUSIC_ID,
            "Album Artist",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_COMPOSER_ID,
            MUSIC_ID,
            "Composer",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_RATING_ID,
            MUSIC_ID,
            "Rating",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_RECENT_ID,
            MUSIC_ID,
            "Recently Added",
            "container.storageFolder",
            false,
        );
        c.add_container(
            IMAGE_ALL_ID,
            IMAGE_ID,
            "All Pictures",
            "container.storageFolder",
            true,
        );
        c.add_container(
            IMAGE_DATE_ID,
            IMAGE_ID,
            "Date Taken",
            "container.album.photoAlbum",
            true,
        );
        c.add_container(
            IMAGE_ALBUM_ID,
            IMAGE_ID,
            "Album",
            "container.storageFolder",
            true,
        );
        c.add_container(
            IMAGE_CAMERA_ID,
            IMAGE_ID,
            "Camera",
            "container.storageFolder",
            true,
        );
        c.add_container(
            IMAGE_DIR_ID,
            IMAGE_ID,
            "Folders",
            "container.storageFolder",
            true,
        );
        c.add_container(
            IMAGE_PLIST_ID,
            IMAGE_ID,
            "Playlists",
            "container.storageFolder",
            true,
        );
        c.add_container(
            IMAGE_RATING_ID,
            IMAGE_ID,
            "Rating",
            "container.storageFolder",
            true,
        );
        c.add_container(
            IMAGE_RECENT_ID,
            IMAGE_ID,
            "Recently Added",
            "container.storageFolder",
            false,
        );
        c.link_child(ROOT_ID, BROWSEDIR_ID);
        c.link_child(ROOT_ID, MUSIC_ID);
        c.link_child(ROOT_ID, VIDEO_ID);
        c.link_child(ROOT_ID, IMAGE_ID);
        c.link_child(VIDEO_ID, VIDEO_ALL_ID);
        c.link_child(VIDEO_ID, VIDEO_DIR_ID);
        c.link_child(VIDEO_ID, VIDEO_RECENT_ID);
        c.link_child(VIDEO_ID, VIDEO_SERIES_ID);
        c.link_child(VIDEO_ID, VIDEO_GENRE_ID);
        c.link_child(VIDEO_ID, VIDEO_ACTOR_ID);
        c.link_child(VIDEO_ID, VIDEO_PLIST_ID);
        c.link_child(VIDEO_ID, VIDEO_RATING_ID);
        c.link_child(MUSIC_ID, MUSIC_ALL_ID);
        c.link_child(MUSIC_ID, MUSIC_GENRE_ID);
        c.link_child(MUSIC_ID, MUSIC_ARTIST_ID);
        c.link_child(MUSIC_ID, MUSIC_ALBUM_ID);
        c.link_child(MUSIC_ID, MUSIC_DIR_ID);
        c.link_child(MUSIC_ID, MUSIC_PLIST_ID);
        c.link_child(MUSIC_ID, MUSIC_CONTRIB_ARTIST_ID);
        c.link_child(MUSIC_ID, MUSIC_ALBUM_ARTIST_ID);
        c.link_child(MUSIC_ID, MUSIC_COMPOSER_ID);
        c.link_child(MUSIC_ID, MUSIC_RATING_ID);
        c.link_child(MUSIC_ID, MUSIC_RECENT_ID);
        c.link_child(IMAGE_ID, IMAGE_ALL_ID);
        c.link_child(IMAGE_ID, IMAGE_DATE_ID);
        c.link_child(IMAGE_ID, IMAGE_ALBUM_ID);
        c.link_child(IMAGE_ID, IMAGE_CAMERA_ID);
        c.link_child(IMAGE_ID, IMAGE_DIR_ID);
        c.link_child(IMAGE_ID, IMAGE_PLIST_ID);
        c.link_child(IMAGE_ID, IMAGE_RATING_ID);
        c.link_child(IMAGE_ID, IMAGE_RECENT_ID);
        c
    }

    /// Apply rows captured by one committed scanner transaction without
    /// rebuilding or replacing the full request-time catalog.
    pub fn apply_patch(&mut self, mut patch: CatalogPatch) {
        let recent_cache_was_saturated =
            self.recent_candidate_ids.len() == self.recent_cache_limit();
        let recent_changed_ids: Vec<String> = patch
            .items
            .iter()
            .map(|item| item.object_id.clone())
            .collect();
        let changed_details: HashSet<i64> = patch.changed_detail_ids.iter().copied().collect();
        let mut removed_objects: HashSet<String> =
            patch.changed_object_ids.iter().cloned().collect();
        removed_objects.extend(patch.items.iter().map(|item| item.object_id.clone()));

        let mut old_container_children = HashMap::new();
        let mut old_parents = HashSet::new();
        for object_id in &removed_objects {
            if let Some(item) = self.items.remove(object_id) {
                old_parents.insert(item.parent_id);
            }
            if let Some(container) = self.containers.remove(object_id) {
                old_parents.insert(container.parent_id.clone());
                old_container_children.insert(object_id.clone(), container.children);
            }
        }
        for parent in old_parents {
            if let Some(container) = self.containers.get_mut(&parent) {
                container
                    .children
                    .retain(|object_id| !removed_objects.contains(object_id));
            }
        }
        self.by_detail
            .retain(|detail_id, _| !changed_details.contains(detail_id));

        let mut container_links = Vec::with_capacity(patch.containers.len());
        for mut container in patch.containers.drain(..) {
            if let Some(children) = old_container_children.remove(&container.object_id) {
                container.children = children;
            }
            let object_id = container.object_id.clone();
            let parent_id = container.parent_id.clone();
            self.containers.insert(object_id.clone(), container);
            container_links.push((object_id, parent_id));
        }
        for (object_id, parent_id) in container_links {
            if let Some(parent) = self.containers.get_mut(&parent_id) {
                if !parent.children.contains(&object_id) {
                    parent.children.push(object_id);
                }
            }
        }
        for item in patch.items {
            let object_id = item.object_id.clone();
            let parent_id = item.parent_id.clone();
            let detail_id = item.detail_id;
            self.next_detail = self.next_detail.max(detail_id.saturating_add(1));
            self.by_detail
                .entry(detail_id)
                .or_insert_with(|| object_id.clone());
            self.items.insert(object_id.clone(), item);
            if let Some(parent) = self.containers.get_mut(&parent_id) {
                if !parent.children.contains(&object_id) {
                    parent.children.push(object_id);
                }
            }
        }
        for album_art_id in patch.changed_album_art_ids {
            match patch.album_art_paths.remove(&album_art_id) {
                Some(path) => {
                    self.album_art_paths.insert(album_art_id, path);
                }
                None => {
                    self.album_art_paths.remove(&album_art_id);
                }
            }
        }
        self.refresh_recent_index(&recent_changed_ids, recent_cache_was_saturated);
    }

    pub(crate) fn add_container(
        &mut self,
        id: &str,
        parent: &str,
        title: &str,
        class: &str,
        searchable: bool,
    ) {
        self.containers.insert(
            id.to_string(),
            Container {
                object_id: id.to_string(),
                parent_id: parent.to_string(),
                title: title.to_string(),
                class: class.to_string(),
                children: Vec::new(),
                searchable,
            },
        );
    }

    pub(crate) fn link_child(&mut self, parent: &str, child: &str) {
        if let Some(p) = self.containers.get_mut(parent) {
            if !p.children.iter().any(|c| c == child) {
                p.children.push(child.to_string());
            }
        }
    }

    pub fn get_item_by_detail(&self, id: i64) -> Option<&MediaItem> {
        let oid = self.by_detail.get(&id)?;
        self.items.get(oid)
    }

    /// Approximate owned bytes for capacity planning. This includes value
    /// structs and the heap buffers directly owned by catalog strings,
    /// vectors, captions, paths, and index entries; allocator/hash-table
    /// bucket overhead is intentionally reported as an estimate.
    pub fn estimated_memory_bytes(&self) -> u64 {
        fn string_bytes(value: &String) -> usize {
            value.capacity()
        }
        fn optional_string_bytes(value: &Option<String>) -> usize {
            value.as_ref().map(string_bytes).unwrap_or(0)
        }
        let mut bytes = self
            .items
            .len()
            .saturating_mul(std::mem::size_of::<MediaItem>())
            .saturating_add(
                self.containers
                    .len()
                    .saturating_mul(std::mem::size_of::<Container>()),
            );
        for (key, item) in &self.items {
            bytes = bytes
                .saturating_add(key.capacity())
                .saturating_add(item.object_id.capacity())
                .saturating_add(item.parent_id.capacity())
                .saturating_add(item.title.capacity())
                .saturating_add(item.class.capacity())
                .saturating_add(item.date.capacity())
                .saturating_add(item.path.as_os_str().as_encoded_bytes().len())
                .saturating_add(
                    item.collection_path
                        .as_ref()
                        .map_or(0, |path| path.as_os_str().as_encoded_bytes().len()),
                )
                .saturating_add(item.mime.capacity())
                .saturating_add(item.ext.capacity())
                .saturating_add(item.probe.container.capacity())
                .saturating_add(item.probe.video.capacity())
                .saturating_add(item.probe.hdr.capacity())
                .saturating_add(item.probe.audio.capacity())
                .saturating_add(item.probe.audio_streams.capacity())
                .saturating_add(optional_string_bytes(&item.dlna_pn))
                .saturating_add(optional_string_bytes(&item.ref_id))
                .saturating_add(optional_string_bytes(&item.duration))
                .saturating_add(optional_string_bytes(&item.resolution))
                .saturating_add(optional_string_bytes(&item.creator))
                .saturating_add(optional_string_bytes(&item.about))
                .saturating_add(optional_string_bytes(&item.plot))
                .saturating_add(optional_string_bytes(&item.artist))
                .saturating_add(optional_string_bytes(&item.album_artist))
                .saturating_add(optional_string_bytes(&item.composer))
                .saturating_add(optional_string_bytes(&item.contributor))
                .saturating_add(optional_string_bytes(&item.album))
                .saturating_add(optional_string_bytes(&item.genre))
                .saturating_add(
                    item.captions
                        .len()
                        .saturating_mul(std::mem::size_of::<Caption>()),
                );
            for caption in &item.captions {
                bytes = bytes
                    .saturating_add(caption.path.as_os_str().as_encoded_bytes().len())
                    .saturating_add(caption.ext.capacity());
            }
        }
        for (key, container) in &self.containers {
            bytes = bytes
                .saturating_add(key.capacity())
                .saturating_add(container.object_id.capacity())
                .saturating_add(container.parent_id.capacity())
                .saturating_add(container.title.capacity())
                .saturating_add(container.class.capacity())
                .saturating_add(
                    container
                        .children
                        .iter()
                        .map(|child| child.capacity())
                        .sum::<usize>(),
                );
        }
        u64::try_from(bytes).unwrap_or(u64::MAX)
    }

    pub fn children_of(&self, id: &str) -> Option<Vec<CatalogChild>> {
        self.page_children(id, 0, usize::MAX).map(|(ch, _)| ch)
    }

    /// Sorted children, cloning only `[start, start+take)`. Folders first,
    /// then title (ASCII case-insensitive) so VLC shows expand controls
    /// above loose files.
    pub fn page_children(
        &self,
        id: &str,
        start: usize,
        take: usize,
    ) -> Option<(Vec<CatalogChild>, u32)> {
        if let Some(root) = recent_root(id) {
            let mut all = self.recent_items(root);
            let total = catalog_count(all.len());
            if start >= all.len() || take == 0 {
                return Some((Vec::new(), total));
            }
            let end = all.len().min(start.saturating_add(take));
            let page = all.drain(start..end).collect();
            return Some((page, total));
        }
        let c = self.containers.get(id)?;
        let mut keys: Vec<(bool, &str, &str)> = Vec::with_capacity(c.children.len());
        for ch in &c.children {
            if let Some(cont) = self.containers.get(ch) {
                keys.push((true, cont.title.as_str(), ch.as_str()));
            } else if let Some(it) = self.items.get(ch) {
                keys.push((false, it.title.as_str(), ch.as_str()));
            }
        }
        keys.sort_by(|a, b| match (a.0, b.0) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => cmp_ignore_ascii_case(a.1, b.1),
        });
        let total = catalog_count(keys.len());
        let page = keys
            .into_iter()
            .skip(start)
            .take(take)
            .filter_map(|(_, _, oid)| {
                if let Some(cont) = self.containers.get(oid) {
                    Some(CatalogChild::Container(cont.clone()))
                } else {
                    self.items
                        .get(oid)
                        .cloned()
                        .map(Box::new)
                        .map(CatalogChild::Item)
                }
            })
            .collect();
        Some((page, total))
    }

    pub fn displayed_child_count(&self, id: &str) -> u32 {
        if recent_root(id).is_some() {
            if id == VIDEO_RECENT_ID && !self.recent_ids.is_empty() {
                return u32::try_from(self.recent_ids.len()).unwrap_or(u32::MAX);
            }
            let class_pat = match id {
                MUSIC_RECENT_ID => "audio",
                IMAGE_RECENT_ID => "image",
                _ => "video",
            };
            let mut seen = HashSet::new();
            for item in self.items.values().filter(|item| {
                item.class.contains(class_pat)
                    && item.ref_id.is_none()
                    && item.object_id.starts_with(BROWSEDIR_ID)
                    && self
                        .recent_cutoff_unix
                        .map(|cutoff| normalized_mtime_seconds(item.mtime) >= cutoff)
                        .unwrap_or(true)
            }) {
                let key = physical_identity_key(item);
                seen.insert(key);
                if seen.len() == self.recent_limit {
                    break;
                }
            }
            return u32::try_from(seen.len()).unwrap_or(u32::MAX);
        }
        self.containers
            .get(id)
            .map(|c| catalog_count(c.children.len()))
            .unwrap_or(0)
    }

    pub fn displayed_container_count(&self, id: &str) -> u32 {
        if recent_root(id).is_some() {
            return 0;
        }
        self.containers
            .get(id)
            .map(|c| {
                c.children
                    .iter()
                    .filter(|ch| self.containers.contains_key(*ch))
                    .count()
            })
            .map(catalog_count)
            .unwrap_or(0)
    }

    /// Newest unique videos (inode-deduped so symlink aliases count once),
    /// newest first, up to `RECENT_MAX`. Object IDs are `2$FF0$` + source id.
    pub fn recent_videos(&self) -> Vec<CatalogChild> {
        self.recent_items(VIDEO_RECENT_ID)
    }

    pub fn recent_items(&self, root: &str) -> Vec<CatalogChild> {
        if root == VIDEO_RECENT_ID && !self.recent_ids.is_empty() {
            return self
                .recent_ids
                .iter()
                .filter_map(|id| {
                    let it = self.items.get(id)?;
                    let mut clone = it.clone();
                    clone.object_id = format!("{root}${id}");
                    clone.parent_id = root.to_string();
                    Some(CatalogChild::Item(Box::new(clone)))
                })
                .collect();
        }
        self.collect_recent_item_ids(root, self.recent_limit)
            .into_iter()
            .filter_map(|id| {
                let it = self.items.get(&id)?;
                let mut clone = it.clone();
                clone.object_id = format!("{root}${id}");
                clone.parent_id = root.to_string();
                Some(CatalogChild::Item(Box::new(clone)))
            })
            .collect()
    }

    fn recent_cache_limit(&self) -> usize {
        self.recent_limit
            .saturating_mul(2)
            .min(self.recent_limit.saturating_add(1_000))
    }

    fn recent_item_is_eligible(&self, item: &MediaItem, class_pat: &str) -> bool {
        item.class.contains(class_pat)
            && item.ref_id.is_none()
            && item.object_id.starts_with(BROWSEDIR_ID)
            && self
                .recent_cutoff_unix
                .map(|cutoff| normalized_mtime_seconds(item.mtime) >= cutoff)
                .unwrap_or(true)
    }

    fn collect_recent_item_ids(&self, root: &str, limit: usize) -> Vec<String> {
        let class_pat = match root {
            MUSIC_RECENT_ID => "audio",
            IMAGE_RECENT_ID => "image",
            _ => "video",
        };
        let mut items: Vec<&MediaItem> = self
            .items
            .values()
            .filter(|item| self.recent_item_is_eligible(item, class_pat))
            .collect();
        items.sort_by(|a, b| {
            normalized_mtime_seconds(b.mtime)
                .cmp(&normalized_mtime_seconds(a.mtime))
                .then_with(|| b.mtime.cmp(&a.mtime))
                .then_with(|| path_is_symlink(&a.path).cmp(&path_is_symlink(&b.path)))
                .then_with(|| a.title.cmp(&b.title))
                .then_with(|| a.object_id.cmp(&b.object_id))
        });
        let mut seen: HashMap<(u64, u64), ()> = HashMap::new();
        let mut ids = Vec::new();
        for it in items {
            let key = physical_identity_key(it);
            if seen.contains_key(&key) {
                continue;
            }
            seen.insert(key, ());
            ids.push(it.object_id.clone());
            if ids.len() == limit {
                break;
            }
        }
        ids
    }

    fn rebuild_recent_index(&mut self) {
        self.recent_candidate_ids =
            self.collect_recent_item_ids(VIDEO_RECENT_ID, self.recent_cache_limit());
        self.recent_ids = self
            .recent_candidate_ids
            .iter()
            .take(self.recent_limit)
            .cloned()
            .collect();
        self.recent_count = catalog_count(self.recent_ids.len());
    }

    fn refresh_recent_index(&mut self, changed_ids: &[String], cache_was_saturated: bool) {
        let mut candidates = std::mem::take(&mut self.recent_candidate_ids);
        candidates.extend(changed_ids.iter().cloned());
        candidates.sort();
        candidates.dedup();
        candidates.retain(|id| {
            self.items
                .get(id)
                .is_some_and(|item| self.recent_item_is_eligible(item, "video"))
        });
        candidates.sort_by(|left, right| {
            let left = &self.items[left];
            let right = &self.items[right];
            normalized_mtime_seconds(right.mtime)
                .cmp(&normalized_mtime_seconds(left.mtime))
                .then_with(|| right.mtime.cmp(&left.mtime))
                .then_with(|| path_is_symlink(&left.path).cmp(&path_is_symlink(&right.path)))
                .then_with(|| left.title.cmp(&right.title))
                .then_with(|| left.object_id.cmp(&right.object_id))
        });
        let mut physical = HashSet::new();
        candidates.retain(|id| {
            let item = &self.items[id];
            let key = physical_identity_key(item);
            physical.insert(key)
        });
        candidates.truncate(self.recent_cache_limit());
        if cache_was_saturated && candidates.len() < self.recent_limit {
            self.rebuild_recent_index();
            return;
        }
        self.recent_ids = candidates.iter().take(self.recent_limit).cloned().collect();
        self.recent_count = catalog_count(self.recent_ids.len());
        self.recent_candidate_ids = candidates;
    }

    pub fn configure_recent_policy(&mut self, limit: usize, days: Option<u32>) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        self.configure_recent_policy_at(limit, days, now);
    }

    pub fn configure_recent_policy_at(&mut self, limit: usize, days: Option<u32>, now: i64) {
        self.recent_limit = limit.max(1);
        self.recent_cutoff_unix =
            days.map(|days| now.saturating_sub(i64::from(days).saturating_mul(24 * 60 * 60)));
        self.recent_ids.clear();
        self.recent_candidate_ids.clear();
        self.recent_count = 0;
        self.rebuild_recent_index();
    }

    pub fn metadata(&self, id: &str) -> Option<CatalogChild> {
        let prefix = format!("{VIDEO_RECENT_ID}$");
        if let Some(real) = id.strip_prefix(&prefix) {
            if !real.is_empty() {
                return self.metadata(real).map(|ch| match ch {
                    CatalogChild::Item(mut it) => {
                        it.object_id = id.to_string();
                        it.parent_id = VIDEO_RECENT_ID.to_string();
                        CatalogChild::Item(it)
                    }
                    other => other,
                });
            }
        }
        if let Some(c) = self.containers.get(id) {
            return Some(CatalogChild::Container(c.clone()));
        }
        if let Some(it) = self.items.get(id) {
            return Some(CatalogChild::Item(Box::new(it.clone())));
        }
        // Infuse / libupnp caches ObjectID. After a rebuild the Browse
        // Folders id may have changed; All Video is `2$8$` + detail hex
        // and some clients send the bare DETAILS.ID.
        self.metadata_by_detail(id)
    }

    fn metadata_by_detail(&self, id: &str) -> Option<CatalogChild> {
        let did = if let Some(hex) = id
            .strip_prefix(VIDEO_ALL_ID)
            .and_then(|s| s.strip_prefix('$'))
            .filter(|s| !s.is_empty() && !s.contains('$'))
        {
            i64::from_str_radix(hex, 16).ok()?
        } else if id.bytes().all(|b| b.is_ascii_digit()) {
            let n: i64 = id.parse().ok()?;
            // `0`/`1`/`2`/`3`/`64` are virtual containers, never DETAILS.ID.
            if matches!(n, 0 | 1 | 2 | 3 | 64) {
                return None;
            }
            n
        } else {
            return None;
        };
        let it = self.get_item_by_detail(did)?.clone();
        let mut it = it;
        if id.starts_with(VIDEO_ALL_ID) {
            it.object_id = id.to_string();
            it.parent_id = VIDEO_ALL_ID.to_string();
        }
        Some(CatalogChild::Item(Box::new(it)))
    }

    /// Mirror Browse Folders video files into `2$15` so Video/Folders works
    /// even when the last `files.db` predates this view.
    pub fn ensure_video_folder_mirrors(&mut self) {
        if !self.containers.contains_key(VIDEO_DIR_ID) {
            self.add_container(
                VIDEO_DIR_ID,
                VIDEO_ID,
                "Folders",
                "container.storageFolder",
                true,
            );
            self.link_child(VIDEO_ID, VIDEO_DIR_ID);
        }
        if !self.containers.contains_key(VIDEO_RECENT_ID) {
            self.add_container(
                VIDEO_RECENT_ID,
                VIDEO_ID,
                "Recently Added",
                "container.storageFolder",
                false,
            );
            self.link_child(VIDEO_ID, VIDEO_RECENT_ID);
        }
        if !self.containers.contains_key(VIDEO_SERIES_ID) {
            self.add_container(
                VIDEO_SERIES_ID,
                VIDEO_ID,
                "Series",
                "container.storageFolder",
                true,
            );
            self.link_child(VIDEO_ID, VIDEO_SERIES_ID);
        }
        if !self.containers.contains_key(VIDEO_GENRE_ID) {
            self.add_container(
                VIDEO_GENRE_ID,
                VIDEO_ID,
                "Genre",
                "container.storageFolder",
                true,
            );
            self.link_child(VIDEO_ID, VIDEO_GENRE_ID);
        }
        let videos: Vec<MediaItem> = self
            .items
            .values()
            .filter(|i| i.class.contains("video") && i.object_id.starts_with(BROWSEDIR_ID))
            .cloned()
            .collect();
        for it in videos {
            self.mirror_video_dir_ancestors(&it.parent_id);
            let vobj = browse_to_typed_dir(&it.object_id, VIDEO_DIR_ID);
            let vparent = browse_to_typed_dir(&it.parent_id, VIDEO_DIR_ID);
            if self.items.contains_key(&vobj) {
                continue;
            }
            let mut clone = it.clone();
            clone.object_id = vobj.clone();
            clone.parent_id = vparent.clone();
            clone.ref_id = Some(it.object_id.clone());
            self.link_child(&vparent, &vobj);
            self.items.insert(vobj, clone);
        }
        self.rebuild_recent_index();
    }

    fn mirror_video_dir_ancestors(&mut self, browse_folder_id: &str) {
        let mut chain = Vec::new();
        let mut cur = browse_folder_id.to_string();
        while cur != BROWSEDIR_ID && cur != ROOT_ID {
            chain.push(cur.clone());
            match self.containers.get(&cur) {
                Some(c) => cur = c.parent_id.clone(),
                None => break,
            }
        }
        chain.reverse();
        for bid in chain {
            let Some(cont) = self.containers.get(&bid).cloned() else {
                continue;
            };
            let vid = browse_to_typed_dir(&bid, VIDEO_DIR_ID);
            let vparent = browse_to_typed_dir(&cont.parent_id, VIDEO_DIR_ID);
            if !self.containers.contains_key(&vid) {
                self.add_container(&vid, &vparent, &cont.title, "container.storageFolder", true);
            }
            self.link_child(&vparent, &vid);
        }
    }
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod arithmetic_tests {
    use super::catalog_count;

    #[test]
    fn catalog_counts_saturate_at_upnp_ui4() {
        assert_eq!(catalog_count(0), 0);
        assert_eq!(
            catalog_count(usize::MAX),
            u32::try_from(usize::MAX).unwrap_or(u32::MAX)
        );
        if let Ok(max_ui4) = usize::try_from(u32::MAX) {
            assert_eq!(catalog_count(max_ui4), u32::MAX);
            if let Some(over_ui4) = max_ui4.checked_add(1) {
                assert_eq!(catalog_count(over_ui4), u32::MAX);
            }
        }
    }
}

#[allow(missing_docs)]
#[derive(Clone, Debug)]
pub enum CatalogChild {
    Container(Container),
    Item(Box<MediaItem>),
}
