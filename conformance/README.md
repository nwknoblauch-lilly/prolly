# Prolly Conformance Fixtures

`prolly-fixtures.v1.json` is the shared contract for language bindings.
It is generated from the Rust reference implementation:

```sh
cargo run --bin prolly-conformance -- --write conformance/prolly-fixtures.v1.json
```

The fixture file uses lowercase hex for every byte string. Bindings should treat
these cases as required before claiming compatibility with a tier:

- node decoding/encoding and CID checks
- boundary decisions
- key helper output
- read-only tree traversal over a Rust-generated store image
- logical diffs over Rust-generated tree roots
- value and blob envelope decoding
- tree-reference envelopes (`tree_ref_fixtures`, tag `2`), for implementations
  that support tree-valued references; bindings without that support skip this
  section and keep decoding `blob_fixtures`
- manifest bytes for implementations that support named roots
- canonical indexed-collection root name, state/descriptor records, physical
  key/value, and source/index/state roots for all projection modes
