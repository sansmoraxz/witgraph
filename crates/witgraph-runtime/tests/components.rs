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
    Host, HostState, LoadError, NodeFault, NodePhase, RuntimeConfig, RuntimeError, RuntimeGraph,
    TickResult,
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
        ManglingAndAbi::Legacy(LiftLowerAbi::AsyncCallback),
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
) -> Result<RuntimeGraph<witgraph_runtime::Perf, Linking<F>>, LoadError>
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
        run: async func() -> outputs;
    }";

#[tokio::test]
async fn a_named_node_interface_loads_and_runs() {
    let wit = "package demo:named@0.1.0;
        interface node {
            record outputs { out: u32 }
            run: async func() -> outputs;
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
    assert!(matches!(err, LoadError::Instantiation { .. }), "{err}");
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
async fn a_labelled_import_is_linked_by_its_label() {
    let wit = format!(
        "package demo:labels@0.1.0;
        interface clock {{ now: func() -> u64; }}
        world w {{
            import primary: clock;
            import backup: clock;
            {NODE}
        }}"
    );
    let contract = contract(&wit, "w");
    assert_eq!(names(&contract), ["backup", "primary"]);
    let bytes = component(&wit, "w", "w");

    let err = load(&contract, &bytes, |linker| {
        linker
            .instance("demo:labels/clock@0.1.0")?
            .func_wrap("now", |_, (): ()| Ok((1u64,)))?;
        Ok(())
    })
    .await
    .err()
    .expect("the interface's own name is not what the component imports");
    assert!(matches!(err, LoadError::Instantiation { .. }), "{err}");
    load(&contract, &bytes, |linker| {
        for label in ["primary", "backup"] {
            linker
                .instance(label)?
                .func_wrap("now", |_, (): ()| Ok((1u64,)))?;
        }
        Ok(())
    })
    .await
    .expect("each label is an instance of its own");
}

#[tokio::test]
async fn a_label_must_stand_for_the_contract_s_interface() {
    let wit = |interface: &str| {
        format!(
            "package demo:labels@0.1.0;
            interface clock {{ now: func() -> u64; }}
            interface timer {{ now: func() -> u64; }}
            world w {{
                import primary: {interface};
                {NODE}
            }}"
        )
    };
    let contract = contract(&wit("clock"), "w");
    let bytes = component(&wit("timer"), "w", "w");
    let err = load(&contract, &bytes, no_capabilities)
        .await
        .err()
        .expect("the bytes label another interface");
    assert!(
        matches!(&err, LoadError::ContractMismatch { message, .. }
            if message.contains("`primary` for `demo:labels/timer@0.1.0`")
                && message.contains("the contract for `demo:labels/clock@0.1.0`")),
        "{err}"
    );
}

#[tokio::test]
async fn a_providers_own_imports_become_the_nodes_capabilities() {
    use std::sync::{Arc, Mutex};
    use witgraph_ir::Capability;
    use witgraph_runtime::CapabilityGap;

    /// Records every capability it is asked for, and links `clock`.
    struct Asked(Arc<Mutex<Vec<String>>>);

    impl Host for Asked {
        type Data = HostState;

        fn link(
            &self,
            _: &NodeId,
            contract: &ComponentContract,
            linker: &mut Linker<HostState>,
        ) -> wasmtime::Result<()> {
            for capability in &contract.capabilities {
                linker
                    .instance(&capability.interface)?
                    .func_wrap("now", |_, (): ()| Ok((1u64,)))?;
            }
            Ok(())
        }

        fn island_data(&self, _: &[NodeId], state: HostState) -> wasmtime::Result<HostState> {
            Ok(state)
        }

        fn check(&self, _: &NodeId, capability: &Capability) -> Result<(), CapabilityGap> {
            self.0.lock().unwrap().push(capability.interface.clone());
            Ok(())
        }
    }

    let wit = format!(
        "package demo:linked@0.1.0;
        interface clock {{ now: func() -> u64; }}
        interface ops {{ twice: func(x: u32) -> u32; }}
        world provider {{
            import clock;
            export ops;
        }}
        world w {{
            import ops;
            {NODE}
        }}"
    );
    let contract = contract(&wit, "w");
    assert_eq!(names(&contract), ["demo:linked/ops@0.1.0"]);
    let node = component(&wit, "w", "w");
    let provider = component(&wit, "provider", "provider");
    let provider_id: ComponentRef = "demo:linked/provider@0.1.0".parse().unwrap();

    let graph = Graph::builder("t")
        .add_component(&contract)
        .add_node("n", contract.id.clone())
        .link(
            "l",
            "n",
            "demo:linked/ops@0.1.0",
            provider_id.clone(),
            "demo:linked/ops@0.1.0",
        )
        .build();
    let compiled = graph
        .compile(std::slice::from_ref(&contract))
        .expect("compiles");
    assert!(
        compiled.required_capabilities().is_empty(),
        "what the provider imports is not known before the load"
    );
    let wasm: HashMap<ComponentRef, &[u8]> = HashMap::from([
        (contract.id.clone(), node.as_slice()),
        (provider_id, provider.as_slice()),
    ]);
    let asked = Arc::new(Mutex::new(Vec::new()));
    RuntimeGraph::load_with_host(
        compiled,
        &wasm,
        RuntimeConfig::default(),
        witgraph_runtime::Perf,
        Asked(asked.clone()),
    )
    .await
    .expect("the host provides the clock the provider imports");
    assert_eq!(
        *asked.lock().unwrap(),
        ["demo:linked/clock@0.1.0"],
        "the node's `ops` is linked; the provider's `clock` is the host's"
    );
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
    // Hosts that check are asked for what the bytes import, not the world.
    let graph = Graph::builder("t")
        .add_component(&contract)
        .add_node("n", contract.id.clone())
        .build();
    let compiled = graph
        .compile(std::slice::from_ref(&contract))
        .expect("compiles");
    let wasm: HashMap<ComponentRef, &[u8]> = HashMap::from([(contract.id.clone(), &bytes[..])]);
    RuntimeGraph::load(
        compiled,
        &wasm,
        RuntimeConfig::default(),
        witgraph_runtime::Perf,
    )
    .await
    .expect("`NoCapabilities` is not asked for `clock`");
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
                run: async func() -> outputs;
            }
        }
        world bare {
            export node: interface {
                record outputs { out: u64 }
                run: async func() -> outputs;
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
        matches!(err, LoadError::ContractMismatch { ref message, .. } if message.contains("demo:undeclared/clock@0.1.0")),
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
        matches!(err, LoadError::ContractMismatch { ref message, .. } if message.contains("`get`")),
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
        matches!(err, LoadError::Instantiation { .. }),
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
                run: async func() -> outputs;
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
    assert!(matches!(err, LoadError::Instantiation { .. }), "{err}");
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
                run: async func(inputs: inputs) -> outputs;
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
async fn a_plugin_serving_the_declared_version_serves_a_merged_import() {
    use witgraph_ir::Capability;
    use witgraph_runtime::{CapabilityPlugin, PluginData, Plugins};

    struct Clock;

    impl CapabilityPlugin for Clock {
        fn id(&self) -> &str {
            "clock"
        }

        fn provides(&self) -> Vec<String> {
            vec!["demo:caps/clock@0.1.0".into()]
        }

        fn link(
            &self,
            _: &NodeId,
            capability: &Capability,
            linker: &mut Linker<PluginData>,
        ) -> wasmtime::Result<()> {
            linker
                .instance(&capability.interface)?
                .func_wrap("now", |_, (): ()| Ok((1u64,)))?;
            Ok(())
        }
    }

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
    // The contract declares `@0.1.0`, which is what `required_capabilities`
    // lists; the bytes import `@0.1.3`.
    let contract = contract(&wit, "w");
    let bytes = component(&wit, "newer", "newer");
    let graph = Graph::builder("t")
        .add_component(&contract)
        .add_node("n", contract.id.clone())
        .build();
    let compiled = graph
        .compile(std::slice::from_ref(&contract))
        .expect("compiles");
    let wasm: HashMap<ComponentRef, &[u8]> = HashMap::from([(contract.id.clone(), &bytes[..])]);
    let plugins = Plugins::new().with(Clock).unwrap();
    RuntimeGraph::load_with_host(
        compiled,
        &wasm,
        RuntimeConfig::default(),
        witgraph_runtime::Perf,
        plugins,
    )
    .await
    .expect("a plugin serving the declared 0.1.0 serves the bytes' 0.1.3 import");
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
    assert!(matches!(err, LoadError::Instantiation { .. }), "{err}");
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
    assert!(matches!(err, LoadError::BadComponent { .. }), "{err}");
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
async fn two_links_on_imports_the_bytes_merged_are_refused() {
    use witgraph_runtime::graph::{LinkedProvider, PreparedComponent};
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
            import demo:caps/clock@0.1.3;
            {NODE}
        }}
        world newer {{
            import demo:caps/clock@0.1.3;
            {NODE}
        }}
        world provider {{
            export demo:caps/clock@0.1.3;
        }}"
    );
    let contract = contract(&wit, "w");
    let bytes = component(&wit, "newer", "newer");
    let provider_bytes = component(&wit, "provider", "provider");
    let provider: ComponentRef = "demo:app/provider@0.1.0".parse().unwrap();
    let links = ["demo:caps/clock@0.1.0", "demo:caps/clock@0.1.3"].map(|import| LinkedProvider {
        import,
        provider: &provider,
        bytes: &provider_bytes,
        export: "demo:caps/clock@0.1.3",
    });
    let engine = RuntimeConfig::default().new_engine().unwrap();
    // Even links that agree: a graph never has them (`ImportLinkedTwice`).
    let err = PreparedComponent::linked(&engine, &contract, &bytes, &links)
        .err()
        .expect("one import, two links");
    assert!(
        matches!(&err, LoadError::BadLink { message, .. } if message.contains("as one")),
        "{err}"
    );
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
    assert!(matches!(err, LoadError::Instantiation { .. }), "{err}");
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
            matches!(err, LoadError::BadComponent { .. }),
            "{what}: {err}"
        );
    }
    let err = load(&contract, b"\0asm not really", no_capabilities)
        .await
        .err()
        .expect("not wasm");
    assert!(matches!(err, LoadError::BadComponent { .. }), "{err}");
}

#[tokio::test]
async fn bytes_without_one_node_world_are_a_bad_component() {
    let wit = "package demo:nonode@0.1.0;
        interface other { f: func(); }
        world node-world {
            export node: interface {
                record outputs { out: u32 }
                run: async func() -> outputs;
            }
        }
        world plain { export other; }";
    let bytes = component(wit, "plain", "plain");
    let err = load(&contract(wit, "node-world"), &bytes, no_capabilities)
        .await
        .err()
        .expect("the bytes export no node");
    assert!(
        matches!(err, LoadError::BadComponent { ref message, .. } if message.contains("exactly one node world")),
        "{err}"
    );
}

#[tokio::test]
async fn bytes_with_a_sync_run_are_a_bad_component() {
    let sync = "package demo:kind@0.1.0;
        world w {
            export node: interface {
                record outputs { out: u32 }
                run: func() -> outputs;
            }
        }";
    let async_wit = format!("package demo:kind@0.1.0; world w {{ {NODE} }}");
    let mut resolve = Resolve::default();
    let package = resolve.push_str("test.wit", sync).unwrap();
    let world = resolve.select_world(&[package], Some("w")).unwrap();
    let mut module =
        wit_component::dummy_module(&resolve, world, ManglingAndAbi::Legacy(LiftLowerAbi::Sync));
    wit_component::embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8)
        .unwrap();
    let bytes = ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .encode()
        .unwrap();
    let err = load(&contract(&async_wit, "w"), &bytes, no_capabilities)
        .await
        .err()
        .expect("sync bytes");
    assert!(
        matches!(err, LoadError::BadComponent { ref message, .. } if message.contains("async func")),
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
                run: async func() -> outputs;
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
            (type $outputs (record (field "out" u32)))
            (core func $tr (canon task.return (result $outputs)))
            (core module $main
                (import "ctl" "should-fail" (func $should_fail (result i32)))
                (import "host" "fatal" (func $fatal (param i32 i32)))
                (import "host" "task-return" (func $task_return (param i32)))
                (import "env" "memory" (memory 1))
                (data (i32.const 16) "boom")
                (func $start
                    (if (call $should_fail)
                        (then (call $fatal (i32.const 16) (i32.const 4)))))
                (start $start)
                ;; An async `run` that returns at once: `task.return`, then
                ;; EXIT (0), so the callback is never called.
                (func (export "run") (result i32)
                    (call $task_return (i32.const 7))
                    (i32.const 0))
                (func (export "callback") (param i32 i32 i32) (result i32) unreachable))
            (core instance $i (instantiate $main
                (with "ctl" (instance (export "should-fail" (func $sf))))
                (with "host" (instance
                    (export "fatal" (func $fl))
                    (export "task-return" (func $tr))))
                (with "env" (instance (export "memory" (memory $memory))))))
            (func $run async (result $outputs)
                (canon lift (core func $i "run") async (callback (core func $i "callback"))))
            (component $shim
                (type $rec (record (field "out" u32)))
                (import "import-type-rec" (type $r (eq $rec)))
                (import "import-func-run" (func $f async (result $r)))
                (export $ro "outputs" (type $r))
                (export "run" (func $f) (func async (result $ro))))
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

#[tokio::test]
async fn a_link_to_an_import_the_code_never_uses_satisfies_nothing() {
    let wit = format!(
        "package demo:idle@0.1.0;
        interface ops {{ twice: func(x: u32) -> u32; }}
        world provider {{
            export ops;
        }}
        world w {{
            import ops;
            {NODE}
        }}
        world bare {{
            {NODE}
        }}"
    );
    let contract = contract(&wit, "w");
    // Built without `ops`: the code never calls it.
    let node = component(&wit, "w", "bare");
    let provider_id: ComponentRef = "demo:idle/provider@0.1.0".parse().unwrap();
    let graph = Graph::builder("t")
        .add_component(&contract)
        .add_node("n", contract.id.clone())
        .link(
            "l",
            "n",
            "demo:idle/ops@0.1.0",
            provider_id.clone(),
            "demo:idle/ops@0.1.0",
        )
        .build();
    let compiled = graph
        .compile(std::slice::from_ref(&contract))
        .expect("compiles");
    // The provider is not even needed: nothing would use it.
    let wasm: HashMap<ComponentRef, &[u8]> =
        HashMap::from([(contract.id.clone(), node.as_slice())]);
    RuntimeGraph::load(
        compiled,
        &wasm,
        RuntimeConfig::default(),
        witgraph_runtime::Perf,
    )
    .await
    .expect("the link has nothing to satisfy, and that is fine");
}
