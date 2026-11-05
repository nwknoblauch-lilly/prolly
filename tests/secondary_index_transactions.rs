use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use prolly::{
    BatchOp, Config, Error, IndexedMapUpdate, IndexedStore, ManifestStore, ManifestUpdate,
    MemStore, Mutation, MutationBudget, Prolly, RootCondition, RootManifest, RootWrite,
    SecondaryIndex, SecondaryIndexRegistry, Store, TransactionNodeWrite, TransactionUpdate,
    TransactionalStore,
};

#[derive(Debug)]
struct AuditStoreError;

impl fmt::Display for AuditStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("secondary-index transaction audit store failure")
    }
}

impl std::error::Error for AuditStoreError {}

struct TransactionAuditStore {
    inner: MemStore,
    node_reads: Mutex<BTreeMap<Vec<u8>, usize>>,
    transactions_enabled: AtomicBool,
    commits: AtomicUsize,
    intercept_reads: AtomicBool,
    intercepted_reads: AtomicUsize,
    second_read_reached: Barrier,
    release_second_read: Barrier,
}

impl TransactionAuditStore {
    fn new(transactions_enabled: bool) -> Self {
        Self {
            inner: MemStore::new(),
            node_reads: Mutex::new(BTreeMap::new()),
            transactions_enabled: AtomicBool::new(transactions_enabled),
            commits: AtomicUsize::new(0),
            intercept_reads: AtomicBool::new(false),
            intercepted_reads: AtomicUsize::new(0),
            second_read_reached: Barrier::new(2),
            release_second_read: Barrier::new(2),
        }
    }

    fn intercept_second_root_read(&self) {
        self.intercepted_reads.store(0, Ordering::SeqCst);
        self.intercept_reads.store(true, Ordering::SeqCst);
    }
}

impl Store for TransactionAuditStore {
    type Error = AuditStoreError;

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        *self
            .node_reads
            .lock()
            .unwrap()
            .entry(key.to_vec())
            .or_default() += 1;
        self.inner.get(key).map_err(|_| AuditStoreError)
    }

    fn put(&self, key: &[u8], value: &[u8]) -> Result<(), Self::Error> {
        self.inner.put(key, value).map_err(|_| AuditStoreError)
    }

    fn delete(&self, key: &[u8]) -> Result<(), Self::Error> {
        self.inner.delete(key).map_err(|_| AuditStoreError)
    }

    fn batch(&self, ops: &[BatchOp<'_>]) -> Result<(), Self::Error> {
        self.inner.batch(ops).map_err(|_| AuditStoreError)
    }
}

impl ManifestStore for TransactionAuditStore {
    type Error = AuditStoreError;

    fn get_root(&self, name: &[u8]) -> Result<Option<RootManifest>, Self::Error> {
        if self.intercept_reads.load(Ordering::SeqCst)
            && self.intercepted_reads.fetch_add(1, Ordering::SeqCst) == 1
        {
            self.second_read_reached.wait();
            self.release_second_read.wait();
        }
        ManifestStore::get_root(&self.inner, name).map_err(|_| AuditStoreError)
    }

    fn put_root(&self, name: &[u8], manifest: &RootManifest) -> Result<(), Self::Error> {
        ManifestStore::put_root(&self.inner, name, manifest).map_err(|_| AuditStoreError)
    }

    fn delete_root(&self, name: &[u8]) -> Result<(), Self::Error> {
        ManifestStore::delete_root(&self.inner, name).map_err(|_| AuditStoreError)
    }

    fn compare_and_swap_root(
        &self,
        name: &[u8],
        expected: Option<&RootManifest>,
        new: Option<&RootManifest>,
    ) -> Result<ManifestUpdate, Self::Error> {
        ManifestStore::compare_and_swap_root(&self.inner, name, expected, new)
            .map_err(|_| AuditStoreError)
    }
}

impl TransactionalStore for TransactionAuditStore {
    fn supports_transactions(&self) -> bool {
        self.transactions_enabled.load(Ordering::SeqCst)
    }

    fn commit_transaction(
        &self,
        node_writes: &[TransactionNodeWrite],
        root_conditions: &[RootCondition],
        root_writes: &[RootWrite],
    ) -> Result<TransactionUpdate, Error> {
        self.commits.fetch_add(1, Ordering::SeqCst);
        TransactionalStore::commit_transaction(
            &self.inner,
            node_writes,
            root_conditions,
            root_writes,
        )
    }
}

impl IndexedStore for TransactionAuditStore {}

#[test]
fn indexed_map_rejects_a_store_without_strict_transactions() {
    let engine = Prolly::new(TransactionAuditStore::new(false), Config::default());
    assert!(matches!(
        engine.indexed_map(b"users", SecondaryIndexRegistry::new()),
        Err(Error::UnsupportedTransactions { .. })
    ));
}

#[test]
fn indexed_publication_commits_through_the_transaction_store() {
    let store = Arc::new(TransactionAuditStore::new(true));
    let engine = Prolly::new(store.clone(), Config::default());
    let indexed = engine
        .indexed_map(b"users", SecondaryIndexRegistry::new())
        .unwrap();
    let commits_after_initialization = store.commits.load(Ordering::SeqCst);
    assert!(commits_after_initialization > 0);

    indexed.put(b"user-1", b"active").unwrap();
    assert!(store.commits.load(Ordering::SeqCst) > commits_after_initialization);
}

#[test]
fn retained_old_values_obey_the_mutation_memory_budget() {
    let engine = Prolly::new(TransactionAuditStore::new(true), Config::default());
    let indexed = engine
        .indexed_map(b"users", SecondaryIndexRegistry::new())
        .unwrap();
    indexed.put(b"a", vec![0; 16]).unwrap();
    let before = indexed.put(b"b", vec![0; 16]).unwrap();
    let budget = MutationBudget {
        max_accounted_memory_bytes: 24,
        ..MutationBudget::default()
    };
    assert!(matches!(
        indexed.apply_with_budget(
            vec![
                Mutation::Delete { key: b"a".to_vec() },
                Mutation::Delete { key: b"b".to_vec() },
            ],
            &budget,
        ),
        Err(Error::IndexResourceLimitExceeded {
            resource: "mutation_accounted_memory_bytes",
            limit: 24,
            actual: 32,
        })
    ));
    assert_eq!(
        indexed.health().unwrap().state_version,
        Some(before.state.id)
    );
    assert_eq!(indexed.get(b"a").unwrap(), Some(vec![0; 16]));
    assert_eq!(indexed.get(b"b").unwrap(), Some(vec![0; 16]));
}

#[test]
fn old_source_reads_do_not_grow_with_active_index_count() {
    fn source_reads(index_count: usize) -> usize {
        let store = Arc::new(TransactionAuditStore::new(true));
        // Disable cache hits so they cannot hide repeated source traversals.
        let config = Config::builder()
            .min_chunk_size(64)
            .max_chunk_size(64)
            .node_cache_max_nodes(0)
            .build();
        let engine = Prolly::new(store.clone(), config);
        let mut registry = SecondaryIndexRegistry::new();
        let names = (0..index_count)
            .map(|index| format!("by-value-{index}"))
            .collect::<Vec<_>>();
        for name in &names {
            registry = registry
                .register(
                    SecondaryIndex::non_unique(name.as_bytes(), 1, name, |_, value| {
                        Ok(vec![value.to_vec()])
                    })
                    .unwrap(),
                )
                .unwrap();
        }
        let indexed = engine.indexed_map(b"users", registry).unwrap();
        let before = indexed
            .apply(
                (0..16)
                    .map(|id| Mutation::Upsert {
                        key: vec![id],
                        val: b"old".to_vec(),
                    })
                    .collect(),
            )
            .unwrap();
        for name in &names {
            indexed.ensure_index(name).unwrap();
        }
        let old_root = before.source.tree.root.as_ref().unwrap();
        store.node_reads.lock().unwrap().clear();
        let mut mutations = vec![Mutation::Upsert {
            key: vec![0],
            val: b"superseded".to_vec(),
        }];
        mutations.extend((0..=16).map(|id| {
            if id % 2 == 0 {
                Mutation::Upsert {
                    key: vec![id],
                    val: b"new".to_vec(),
                }
            } else {
                Mutation::Delete { key: vec![id] }
            }
        }));
        mutations.push(Mutation::Delete { key: vec![99] });
        let after = indexed.apply(mutations).unwrap();
        let reads = store.node_reads.lock().unwrap()[old_root.as_bytes()];
        assert!(reads > 0);
        assert_eq!(engine.len(&after.source.tree).unwrap(), 9);
        assert_eq!(engine.len(&before.source.tree).unwrap(), 16);
        let snapshot = indexed.snapshot().unwrap();
        let expected = (0..=16).step_by(2).map(|id| vec![id]).collect::<Vec<_>>();
        for name in &names {
            let index = snapshot.index(name).unwrap();
            assert_eq!(index.primary_keys(b"new").unwrap(), expected);
            assert!(index.primary_keys(b"old").unwrap().is_empty());
            assert!(index.primary_keys(b"superseded").unwrap().is_empty());
        }
        for index in &after.indexes {
            assert_eq!(index.entry_count, 9);
        }
        reads
    }

    let one_index = source_reads(1);
    let four_indexes = source_reads(4);
    eprintln!("old source root reads: one index={one_index}, four indexes={four_indexes}");
    assert_eq!(one_index, four_indexes);
}

#[test]
fn apply_if_never_applies_after_the_expected_version_is_superseded() {
    let store = Arc::new(TransactionAuditStore::new(true));
    let engine = Arc::new(Prolly::new(store.clone(), Config::default()));
    let indexed = engine
        .indexed_map(b"users", SecondaryIndexRegistry::new())
        .unwrap();
    let expected = indexed.put(b"initial", b"0").unwrap().source.id;
    let conditional_ready = Arc::new(Barrier::new(2));
    let conditional_start = Arc::new(Barrier::new(2));

    let conditional_engine = engine.clone();
    let thread_ready = conditional_ready.clone();
    let thread_start = conditional_start.clone();
    let conditional = std::thread::spawn(move || {
        let conditional_indexed = conditional_engine
            .indexed_map(b"users", SecondaryIndexRegistry::new())
            .unwrap();
        thread_ready.wait();
        thread_start.wait();
        conditional_indexed.apply_if(
            Some(&expected),
            vec![Mutation::Upsert {
                key: b"conditional".to_vec(),
                val: b"1".to_vec(),
            }],
        )
    });

    conditional_ready.wait();
    store.intercept_second_root_read();
    conditional_start.wait();
    store.second_read_reached.wait();
    store.intercept_reads.store(false, Ordering::SeqCst);
    indexed.put(b"concurrent", b"2").unwrap();
    store.release_second_read.wait();

    let update = conditional.join().unwrap().unwrap();
    assert!(matches!(update, IndexedMapUpdate::Conflict { .. }));
    assert_eq!(indexed.get(b"conditional").unwrap(), None);
    assert_eq!(indexed.get(b"concurrent").unwrap(), Some(b"2".to_vec()));
}
