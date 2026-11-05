use super::super::cid::Cid;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

pub(crate) const DEFAULT_PROXIMITY_CACHE_NODES: usize = 8_192;

struct Entry<T> {
    value: Arc<T>,
    bytes: usize,
    generation: u64,
}

/// Bounded, generation-LRU content cache. Separate instantiations keep object
/// types isolated even when their CIDs happen to be equal.
pub(crate) struct ContentCache<T> {
    max_nodes: usize,
    entries: HashMap<Cid, Entry<T>>,
    access_log: VecDeque<(Cid, u64)>,
    generation: u64,
}

impl<T> ContentCache<T> {
    pub(crate) fn new(max_nodes: usize) -> Self {
        Self {
            max_nodes,
            entries: HashMap::new(),
            access_log: VecDeque::new(),
            generation: 0,
        }
    }

    pub(crate) fn get(&mut self, cid: &Cid) -> Option<(Arc<T>, usize)> {
        let entry = self.entries.get_mut(cid)?;
        self.generation = self.generation.wrapping_add(1);
        entry.generation = self.generation;
        self.access_log.push_back((cid.clone(), self.generation));
        let result = (entry.value.clone(), entry.bytes);
        self.compact_access_log_if_needed();
        Some(result)
    }

    pub(crate) fn insert(&mut self, cid: Cid, value: Arc<T>, bytes: usize) {
        if self.max_nodes == 0 {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        self.entries.insert(
            cid.clone(),
            Entry {
                value,
                bytes,
                generation: self.generation,
            },
        );
        self.access_log.push_back((cid, self.generation));
        while self.entries.len() > self.max_nodes {
            let Some((candidate, generation)) = self.access_log.pop_front() else {
                break;
            };
            if self
                .entries
                .get(&candidate)
                .is_some_and(|entry| entry.generation == generation)
            {
                self.entries.remove(&candidate);
            }
        }
        self.compact_access_log_if_needed();
    }

    fn compact_access_log_if_needed(&mut self) {
        let max_log_len = self.entries.len().saturating_mul(4).max(64);
        if self.access_log.len() > max_log_len {
            self.access_log.retain(|(cid, generation)| {
                self.entries
                    .get(cid)
                    .is_some_and(|entry| entry.generation == *generation)
            });
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.access_log.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_content_cache_bounds_recency_for_hits_and_replacements() {
        for replace in [false, true] {
            let mut cache = ContentCache::new(2);
            let one = Cid::from_bytes(b"one");
            let two = Cid::from_bytes(b"two");
            let three = Cid::from_bytes(b"three");
            cache.insert(one.clone(), Arc::new(1u8), 1);
            cache.insert(two.clone(), Arc::new(2u8), 2);
            for _ in 0..1_024 {
                if replace {
                    cache.insert(one.clone(), Arc::new(3u8), 3);
                } else {
                    assert_eq!(cache.get(&one), Some((Arc::new(1u8), 1)));
                }
                assert!(cache.access_log.len() <= 64);
                assert_eq!(cache.entries.len(), 2);
            }
            cache.insert(three.clone(), Arc::new(4u8), 4);
            assert!(cache.get(&two).is_none());
            let expected = if replace { 3 } else { 1 };
            assert_eq!(
                cache.get(&one),
                Some((Arc::new(expected), expected as usize))
            );
            assert_eq!(cache.get(&three), Some((Arc::new(4u8), 4)));
        }
    }

    #[test]
    fn typed_content_cache_is_bounded_and_refreshes_recent_entries() {
        let mut cache = ContentCache::new(2);
        let one = Cid::from_bytes(b"one");
        let two = Cid::from_bytes(b"two");
        let three = Cid::from_bytes(b"three");
        cache.insert(one.clone(), Arc::new(1u8), 1);
        cache.insert(two.clone(), Arc::new(2u8), 1);
        assert_eq!(*cache.get(&one).unwrap().0, 1);
        cache.insert(three.clone(), Arc::new(3u8), 1);
        assert!(cache.get(&two).is_none());
        assert!(cache.get(&one).is_some());
        assert!(cache.get(&three).is_some());
        cache.clear();
        assert!(cache.get(&one).is_none());
        assert!(cache.get(&three).is_none());
    }
}
