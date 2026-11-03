//! Tree structure for Prolly Trees

use serde::{Deserialize, Serialize};

use std::hash::{Hash, Hasher};

use super::cid::Cid;
use super::config::{Config, RuntimeConfig};
use super::error::Error;
use super::format::TreeFormat;

/// A Prolly Tree handle
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tree {
    /// Root node CID (None if empty)
    pub root: Option<Cid>,
    /// Tree configuration
    pub config: Config,
}

impl Tree {
    /// Create a new empty tree with the given configuration
    pub fn new(config: Config) -> Self {
        Self { root: None, config }
    }

    /// Check if the tree is empty
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }
}

impl Default for Tree {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

/// Immutable reference from a leaf value to another ordered tree.
///
/// A reference records only the persisted identity of the target tree: its
/// optional root CID and its canonical [`TreeFormat`]. Runtime settings such as
/// cache limits never affect reference bytes, equality, or hashing. Empty trees
/// are referenced with `root == None`.
///
/// Construction validates the format but does not fetch the target; missing
/// targets and mismatched target formats are detected when closure operations
/// follow the reference.
///
/// Store a reference with [`ValueRef::Tree`](crate::ValueRef::Tree) and an
/// ordinary put. Raw reads return the envelope bytes; node GC, node copy, and
/// snapshot export follow it.
///
/// ```
/// use prolly::{Config, MemStore, Prolly, RuntimeConfig, TreeRef, ValueRef};
/// use std::sync::Arc;
///
/// let prolly = Prolly::new(Arc::new(MemStore::new()), Config::default());
/// let child = prolly
///     .put(&prolly.create(), b"file".to_vec(), b"contents".to_vec())
///     .unwrap();
/// let reference = TreeRef::try_from(&child).unwrap();
/// let parent = prolly
///     .put(&prolly.create(), b"dir".to_vec(), ValueRef::Tree(reference).to_bytes())
///     .unwrap();
///
/// // Node copy follows the reference, so the child is readable at the destination.
/// let destination = Arc::new(MemStore::new());
/// prolly.copy_missing_nodes(&parent, &destination).unwrap();
/// let reader = Prolly::new(destination, Config::default());
/// let Some(ValueRef::Tree(found)) = reader.get_value_ref(&parent, b"dir").unwrap() else {
///     panic!("expected a tree reference");
/// };
/// let child_tree = found.to_tree(RuntimeConfig::default());
/// assert_eq!(reader.get(&child_tree, b"file").unwrap(), Some(b"contents".to_vec()));
///
/// // Empty trees are referenced without a root.
/// assert!(TreeRef::try_from(&prolly.create()).unwrap().root().is_none());
/// ```
#[derive(Clone, Debug)]
pub struct TreeRef {
    root: Option<Cid>,
    format: TreeFormat,
    /// Validated `format.canonical_bytes()`, cached so encoding is infallible.
    format_bytes: Vec<u8>,
}

impl TreeRef {
    /// Create a reference to a tree with the given root and persisted format.
    pub fn new(root: Option<Cid>, format: TreeFormat) -> Result<Self, Error> {
        let format_bytes = format.canonical_bytes()?;
        Ok(Self {
            root,
            format,
            format_bytes,
        })
    }

    /// Decode a reference from a root and canonical format bytes, rejecting
    /// bytes that do not re-encode identically.
    pub(crate) fn from_canonical_parts(
        root: Option<Cid>,
        format_bytes: &[u8],
    ) -> Result<Self, Error> {
        let format = TreeFormat::from_canonical_bytes(format_bytes)?;
        let reference = Self::new(root, format)?;
        if reference.format_bytes != format_bytes {
            return Err(Error::InvalidFormat(
                "tree format bytes are not canonical".to_string(),
            ));
        }
        Ok(reference)
    }

    /// Root CID of the referenced tree, or `None` for an empty tree.
    pub fn root(&self) -> Option<&Cid> {
        self.root.as_ref()
    }

    /// Persisted format of the referenced tree.
    pub fn format(&self) -> &TreeFormat {
        &self.format
    }

    /// Canonical persisted format bytes.
    pub(crate) fn format_bytes(&self) -> &[u8] {
        &self.format_bytes
    }

    /// Open the referenced tree handle with local runtime settings. No I/O.
    pub fn to_tree(&self, runtime: RuntimeConfig) -> Tree {
        Tree {
            root: self.root.clone(),
            config: Config {
                format: self.format.clone(),
                runtime,
            },
        }
    }
}

impl TryFrom<&Tree> for TreeRef {
    type Error = Error;

    fn try_from(tree: &Tree) -> Result<Self, Error> {
        Self::new(tree.root.clone(), tree.config.format.clone())
    }
}

impl PartialEq for TreeRef {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root && self.format_bytes == other.format_bytes
    }
}

impl Eq for TreeRef {}

impl Hash for TreeRef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.root.hash(state);
        self.format_bytes.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tree_new() {
        let tree = Tree::new(Config::default());
        assert!(tree.is_empty());
        assert!(tree.root.is_none());
    }

    #[test]
    fn test_tree_default() {
        let tree = Tree::default();
        assert!(tree.is_empty());
    }

    #[test]
    fn test_tree_with_root() {
        let cid = Cid::from_bytes(b"test");
        let tree = Tree {
            root: Some(cid.clone()),
            config: Config::default(),
        };
        assert!(!tree.is_empty());
        assert_eq!(tree.root, Some(cid));
    }
}
