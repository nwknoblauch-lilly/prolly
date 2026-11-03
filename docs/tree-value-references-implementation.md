# Implement native tree-valued references in prolly-map

**Status: Implemented.** This plan was implemented on branch `feat/tree-value-refs`. The implementation also covers the `extensions/dynamodb/core` consumer, which fails closed on tree references. The binding API inventory was not regenerated because the nightly toolchain was unavailable.

All source paths and commands below are relative to the prolly-map repository root, not this `docs/` directory.

## 1. Assignment and selected scope

Implement `ValueRef::Tree(TreeRef)` so an ordered-tree leaf can refer to another immutable ordered tree. Reuse the existing library: do not create another storage engine or an application-specific reference framework.

The user explicitly selected:

1. **Existing reachability, sync, snapshot, and GC operations follow recognized tree-reference envelopes.** Do not substitute a parallel opt-in traversal API. Raw point/range reads, flat diff/merge, logical cardinality, and ordinary per-tree statistics remain byte-oriented; they do not flatten a child tree into its parent.
2. **Rust core plus binding compatibility.** Keep existing binding crates buildable and preserve existing behavior. A typed binding that cannot represent Tree must return an explicit unsupported-kind error. Full exposure through every language binding and a cross-language release are outside this assignment.

Execute this document only when the user asks you to implement it. Code changes, callers, tests, and affected docs belong to the feature. Commit, push, PR creation, version changes, dependency/tool installation, and deployed-data migration retain their separate authorization requirements. Do not accidentally publish to the upstream `origin`; publication is a separate operation.

Base inspected: `6ee959eaed2625bf086ae0c4d1a2b5934f7e3872`, package `prolly-map` 0.7.2. The source checkout was clean at planning time. Recheck revision, status, and affected diffs before editing and preserve unrelated work.

### Success criteria

| ID | Required end state |
| --- | --- |
| G1 | Rust callers encode, inspect, and reopen empty/nonempty Tree references with identity independent of runtime cache settings. |
| G2 | Legacy Inline/Blob bytes and behavior are preserved. Raw reads return leaf bytes; byte-only blob-resolution helpers reject Tree rather than materializing a tree into bytes. |
| G3 | Existing reachability, node copy, snapshot export/import verification, and GC include recursively referenced trees, including mixed formats and shared children. |
| G4 | Missing/corrupt/misdeclared targets and exhausted traversal limits cannot produce false closure success or deletion from a partial GC plan. |
| G5 | Blob reachability includes blobs in referenced child trees; node-copy APIs remain node-copy APIs and do not implicitly acquire a blob store. |
| G6 | Required Rust binding consumers compile; unsupported foreign Tree conversions fail explicitly and new binding mappings remain pending. |

Non-goals: VCS/commit/ref/reflog implementation, application conflict semantics, arbitrary reference discovery inside JSON/CBOR, implicit dereferencing of raw reads, recursive semantic merge, general-purpose list trees, blob chunking, new storage backends, new hash algorithms, node-layout changes, or a new distributed GC/publication protocol.

## 2. Evidence and source map

These locations were inspected or traced at the base revision. Use symbols when line numbers move.

| Surface | Starting point |
| --- | --- |
| Owned envelope and byte resolvers | `src/prolly/blob.rs`: `ValueRef`, `value_ref_view_from_stored_bytes`, `decode_value_ref`, `encode_stored_value`, `resolve_stored_value`, async equivalents |
| Borrowed inspection | `src/prolly/read.rs`: `ValueRefView`, `to_owned`, `get_value_ref_with` variants |
| Tree and configuration | `src/prolly/tree.rs`: `Tree`; `src/prolly/format.rs`: `TreeFormat::canonical_bytes`, `from_canonical_bytes`; `src/prolly/config.rs`: `Config`, `RuntimeConfig` |
| Reachability and transfer | `src/prolly/mod.rs`: `mark_reachable*`, `plan_missing_nodes`, `copy_missing_nodes`, `export_snapshot*`, node/blob GC helpers; trace their engine callees |
| Bundle validation | `src/prolly/sync.rs`: `SnapshotBundle::verify`, `reachable_snapshot_nodes` |
| Versioned-map consumers | `src/prolly/versioned_map.rs`: `verify_catalog` calls `mark_reachable`, so its `reachable_nodes`/`reachable_bytes` start counting child-tree nodes; `export_snapshot`/`copy_missing_nodes` wrappers delegate to the core and inherit closure behavior |
| Typed content graph | `src/prolly/content_graph/walk.rs`: sync/async walkers, `content_references*`, `references` OrderedNode arm; adjacent copy/GC consumers |
| Exports | `src/lib.rs` |
| Foreign conversions | `bindings/uniffi/src/lib.rs`: `ValueRefRecord`, `From<ValueRef>`, typed reference exports; `bindings/node/native/src/lib.rs`: `NodeValueRefRecord` and mapping helpers |
| Conformance generation | `src/bin/prolly-conformance.rs`: `blob_fixtures`; `tests/conformance_fixtures.rs`; `conformance/prolly-fixtures.v1.json` |
| Binding inventory | `bindings/api/README.md`, `scripts/binding_api_inventory.py` |
| Related proposal, not implemented VCS code | `docs/prolly-vcs-design.md`, Tree References section |

Confirmed facts:

- PLVB version 1 currently has Inline tag 0 and Blob tag 1. `ValueRef::to_bytes` and borrowed `to_owned` are infallible public methods.
- `Tree` contains full `Config`, including runtime settings that must not enter TreeRef identity.
- `TreeFormat` derives `PartialEq`/`Eq` but not `Hash`, so `TreeRef`'s `Hash` must derive from canonical format bytes (or a validated cached copy), not from `TreeFormat` itself.
- Physical and typed OrderedNode graph walks currently stop at leaves. Blob marking separately scans recognized leaf envelopes.
- `SnapshotBundle::verify` currently computes closure through structural edges only.
- UniFFI's Rust-to-record conversion is infallible today, but its exported reference decoding/read operations already return Result. They can propagate a new unsupported-kind error without adding a foreign Tree record.
- The top of `docs/wire-format.md` has pre-existing stale CRAB version-1 prose; the current node writer uses version 2. Do not implement against that stale description or change the node format for this feature.

Earlier qualification at this exact revision passed 240 distinct selected tests, including the feature-enabled async-store suite and extended splice oracle. This is pre-change evidence, not a test of Tree references. No feature code or tests ran during planning.

## 3. Type, encoding, and read contract

### TreeRef

Add and export TreeRef beside Tree in `src/prolly/tree.rs`. Its logical shape is:

```rust
// Use private fields and checked construction.
struct TreeRef {
    root: Option<Cid>,
    format: TreeFormat,
}
```

Required API:

- `TreeRef::new(root: Option<Cid>, format: TreeFormat) -> Result<TreeRef, Error>`.
- `TryFrom<&Tree> for TreeRef`, copying only root and persisted format.
- Read-only root/format accessors.
- `to_tree(&self, runtime: RuntimeConfig) -> Tree`, attaching local runtime settings without I/O.
- Clone, Debug, PartialEq, Eq, and Hash, consistent with root plus canonical persisted format.

Validate the format with the existing canonical format codec at construction. Do not fetch the target: missing targets and mismatched target formats are checked when the reference is followed. Empty trees use None, not a fabricated empty-node CID.

Keep fields private and do not permit unchecked deserialization or later mutation to invalidate them. Preserve the existing infallible `ValueRef::to_bytes` signature. A small private cache of validated canonical format bytes is acceptable if necessary for infallible encoding and Hash; it must not create an independent identity. Do not use panic/unwrap/expect on untrusted input or fabricate bytes after an encoding failure.

Add these variants:

```rust
ValueRef::Tree(TreeRef)
ValueRefView::Tree(TreeRef)
```

The view may own the small validated descriptor; it must not load or copy child-tree contents. Preserve existing Inline borrowing and Blob behavior. Do not add another public borrowed-reference abstraction solely for this feature.

### Wire encoding

Preserve existing magic/version/tag meanings. Allocate tag 2:

```text
"PLVB" | version=1 | tag=2
root_present:u8                    # 0 or 1
root_cid:32 bytes                   # only if root_present == 1
format_length:u64 big-endian
format_bytes:format_length bytes    # TreeFormat::canonical_bytes()
```

Empty references still carry the persisted format. Do not include runtime config, timestamps, names, or store identifiers.

Owned and borrowed decoders must agree. Reject truncation, invalid root flags, impossible/overflowing lengths, trailing bytes, noncanonical/invalid format bytes, unsupported versions, and unknown tags. Validate lengths against actual input before allocation. Reuse the existing format decoder and verify byte-identical canonical re-encoding where necessary; do not create a second format codec.

Legacy Inline/Blob envelopes, ordinary tree-node encodings, and existing frozen vectors remain byte-for-byte unchanged.

### Reads and writers

- Typed `get_value_ref`, callback/session/async variants, and `ValueRefView::to_owned` preserve Tree.
- Raw get/range/diff still return the same bytes. Do not auto-open child trees.
- `get_large_value` and corresponding byte-only resolvers return an existing `Error::Deserialize` with a clear unsupported-tree-as-bytes diagnostic for Tree. Preserve return types; tests assert the category rather than arbitrary prose.
- `put_large_value` still accepts raw application bytes, with its existing escape/offload behavior. Store an actual reference through ordinary put using `ValueRef::Tree(reference).to_bytes()`. No new builder framework is needed.

## 4. Extend existing closure operations

### Edge interpretation

Extend node marking/GC, missing-node planning/copy, snapshot export/verification/import, typed OrderedNode content-graph walking/copy/GC, and blob marking/GC, including async paths.

At a leaf, parse only the outer stored-value envelope with the shared decoder:

| Leaf representation | Meaning for closure |
| --- | --- |
| Non-PLVB raw bytes | Opaque inline data; no edge. |
| Inline envelope | Opaque payload, even if that payload begins with PLVB. |
| Blob envelope | Existing blob semantics; never parse the blob payload recursively. |
| Tree with a root | Follow that root under its declared persisted format. |
| Empty Tree | Valid reference, no node to fetch. |
| Malformed or unknown PLVB envelope | Fail closure traversal; do not silently ignore a possible edge during GC. |

Do not discover references inside keys, arbitrary JSON/CBOR, escaped Inline content, or blob bodies. No generic plugin/discovery framework is requested.

A Tree edge starts a new ordered-tree context. Do not impose the parent's separator range, physical level, or logical subtree count on it. A parent entry remains one entry regardless of child size. `len`, `rank`, `select`, flat diff/merge, and ordinary single-tree statistics keep their existing meaning.

### Validation and limits

- Validate content IDs and every declared target format through existing checks.
- Missing targets are errors in error-returning operations. Snapshot verification may report missing CIDs through its existing result, but that result cannot allow import or successful closure publication.
- Deduplication must not hide a second, incompatible format declaration for the same CID. Check every incoming requirement, even when the physical object was seen before.
- Mixed formats are valid when references match their targets.
- Carry existing traversal budgets across the entire graph, including cross-tree hops. Do not reset them for each child. Leaf-to-tree chains consume depth even when every individual tree has height zero.
- Preserve batching, bounded frontier reads, async cancellation, and error propagation.
- Reuse a shared internal leaf-edge parser. Extend private traversal context as necessary to retain expected formats; do not discard a TreeRef's format just because TypedContentRoot currently carries only kind/CID/dimensions.
- Edge enumeration is not proof that the referenced closure exists. Full walkers/copy/verification paths establish that proof.

### Copy, bundles, and GC

A destination may contain an outer root but lack its child trees. Parent presence is not closure proof. Missing-node copy must establish complete destination node closure before returning success, reusing validated destination objects where appropriate. A failed copy may leave immutable objects behind; do not invent rollback or report successful named-root publication.

Export includes referenced child-tree nodes. Verification recognizes them as reachable, checks crossed formats, and reports/rejects missing descendants. Import validates required closure before adoption through its existing path. Preserve validity of old bundles without Tree references.

Node GC retains all nested nodes. Removing one of several references does not make a shared child collectible; removing its final retained reference does. Blob marking follows child trees and collects their Blob references.

**Keep the store boundary precise:** APIs accepting only node stores still copy/export nodes only. They do not automatically transfer separate blob-store payloads or arbitrary application objects. Do not add an implicit blob store or claim full blob dereference after node-only copy.

Complete marking precedes destructive sweeping. Missing/corrupt targets, malformed envelopes, format mismatch, and limit failures must not lead to deletion from a partial plan. This feature does not solve distributed GC coordination; existing retained-root and concurrent-publication assumptions remain.

## 5. Compatibility and bindings

A new public Rust enum variant changes exhaustive consumers. Update necessary matches. Existing let-else tests with an else branch do not automatically need changes; keep their assertions.

The user selected a behavioral change to existing closure APIs. Literal PLVB-prefixed application data must use the existing Inline escape. Do not silently migrate data or alter raw get/put behavior.

Older ValueRef decoders reject tag 2, but **older node-only GC may ignore it and delete referenced children**. Upgrade every closure-verification/GC process before using these values in shared data. Do not claim the old decoder's rejection makes old generic GC safe. No deployed migration or package release is authorized here.

Binding compatibility for this phase:

- Preserve current foreign Inline/Blob records and generated-client ABI.
- Make Rust-to-foreign conversions fallible where needed. Propagate an explicit unsupported Tree-kind error through existing Result-returning APIs.
- Never map Tree to Blob or Inline, panic on it, or invent a child-content byte materialization.
- Update manual adapters only where needed to compile or avoid silent new-tag misclassification. Raw byte operations remain available.
- Do not regenerate all clients to expose a new foreign Tree record. If a specific boundary cannot stay compatible, report the concrete blocker rather than silently widening scope.
- Refresh Rust API inventory through existing tooling when available; leave new language mappings planned/pending, not implemented or release-complete.
- Put new wire fixtures in a separate tree-reference section. Do not add tag-2 rows to the existing shared blob fixtures that unsupported bindings still consume.

## 6. Implementation units

### S0. Establish baseline evidence

**Outcome:** Pre-existing behavior and environmental failures are recorded before feature failures are classified.

**Change or evidence:** Baseline: concurrent. Reuse prior evidence only after confirming unchanged sources, otherwise run the targeted baseline on the pre-change tree. Results feed failure classification, not a new product decision.

**Targets:** Existing reference/read/sync/GC tests and current Git status.

**Preconditions and dependencies:** None. Every mutation unit independently checks its current files and diffs.

**Shared resources and collision reasons:** Isolate pre-change tests when running alongside implementation. Otherwise finish them before mutation; shared Cargo output is not safe for independent concurrent writers. Do not commit/stash user work to create isolation.

**Acceptance criteria:** S0.A1: record revision, dirty paths, exact commands/results, and unavailable checks without claiming a missing check passed.

**Verification:** `git status --short`; `cargo test --locked --offline --test large_value_offload --test zero_copy_reads --test gc --test mixed_format_gc --test sync_primitives --test conformance_fixtures`; `cargo test --locked --offline --features async-store --test async_store`. Expect exit 0 or an explicit pre-existing/environment disposition.

**Stop conditions:** Current code contradicts the specified contract or edits would overlap uncertain user work. Missing tools block their dependent checks, not independent safe work.

### S1. Add TreeRef and envelope support

**Outcome:** Rust construction, encoding, and inspection support Tree while preserving legacy bytes and existing method signatures.

**Change or evidence:** Implement section 3, public exports, checked conversions, owned/borrowed decoding, and explicit byte-resolver errors.

**Targets:** `src/prolly/tree.rs`, `blob.rs`, `read.rs`, `src/lib.rs`; new `tests/tree_value_refs.rs`; necessary exhaustive-match fixes in existing Rust tests.

**Preconditions and dependencies:** None beyond checking the current source contract. Use real library trees/stores in tests, not a replacement engine.

**Shared resources and collision reasons:** Own reference types, exports, and their tests. Later units consume the completed API; no concurrent writer touches these shared files.

**Acceptance criteria:**
- S1.A1: empty/nonempty references round-trip in owned and borrowed APIs; runtime-only changes preserve reference bytes, Eq, and Hash behavior.
- S1.A2: legacy fixtures are unchanged; all malformed tag-2 fields are rejected before unchecked allocation/truncation.
- S1.A3: raw reads preserve bytes, typed reads return Tree, and byte-only resolution returns the specified error without dereferencing a target.

**Verification:** `cargo test --locked --offline --test tree_value_refs --test large_value_offload --test zero_copy_reads`. Compare bytes with the explicit tag/root/length layout, not only a same-implementation round trip.

**Stop conditions:** Maintaining existing infallible methods would require panic/fabricated data, or a consumer requires an unselected foreign ABI change.

### S2. Extend marking and GC traversal

**Outcome:** Existing relevant markers traverse nested trees, check format context, and retain nested nodes/blobs safely.

**Change or evidence:** Implement shared leaf-edge discovery and reference-aware queues under section 4. Preserve logical cardinality and raw semantics.

**Targets:** `src/prolly/mod.rs` and engine callees; `content_graph/walk.rs` and GC consumers; new cases in `tests/tree_value_refs.rs`, `gc.rs`, `mixed_format_gc.rs`, `large_value_offload.rs`, `versioned_map.rs`, and `async_store.rs` as appropriate.

**Preconditions and dependencies:** S1.

**Shared resources and collision reasons:** Own traversal changes; they overlap S3's core graph internals, so S2 precedes S3 in a shared checkout.

**Acceptance criteria:**
- S2.A1: retaining only a parent retains child/grandchild nodes; shared physical nodes count once; empty child trees cause no fetch or missing-object error.
- S2.A2: matching mixed formats work; wrong format fails even after a valid reference visited that CID; cross-tree chains consume the original limits.
- S2.A3: nested blobs are marked; Inline escapes and blob contents are opaque.
- S2.A4: removing one shared reference retains the child; removing the last retained reference permits reclamation; incomplete marking causes no deletion.
- S2.A5: parent len/rank/select and flat diff/merge are unchanged; child keys and height need not fit the parent's separator/level context.
- S2.A6: `VersionedMap::verify_catalog` reachable node/byte counts include child-tree nodes and fail on missing child targets.

**Verification:** `cargo test --locked --offline --test tree_value_refs --test gc --test mixed_format_gc --test large_value_offload --test cardinality --test diff_merge --test versioned_map`; feature-enabled async_store tests. Cover standard and typed content-graph walkers. Failing/counting stores assert zero deletes after marking failure.

**Stop conditions:** Traversal reuse loses incoming format checks or limits, or implementing it requires weakening retention safeguards. Do not replace selected existing-API behavior with opt-in semantics.

### S3. Extend transfer and snapshot closure

**Outcome:** Existing node-copy and snapshot APIs transfer and validate nested node closure.

**Change or evidence:** Reuse reference-aware closure for missing-node planning/copy, export/verify/import, and content-graph copy. Preserve hash validation and publication boundaries.

**Targets:** `src/prolly/mod.rs`, `sync.rs`, engine callees, `content_graph/sync.rs`; `tests/tree_value_refs.rs`, `sync_primitives.rs`, `node_publication.rs`, `async_store.rs`.

**Preconditions and dependencies:** S1 and S2.

**Shared resources and collision reasons:** Core graph routines and shared feature tests overlap S2; serialize or safely isolate with a parent-owned integration barrier.

**Acceptance criteria:**
- S3.A1: copying a parent into an empty destination makes every referenced child tree readable; seeding only the parent node does not hide missing descendants.
- S3.A2: export includes nested nodes and verification does not label them extra; omitting a required descendant prevents successful verification/import; legacy bundles remain valid.
- S3.A3: repeated copy reuses objects; CID/format/missing/cancellation/limit errors do not report closure success or publish named roots prematurely.
- S3.A4: sync and async agree; node-only copy/export does not claim to transfer blob-store bytes.

**Verification:** `cargo test --locked --offline --test tree_value_refs --test sync_primitives --test node_publication`; `cargo test --locked --offline --features async-store --test async_store`. Assert destination readability and the expected reachable CID set, not just the parent root.

**Stop conditions:** A shortcut treats parent presence as closure proof, or bundle validation cannot preserve crossed format requirements. Fix shared logic rather than weakening assertions.

### S4. Preserve binding compatibility and conformance

**Outcome:** Supported foreign kinds remain correct/buildable; Tree conversions fail explicitly until supported; Rust gets independent new wire fixtures.

**Change or evidence:** Replace required infallible Rust-to-binding conversions with fallible conversions and propagate errors. Preserve old foreign records. Add a new fixture section at the Rust generator source and refresh Rust conformance tests and API inventory honestly.

**Targets:** `bindings/uniffi/src/lib.rs` and affected domain callers; `bindings/node/native/src/lib.rs`; manual consumers found by a fresh ValueRef/ValueRefView search; `src/bin/prolly-conformance.rs`, `conformance/prolly-fixtures.v1.json`, `tests/conformance_fixtures.rs`, generated `bindings/api/` inventory.

**Preconditions and dependencies:** S1. May run alongside S2/S3 only with safe source/build isolation.

**Shared resources and collision reasons:** Own binding conversion fixes and generator outputs. Do not hand-edit generated clients or run competing generators.

**Acceptance criteria:**
- S4.A1: existing Rust binding consumers compile; typed Tree access returns unsupported-kind errors, not substituted values or panic.
- S4.A2: existing foreign records and legacy fixture bytes are unchanged; new Rust-only tree fixtures validate the selected format separately.
- S4.A3: new binding mappings are pending unless actually implemented/tested; no full parity claim.

**Verification:** `cargo check --locked --offline --manifest-path bindings/uniffi/Cargo.toml`; equivalent check for `bindings/node/native/Cargo.toml`; affected conversion unit tests; `cargo test --locked --offline --test conformance_fixtures`. Inspect generator output behavior before `cargo run --locked --offline --bin prolly-conformance`. Follow `bindings/api/README.md` for inventory generation/check using available qualified tools and the project's environment. Do not auto-install nightly or binding tooling. Report unavailable generation/build checks and do not claim their artifacts were regenerated or verified.

**Stop conditions:** A boundary requires a new foreign ABI rather than fallible conversion. Missing tooling blocks its dependent artifact/release claim, not unrelated completed Rust code.

### S5. Integrate, document, and verify

**Outcome:** The complete Rust feature is verified and documented, including changed existing closure semantics and binding/rollout limits.

**Change or evidence:** Integrate units, update affected rustdoc/docs/examples, document the Tree envelope and upgrade-before-GC warning, and clarify the proposed VCS TreeRef section to reuse the real core type. Do not implement the VCS design.

**Targets:** README; affected `docs/wire-format.md`, `docs/design-spec.md`, `docs/prolly-vcs-design.md` sections; reference rustdoc; final feature tests and shared exports. Preserve unrelated historical docs. Flag pre-existing node-format prose instead of treating it as the new source of truth.

**Preconditions and dependencies:** S2, S3, S4; all mutation lanes integrated.

**Shared resources and collision reasons:** One integration writer owns shared docs and aggregate checks; no other lane changes the tested snapshot.

**Acceptance criteria:**
- S5.A1: a public API example constructs child/parent trees, stores TreeRef, copies the parent graph, and reads the child at the destination; empty references are documented.
- S5.A2: new and relevant existing tests pass, with formatting/clippy/full-core outcomes recorded and failures classified against baseline.
- S5.A3: docs accurately state closure behavior (including the changed meaning of versioned-map catalog reachable counts), raw/byte-only behavior, opaque payload boundaries, pending bindings, unchanged hashes/node formats, and unsafe old GC.
- S5.A4: final diff is limited to required feature code/callers/tests/generated sources/docs; no unauthorized dependency/version/publication changes occurred.

**Verification:** `cargo fmt --all -- --check`; `cargo clippy --locked --offline --all-targets --features async-store -- -D warnings`; `cargo test --locked --offline`; `cargo test --locked --offline --features async-store --test async_store`; final status/diff. Focused source review checks completeness, format-before-dedup, marking failure before deletion, and legacy bytes. Passing tests are finite evidence, not universal graph or deployment proof.

**Stop conditions:** A regression requires an unselected public-contract change or weaker safety property. Report it rather than hiding it behind a passing subset or alternative opt-in API.

## 7. Sequencing and coverage

Default sequence: S0 baseline, S1 types/codec, S2 marking, S3 sync, S4 compatibility, S5 integration. S0 is independent baseline evidence. After S1, S4 is independent of S2/S3 but can run in parallel only with safe isolation; otherwise serialize. One writer per checkout is the default, not a total task-launch cap. Do not create Git history merely to enable isolation or preselect models in this plan.

Each dependent packet receives the complete relevant contract, exact acceptance criteria, current source/diff state, S1's checked API, and prior verification. Progress summaries do not replace the contract.

| Requirement | Acceptance evidence |
| --- | --- |
| G1 stable references and no runtime identity | S1.A1-A2, S5.A1 |
| G2 legacy/raw/byte-only behavior | S1.A2-A3, S4.A2 |
| G3 existing APIs follow nested trees | S2.A1-A2/A5-A6, S3.A1-A2 |
| G4 safe closure and GC | S2.A2/A4, S3.A2-A3 |
| G5 blob retention versus transport boundary | S2.A3, S3.A4 |
| G6 compatibility without parity overclaim | S4.A1-A3, S5.A3 |
| Empty/shared/mixed-format/deep cases | S1.A1, S2.A1-A2, S3.A1 |
| Warm-destination repair and exact snapshots | S3.A1-A2 |
| Hidden JSON/CBOR references | Excluded; Inline/blob payloads opaque |
| VCS, semantic merge, lists, blob chunking, hashes | Excluded |
| Compatibility and old-GC risk | Section 5 and S5.A3 |

The final report lists changed files, exact commands/results, unavailable checks/generators, and compatibility risks. Never label pending bindings supported or claim a deployed migration, publication, or new distributed-GC guarantee.

## 8. Risks and remaining checks

- Old GC ignores new edges: upgrade every closure/GC reader before using these values in shared data; deployment is not part of this task.
- Literal PLVB prefixes gain traversal meaning: preserve Inline escaping and document this selected behavior change. Never scan inside escaped or blob payloads.
- Same CID under incompatible format declarations: check every incoming constraint before dedup; test live-store and snapshot paths.
- Hidden consumers: the source map is a verified starting point, not permission to skip current callsite discovery. Required callers belong to the feature.
- Missing binding tooling: use documented generators with available environments; report exact limitations rather than installing tools or fabricating outputs.
- Scope creep: no generic reference plugin, VCS layer, new database abstraction, or parallel opt-in walker. Reuse parsers, queues, stores, and error categories.

There is no unresolved product choice blocking this scope. Existing-API reference traversal and Rust-plus-binding-compatibility were explicitly selected by the user. Private representation choices and generator availability are bounded implementation checks, not permission to reopen those decisions.
