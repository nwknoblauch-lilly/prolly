//! Recursive diff and merge across [`ValueRef::Tree`] references.
//!
//! Flat [`Prolly::diff`] and [`Prolly::merge`] compare stored value bytes, so
//! a child tree is one opaque entry of its parent. The operations here descend
//! into a parent entry only when both compared values are tree references with
//! the same persisted format; every other value stays byte-compared. Content
//! addressing rules out reference cycles, so descent always terminates.
//!
//! Both operations use explicit work stacks over the ordinary flat engine
//! operations instead of recursive futures, which keeps the returned futures
//! `Send` whenever the store's and merge policy's futures are.

use std::collections::HashMap;
use std::future::{ready, Future};
use std::vec;

use super::blob::{value_ref_view_from_stored_bytes, ValueRef};
use super::config::RuntimeConfig;
use super::diff::AsyncConflictIter;
use super::engine;
use super::error::{
    Conflict, Diff, Error, NestedConflict, NestedDiff, NestedResolver, Resolution, Resolver,
};
use super::read::ValueRefView;
use super::store::{AsyncStore, Store};
use super::tree::{Tree, TreeRef};
use super::{AsyncProlly, Prolly};

/// A merge policy's decision for one conflict of a recursive merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NestedDecision {
    /// Merge the conflicting child trees and store a reference to the result.
    ///
    /// Valid only when the policy was told the conflict is descendable;
    /// otherwise the merge fails with [`Error::InvalidFormat`].
    Descend,
    /// Store this value at the conflicting key.
    Value(Vec<u8>),
    /// Remove the conflicting key.
    Delete,
    /// Fail the merge with [`Error::NestedConflict`].
    Unresolved,
}

/// The decision [`Prolly::merge_recursive`] makes without a resolver.
fn default_decision(descendable: bool) -> NestedDecision {
    if descendable {
        NestedDecision::Descend
    } else {
        NestedDecision::Unresolved
    }
}

/// Decides every conflict of [`Prolly::merge_recursive_with`].
///
/// Only true conflicts reach the policy: keys changed on one side, and keys
/// changed identically on both sides, are applied by the flat merge without a
/// call. The policy is called once per conflict, before any descent, with
/// `descendable` set when the left and right values are both tree references
/// with the same persisted format. On [`NestedDecision::Descend`], the base
/// child is the base value when it is a tree in that format and an empty tree
/// otherwise.
///
/// The policy may write nodes through `prolly`, for example to build a
/// conflict value that references them. Those nodes are referenced only if the
/// merged tree uses them; if the merge fails they remain unreferenced, which
/// is safe for content-addressed storage. An `Err` aborts the whole merge.
/// Nothing is published either way: the result is a returned [`Tree`].
///
/// The default method descends whenever possible and leaves every other
/// conflict unresolved, which matches [`Prolly::merge_recursive`] without a
/// resolver.
pub trait NestedMergePolicy<S: Store> {
    /// Decide one conflict.
    fn on_conflict(
        &self,
        prolly: &Prolly<S>,
        conflict: &NestedConflict,
        descendable: bool,
    ) -> Result<NestedDecision, Error> {
        let _ = (prolly, conflict);
        Ok(default_decision(descendable))
    }
}

/// Async counterpart of [`NestedMergePolicy`] for
/// [`AsyncProlly::merge_recursive_with`], with the same contract.
///
/// The returned futures must be `Send` so the merge future stays `Send`.
pub trait AsyncNestedMergePolicy<S>: Sync
where
    S: AsyncStore,
    S::Error: Send + Sync,
{
    /// Decide one conflict.
    fn on_conflict(
        &self,
        prolly: &AsyncProlly<S>,
        conflict: &NestedConflict,
        descendable: bool,
    ) -> impl Future<Output = Result<NestedDecision, Error>> + Send {
        let _ = (prolly, conflict);
        ready(Ok(default_decision(descendable)))
    }
}

/// Engine-agnostic access to a policy from the shared merge loop.
trait ConflictDriver {
    fn on_conflict<'a>(
        &'a self,
        conflict: &'a NestedConflict,
        descendable: bool,
    ) -> impl Future<Output = Result<NestedDecision, Error>> + Send + 'a;
}

/// Adapts [`NestedResolver`] to the default descent rule.
struct ResolverDriver {
    resolver: Option<NestedResolver>,
}

impl ConflictDriver for ResolverDriver {
    fn on_conflict<'a>(
        &'a self,
        conflict: &'a NestedConflict,
        descendable: bool,
    ) -> impl Future<Output = Result<NestedDecision, Error>> + Send + 'a {
        let decision = match (&self.resolver, descendable) {
            (_, true) | (None, false) => default_decision(descendable),
            (Some(resolve), false) => match resolve(conflict) {
                Resolution::Value(value) => NestedDecision::Value(value),
                Resolution::Delete => NestedDecision::Delete,
                Resolution::Unresolved => NestedDecision::Unresolved,
            },
        };
        ready(Ok(decision))
    }
}

struct SyncPolicyDriver<'p, S: Store, P: ?Sized> {
    prolly: &'p Prolly<S>,
    policy: &'p P,
}

impl<S: Store, P: NestedMergePolicy<S> + ?Sized> ConflictDriver for SyncPolicyDriver<'_, S, P> {
    fn on_conflict<'a>(
        &'a self,
        conflict: &'a NestedConflict,
        descendable: bool,
    ) -> impl Future<Output = Result<NestedDecision, Error>> + Send + 'a {
        // Evaluated before returning so the future captures no manager state;
        // the sync engine drives the loop on a ready store.
        ready(self.policy.on_conflict(self.prolly, conflict, descendable))
    }
}

struct AsyncPolicyDriver<'p, S: AsyncStore, P: ?Sized> {
    prolly: &'p AsyncProlly<S>,
    policy: &'p P,
}

impl<S, P> ConflictDriver for AsyncPolicyDriver<'_, S, P>
where
    S: AsyncStore,
    S::Error: Send + Sync,
    P: AsyncNestedMergePolicy<S> + ?Sized,
{
    fn on_conflict<'a>(
        &'a self,
        conflict: &'a NestedConflict,
        descendable: bool,
    ) -> impl Future<Output = Result<NestedDecision, Error>> + Send + 'a {
        self.policy.on_conflict(self.prolly, conflict, descendable)
    }
}

fn decode_tree_ref(value: &[u8]) -> Result<Option<TreeRef>, Error> {
    Ok(match value_ref_view_from_stored_bytes(value)? {
        ValueRefView::Tree(reference) => Some(reference),
        ValueRefView::Inline(_) | ValueRefView::Blob { .. } => None,
    })
}

/// Return both references when the pair can be compared as trees.
fn same_format_trees(old: &[u8], new: &[u8]) -> Result<Option<(TreeRef, TreeRef)>, Error> {
    let (Some(old), Some(new)) = (decode_tree_ref(old)?, decode_tree_ref(new)?) else {
        return Ok(None);
    };
    Ok((old.format_bytes() == new.format_bytes()).then_some((old, new)))
}

/// Child trees for a conflict that can be settled by merging subtrees.
///
/// A base value that is absent, not a tree, or in another format is replaced
/// by an empty tree in the children's format, which makes the child merge a
/// two-way union of both sides.
fn conflict_child_trees(
    conflict: &Conflict,
    runtime: &RuntimeConfig,
) -> Result<Option<(Tree, Tree, Tree)>, Error> {
    let (Some(left), Some(right)) = (&conflict.left, &conflict.right) else {
        return Ok(None);
    };
    let Some((left, right)) = same_format_trees(left, right)? else {
        return Ok(None);
    };
    let base = match &conflict.base {
        Some(base) => {
            decode_tree_ref(base)?.filter(|base| base.format_bytes() == left.format_bytes())
        }
        None => None,
    };
    let left = left.to_tree(runtime.clone());
    let base = match base {
        Some(base) => base.to_tree(runtime.clone()),
        None => Tree {
            root: None,
            config: left.config.clone(),
        },
    };
    Ok(Some((base, left, right.to_tree(runtime.clone()))))
}

struct DiffFrame {
    path: Vec<Vec<u8>>,
    diffs: vec::IntoIter<Diff>,
}

struct MergeFrame {
    path: Vec<Vec<u8>>,
    base: Tree,
    left: Tree,
    right: Tree,
    conflicts: vec::IntoIter<Conflict>,
    /// Key of the conflict whose child merge is the frame above this one.
    pending: Option<Vec<u8>>,
    resolutions: HashMap<Vec<u8>, Resolution>,
}

impl<S> AsyncProlly<S>
where
    S: AsyncStore,
    S::Error: Send + Sync,
{
    /// Diff two trees, descending into changed tree-valued references.
    ///
    /// Returns differences in depth-first key order: the entries inside a
    /// changed child tree appear where its parent key would. See
    /// [`NestedDiff`] for which parent entries are expanded.
    pub async fn diff_recursive(
        &self,
        base: &Tree,
        other: &Tree,
    ) -> Result<Vec<NestedDiff>, Error> {
        let runtime = base.config.runtime.clone();
        let mut out = Vec::new();
        let mut stack = vec![DiffFrame {
            path: Vec::new(),
            diffs: self.diff(base, other).await?.into_iter(),
        }];
        while let Some(frame) = stack.last_mut() {
            let Some(diff) = frame.diffs.next() else {
                stack.pop();
                continue;
            };
            if let Diff::Changed { key, old, new } = &diff {
                if let Some((old, new)) = same_format_trees(old, new)? {
                    let mut path = frame.path.clone();
                    path.push(key.clone());
                    let diffs = self
                        .diff(&old.to_tree(runtime.clone()), &new.to_tree(runtime.clone()))
                        .await?;
                    stack.push(DiffFrame {
                        path,
                        diffs: diffs.into_iter(),
                    });
                    continue;
                }
            }
            out.push(NestedDiff {
                path: frame.path.clone(),
                diff,
            });
        }
        Ok(out)
    }

    /// Three-way merge that merges conflicting child trees recursively.
    ///
    /// A conflict whose left and right values are both tree references with
    /// the same persisted format is settled by merging those child trees and
    /// storing a reference to the result. The base child is the base value
    /// when it is a tree in the same format, otherwise an empty tree. Every
    /// other conflict goes to `resolver`; without one, or when it returns
    /// [`Resolution::Unresolved`], the merge fails with
    /// [`Error::NestedConflict`] carrying the conflict's parent-key path.
    pub async fn merge_recursive(
        &self,
        base: &Tree,
        left: &Tree,
        right: &Tree,
        resolver: Option<NestedResolver>,
    ) -> Result<Tree, Error> {
        self.merge_recursive_driven(base, left, right, &ResolverDriver { resolver })
            .await
    }

    /// Recursive three-way merge whose conflicts are decided by `policy`.
    ///
    /// See [`NestedMergePolicy`] for when the policy is called and what its
    /// decisions mean.
    pub async fn merge_recursive_with<P>(
        &self,
        base: &Tree,
        left: &Tree,
        right: &Tree,
        policy: &P,
    ) -> Result<Tree, Error>
    where
        P: AsyncNestedMergePolicy<S> + ?Sized,
    {
        let driver = AsyncPolicyDriver {
            prolly: self,
            policy,
        };
        self.merge_recursive_driven(base, left, right, &driver)
            .await
    }

    async fn merge_recursive_driven<D: ConflictDriver>(
        &self,
        base: &Tree,
        left: &Tree,
        right: &Tree,
        driver: &D,
    ) -> Result<Tree, Error> {
        let runtime = base.config.runtime.clone();
        let mut stack = vec![
            self.merge_frame(Vec::new(), base.clone(), left.clone(), right.clone())
                .await?,
        ];
        loop {
            let Some(frame) = stack.last_mut() else {
                unreachable!("the root merge frame returns before the stack empties");
            };
            if let Some(conflict) = frame.conflicts.next() {
                let children = conflict_child_trees(&conflict, &runtime)?;
                let nested = NestedConflict {
                    path: frame.path.clone(),
                    conflict,
                };
                let decision = driver.on_conflict(&nested, children.is_some()).await?;
                let resolution = match decision {
                    NestedDecision::Descend => {
                        let Some((child_base, child_left, child_right)) = children else {
                            return Err(Error::InvalidFormat(
                                "merge policy chose to descend into a conflict that is not a \
                                 same-format tree pair"
                                    .to_string(),
                            ));
                        };
                        let NestedConflict { mut path, conflict } = nested;
                        path.push(conflict.key.clone());
                        frame.pending = Some(conflict.key);
                        let child = self
                            .merge_frame(path, child_base, child_left, child_right)
                            .await?;
                        stack.push(child);
                        continue;
                    }
                    NestedDecision::Value(value) => Resolution::Value(value),
                    NestedDecision::Delete => Resolution::Delete,
                    NestedDecision::Unresolved => {
                        return Err(Error::NestedConflict(Box::new(nested)));
                    }
                };
                frame.resolutions.insert(nested.conflict.key, resolution);
                continue;
            }

            let Some(frame) = stack.pop() else {
                unreachable!("a merge frame was just inspected");
            };
            let merged = self.finish_merge_frame(frame).await?;
            let Some(parent) = stack.last_mut() else {
                return Ok(merged);
            };
            let Some(key) = parent.pending.take() else {
                unreachable!("a child merge frame always has a pending parent key");
            };
            let reference = ValueRef::Tree(TreeRef::try_from(&merged)?).to_bytes();
            parent.resolutions.insert(key, Resolution::Value(reference));
        }
    }

    async fn merge_frame(
        &self,
        path: Vec<Vec<u8>>,
        base: Tree,
        left: Tree,
        right: Tree,
    ) -> Result<MergeFrame, Error> {
        let trivial = left.root == right.root || left.root == base.root || right.root == base.root;
        let conflicts = if trivial {
            Vec::new()
        } else {
            AsyncConflictIter::new(self, &base, &left, &right)
                .collect()
                .await?
        };
        Ok(MergeFrame {
            path,
            base,
            left,
            right,
            conflicts: conflicts.into_iter(),
            pending: None,
            resolutions: HashMap::new(),
        })
    }

    async fn finish_merge_frame(&self, frame: MergeFrame) -> Result<Tree, Error> {
        let MergeFrame {
            path,
            base,
            left,
            right,
            resolutions,
            ..
        } = frame;
        let resolver: Option<Resolver> = if resolutions.is_empty() {
            None
        } else {
            Some(Box::new(move |conflict: &Conflict| {
                resolutions
                    .get(&conflict.key)
                    .cloned()
                    .unwrap_or(Resolution::Unresolved)
            }))
        };
        match self.merge(&base, &left, &right, resolver).await {
            Err(Error::Conflict(conflict)) => {
                Err(Error::NestedConflict(Box::new(NestedConflict {
                    path,
                    conflict,
                })))
            }
            result => result,
        }
    }
}

impl<S: Store> Prolly<S> {
    /// Diff two trees, descending into changed tree-valued references.
    ///
    /// This mirrors [`AsyncProlly::diff_recursive`]. See [`NestedDiff`] for
    /// which parent entries are expanded.
    ///
    /// # Example
    /// ```
    /// use prolly::{Config, Diff, MemStore, Prolly, TreeRef, ValueRef};
    ///
    /// let prolly = Prolly::new(MemStore::new(), Config::default());
    /// let child = prolly.put(&prolly.create(), b"a".to_vec(), b"1".to_vec()).unwrap();
    /// let edited = prolly.put(&child, b"a".to_vec(), b"2".to_vec()).unwrap();
    /// let reference = |tree| ValueRef::Tree(TreeRef::try_from(tree).unwrap()).to_bytes();
    /// let before = prolly.put(&prolly.create(), b"dir".to_vec(), reference(&child)).unwrap();
    /// let after = prolly.put(&before, b"dir".to_vec(), reference(&edited)).unwrap();
    ///
    /// let diffs = prolly.diff_recursive(&before, &after).unwrap();
    /// assert_eq!(diffs.len(), 1);
    /// assert_eq!(diffs[0].path, vec![b"dir".to_vec()]);
    /// assert_eq!(
    ///     diffs[0].diff,
    ///     Diff::Changed { key: b"a".to_vec(), old: b"1".to_vec(), new: b"2".to_vec() }
    /// );
    /// ```
    pub fn diff_recursive(&self, base: &Tree, other: &Tree) -> Result<Vec<NestedDiff>, Error> {
        let ready_store = self.engine.store.clone();
        let future = self.engine.diff_recursive(base, other);
        engine::ready::run_ready(ready_store.ready(future))
    }

    /// Three-way merge that merges conflicting child trees recursively.
    ///
    /// This mirrors [`AsyncProlly::merge_recursive`], including its conflict
    /// rules and [`Error::NestedConflict`] reporting.
    ///
    /// # Example
    /// ```
    /// use prolly::{Config, MemStore, Prolly, TreeRef, ValueRef};
    ///
    /// let prolly = Prolly::new(MemStore::new(), Config::default());
    /// let reference = |tree| ValueRef::Tree(TreeRef::try_from(tree).unwrap()).to_bytes();
    /// let child = prolly.put(&prolly.create(), b"a".to_vec(), b"1".to_vec()).unwrap();
    /// let base = prolly.put(&prolly.create(), b"dir".to_vec(), reference(&child)).unwrap();
    ///
    /// // Both branches edit different keys inside the same child tree.
    /// let left_child = prolly.put(&child, b"b".to_vec(), b"2".to_vec()).unwrap();
    /// let right_child = prolly.put(&child, b"c".to_vec(), b"3".to_vec()).unwrap();
    /// let left = prolly.put(&base, b"dir".to_vec(), reference(&left_child)).unwrap();
    /// let right = prolly.put(&base, b"dir".to_vec(), reference(&right_child)).unwrap();
    ///
    /// let merged = prolly.merge_recursive(&base, &left, &right, None).unwrap();
    /// let Some(ValueRef::Tree(found)) = prolly.get_value_ref(&merged, b"dir").unwrap() else {
    ///     panic!("expected a tree reference");
    /// };
    /// let merged_child = found.to_tree(Default::default());
    /// assert_eq!(prolly.get(&merged_child, b"b").unwrap(), Some(b"2".to_vec()));
    /// assert_eq!(prolly.get(&merged_child, b"c").unwrap(), Some(b"3".to_vec()));
    /// ```
    pub fn merge_recursive(
        &self,
        base: &Tree,
        left: &Tree,
        right: &Tree,
        resolver: Option<NestedResolver>,
    ) -> Result<Tree, Error> {
        let ready_store = self.engine.store.clone();
        let future = self.engine.merge_recursive(base, left, right, resolver);
        engine::ready::run_ready(ready_store.ready(future))
    }

    /// Recursive three-way merge whose conflicts are decided by `policy`.
    ///
    /// This mirrors [`AsyncProlly::merge_recursive_with`]. See
    /// [`NestedMergePolicy`] for when the policy is called and what its
    /// decisions mean.
    ///
    /// # Example
    /// ```
    /// use prolly::{
    ///     Config, Error, MemStore, NestedConflict, NestedDecision, NestedMergePolicy, Prolly,
    /// };
    ///
    /// /// Keep the left side of every conflict instead of descending.
    /// struct PreferLeft;
    ///
    /// impl NestedMergePolicy<MemStore> for PreferLeft {
    ///     fn on_conflict(
    ///         &self,
    ///         _prolly: &Prolly<MemStore>,
    ///         conflict: &NestedConflict,
    ///         _descendable: bool,
    ///     ) -> Result<NestedDecision, Error> {
    ///         Ok(match &conflict.conflict.left {
    ///             Some(value) => NestedDecision::Value(value.clone()),
    ///             None => NestedDecision::Delete,
    ///         })
    ///     }
    /// }
    ///
    /// let prolly = Prolly::new(MemStore::new(), Config::default());
    /// let base = prolly.put(&prolly.create(), b"k".to_vec(), b"base".to_vec()).unwrap();
    /// let left = prolly.put(&base, b"k".to_vec(), b"left".to_vec()).unwrap();
    /// let right = prolly.put(&base, b"k".to_vec(), b"right".to_vec()).unwrap();
    ///
    /// let merged = prolly.merge_recursive_with(&base, &left, &right, &PreferLeft).unwrap();
    /// assert_eq!(prolly.get(&merged, b"k").unwrap(), Some(b"left".to_vec()));
    /// ```
    pub fn merge_recursive_with<P>(
        &self,
        base: &Tree,
        left: &Tree,
        right: &Tree,
        policy: &P,
    ) -> Result<Tree, Error>
    where
        P: NestedMergePolicy<S> + ?Sized,
    {
        let driver = SyncPolicyDriver {
            prolly: self,
            policy,
        };
        let ready_store = self.engine.store.clone();
        let future = self
            .engine
            .merge_recursive_driven(base, left, right, &driver);
        engine::ready::run_ready(ready_store.ready(future))
    }
}
