#![cfg(test)]
#![allow(missing_docs)]

//! Loading components synthesized from WIT: a core module stub (every
//! export traps) encoded with a world's metadata. A stub can come from a
//! smaller world than the metadata, which models a guest that does not use
//! every import its world declares.

use std::collections::HashMap;
use std::time::Duration;

use wasmtime::component::{Linker, ResourceType, Val};
use wit_component::{ComponentEncoder, StringEncoding};
use wit_parser::{LiftLowerAbi, ManglingAndAbi, Resolve};
use witgraph_ir::NodeId;
use witgraph_ir::{ComponentContract, ComponentRef, Graph};
use witgraph_runtime::{
    Host, HostState, NodeFault, NodePhase, RuntimeConfig, RuntimeError, RuntimeGraph, TickResult,
};

/// A component with `world`'s metadata whose core module imports only
/// what `module_world` imports.
fn component(wit: &str, world: &str, module_world: &str) -> Vec<u8> {
    let mut resolve = Resolve {
        all_features: true,
        ..Resolve::default()
    };
    let package = resolve.push_str("test.wit", wit).unwrap();
    let world = resolve.select_world(&[package], Some(world)).unwrap();
    let module_world = resolve
        .select_world(&[package], Some(module_world))
        .unwrap();
    let mut module = wit_component::dummy_module(
        &resolve,
        module_world,
        ManglingAndAbi::Legacy(LiftLowerAbi::Sync),
    );
    wit_component::embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8)
        .unwrap();
    ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .encode()
        .unwrap()
}

/// The contract lowered from `world` in `wit`.
fn contract(wit: &str, world: &str) -> ComponentContract {
    let source = witgraph_wit::load::load_str("test.wit", wit).unwrap();
    witgraph_wit::lower::lower(&source)
        .unwrap()
        .into_iter()
        .map(|lowered| lowered.contract)
        .find(|c| c.id.world == world)
        .unwrap()
}

/// A host that links every node's capabilities with one function.
struct Linking<F>(F);

impl<F> Host for Linking<F>
where
    F: Fn(&mut Linker<HostState>) -> wasmtime::Result<()> + 'static,
{
    type Data = HostState;

    fn link(
        &self,
        _node: &NodeId,
        _contract: &ComponentContract,
        linker: &mut Linker<HostState>,
    ) -> wasmtime::Result<()> {
        (self.0)(linker)
    }

    fn island_data(&self, _members: &[NodeId], state: HostState) -> wasmtime::Result<HostState> {
        Ok(state)
    }
}

/// A one-node graph over `contract`, with `bytes` as its component.
async fn load<F>(
    contract: &ComponentContract,
    bytes: &[u8],
    linker: F,
) -> Result<RuntimeGraph<witgraph_runtime::Perf, Linking<F>>, RuntimeError>
where
    F: Fn(&mut Linker<HostState>) -> wasmtime::Result<()> + 'static,
{
    let graph = Graph::builder("t")
        .add_component(contract)
        .add_node("n", contract.id.clone())
        .build();
    let compiled = graph
        .compile(std::slice::from_ref(contract))
        .expect("compiles");
    let wasm: HashMap<ComponentRef, &[u8]> = HashMap::from([(contract.id.clone(), bytes)]);
    RuntimeGraph::load_with_host(
        compiled,
        &wasm,
        RuntimeConfig::default(),
        witgraph_runtime::Perf,
        Linking(linker),
    )
    .await
}

fn names(contract: &ComponentContract) -> Vec<&str> {
    contract
        .capabilities
        .iter()
        .map(|c| c.interface.as_str())
        .collect()
}

fn no_capabilities(_: &mut Linker<HostState>) -> wasmtime::Result<()> {
    Ok(())
}

const NODE: &str = "export node: interface {
        record outputs { out: u32 }
        run: func() -> outputs;
    }";

#[tokio::test]
async fn a_named_node_interface_loads_and_runs() {
    let wit = "package demo:named@0.1.0;
        interface node {
            record outputs { out: u32 }
            run: func() -> outputs;
        }
        world w { export node; }";
    let bytes = component(wit, "w", "w");
    let mut rt = load(&contract(wit, "w"), &bytes, no_capabilities)
        .await
        .expect("the export is found under the interface's id");
    let tick = tokio::time::timeout(Duration::from_secs(30), rt.tick())
        .await
        .unwrap();
    assert!(matches!(tick, TickResult::Progress), "{tick:?}");
    assert!(
        matches!(
            rt.node_state(&"n".into()).unwrap().fault_cause(),
            Some(NodeFault::WasmTrap { .. })
        ),
        "`run` was called (the stub traps)"
    );
}

#[tokio::test]
async fn an_inline_capability_is_linked_by_its_import_name() {
    let wit = format!(
        "package demo:inline@0.1.0;
        world w {{
            import config: interface {{ get: func() -> u32; }}
            {NODE}
        }}"
    );
    let contract = contract(&wit, "w");
    assert_eq!(names(&contract), ["config"]);
    let bytes = component(&wit, "w", "w");

    let err = load(&contract, &bytes, no_capabilities)
        .await
        .err()
        .expect("`config` is not provided");
    assert!(matches!(err, RuntimeError::Instantiation { .. }), "{err}");
    load(&contract, &bytes, |linker| {
        linker
            .instance("config")?
            .func_wrap("get", |_, (): ()| Ok((7u32,)))?;
        Ok(())
    })
    .await
    .expect("the capability's name is what the host links");
}

#[tokio::test]
async fn imports_the_guest_does_not_use_are_not_required() {
    let wit = format!(
        "package demo:unused@0.1.0;
        interface clock {{ now: func() -> u64; }}
        world w {{
            import clock;
            {NODE}
        }}
        world bare {{
            {NODE}
        }}"
    );
    let contract = contract(&wit, "w");
    assert_eq!(
        names(&contract),
        ["demo:unused/clock@0.1.0"],
        "the world declares the capability"
    );
    let bytes = component(&wit, "w", "bare");
    load(&contract, &bytes, no_capabilities)
        .await
        .expect("the component never imports `clock`");
}

#[tokio::test]
async fn an_interface_used_only_for_types_is_not_required() {
    let wit = "package demo:types@0.1.0;
        interface clock {
            type instant = u64;
            now: func() -> instant;
        }
        world w {
            export node: interface {
                use clock.{instant};
                record outputs { out: instant }
                run: func() -> outputs;
            }
        }
        world bare {
            export node: interface {
                record outputs { out: u64 }
                run: func() -> outputs;
            }
        }";
    let bytes = component(wit, "w", "bare");
    load(&contract(wit, "w"), &bytes, no_capabilities)
        .await
        .expect("the component imports only `clock`'s types");
}

#[tokio::test]
async fn an_import_the_contract_does_not_declare_is_rejected() {
    let wit = format!(
        "package demo:undeclared@0.1.0;
        interface clock {{ now: func() -> u64; }}
        world w {{
            import clock;
            {NODE}
        }}
        world bare {{
            {NODE}
        }}"
    );
    // Compiled against `bare`, but the bytes import (and use) `clock`.
    let bytes = component(&wit, "w", "w");
    let err = load(&contract(&wit, "bare"), &bytes, no_capabilities)
        .await
        .err()
        .expect("the bytes need more than the contract declares");
    assert!(
        matches!(err, RuntimeError::ContractMismatch { ref message, .. } if message.contains("demo:undeclared/clock@0.1.0")),
        "{err}"
    );
}

#[tokio::test]
async fn a_capability_imported_with_another_signature_is_rejected() {
    let wit = |result: &str| {
        format!(
            "package demo:sig@0.1.0;
            world w {{
                import config: interface {{ get: func() -> {result}; }}
                {NODE}
            }}"
        )
    };
    // Compiled against `get -> u32`, but the bytes import `get -> string`.
    let bytes = component(&wit("string"), "w", "w");
    let err = load(&contract(&wit("u32"), "w"), &bytes, no_capabilities)
        .await
        .err()
        .expect("the signatures differ");
    assert!(
        matches!(err, RuntimeError::ContractMismatch { ref message, .. } if message.contains("`get`")),
        "{err}"
    );
}

#[tokio::test]
async fn capability_signatures_may_use_maps() {
    let wit = format!(
        "package demo:maps@0.1.0;
        world w {{
            import get: func() -> map<string, u32>;
            {NODE}
        }}"
    );
    let bytes = component(&wit, "w", "w");
    let err = load(&contract(&wit, "w"), &bytes, no_capabilities)
        .await
        .err()
        .expect("`get` is not provided");
    assert!(
        matches!(err, RuntimeError::Instantiation { .. }),
        "the engine accepts the component; only the import is missing: {err}"
    );
}

#[tokio::test]
async fn a_resource_only_interface_is_a_capability() {
    let wit = "package demo:res@0.1.0;
        interface handles { resource token; }
        world w {
            import handles;
            export node: interface {
                record outputs { out: u32 }
                run: func() -> outputs;
            }
        }";
    let contract = contract(wit, "w");
    let graph = Graph::builder("t")
        .add_component(&contract)
        .add_node("n", contract.id.clone())
        .build();
    let compiled = graph.compile(std::slice::from_ref(&contract)).unwrap();
    let required: Vec<String> = compiled
        .required_capabilities()
        .into_iter()
        .map(|c| c.interface)
        .collect();
    assert_eq!(required, ["demo:res/handles@0.1.0"]);

    let bytes = component(wit, "w", "w");
    let err = load(&contract, &bytes, no_capabilities)
        .await
        .err()
        .expect("`token` has no implementation");
    assert!(matches!(err, RuntimeError::Instantiation { .. }), "{err}");
    struct Token;
    load(&contract, &bytes, |linker| {
        linker.instance("demo:res/handles@0.1.0")?.resource(
            "token",
            ResourceType::host::<Token>(),
            |_, _| Ok(()),
        )?;
        Ok(())
    })
    .await
    .expect("loads once the host implements the resource");
}

#[tokio::test]
async fn flags_are_kept_in_declaration_order() {
    let wit = "package demo:perms@0.1.0;
        world w {
            export node: interface {
                flags perms { write, read }
                record inputs { p: option<perms> }
                record outputs { out: u32 }
                run: func(inputs: inputs) -> outputs;
            }
        }";
    let bytes = component(wit, "w", "w");
    let mut rt = load(&contract(wit, "w"), &bytes, no_capabilities)
        .await
        .expect("loads");
    let flags = |names: &[&str]| Val::Flags(names.iter().map(|n| n.to_string()).collect());
    let err = rt
        .inject(&"n".into(), &"p".into(), flags(&["read", "read"]))
        .unwrap_err();
    assert!(matches!(err, RuntimeError::ValueType { .. }), "{err}");

    rt.inject(&"n".into(), &"p".into(), flags(&["read", "write"]))
        .unwrap();
    // The stub traps, which settles the island with nothing owed.
    let _ = tokio::time::timeout(Duration::from_secs(30), rt.tick()).await;
    let snapshot = rt.snapshot();
    assert_eq!(snapshot.inputs[&"n".into()][&"p".into()], "{write, read}");
    assert!(snapshot.islands.is_empty());

    rt.inject(&"n".into(), &"p".into(), flags(&["write", "read"]))
        .unwrap();
    assert!(
        rt.snapshot().islands.is_empty(),
        "the same set in another order is not a change"
    );
    rt.restore(&snapshot).expect("restores its own snapshot");
    let mut after = rt.snapshot();
    assert!(
        after.phases.values().all(|p| *p == NodePhase::Pending),
        "a restore drops guest state"
    );
    after.phases = snapshot.phases.clone();
    assert_eq!(after, snapshot);
}

#[tokio::test]
async fn a_semver_compatible_capability_version_is_accepted() {
    let wit = format!(
        "package demo:app@0.1.0;
        package demo:caps@0.1.0 {{
            interface clock {{ now: func() -> u64; }}
        }}
        package demo:caps@0.1.3 {{
            interface clock {{ now: func() -> u64; }}
        }}
        world w {{
            import demo:caps/clock@0.1.0;
            {NODE}
        }}
        world newer {{
            import demo:caps/clock@0.1.3;
            {NODE}
        }}"
    );
    // Compiled against `@0.1.0`; the bytes import the newer patch, as
    // wit-component does when a dependency pins it.
    let bytes = component(&wit, "newer", "newer");
    load(&contract(&wit, "w"), &bytes, |linker| {
        linker
            .instance("demo:caps/clock@0.1.0")?
            .func_wrap("now", |_, (): ()| Ok((1u64,)))?;
        Ok(())
    })
    .await
    .expect("the host's 0.1.0 satisfies a 0.1.3 import");
}

#[tokio::test]
async fn a_world_resource_is_a_capability() {
    let wit = format!(
        "package demo:worldres@0.1.0;
        world w {{
            resource r;
            {NODE}
        }}"
    );
    let contract = contract(&wit, "w");
    assert_eq!(names(&contract), ["resource:r"]);
    let bytes = component(&wit, "w", "w");
    let err = load(&contract, &bytes, no_capabilities)
        .await
        .err()
        .expect("`r` has no implementation");
    assert!(matches!(err, RuntimeError::Instantiation { .. }), "{err}");
    struct R;
    load(&contract, &bytes, |linker| {
        linker
            .root()
            .resource("r", ResourceType::host::<R>(), |_, _| Ok(()))?;
        Ok(())
    })
    .await
    .expect("loads once the host implements `r`");
}

#[tokio::test]
async fn bytes_wit_parser_cannot_decode_are_a_bad_component() {
    // A valid component importing a component type: wit-parser's decoder
    // panics on it.
    let bytes =
        wat::parse_str(r#"(component (type $c (component)) (import "t" (type (eq $c))))"#).unwrap();
    let wit = format!("package demo:bad@0.1.0; world w {{ {NODE} }}");
    let err = load(&contract(&wit, "w"), &bytes, no_capabilities)
        .await
        .err()
        .expect("not a node");
    assert!(matches!(err, RuntimeError::BadComponent { .. }), "{err}");
}

#[tokio::test]
async fn resources_keep_their_identity_across_semver_compatible_versions() {
    let caps = |version: &str| {
        format!(
            "package demo:caps@{version} {{
                interface clock {{
                    resource timer;
                    start: func() -> timer;
                }}
            }}"
        )
    };
    let wit = format!(
        "package demo:app@0.1.0;
        {}
        {}
        world w {{
            import demo:caps/clock@0.1.0;
            {NODE}
        }}
        world newer {{
            import demo:caps/clock@0.1.3;
            {NODE}
        }}",
        caps("0.1.0"),
        caps("0.1.3")
    );
    let bytes = component(&wit, "newer", "newer");
    struct Timer;
    load(&contract(&wit, "w"), &bytes, |linker| {
        let mut clock = linker.instance("demo:caps/clock@0.1.0")?;
        clock.resource("timer", ResourceType::host::<Timer>(), |_, _| Ok(()))?;
        clock.func_wrap("start", |_, (): ()| {
            Ok((wasmtime::component::Resource::<Timer>::new_own(0),))
        })?;
        Ok(())
    })
    .await
    .expect("`own<timer>` is the same resource at 0.1.0 and 0.1.3");
}

#[tokio::test]
async fn a_merged_import_is_checked_against_the_newest_compatible_declaration() {
    let wit = format!(
        "package demo:app@0.1.0;
        package demo:caps@0.1.0 {{
            interface clock {{ now: func() -> u64; }}
        }}
        package demo:caps@0.1.2 {{
            interface clock {{ now: func() -> u64; later: func() -> u64; }}
        }}
        package demo:caps@0.1.3 {{
            interface clock {{ now: func() -> u64; later: func() -> u64; }}
        }}
        world w {{
            import demo:caps/clock@0.1.0;
            import demo:caps/clock@0.1.2;
            {NODE}
        }}
        world newer {{
            import demo:caps/clock@0.1.3;
            {NODE}
        }}"
    );
    let bytes = component(&wit, "newer", "newer");
    load(&contract(&wit, "w"), &bytes, |linker| {
        linker
            .instance("demo:caps/clock@0.1.0")?
            .func_wrap("now", |_, (): ()| Ok((1u64,)))?;
        let mut newer = linker.instance("demo:caps/clock@0.1.2")?;
        newer.func_wrap("now", |_, (): ()| Ok((1u64,)))?;
        newer.func_wrap("later", |_, (): ()| Ok((2u64,)))?;
        Ok(())
    })
    .await
    .expect("`later` is declared at 0.1.2, the newest compatible version");
}

#[tokio::test]
async fn resource_methods_and_async_functions_match_their_source() {
    let wit = format!(
        "package demo:methods@0.1.0;
        interface files {{
            resource file {{
                constructor(name: string);
                read: func(len: u32) -> list<u8>;
                open: static func(name: string) -> file;
            }}
            fetch: async func(url: string) -> list<u8>;
        }}
        world w {{
            import files;
            {NODE}
        }}"
    );
    let contract = contract(&wit, "w");
    assert_eq!(names(&contract), ["demo:methods/files@0.1.0"]);
    let bytes = component(&wit, "w", "w");
    // Linking is the host's business; what matters is that the items
    // lowered from the bytes match those lowered from the source.
    let err = load(&contract, &bytes, no_capabilities)
        .await
        .err()
        .expect("nothing implements `files`");
    assert!(matches!(err, RuntimeError::Instantiation { .. }), "{err}");
}

#[tokio::test]
async fn growing_memory_past_the_island_limit_faults() {
    let wit = format!("package demo:grow@0.1.0; world w {{ {NODE} }}");
    let mut resolve = Resolve::default();
    let package = resolve.push_str("test.wit", &wit).unwrap();
    let world = resolve.select_world(&[package], Some("w")).unwrap();
    // `run` grows its memory by one page, then by 64 MiB.
    let mut module = wat::parse_str(
        r#"(module
            (memory (export "memory") 1)
            (func (export "node#run") (result i32)
                (drop (memory.grow (i32.const 1)))
                (memory.grow (i32.const 1024))))"#,
    )
    .unwrap();
    wit_component::embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8)
        .unwrap();
    let bytes = ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .encode()
        .unwrap();
    let contract = contract(&wit, "w");
    let graph = Graph::builder("t")
        .add_component(&contract)
        .add_node("n", contract.id.clone())
        .build();
    let compiled = graph
        .compile(std::slice::from_ref(&contract))
        .expect("compiles");
    let wasm: HashMap<ComponentRef, &[u8]> = HashMap::from([(contract.id.clone(), &bytes[..])]);
    let config = RuntimeConfig {
        max_island_memory: Some(4 << 20),
        ..RuntimeConfig::default()
    };
    let mut rt = RuntimeGraph::load(compiled, &wasm, config, witgraph_runtime::Perf)
        .await
        .expect("loads");
    let tick = tokio::time::timeout(Duration::from_secs(20), rt.tick())
        .await
        .expect("the tick ends");
    assert!(matches!(tick, TickResult::Progress), "{tick:?}");
    let state = rt.node_state(&"n".into()).unwrap();
    assert_eq!(state.phase(), NodePhase::Faulted);
    assert!(
        matches!(
            state.fault_cause(),
            Some(NodeFault::MemoryLimit { limit, requested }) if *limit == 4 << 20 && *requested > *limit
        ),
        "{:?}",
        state.fault_cause()
    );
}

#[tokio::test]
async fn bytes_importing_what_no_node_imports_are_a_bad_component() {
    let wit = format!("package demo:shapes@0.1.0; world w {{ {NODE} }}");
    let contract = contract(&wit, "w");
    for (what, wat) in [
        ("a core module", r#"(component (import "m" (core module)))"#),
        (
            "a nested component",
            r#"(component (import "c" (component)))"#,
        ),
        (
            "a function type inside an instance",
            r#"(component (import "i" (instance (type $f (func)) (export "t" (type (eq $f))))))"#,
        ),
    ] {
        let bytes = wat::parse_str(wat).unwrap();
        let err = load(&contract, &bytes, no_capabilities)
            .await
            .err()
            .expect(what);
        assert!(
            matches!(err, RuntimeError::BadComponent { .. }),
            "{what}: {err}"
        );
    }
    let err = load(&contract, b"\0asm not really", no_capabilities)
        .await
        .err()
        .expect("not wasm");
    assert!(matches!(err, RuntimeError::BadComponent { .. }), "{err}");
}

#[tokio::test]
async fn bytes_without_one_node_world_are_a_bad_component() {
    let wit = "package demo:nonode@0.1.0;
        interface other { f: func(); }
        world node-world {
            export node: interface {
                record outputs { out: u32 }
                run: func() -> outputs;
            }
        }
        world plain { export other; }";
    let bytes = component(wit, "plain", "plain");
    let err = load(&contract(wit, "node-world"), &bytes, no_capabilities)
        .await
        .err()
        .expect("the bytes export no node");
    assert!(
        matches!(err, RuntimeError::BadComponent { ref message, .. } if message.contains("exactly one node world")),
        "{err}"
    );
}

#[tokio::test]
async fn bytes_with_another_run_kind_do_not_match() {
    let sync = format!("package demo:kind@0.1.0; world w {{ {NODE} }}");
    let async_wit = "package demo:kind@0.1.0;
        world w {
            export node: interface {
                record outputs { out: u32 }
                run: async func() -> outputs;
            }
        }";
    let bytes = component(&sync, "w", "w");
    let err = load(&contract(async_wit, "w"), &bytes, no_capabilities)
        .await
        .err()
        .expect("sync bytes, async contract");
    assert!(
        matches!(err, RuntimeError::ContractMismatch { ref message, .. } if message.contains("`run`")),
        "{err}"
    );
}

/// A component whose start function asks the host whether to fail, and
/// calls `fatal("boom")` when it should. Written by hand: wit-component
/// fills its import shims only after a core module's start function ran.
fn fails_on_start() -> (ComponentContract, Vec<u8>) {
    let wit = "package demo:startfatal@0.1.0;
        interface ctl { should-fail: func() -> bool; }
        world w {
            import ctl;
            export node: interface {
                record outputs { out: u32 }
                run: func() -> outputs;
            }
        }";
    let bytes = wat::parse_str(
        r#"(component
            (import "demo:startfatal/ctl@0.1.0" (instance $ctl
                (export "should-fail" (func (result bool)))))
            (import "witgraph:runtime/host@0.1.0" (instance $host
                (export "fatal" (func (param "message" string)))))
            (core module $mem (memory (export "memory") 1))
            (core instance $m (instantiate $mem))
            (alias core export $m "memory" (core memory $memory))
            (alias export $ctl "should-fail" (func $should_fail))
            (alias export $host "fatal" (func $fatal))
            (core func $sf (canon lower (func $should_fail)))
            (core func $fl (canon lower (func $fatal) (memory $memory)))
            (core module $main
                (import "ctl" "should-fail" (func $should_fail (result i32)))
                (import "host" "fatal" (func $fatal (param i32 i32)))
                (import "env" "memory" (memory 1))
                (data (i32.const 16) "boom")
                (func $start
                    (if (call $should_fail)
                        (then (call $fatal (i32.const 16) (i32.const 4)))))
                (start $start)
                (func (export "run") (result i32) (i32.const 7)))
            (core instance $i (instantiate $main
                (with "ctl" (instance (export "should-fail" (func $sf))))
                (with "host" (instance (export "fatal" (func $fl))))
                (with "env" (instance (export "memory" (memory $memory))))))
            (type $outputs (record (field "out" u32)))
            (func $run (result $outputs) (canon lift (core func $i "run")))
            (component $shim
                (type $rec (record (field "out" u32)))
                (import "import-type-rec" (type $r (eq $rec)))
                (import "import-func-run" (func $f (result $r)))
                (export $ro "outputs" (type $r))
                (export "run" (func $f) (func (result $ro))))
            (instance $node (instantiate $shim
                (with "import-type-rec" (type $outputs))
                (with "import-func-run" (func $run))))
            (export "node" (instance $node)))"#,
    )
    .unwrap();
    (contract(wit, "w"), bytes)
}

#[tokio::test]
async fn fatal_from_a_start_function_during_a_rebuild_aborts_the_tick() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let (contract, bytes) = fails_on_start();
    let fail = Arc::new(AtomicBool::new(false));
    let flag = fail.clone();
    let mut rt = load(&contract, &bytes, move |linker| {
        let flag = flag.clone();
        linker.instance("demo:startfatal/ctl@0.1.0")?.func_wrap(
            "should-fail",
            move |_: wasmtime::StoreContextMut<'_, HostState>, (): ()| {
                Ok((flag.load(Ordering::SeqCst),))
            },
        )?;
        Ok(())
    })
    .await
    .expect("loads while the start function succeeds");
    let tick = tokio::time::timeout(Duration::from_secs(20), rt.tick())
        .await
        .unwrap();
    assert!(matches!(tick, TickResult::Progress), "{tick:?}");
    assert_eq!(
        rt.read_output(&"n".into(), &"out".into()).unwrap(),
        Some(Val::U32(7))
    );

    fail.store(true, Ordering::SeqCst);
    rt.cancel(&"n".into()).unwrap();
    rt.rerun(&"n".into()).unwrap();
    let tick = tokio::time::timeout(Duration::from_secs(20), rt.tick())
        .await
        .unwrap();
    assert!(
        matches!(&tick, TickResult::Aborted { node, fault: NodeFault::Fatal { message } }
            if node.as_str() == "n" && message == "boom"),
        "{tick:?}: {:?}",
        rt.take_faults()
    );
    assert_eq!(
        rt.node_state(&"n".into()).unwrap().culprit(),
        Some(&"n".into())
    );
}

#[tokio::test]
async fn the_host_is_reachable_from_the_graph() {
    let wit = format!("package demo:hostref@0.1.0; world w {{ {NODE} }}");
    let contract = contract(&wit, "w");
    let bytes = component(&wit, "w", "w");
    let mut rt = load(&contract, &bytes, no_capabilities).await.unwrap();
    let _: &Linking<_> = rt.host();
    let _: &mut Linking<_> = rt.host_mut();
}
