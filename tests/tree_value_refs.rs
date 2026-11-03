mod common;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use prolly::{
    walk_content_graph, Cid, Config, ContentGraphLimits, ContentObjectKind, Error,
    LargeValueConfig, MemBlobStore, MemStore, Mutation, Node, NodeLayoutSpec, Prolly,
    RuntimeConfig, Store, Tree, TreeFormat, TreeRef, TypedContentRoot, ValueRef, ValueRefView,
};

fn small_config() -> Config {
    Config::builder()
        .min_chunk_size(2)
        .max_chunk_size(4)
        .chunking_factor(2)
        .build()
}

fn hash_of(value: &TreeRef) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn build_tree<S: Store>(prolly: &Prolly<S>, prefix: &str, count: usize) -> Tree {
    let mut tree = prolly.create();
    for index in 0..count {
        tree = prolly
            .put(
                &tree,
                format!("{prefix}/{index:04}").into_bytes(),
                format!("value-{prefix}-{index}").into_bytes(),
            )
            .unwrap();
    }
    tree
}

fn tree_envelope(root: Option<&Cid>, format: &[u8]) -> Vec<u8> {
    let mut out = b"PLVB".to_vec();
    out.push(1);
    out.push(2);
    match root {
        Some(root) => {
            out.push(1);
            out.extend_from_slice(root.as_bytes());
        }
        None => out.push(0),
    }
    out.extend_from_slice(&(format.len() as u64).to_be_bytes());
    out.extend_from_slice(format);
    out
}

#[test]
fn tree_refs_round_trip_and_ignore_runtime_settings() {
    let prolly = Prolly::new(Arc::new(MemStore::new()), small_config());
    let child = build_tree(&prolly, "child", 12);
    let empty = prolly.create();

    for tree in [&child, &empty] {
        let reference = TreeRef::try_from(tree).unwrap();
        assert_eq!(reference.root(), tree.root.as_ref());
        assert_eq!(reference.format(), &tree.config.format);

        let bytes = ValueRef::Tree(reference.clone()).to_bytes();
        assert_eq!(
            ValueRef::from_bytes(&bytes).unwrap(),
            ValueRef::Tree(reference.clone())
        );
        assert_eq!(
            ValueRef::from_stored_bytes(&bytes).unwrap(),
            ValueRef::Tree(reference.clone())
        );

        let mut other_runtime = tree.clone();
        other_runtime.config.runtime = RuntimeConfig {
            node_cache_max_nodes: Some(3),
            node_cache_max_bytes: None,
            read_parallelism: 1,
        };
        let other = TreeRef::try_from(&other_runtime).unwrap();
        assert_eq!(other, reference);
        assert_eq!(hash_of(&other), hash_of(&reference));
        assert_eq!(ValueRef::Tree(other).to_bytes(), bytes);

        let reopened = reference.to_tree(RuntimeConfig::default());
        assert_eq!(reopened.root, tree.root);
        assert_eq!(reopened.config.format, tree.config.format);
    }
}

#[test]
fn tree_ref_bytes_match_the_documented_layout() {
    let format = TreeFormat::default();
    let format_bytes = format.canonical_bytes().unwrap();
    let root = Cid::from_bytes(b"child-root");

    let present = TreeRef::new(Some(root.clone()), format.clone()).unwrap();
    assert_eq!(
        ValueRef::Tree(present).to_bytes(),
        tree_envelope(Some(&root), &format_bytes)
    );
    let empty = TreeRef::new(None, format).unwrap();
    assert_eq!(
        ValueRef::Tree(empty).to_bytes(),
        tree_envelope(None, &format_bytes)
    );
}

#[test]
fn malformed_tree_envelopes_are_rejected() {
    let format_bytes = TreeFormat::default().canonical_bytes().unwrap();
    let root = Cid::from_bytes(b"child-root");
    let valid = tree_envelope(Some(&root), &format_bytes);

    let mut cases: Vec<Vec<u8>> = (6..valid.len()).map(|len| valid[..len].to_vec()).collect();
    let mut bad_flag = valid.clone();
    bad_flag[6] = 2;
    cases.push(bad_flag);
    let mut trailing = valid.clone();
    trailing.push(0);
    cases.push(trailing);
    let mut huge_len = tree_envelope(None, &[]);
    huge_len.truncate(7);
    huge_len.extend_from_slice(&u64::MAX.to_be_bytes());
    cases.push(huge_len);
    let mut bad_format = format_bytes.clone();
    *bad_format.last_mut().unwrap() = 0xff;
    cases.push(tree_envelope(None, &bad_format));
    let mut unknown_tag = valid.clone();
    unknown_tag[5] = 3;
    cases.push(unknown_tag);

    let prolly = Prolly::new(Arc::new(MemStore::new()), Config::default());
    for (index, bytes) in cases.into_iter().enumerate() {
        assert!(
            matches!(ValueRef::from_bytes(&bytes), Err(Error::Deserialize(_))),
            "owned decoder accepted case {index}"
        );
        let tree = prolly
            .put(&prolly.create(), b"k".to_vec(), bytes.clone())
            .unwrap();
        assert!(
            prolly
                .get_value_ref_with(&tree, b"k", |view| view.to_owned())
                .is_err(),
            "borrowed decoder accepted case {index}"
        );
    }
}

#[test]
fn reads_preserve_raw_bytes_and_byte_resolvers_reject_tree_refs() {
    let store = Arc::new(MemStore::new());
    let prolly = Prolly::new(store, small_config());
    let child = build_tree(&prolly, "child", 6);
    let reference = TreeRef::try_from(&child).unwrap();
    let envelope = ValueRef::Tree(reference.clone()).to_bytes();
    let parent = prolly
        .put(&prolly.create(), b"child".to_vec(), envelope.clone())
        .unwrap();

    assert_eq!(prolly.get(&parent, b"child").unwrap(), Some(envelope));
    assert_eq!(
        prolly.get_value_ref(&parent, b"child").unwrap(),
        Some(ValueRef::Tree(reference.clone()))
    );
    assert_eq!(
        prolly
            .get_value_ref_with(&parent, b"child", |view| match view {
                ValueRefView::Tree(found) => Some(found),
                _ => None,
            })
            .unwrap(),
        Some(Some(reference))
    );
    assert!(matches!(
        prolly.get_large_value(&MemBlobStore::new(), &parent, b"child"),
        Err(Error::Deserialize(_))
    ));
}

fn tree_value(tree: &Tree) -> Vec<u8> {
    ValueRef::Tree(TreeRef::try_from(tree).unwrap()).to_bytes()
}

fn plain_config() -> Config {
    Config::builder()
        .min_chunk_size(2)
        .max_chunk_size(4)
        .chunking_factor(2)
        .node_layout(NodeLayoutSpec::Plain)
        .build()
}

#[test]
fn marking_retains_nested_trees_once_and_skips_empty_refs() {
    let store = Arc::new(MemStore::new());
    let prolly = Prolly::new(store.clone(), small_config());
    let grandchild = build_tree(&prolly, "grandchild", 20);
    let child = prolly
        .put(
            &build_tree(&prolly, "child", 20),
            b"nested".to_vec(),
            tree_value(&grandchild),
        )
        .unwrap();
    let parent = prolly
        .put(&prolly.create(), b"a".to_vec(), tree_value(&child))
        .unwrap();
    let parent = prolly
        .put(&parent, b"b".to_vec(), tree_value(&child))
        .unwrap();
    let parent = prolly
        .put(&parent, b"empty".to_vec(), tree_value(&prolly.create()))
        .unwrap();

    let child_live = prolly.mark_reachable(std::slice::from_ref(&child)).unwrap();
    let grandchild_live = prolly
        .mark_reachable(std::slice::from_ref(&grandchild))
        .unwrap();
    let reachable = prolly
        .mark_reachable(std::slice::from_ref(&parent))
        .unwrap();
    for cid in child_live.cids().iter().chain(grandchild_live.cids()) {
        assert!(reachable.contains(cid));
    }
    let mut parent_only = 0;
    for cid in reachable.cids() {
        if !child_live.contains(cid) {
            parent_only += 1;
        }
    }
    assert!(parent_only >= 1);
    assert_eq!(
        reachable.live_nodes,
        parent_only + child_live.live_nodes,
        "shared child counted once"
    );
}

#[test]
fn marking_checks_every_declared_format() {
    let store = Arc::new(MemStore::new());
    let prolly = Prolly::new(store.clone(), small_config());
    let plain = Prolly::new(store.clone(), plain_config());
    let child = build_tree(&plain, "plain-child", 10);
    assert_ne!(child.config.format, TreeFormat::default());

    let good = prolly
        .put(&prolly.create(), b"a".to_vec(), tree_value(&child))
        .unwrap();
    let reachable = prolly.mark_reachable(std::slice::from_ref(&good)).unwrap();
    assert!(reachable.contains(child.root.as_ref().unwrap()));

    let wrong = TreeRef::new(child.root.clone(), small_config().format).unwrap();
    let bad = prolly
        .put(&good, b"b".to_vec(), ValueRef::Tree(wrong).to_bytes())
        .unwrap();
    assert!(matches!(
        prolly.mark_reachable(std::slice::from_ref(&bad)),
        Err(Error::FormatMismatch { .. })
    ));
    assert!(prolly
        .mark_reachable_blobs(std::slice::from_ref(&bad))
        .is_err());
}

#[test]
fn typed_walk_follows_tree_refs_with_format_and_depth_limits() {
    let store = Arc::new(MemStore::new());
    let prolly = Prolly::new(store.clone(), Config::default());
    let mut chain = prolly
        .put(&prolly.create(), b"k".to_vec(), b"leaf".to_vec())
        .unwrap();
    for _ in 0..5 {
        chain = prolly
            .put(&prolly.create(), b"k".to_vec(), tree_value(&chain))
            .unwrap();
    }
    let root = [TypedContentRoot::new(
        ContentObjectKind::OrderedNode,
        chain.root.clone().unwrap(),
    )];
    let walk = walk_content_graph(&*store, &root, &ContentGraphLimits::default()).unwrap();
    assert_eq!(walk.objects.len(), 6);
    let shallow = ContentGraphLimits {
        max_depth: 3,
        ..ContentGraphLimits::default()
    };
    assert!(matches!(
        walk_content_graph(&*store, &root, &shallow),
        Err(Error::ContentGraphResourceLimitExceeded { .. })
    ));

    let plain = Prolly::new(store.clone(), plain_config());
    let child = build_tree(&plain, "plain-child", 4);
    let wrong = TreeRef::new(child.root.clone(), TreeFormat::default()).unwrap();
    let parent = prolly
        .put(&prolly.create(), b"a".to_vec(), tree_value(&child))
        .unwrap();
    let parent = prolly
        .put(&parent, b"b".to_vec(), ValueRef::Tree(wrong).to_bytes())
        .unwrap();
    let root = [TypedContentRoot::new(
        ContentObjectKind::OrderedNode,
        parent.root.clone().unwrap(),
    )];
    assert!(walk_content_graph(&*store, &root, &ContentGraphLimits::default()).is_err());
}

#[test]
fn blob_marking_follows_child_trees_and_keeps_payloads_opaque() {
    let store = Arc::new(MemStore::new());
    let blobs = MemBlobStore::new();
    let prolly = Prolly::new(store, small_config());
    let child = prolly
        .put_large_value(
            &blobs,
            &prolly.create(),
            b"big".to_vec(),
            vec![7; 64],
            LargeValueConfig::new(8),
        )
        .unwrap();
    let missing = TreeRef::new(Some(Cid::from_bytes(b"absent")), TreeFormat::default()).unwrap();
    let escaped = ValueRef::Tree(missing).to_bytes();
    let parent = prolly
        .put_large_value(
            &blobs,
            &prolly.create(),
            b"escaped".to_vec(),
            escaped.clone(),
            LargeValueConfig::new(4096),
        )
        .unwrap();
    assert_eq!(
        prolly.get_large_value(&blobs, &parent, b"escaped").unwrap(),
        Some(escaped)
    );
    let parent = prolly
        .put(&parent, b"child".to_vec(), tree_value(&child))
        .unwrap();

    let reachable = prolly
        .mark_reachable_blobs(std::slice::from_ref(&parent))
        .unwrap();
    assert_eq!(reachable.live_blob_count, 1);
    prolly
        .mark_reachable(std::slice::from_ref(&parent))
        .unwrap();
}

#[test]
fn gc_retains_shared_children_until_the_last_reference_and_fails_closed() {
    let store = Arc::new(MemStore::new());
    let prolly = Prolly::new(store.clone(), small_config());
    let child = build_tree(&prolly, "child", 30);
    let first = prolly
        .put(&prolly.create(), b"a".to_vec(), tree_value(&child))
        .unwrap();
    let second = prolly
        .put(&prolly.create(), b"b".to_vec(), tree_value(&child))
        .unwrap();
    let unrelated = build_tree(&prolly, "other", 3);
    let candidates = prolly
        .mark_reachable(&[first.clone(), second.clone(), unrelated.clone()])
        .unwrap()
        .live_cids;

    let sweep = prolly
        .sweep_gc(std::slice::from_ref(&second), &candidates)
        .unwrap();
    assert!(sweep.deleted_nodes > 0);
    assert_eq!(
        common::entries(&prolly, &child).len(),
        30,
        "child survives through its remaining reference"
    );

    let survivors = prolly
        .mark_reachable(std::slice::from_ref(&second))
        .unwrap()
        .live_cids;
    let plan = prolly.plan_gc(&[], &survivors).unwrap();
    assert!(plan
        .reclaimable_cids()
        .contains(child.root.as_ref().unwrap()));

    let dangling = TreeRef::new(Some(Cid::from_bytes(b"absent")), small_config().format).unwrap();
    let broken = prolly
        .put(
            &second,
            b"dangling".to_vec(),
            ValueRef::Tree(dangling).to_bytes(),
        )
        .unwrap();
    let before = survivors;
    assert!(prolly
        .sweep_gc(std::slice::from_ref(&broken), &before)
        .is_err());
    for cid in &before {
        assert!(
            store.get(cid.as_bytes()).unwrap().is_some(),
            "no deletion after failed marking"
        );
    }
}

#[test]
fn parent_cardinality_and_flat_diff_ignore_child_contents() {
    let store = Arc::new(MemStore::new());
    let prolly = Prolly::new(store, small_config());
    let small_child = build_tree(&prolly, "child", 2);
    let large_child = build_tree(&prolly, "child", 200);
    let base = build_tree(&prolly, "parent", 5);
    let before = prolly
        .put(&base, b"ref".to_vec(), tree_value(&small_child))
        .unwrap();
    let after = prolly
        .put(&base, b"ref".to_vec(), tree_value(&large_child))
        .unwrap();

    assert_eq!(prolly.len(&before).unwrap(), 6);
    assert_eq!(prolly.len(&after).unwrap(), 6);
    assert_eq!(
        prolly.rank(&after, b"ref").unwrap(),
        prolly.rank(&before, b"ref").unwrap()
    );
    assert_eq!(
        prolly.select(&after, 5).unwrap().map(|entry| entry.0),
        prolly.select(&before, 5).unwrap().map(|entry| entry.0)
    );
    assert_eq!(prolly.diff(&before, &after).unwrap().len(), 1);
}

#[test]
fn versioned_catalog_verification_counts_child_trees() {
    let prolly = Prolly::new(MemStore::new(), small_config());
    let child = build_tree(&prolly, "child", 40);
    let child_nodes = prolly
        .mark_reachable(std::slice::from_ref(&child))
        .unwrap()
        .live_nodes;
    let map = prolly.versioned_map(b"with-child");
    map.apply_at_millis(
        vec![Mutation::Upsert {
            key: b"child".to_vec(),
            val: tree_value(&child),
        }],
        1_000,
    )
    .unwrap();
    let verified = map.verify_catalog().unwrap();
    assert!(verified.reachable_nodes > child_nodes);

    let dangling = TreeRef::new(Some(Cid::from_bytes(b"absent")), small_config().format).unwrap();
    let broken = prolly.versioned_map(b"dangling");
    broken
        .apply_at_millis(
            vec![Mutation::Upsert {
                key: b"child".to_vec(),
                val: ValueRef::Tree(dangling).to_bytes(),
            }],
            1_000,
        )
        .unwrap();
    assert!(broken.verify_catalog().is_err());
}

fn parent_with_nested_children<S: Store>(
    prolly: &Prolly<S>,
    plain: &Prolly<S>,
) -> (Tree, Tree, Tree) {
    let grandchild = build_tree(plain, "grandchild", 25);
    let child = prolly
        .put(
            &build_tree(prolly, "child", 25),
            b"nested".to_vec(),
            tree_value(&grandchild),
        )
        .unwrap();
    let parent = prolly
        .put(
            &build_tree(prolly, "parent", 5),
            b"child".to_vec(),
            tree_value(&child),
        )
        .unwrap();
    let parent = prolly
        .put(&parent, b"empty".to_vec(), tree_value(&prolly.create()))
        .unwrap();
    (parent, child, grandchild)
}

#[test]
fn copy_transfers_nested_closure_and_repairs_warm_destinations() {
    let source = Arc::new(MemStore::new());
    let prolly = Prolly::new(source.clone(), small_config());
    let plain = Prolly::new(source.clone(), plain_config());
    let (parent, child, grandchild) = parent_with_nested_children(&prolly, &plain);
    let required = prolly
        .mark_reachable(std::slice::from_ref(&parent))
        .unwrap();

    let empty_destination = Arc::new(MemStore::new());
    let copied = prolly
        .copy_missing_nodes(&parent, &empty_destination)
        .unwrap();
    assert_eq!(copied.copied_nodes, required.live_nodes);
    let reader = Prolly::new(empty_destination.clone(), small_config());
    assert_eq!(common::entries(&reader, &child).len(), 26);
    let plain_reader = Prolly::new(empty_destination.clone(), plain_config());
    assert_eq!(common::entries(&plain_reader, &grandchild).len(), 25);
    assert_eq!(
        prolly
            .copy_missing_nodes(&parent, &empty_destination)
            .unwrap()
            .copied_nodes,
        0
    );

    let warm = Arc::new(MemStore::new());
    let root = parent.root.as_ref().unwrap();
    warm.put(
        root.as_bytes(),
        &source.get(root.as_bytes()).unwrap().unwrap(),
    )
    .unwrap();
    let plan = prolly.plan_missing_nodes(&parent, &warm).unwrap();
    assert_eq!(plan.missing_nodes, required.live_nodes - 1);
    prolly.copy_missing_nodes(&parent, &warm).unwrap();
    let reader = Prolly::new(warm, plain_config());
    assert_eq!(common::entries(&reader, &grandchild).len(), 25);

    let dangling = TreeRef::new(Some(Cid::from_bytes(b"absent")), small_config().format).unwrap();
    let broken = prolly
        .put(
            &parent,
            b"dangling".to_vec(),
            ValueRef::Tree(dangling).to_bytes(),
        )
        .unwrap();
    let untouched = Arc::new(MemStore::new());
    assert!(prolly.copy_missing_nodes(&broken, &untouched).is_err());
    assert!(untouched.get(root.as_bytes()).unwrap().is_none());
    let broken_root = broken.root.as_ref().unwrap();
    assert!(untouched.get(broken_root.as_bytes()).unwrap().is_none());
}

#[test]
fn snapshots_include_and_verify_nested_closure() {
    let source = Arc::new(MemStore::new());
    let prolly = Prolly::new(source.clone(), small_config());
    let plain = Prolly::new(source.clone(), plain_config());
    let (parent, child, grandchild) = parent_with_nested_children(&prolly, &plain);
    let required = prolly
        .mark_reachable(std::slice::from_ref(&parent))
        .unwrap();

    let bundle = prolly.export_snapshot(&parent).unwrap();
    assert_eq!(bundle.nodes.len(), required.live_nodes);
    let verification = bundle.verify().unwrap();
    assert!(verification.valid);
    assert!(verification.extra_cids.is_empty());
    assert_eq!(verification.reachable_nodes, required.live_nodes);

    let imported = Arc::new(MemStore::new());
    let destination = Prolly::new(imported.clone(), small_config());
    destination.import_snapshot(&bundle).unwrap();
    assert_eq!(common::entries(&destination, &child).len(), 26);
    let plain_reader = Prolly::new(imported, plain_config());
    assert_eq!(common::entries(&plain_reader, &grandchild).len(), 25);

    let grandchild_root = grandchild.root.clone().unwrap();
    let mut incomplete = bundle.clone();
    incomplete.nodes.retain(|node| node.cid != grandchild_root);
    let verification = incomplete.verify().unwrap();
    assert!(!verification.valid);
    assert_eq!(verification.missing_cids, vec![grandchild_root]);
    let empty = Prolly::new(Arc::new(MemStore::new()), small_config());
    assert!(empty.import_snapshot(&incomplete).is_err());

    let limited = prolly.export_snapshot_with_limits(
        &parent,
        prolly::SnapshotExportLimits::new(required.live_nodes - 1, usize::MAX),
    );
    assert!(matches!(
        limited,
        Err(Error::SnapshotExportLimitExceeded {
            resource: "nodes",
            ..
        })
    ));

    let wrong = TreeRef::new(grandchild.root.clone(), small_config().format).unwrap();
    let mislabeled = prolly
        .put(
            &prolly.create(),
            b"wrong".to_vec(),
            ValueRef::Tree(wrong).to_bytes(),
        )
        .unwrap();
    assert!(prolly.export_snapshot(&mislabeled).is_err());
    let mut nodes = prolly.export_snapshot(&grandchild).unwrap().nodes;
    let mislabeled_root = mislabeled.root.clone().unwrap();
    nodes.push(prolly::SnapshotBundleNode {
        cid: mislabeled_root.clone(),
        bytes: source.get(mislabeled_root.as_bytes()).unwrap().unwrap(),
    });
    let forged = prolly::SnapshotBundle::new(mislabeled, nodes);
    assert!(matches!(forged.verify(), Err(Error::FormatMismatch { .. })));
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let waker = futures_util::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        std::thread::yield_now();
    }
}

fn typed_walk(
    store: &Arc<MemStore>,
    root: &Tree,
    limits: &ContentGraphLimits,
) -> [Result<prolly::ContentGraphWalk, Error>; 2] {
    let roots = [TypedContentRoot::new(
        ContentObjectKind::OrderedNode,
        root.root.clone().unwrap(),
    )];
    [
        walk_content_graph(&**store, &roots, limits),
        block_on(prolly::walk_content_graph_async(
            &prolly::SyncStoreAsAsync::new(store.clone()),
            &roots,
            limits,
        )),
    ]
}

#[test]
fn partially_cached_frontiers_still_check_conflicting_formats() {
    // FileNodeStore does not prefer batch reads, so a frontier with one
    // uncached node takes the single-miss load path.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "prolly-tree-ref-cache-{}-{nanos}",
        std::process::id()
    ));
    let store = Arc::new(prolly::FileNodeStore::open(&path).unwrap());
    assert!(!store.prefers_batch_reads());
    let writer = Prolly::new(store.clone(), small_config());
    let plain_writer = Prolly::new(store.clone(), plain_config());
    let cached = writer
        .put(&writer.create(), b"c".to_vec(), b"c".to_vec())
        .unwrap();
    let uncached = plain_writer
        .put(&plain_writer.create(), b"d".to_vec(), b"d".to_vec())
        .unwrap();
    let wrong = TreeRef::new(cached.root.clone(), plain_config().format).unwrap();
    let parent = writer
        .put(&writer.create(), b"a".to_vec(), tree_value(&cached))
        .unwrap();
    let parent = writer
        .put(&parent, b"b".to_vec(), ValueRef::Tree(wrong).to_bytes())
        .unwrap();
    let parent = writer
        .put(&parent, b"d".to_vec(), tree_value(&uncached))
        .unwrap();

    for _ in 0..3 {
        let reader = Prolly::new(store.clone(), small_config());
        assert!(matches!(
            reader.mark_reachable(std::slice::from_ref(&parent)),
            Err(Error::FormatMismatch { .. })
        ));
        let reader = Prolly::new(store.clone(), small_config());
        assert!(reader
            .mark_reachable_blobs(std::slice::from_ref(&parent))
            .is_err());
        let reader = Prolly::new(store.clone(), small_config());
        assert!(reader.export_snapshot(&parent).is_err());
    }
    let _ = std::fs::remove_dir_all(&path);
}

#[test]
fn typed_walk_requires_structural_children_to_inherit_the_referenced_format() {
    let store = Arc::new(MemStore::new());
    let prolly = Prolly::new(store.clone(), small_config());
    let plain = Prolly::new(store.clone(), plain_config());
    let foreign_leaf = plain
        .put(&plain.create(), b"x".to_vec(), b"x".to_vec())
        .unwrap()
        .root
        .unwrap();

    let multi_level = build_tree(&prolly, "internal", 30);
    let root_cid = multi_level.root.clone().unwrap();
    let mut internal = Node::from_bytes(&store.get(root_cid.as_bytes()).unwrap().unwrap()).unwrap();
    assert!(!internal.leaf);
    internal.vals[0] = foreign_leaf.as_bytes().to_vec();
    let bytes = internal.to_bytes();
    let forged_root = Cid::from_bytes(&bytes);
    store.put(forged_root.as_bytes(), &bytes).unwrap();

    let reference = TreeRef::new(Some(forged_root), small_config().format).unwrap();
    let parent = prolly
        .put(
            &prolly.create(),
            b"ref".to_vec(),
            ValueRef::Tree(reference).to_bytes(),
        )
        .unwrap();
    for result in typed_walk(&store, &parent, &ContentGraphLimits::default()) {
        assert!(
            result.is_err(),
            "typed walk accepted a structural format change"
        );
    }
    assert!(prolly
        .mark_reachable(std::slice::from_ref(&parent))
        .is_err());
}

#[test]
fn typed_walk_depth_limits_cover_longer_paths_through_shared_objects() {
    let store = Arc::new(MemStore::new());
    let prolly = Prolly::new(store.clone(), Config::default());
    let leaf_with = |entries: &[(&str, Vec<u8>)]| {
        let mut tree = prolly.create();
        for (key, value) in entries {
            tree = prolly
                .put(&tree, key.as_bytes().to_vec(), value.clone())
                .unwrap();
        }
        tree
    };
    let shallow = ContentGraphLimits {
        max_depth: 2,
        ..ContentGraphLimits::default()
    };

    // Vary content so CID ordering visits the short path first in some cases.
    for variant in 0..12u8 {
        // An already-seen object reached beyond the limit: R -> T, R -> A -> B -> T.
        let target = leaf_with(&[("t", vec![variant])]);
        let b = leaf_with(&[("b", tree_value(&target))]);
        let a = leaf_with(&[("a", tree_value(&b))]);
        let root = leaf_with(&[("a", tree_value(&a)), ("t", tree_value(&target))]);
        for result in typed_walk(&store, &root, &shallow) {
            assert!(
                matches!(result, Err(Error::ContentGraphResourceLimitExceeded { .. })),
                "variant {variant}: over-limit shared target accepted"
            );
        }
        for result in typed_walk(&store, &root, &ContentGraphLimits::default()) {
            assert_eq!(result.unwrap().maximum_depth, 3, "variant {variant}");
        }

        // An object within the limit whose seen descendant exceeds it:
        // R -> X -> Y, R -> A -> X -> Y.
        let y = leaf_with(&[("y", vec![variant])]);
        let x = leaf_with(&[("x", tree_value(&y))]);
        let a = leaf_with(&[("ax", tree_value(&x))]);
        let root = leaf_with(&[("a", tree_value(&a)), ("x", tree_value(&x))]);
        for result in typed_walk(&store, &root, &shallow) {
            assert!(
                matches!(result, Err(Error::ContentGraphResourceLimitExceeded { .. })),
                "variant {variant}: over-limit shared descendant accepted"
            );
        }
    }
}
