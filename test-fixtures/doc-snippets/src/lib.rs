//! The Rust snippets of the README and docs, compiled and never run, so a snippet
//! that no longer fits the code fails the build. The names a snippet takes for
//! granted are the stand-ins below.

#![allow(
    missing_docs,
    dead_code,
    unused_variables,
    unused_mut,
    unused_imports,
    unused_assignments,
    unreachable_code,
    clippy::all
)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use tokio::time::sleep;
use wasmtime::Engine;
use wasmtime::component::{Linker, ResourceDynamic, Val};
use witgraph_ir::{
    Capability, CompiledGraph, ComponentContract, ComponentEntry, ComponentName, Graph, NodeId,
    PortName,
};
use witgraph_runtime::Artifact;
use witgraph_runtime::{
    CapabilityPlugin, OutputReader, Perf, PluginData, Plugins, PortVersion, PreparedGraph,
    RunningGraph, RuntimeConfig, RuntimeGraph, StoreData, TickResult, Val as _, ValReader, shared,
};
use witgraph_source::{Digest, Resolver};

/// A capability, for the snippet that adds some.
struct Clock;

/// Another, named.
struct Store(&'static str);

macro_rules! plugin {
    ($name:ty) => {
        impl CapabilityPlugin for $name {
            fn id(&self) -> &str {
                unimplemented!()
            }

            fn provides(&self) -> Vec<String> {
                unimplemented!()
            }

            fn link(&self, _: &Capability, _: &mut Linker<PluginData>) -> wasmtime::Result<()> {
                unimplemented!()
            }
        }
    };
}

plugin!(Clock);
plugin!(Store);

include!(concat!(env!("OUT_DIR"), "/snippets.rs"));

#[test]
fn the_docs_hold_snippets() {
    assert!(SNIPPETS > 10, "found {SNIPPETS}: did their fences change?");
}
