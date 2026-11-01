#![cfg(test)]
#![allow(missing_docs)]

//! Handle ports: nodes passing tensors the host keeps (`test:data`), over
//! a CPU-backed provider.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use test_components::{
    Guest, TENSOR_COPY_INVERT, TENSOR_INVERT, TENSOR_KEEP, TENSOR_SINK, TENSOR_SOURCE,
};
use wasmtime::StoreContextMut;
use wasmtime::component::{ComponentType, Lift, Linker, Lower, ResourceDynamic};
use witgraph_ir::{
    Capability, ComponentContract, ComponentRef, Graph, GraphBuilder, NodeId, PortRef,
};
use witgraph_runtime::shared::link_resource;
use witgraph_runtime::{
    CapabilityPlugin, IslandData, LoadError, NodeFault, Perf, PluginData, Plugins, RuntimeConfig,
    RuntimeGraph, TickResult, Val,
};

const TENSOR: &str = "test:data/tensor@0.1.0";

#[derive(ComponentType, Lift, Lower, Clone, Copy, PartialEq, Debug)]
#[component(enum)]
#[repr(u8)]
#[allow(dead_code, reason = "made by lifting")]
enum Dtype {
    #[component(name = "uint8")]
    Uint8,
    #[component(name = "float32")]
    Float32,
}

impl Dtype {
    fn size(self) -> usize {
        match self {
            Dtype::Uint8 => 1,
            Dtype::Float32 => 4,
        }
    }
}

/// A tensor in host memory.
struct Cpu {
    dtype: Dtype,
    shape: Vec<u32>,
    data: Arc<[u8]>,
}

/// Serves `test:data/tensor`, its tensors in host memory. With `shares`
/// unset, it serves the interface but does not say handle ports may carry
/// its tensors.
struct Tensors {
    shares: bool,
}

type Store<'a> = StoreContextMut<'a, PluginData>;

fn tensor(store: &mut Store<'_>, handle: &ResourceDynamic) -> wasmtime::Result<Arc<Cpu>> {
    let shared = store.data_mut().host_state().shared(handle)?;
    let cpu = shared
        .downcast_ref::<Arc<Cpu>>()
        .ok_or_else(|| wasmtime::format_err!("not a tensor"))?;
    Ok(cpu.clone())
}

fn share(store: &mut Store<'_>, cpu: Cpu) -> wasmtime::Result<ResourceDynamic> {
    let bytes = cpu.data.len();
    store
        .data_mut()
        .host_state()
        .share(TENSOR, "tensor", Arc::new(cpu), bytes)
}

impl CapabilityPlugin for Tensors {
    fn id(&self) -> &str {
        "tensors"
    }

    fn provides(&self) -> Vec<String> {
        vec![TENSOR.into()]
    }

    fn shares(&self, interface: &str, resource: &str) -> bool {
        self.shares && interface == TENSOR && resource == "tensor"
    }

    fn link(
        &self,
        _: &NodeId,
        capability: &Capability,
        linker: &mut Linker<PluginData>,
    ) -> wasmtime::Result<()> {
        let mut instance = linker.instance(&capability.interface)?;
        link_resource(&mut instance, &capability.interface, "tensor")?;
        instance.func_wrap(
            "[static]tensor.from-bytes",
            |mut store: Store<'_>, (dtype, shape, data): (Dtype, Vec<u32>, Vec<u8>)| {
                let elements: usize = shape.iter().map(|&d| d as usize).product();
                if elements * dtype.size() != data.len() {
                    return Ok((Err(format!("{} bytes for {elements} elements", data.len())),));
                }
                let cpu = Cpu {
                    dtype,
                    shape,
                    data: data.into(),
                };
                Ok((Ok(share(&mut store, cpu)?),))
            },
        )?;
        instance.func_wrap(
            "[method]tensor.dtype",
            |mut store: Store<'_>, (this,): (ResourceDynamic,)| {
                Ok((tensor(&mut store, &this)?.dtype,))
            },
        )?;
        instance.func_wrap(
            "[method]tensor.shape",
            |mut store: Store<'_>, (this,): (ResourceDynamic,)| {
                Ok((tensor(&mut store, &this)?.shape.clone(),))
            },
        )?;
        instance.func_wrap(
            "[method]tensor.read",
            |mut store: Store<'_>, (this, offset, len): (ResourceDynamic, u64, u64)| {
                let cpu = tensor(&mut store, &this)?;
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(cpu.data.len());
                let end = start.saturating_add(usize::try_from(len).unwrap_or(usize::MAX));
                Ok((cpu.data[start..end.min(cpu.data.len())].to_vec(),))
            },
        )?;
        instance.func_wrap(
            "[method]tensor.clone",
            |mut store: Store<'_>, (this,): (ResourceDynamic,)| {
                let state = store.data_mut().host_state();
                let shared = state.shared(&this)?;
                Ok((state.share_again(shared)?,))
            },
        )?;
        instance.func_wrap(
            "[method]tensor.invert",
            |mut store: Store<'_>, (this,): (ResourceDynamic,)| {
                let cpu = tensor(&mut store, &this)?;
                let data = match cpu.dtype {
                    Dtype::Uint8 => cpu.data.iter().map(|&b| 255 - b).collect(),
                    Dtype::Float32 => cpu
                        .data
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .flat_map(|x| (1.0 - f32::from_le_bytes(*x)).to_le_bytes())
                        .collect(),
                };
                let inverted = Cpu {
                    dtype: cpu.dtype,
                    shape: cpu.shape.clone(),
                    data,
                };
                Ok((share(&mut store, inverted)?,))
            },
        )?;
        instance.func_wrap(
            "[method]tensor.conv3x3",
            |_: Store<'_>, (_, _): (ResourceDynamic, Vec<f32>)| {
                Ok((Err::<ResourceDynamic, _>("not on the CPU".to_string()),))
            },
        )?;
        Ok(())
    }
}

fn contract(guest: Guest) -> ComponentContract {
    let mut contracts = witgraph_wit::load_components(guest.wit).expect("guest WIT lowers");
    contracts.remove(0)
}

struct Kit {
    contracts: Vec<ComponentContract>,
    wasm: HashMap<ComponentRef, &'static [u8]>,
}

impl Kit {
    fn new() -> Self {
        let mut kit = Kit {
            contracts: Vec::new(),
            wasm: HashMap::new(),
        };
        for guest in [
            TENSOR_SOURCE,
            TENSOR_INVERT,
            TENSOR_COPY_INVERT,
            TENSOR_SINK,
            TENSOR_KEEP,
        ] {
            let c = contract(guest);
            kit.wasm.insert(c.id.clone(), guest.wasm);
            kit.contracts.push(c);
        }
        kit
    }

    fn builder(&self) -> GraphBuilder {
        self.contracts
            .iter()
            .fold(Graph::builder("tensors"), |b, c| b.add_component(c))
    }

    async fn load(
        &self,
        graph: Graph,
        config: RuntimeConfig,
        shares: bool,
    ) -> Result<RuntimeGraph<Perf, Plugins>, LoadError> {
        let compiled = graph.compile(&self.contracts).expect("graph compiles");
        let plugins = Plugins::new().with(Tensors { shares }).unwrap();
        RuntimeGraph::load_with_host(compiled, &self.wasm, config, Perf, plugins).await
    }
}

fn of(guest: Guest) -> ComponentRef {
    contract(guest).id
}

async fn settle(rt: &mut RuntimeGraph<Perf, Plugins>) {
    for _ in 0..100 {
        let result = tokio::time::timeout(Duration::from_secs(30), rt.tick())
            .await
            .expect("tick hung");
        match result {
            TickResult::Idle => return,
            TickResult::Progress => {}
            other => panic!("unexpected tick result: {other:?}"),
        }
    }
    panic!("graph did not settle");
}

fn u64_of(rt: &RuntimeGraph<Perf, Plugins>, node: &str, port: &str) -> u64 {
    match rt.read_output(&node.into(), &port.into()).unwrap() {
        Some(Val::U64(v)) => v,
        other => panic!("{node}.{port}: {other:?}"),
    }
}

/// The bytes of the default 64 x 48 RGBA source tensor.
const SOURCE_BYTES: usize = 64 * 48 * 4;

/// `src` feeding `inv` (inverted by the host) and `copy` (by the guest),
/// each into a sink, and `direct`, a sink of `src` itself.
fn fan_out(kit: &Kit) -> Graph {
    kit.builder()
        .add_node("src", of(TENSOR_SOURCE))
        .add_node("inv", of(TENSOR_INVERT))
        .add_node("copy", of(TENSOR_COPY_INVERT))
        .add_node("direct", of(TENSOR_SINK))
        .add_node("inv-sink", of(TENSOR_SINK))
        .add_node("copy-sink", of(TENSOR_SINK))
        .connect(
            "a",
            PortRef::new("src", "tensor"),
            PortRef::new("inv", "tensor"),
        )
        .connect(
            "b",
            PortRef::new("src", "tensor"),
            PortRef::new("copy", "tensor"),
        )
        .connect(
            "c",
            PortRef::new("src", "tensor"),
            PortRef::new("direct", "tensor"),
        )
        .connect(
            "d",
            PortRef::new("inv", "tensor"),
            PortRef::new("inv-sink", "tensor"),
        )
        .connect(
            "e",
            PortRef::new("copy", "tensor"),
            PortRef::new("copy-sink", "tensor"),
        )
        .build()
}

#[tokio::test]
async fn tensors_pass_between_islands_as_handles() {
    let kit = Kit::new();
    let mut rt = kit
        .load(fan_out(&kit), RuntimeConfig::default(), true)
        .await
        .unwrap();
    settle(&mut rt).await;

    let sum = u64_of(&rt, "direct", "sum");
    assert_eq!(u64_of(&rt, "direct", "len"), SOURCE_BYTES as u64);
    let inverted = 255 * SOURCE_BYTES as u64 - sum;
    assert_eq!(
        u64_of(&rt, "inv-sink", "sum"),
        inverted,
        "inverted by the host"
    );
    assert_eq!(
        u64_of(&rt, "copy-sink", "sum"),
        inverted,
        "inverted by the guest"
    );
    assert!(
        rt.read_output(&"src".into(), &"tensor".into()).is_err(),
        "a handle is no Val"
    );
    let source = rt.read_handle(&"src".into(), &"tensor".into()).unwrap();
    assert_eq!(source.map(|s| s.bytes()), Some(SOURCE_BYTES));
    assert_eq!(
        rt.shared_memory(),
        3 * SOURCE_BYTES,
        "one source tensor for its three readers, and the two inverted ones"
    );

    // A new source tensor replaces the old, which nothing holds any more.
    rt.inject(&"src".into(), &"seed".into(), Val::U32(7))
        .unwrap();
    settle(&mut rt).await;
    assert_ne!(u64_of(&rt, "direct", "sum"), sum);
    assert_eq!(
        rt.shared_memory(),
        3 * SOURCE_BYTES,
        "the old tensors are freed"
    );
    drop(rt);
}

#[tokio::test]
async fn a_node_keeps_its_own_reference() {
    let kit = Kit::new();
    let graph = kit
        .builder()
        .add_node("src", of(TENSOR_SOURCE))
        .add_node("keep", of(TENSOR_KEEP))
        .connect(
            "a",
            PortRef::new("src", "tensor"),
            PortRef::new("keep", "tensor"),
        )
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), true)
        .await
        .unwrap();
    settle(&mut rt).await;
    assert_eq!(u64_of(&rt, "keep", "previous"), 0);
    let first = rt
        .read_handle(&"src".into(), &"tensor".into())
        .unwrap()
        .unwrap();

    rt.inject(&"src".into(), &"seed".into(), Val::U32(1))
        .unwrap();
    settle(&mut rt).await;
    assert_eq!(
        rt.shared_memory(),
        2 * SOURCE_BYTES,
        "`keep` holds the first tensor, the latch the second"
    );
    let second = rt
        .read_handle(&"src".into(), &"tensor".into())
        .unwrap()
        .unwrap();
    assert_ne!(first, second);
    let first_sum: u64 = first
        .downcast_ref::<Arc<Cpu>>()
        .unwrap()
        .data
        .iter()
        .map(|&b| u64::from(b))
        .sum();
    drop(first);
    assert_eq!(
        u64_of(&rt, "keep", "previous"),
        first_sum,
        "its clone of the first outlived the first's latch"
    );
    drop(second);

    rt.inject(&"src".into(), &"seed".into(), Val::U32(2))
        .unwrap();
    settle(&mut rt).await;
    assert_eq!(
        rt.shared_memory(),
        SOURCE_BYTES,
        "`keep` let go of the second: its reference is to the latched third"
    );
}

#[tokio::test]
async fn making_tensors_past_the_limit_faults_the_maker() {
    let kit = Kit::new();
    let graph = kit
        .builder()
        .add_node("src", of(TENSOR_SOURCE))
        .add_node("sink", of(TENSOR_SINK))
        .connect(
            "a",
            PortRef::new("src", "tensor"),
            PortRef::new("sink", "tensor"),
        )
        .build();
    let config = RuntimeConfig {
        max_shared_memory: Some(SOURCE_BYTES - 1),
        ..RuntimeConfig::default()
    };
    let mut rt = kit.load(graph, config, true).await.unwrap();
    let _ = rt.tick().await;
    let faults = rt.take_faults();
    assert!(
        faults.iter().any(
            |report| report.culprit.as_ref() == Some(&NodeId::from("src"))
                && matches!(
                    report.fault,
                    NodeFault::MemoryLimit { requested, .. } if requested == SOURCE_BYTES
                )
        ),
        "{faults:?}"
    );
    assert_eq!(rt.shared_memory(), 0);
}

#[tokio::test]
async fn a_resource_the_host_does_not_share_fails_the_load() {
    let kit = Kit::new();
    let graph = kit
        .builder()
        .add_node("src", of(TENSOR_SOURCE))
        .add_node("sink", of(TENSOR_SINK))
        .connect(
            "a",
            PortRef::new("src", "tensor"),
            PortRef::new("sink", "tensor"),
        )
        .build();
    let error = kit
        .load(graph, RuntimeConfig::default(), false)
        .await
        .err()
        .expect("the load fails");
    assert!(
        matches!(&error, LoadError::NotShareable { node, resource, .. }
            if node.as_str() == "src" && resource == "test:data/tensor@0.1.0.tensor"),
        "{error}"
    );
}

#[tokio::test]
async fn a_restore_makes_the_tensors_anew() {
    let kit = Kit::new();
    let mut rt = kit
        .load(fan_out(&kit), RuntimeConfig::default(), true)
        .await
        .unwrap();
    rt.inject(&"src".into(), &"seed".into(), Val::U32(3))
        .unwrap();
    settle(&mut rt).await;
    let sum = u64_of(&rt, "inv-sink", "sum");
    let snapshot = rt.snapshot().unwrap();
    drop(rt);

    let mut restored = kit
        .load(fan_out(&kit), RuntimeConfig::default(), true)
        .await
        .unwrap();
    restored.restore(&snapshot).unwrap();
    settle(&mut restored).await;
    assert_eq!(u64_of(&restored, "inv-sink", "sum"), sum);
    assert_eq!(restored.shared_memory(), 3 * SOURCE_BYTES);
}
