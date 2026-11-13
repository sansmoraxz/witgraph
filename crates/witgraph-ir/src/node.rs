//! What a node manifest says a component is, whatever packs it: a
//! `*.node.json` beside its files, or the config of an OCI node artifact.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{WitRef, WorldRef};

/// What a component is: the WIT package its worlds are in, the worlds it
/// provides, and how an editor previews their outputs. Where its WIT and
/// bytes are is not said here: that is how it is packed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeDescription {
    /// The WIT package its worlds are in.
    pub wit: WitRef,
    /// Every world of `wit` it provides. A node runs one of them; a link's
    /// provider need not export `node`.
    pub worlds: Vec<WorldRef>,
    /// How an editor previews outputs: per world, output port to viewer.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub viewers: BTreeMap<WorldRef, BTreeMap<String, String>>,
}
