//! What the header walk shares across its readers: the document-scoped memo, and the decline
//! plumbing a fault is classified with.
//!
//! Both live below every reader rather than beside one of them. `image` builds the memo and
//! threads it through the walk, `keywords` assembles chains into it and `attributes` classifies
//! its faults through the same three constructors, so a home inside any one of those three
//! would put the other two behind it. The dependency runs one way and nothing here reaches back
//! up.

use crate::error::Error;
use crate::header::{DeclineClass, DeclineReason};
use crate::metadata::{
    Cfa, DisplayFunction, Keyword, KeywordOrigin, Property, PropertyScope, Resolution,
};
use std::sync::Arc;

/// What an assembled `CONTINUE` chain is a function of, and nothing else: the opening record's
/// reported origin, then the node of that record and of every continuation folded into it, in
/// the order they were folded. See [`close_chain`].
pub(super) type ChainKey = (KeywordOrigin, Vec<usize>);

/// Every element this walk reads more than once, read once for the **whole document**.
///
/// A memo scoped to one function call reads a node once per *call*, which is once per image —
/// so a root `<ColorFilterArray>`, `<FITSKeyword>` or `<Property>` referenced by N distinct
/// images was still read N times, and the two caps that bound that do not multiply
/// (§ intentional-patterns, *Header-derived text is `Arc<str>`*). One cache threaded through
/// the walk is what makes "read once and shared" mean once per document.
///
/// The key is the `Doc` node index alone, except for a `Property`, whose reported `scope` is
/// the scope of the element it attaches *to* — a root property reached from `<Metadata>` and
/// again from inside an image is two different reported values, so the scope is part of its
/// key.
///
/// **Consulted only for reference-reached nodes.** A direct child appears once by
/// construction, so memoizing those is pure overhead on the element-heavy shapes — 40 000
/// distinct properties, 80 000 distinct keywords — and measurably so. Duplication is what
/// `Reference` introduces, which is where the gate sits.
#[derive(Default)]
pub(super) struct Cache {
    pub(super) keywords: std::collections::HashMap<usize, Keyword>,
    /// Assembled `CONTINUE` chains, keyed on the records each one is assembled from — see
    /// [`close_chain`], which is where the reason a per-record memo does not cover this is
    /// written down.
    pub(super) chains: std::collections::HashMap<ChainKey, Keyword>,
    pub(super) properties: std::collections::HashMap<(usize, PropertyScope), Option<Property>>,
    pub(super) cfa: std::collections::HashMap<usize, Option<Cfa>>,
    pub(super) resolution: std::collections::HashMap<usize, Resolution>,
    pub(super) display_function: std::collections::HashMap<usize, DisplayFunction>,
}

/// Read `node` once per document: `read` runs on a miss and its result is memoized, and a hit
/// is a clone of the built value — every text inside one is an `Arc`, so that is a refcount.
///
/// `referenced` is the gate [`Cache`] describes: a directly-attached node skips the map
/// entirely rather than paying an insert for a lookup that can never hit.
pub(super) fn memoized<K, V: Clone>(
    map: &mut std::collections::HashMap<K, V>,
    key: K,
    referenced: bool,
    read: impl FnOnce() -> V,
) -> V
where
    K: std::hash::Hash + Eq,
{
    if !referenced {
        return read();
    }
    if let Some(shared) = map.get(&key) {
        return shared.clone();
    }
    let fresh = read();
    map.insert(key, fresh.clone());
    fresh
}

// ------------------------------------------------------------------ decline plumbing

pub(super) fn malformed(reason: impl Into<Arc<str>>) -> DeclineReason {
    DeclineReason::new(DeclineClass::Malformed, reason)
}

pub(super) fn unsupported(reason: impl Into<Arc<str>>) -> DeclineReason {
    DeclineReason::new(DeclineClass::Unsupported, reason)
}

/// Carry a parse helper's own classification through to the declined position.
pub(super) fn decline_from(error: Error) -> DeclineReason {
    match error {
        Error::Unsupported(reason) => unsupported(reason),
        Error::LimitExceeded(reason) => DeclineReason::new(DeclineClass::LimitExceeded, reason),
        Error::ChecksumMismatch(reason) => {
            DeclineReason::new(DeclineClass::ChecksumMismatch, reason)
        }
        other => malformed(other.to_string()),
    }
}
