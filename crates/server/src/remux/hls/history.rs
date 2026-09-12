//! Append-only metadata with bounded copy-on-write tails. Views retain completed
//! chunks without copying the movie history or holding the live index lock.
use std::sync::Arc;

const CHUNK_ENTRIES: usize = 256;

#[derive(Clone, Debug)]
pub(super) struct History<T> {
    chunks: Vec<Arc<Vec<T>>>,
    length: usize,
}

impl<T> Default for History<T> {
    fn default() -> Self {
        Self {
            chunks: Vec::new(),
            length: 0,
        }
    }
}

impl<T: Clone> History<T> {
    pub(super) fn push(&mut self, item: T) {
        if let Some(chunk) = self
            .chunks
            .last_mut()
            .filter(|chunk| chunk.len() < CHUNK_ENTRIES)
        {
            Arc::make_mut(chunk).push(item);
        } else {
            let mut chunk = Vec::with_capacity(CHUNK_ENTRIES);
            chunk.push(item);
            self.chunks.push(Arc::new(chunk));
        }
        self.length += 1;
    }

    pub(super) fn len(&self) -> usize {
        self.length
    }

    pub(super) fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub(super) fn view(&self, start: usize, end: usize) -> View<T> {
        let first = start / CHUNK_ENTRIES;
        let last = end.div_ceil(CHUNK_ENTRIES);
        View {
            chunks: self.chunks[first..last].to_vec(),
            skip: start % CHUNK_ENTRIES,
            length: end - start,
        }
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.chunks.capacity() * std::mem::size_of::<Arc<Vec<T>>>()
            + self
                .chunks
                .iter()
                .map(|chunk| {
                    std::mem::size_of::<Vec<T>>()
                        + 2 * std::mem::size_of::<usize>()
                        + chunk.capacity() * std::mem::size_of::<T>()
                })
                .sum::<usize>()
    }
}

#[cfg(test)]
impl<T: Clone> FromIterator<T> for History<T> {
    fn from_iter<I: IntoIterator<Item = T>>(items: I) -> Self {
        let mut history = Self::default();
        for item in items {
            history.push(item);
        }
        history
    }
}

#[cfg(test)]
impl<T> std::ops::Index<usize> for History<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        &self.chunks[index / CHUNK_ENTRIES][index % CHUNK_ENTRIES]
    }
}

#[derive(Debug)]
pub(super) struct View<T> {
    chunks: Vec<Arc<Vec<T>>>,
    skip: usize,
    length: usize,
}

impl<T> View<T> {
    pub(super) fn len(&self) -> usize {
        self.length
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &T> {
        self.chunks
            .iter()
            .flat_map(|chunk| chunk.iter())
            .skip(self.skip)
            .take(self.length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_title_snapshot_and_deep_page_clone_only_the_appended_tail() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Counted(Arc<AtomicUsize>);
        impl Clone for Counted {
            fn clone(&self) -> Self {
                self.0.fetch_add(1, Ordering::Relaxed);
                Self(self.0.clone())
            }
        }

        for length in [7_200, 28_800] {
            let clones = Arc::new(AtomicUsize::new(0));
            let mut history: History<_> = (0..length).map(|_| Counted(clones.clone())).collect();
            let snapshot = history.view(0, length);
            let deep_page = history.view(length - 256, length);
            assert_eq!(clones.load(Ordering::Relaxed), 0);
            assert!(
                deep_page.chunks.len() <= 2,
                "a deep page must retain only intersecting chunks"
            );
            history.push(Counted(clones.clone()));
            assert_eq!(clones.load(Ordering::Relaxed), length % CHUNK_ENTRIES);
            assert_eq!(snapshot.iter().count(), length);
            assert_eq!(deep_page.iter().count(), 256);
            assert_eq!(history.len(), length + 1);
        }
    }

    #[test]
    fn views_share_sealed_chunks_and_remain_consistent_during_append() {
        let mut history: History<_> = (0..600).collect();
        let view = history.view(0, 600);
        let page = history.view(250, 510);
        for value in 600..7200 {
            history.push(value);
        }
        assert!(Arc::ptr_eq(&history.chunks[0], &view.chunks[0]));
        assert!(Arc::ptr_eq(&history.chunks[1], &view.chunks[1]));
        assert!(!Arc::ptr_eq(&history.chunks[2], &view.chunks[2]));
        assert_eq!(
            view.iter().copied().collect::<Vec<_>>(),
            (0..600).collect::<Vec<_>>()
        );
        assert_eq!(
            page.iter().copied().collect::<Vec<_>>(),
            (250..510).collect::<Vec<_>>()
        );
        assert_eq!(history.view(7200, 7200).iter().count(), 0);
    }
}
