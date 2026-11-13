//! Generated hubs, over generated types and values: the hub generator writes wasm by
//! hand for each type, so its tests should not only know the types someone thought
//! of. The nodes are mocked, so a case needs no guest: `a.v -> b.v`, where a node's
//! `run` returns what it was given.
//!
//! A failure names its seed. `WITGRAPH_RANDOM_CASES` runs more of them, and
//! `WITGRAPH_RANDOM_SEED` just the one.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use wasmtime::component::{Component, Func, Instance, Linker, Val};
use wasmtime::{Engine, Store};
use wit_component::{ComponentEncoder, StringEncoding};
use wit_parser::{LiftLowerAbi, ManglingAndAbi, Resolve};
use witgraph_ir::{CompiledGraph, ContractId, Graph, PortRef, PortTable};
use witgraph_wit::hub::{self, HubMember};
use witgraph_wit::load::WitSource;

/// A small generator, so a seed means the same on every machine.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Seeds near each other should not start out alike.
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x2545_F491_4F6C_DD1D)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap()
    }

    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }
}

#[derive(Debug, Clone)]
enum Ty {
    Bool,
    S8,
    U8,
    S16,
    U16,
    S32,
    U32,
    S64,
    U64,
    F32,
    F64,
    Char,
    String,
    List(Box<Ty>),
    Option(Box<Ty>),
    Result(Option<Box<Ty>>, Option<Box<Ty>>),
    Tuple(Vec<Ty>),
    Record(Vec<(String, Ty)>),
    Variant(Vec<(String, Option<Ty>)>),
    Enum(Vec<String>),
    Flags(Vec<String>),
}

fn gen_ty(rng: &mut Rng, depth: usize) -> Ty {
    const LEAVES: usize = 13;
    let kinds = if depth == 0 { LEAVES } else { LEAVES + 8 };
    let names = |prefix: &str, n: usize| -> Vec<String> {
        (0..n).map(|i| format!("{prefix}{i}")).collect()
    };
    match rng.below(kinds) {
        0 => Ty::Bool,
        1 => Ty::S8,
        2 => Ty::U8,
        3 => Ty::S16,
        4 => Ty::U16,
        5 => Ty::S32,
        6 => Ty::U32,
        7 => Ty::S64,
        8 => Ty::U64,
        9 => Ty::F32,
        10 => Ty::F64,
        11 => Ty::Char,
        12 => Ty::String,
        13 => Ty::List(Box::new(gen_ty(rng, depth - 1))),
        14 => Ty::Option(Box::new(gen_ty(rng, depth - 1))),
        15 => {
            let ok = rng
                .chance(4)
                .then_some(())
                .map_or_else(|| Some(Box::new(gen_ty(rng, depth - 1))), |()| None);
            let err = rng
                .chance(4)
                .then_some(())
                .map_or_else(|| Some(Box::new(gen_ty(rng, depth - 1))), |()| None);
            Ty::Result(ok, err)
        }
        16 => {
            let n = 1 + rng.below(4);
            Ty::Tuple((0..n).map(|_| gen_ty(rng, depth - 1)).collect())
        }
        17 => {
            // Now and then past the 16 flat values a call passes before it passes
            // a pointer.
            let n = if rng.chance(4) {
                17 + rng.below(6)
            } else {
                1 + rng.below(5)
            };
            let fields = names("f", n)
                .into_iter()
                .map(|name| (name, gen_ty(rng, depth - 1)))
                .collect();
            Ty::Record(fields)
        }
        18 => {
            let n = 1 + rng.below(5);
            let cases = names("c", n)
                .into_iter()
                .map(|name| {
                    let payload = (!rng.chance(3)).then(|| gen_ty(rng, depth - 1));
                    (name, payload)
                })
                .collect();
            Ty::Variant(cases)
        }
        19 => Ty::Enum(names("e", 1 + rng.below(6))),
        // Across the sizes flags are stored in.
        _ => Ty::Flags(names("g", [1, 8, 9, 16, 17, 32][rng.below(6)])),
    }
}

impl Ty {
    /// The type as WIT, its named types added to `defs`.
    fn wit(&self, defs: &mut Vec<String>) -> String {
        let name = |defs: &[String]| format!("t{}", defs.len());
        match self {
            Ty::Bool => "bool".into(),
            Ty::S8 => "s8".into(),
            Ty::U8 => "u8".into(),
            Ty::S16 => "s16".into(),
            Ty::U16 => "u16".into(),
            Ty::S32 => "s32".into(),
            Ty::U32 => "u32".into(),
            Ty::S64 => "s64".into(),
            Ty::U64 => "u64".into(),
            Ty::F32 => "f32".into(),
            Ty::F64 => "f64".into(),
            Ty::Char => "char".into(),
            Ty::String => "string".into(),
            Ty::List(ty) => format!("list<{}>", ty.wit(defs)),
            Ty::Option(ty) => format!("option<{}>", ty.wit(defs)),
            Ty::Result(ok, err) => {
                let ok = ok.as_ref().map(|ty| ty.wit(defs));
                let err = err.as_ref().map(|ty| ty.wit(defs));
                match (ok, err) {
                    (None, None) => "result".into(),
                    (Some(ok), None) => format!("result<{ok}>"),
                    (None, Some(err)) => format!("result<_, {err}>"),
                    (Some(ok), Some(err)) => format!("result<{ok}, {err}>"),
                }
            }
            Ty::Tuple(types) => {
                let types: Vec<String> = types.iter().map(|ty| ty.wit(defs)).collect();
                format!("tuple<{}>", types.join(", "))
            }
            Ty::Record(fields) => {
                let fields: Vec<String> = fields
                    .iter()
                    .map(|(field, ty)| format!("{field}: {}", ty.wit(defs)))
                    .collect();
                let name = name(defs);
                defs.push(format!("record {name} {{ {} }}", fields.join(", ")));
                name
            }
            Ty::Variant(cases) => {
                let cases: Vec<String> = cases
                    .iter()
                    .map(|(case, ty)| match ty {
                        Some(ty) => format!("{case}({})", ty.wit(defs)),
                        None => case.clone(),
                    })
                    .collect();
                let name = name(defs);
                defs.push(format!("variant {name} {{ {} }}", cases.join(", ")));
                name
            }
            Ty::Enum(cases) => {
                let name = name(defs);
                defs.push(format!("enum {name} {{ {} }}", cases.join(", ")));
                name
            }
            Ty::Flags(flags) => {
                let name = name(defs);
                defs.push(format!("flags {name} {{ {} }}", flags.join(", ")));
                name
            }
        }
    }

    fn value(&self, rng: &mut Rng) -> Val {
        // The values at the edges come up far more often than chance would have it.
        let edge = |rng: &mut Rng| -> Option<u64> {
            match rng.below(4) {
                0 => Some(0),
                1 => Some(u64::MAX),
                _ => None,
            }
        };
        macro_rules! int {
            ($variant:ident, $ty:ty) => {{
                let bits = edge(rng).unwrap_or_else(|| rng.next());
                Val::$variant(bits as $ty)
            }};
        }
        match self {
            Ty::Bool => Val::Bool(rng.chance(2)),
            Ty::S8 => int!(S8, i8),
            Ty::U8 => int!(U8, u8),
            Ty::S16 => int!(S16, i16),
            Ty::U16 => int!(U16, u16),
            Ty::S32 => int!(S32, i32),
            Ty::U32 => int!(U32, u32),
            Ty::S64 => int!(S64, i64),
            Ty::U64 => int!(U64, u64),
            Ty::F32 => Val::Float32(
                [
                    0.0,
                    -0.0,
                    f32::NAN,
                    f32::INFINITY,
                    1.5,
                    f32::MIN_POSITIVE,
                    rng.next() as f32,
                ][rng.below(7)],
            ),
            Ty::F64 => Val::Float64(
                [
                    0.0,
                    -0.0,
                    f64::NAN,
                    f64::NEG_INFINITY,
                    2.5,
                    f64::MIN_POSITIVE,
                    rng.next() as f64,
                ][rng.below(7)],
            ),
            Ty::Char => Val::Char(['a', 'é', '語', '🦀', '\0', char::MAX][rng.below(6)]),
            Ty::String => Val::String(gen_string(rng)),
            Ty::List(ty) => {
                let n = gen_len(rng);
                Val::List((0..n).map(|_| ty.value(rng)).collect())
            }
            Ty::Option(ty) => Val::Option((!rng.chance(3)).then(|| Box::new(ty.value(rng)))),
            Ty::Result(ok, err) => {
                let payload = |ty: &Option<Box<Ty>>, rng: &mut Rng| {
                    ty.as_ref().map(|ty| Box::new(ty.value(rng)))
                };
                Val::Result(if rng.chance(2) {
                    Ok(payload(ok, rng))
                } else {
                    Err(payload(err, rng))
                })
            }
            Ty::Tuple(types) => Val::Tuple(types.iter().map(|ty| ty.value(rng)).collect()),
            Ty::Record(fields) => Val::Record(
                fields
                    .iter()
                    .map(|(field, ty)| (field.clone(), ty.value(rng)))
                    .collect(),
            ),
            Ty::Variant(cases) => {
                let (case, ty) = &cases[rng.below(cases.len())];
                Val::Variant(case.clone(), ty.as_ref().map(|ty| Box::new(ty.value(rng))))
            }
            Ty::Enum(cases) => Val::Enum(cases[rng.below(cases.len())].clone()),
            Ty::Flags(flags) => {
                Val::Flags(flags.iter().filter(|_| rng.chance(2)).cloned().collect())
            }
        }
    }
}

/// Mostly short, now and then past the size classes small blocks come from.
fn gen_len(rng: &mut Rng) -> usize {
    match rng.below(8) {
        0 => 0,
        1 => 40 + rng.below(200),
        _ => 1 + rng.below(4),
    }
}

fn gen_string(rng: &mut Rng) -> String {
    let n = gen_len(rng);
    (0..n)
        .map(|_| ['a', 'é', '語', '🦀', '\0'][rng.below(5)])
        .collect()
}

/// Whether two values are one to the hub: `NaN` is `NaN`, and `0.0` is not `-0.0`.
/// Written apart from the generated `eq` it checks.
fn same(a: &Val, b: &Val) -> bool {
    let all = |a: &[Val], b: &[Val]| a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b));
    let boxed = |a: &Option<Box<Val>>, b: &Option<Box<Val>>| match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => same(a, b),
        _ => false,
    };
    match (a, b) {
        (Val::Float32(a), Val::Float32(b)) => {
            (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
        }
        (Val::Float64(a), Val::Float64(b)) => {
            (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
        }
        (Val::List(a), Val::List(b)) | (Val::Tuple(a), Val::Tuple(b)) => all(a, b),
        (Val::Option(a), Val::Option(b)) => boxed(a, b),
        (Val::Result(Ok(a)), Val::Result(Ok(b))) | (Val::Result(Err(a)), Val::Result(Err(b))) => {
            boxed(a, b)
        }
        (Val::Record(a), Val::Record(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|((an, av), (bn, bv))| an == bn && same(av, bv))
        }
        (Val::Variant(an, av), Val::Variant(bn, bv)) => an == bn && boxed(av, bv),
        (Val::Result(_), Val::Result(_)) => false,
        _ => a == b,
    }
}

/// A node of `input` and `output` port `v`, in `T`.
fn world(ty: &Ty) -> String {
    let mut defs = Vec::new();
    let text = ty.wit(&mut defs);
    let defs = defs.join("\n");
    format!(
        "package test:node@0.1.0;
        world head {{
            export node: interface {{
                {defs}
                record inputs {{ v: option<{text}> }}
                record outputs {{ v: {text} }}
                run: async func(inputs: inputs) -> outputs;
            }}
        }}"
    )
}

fn tail_world(ty: &Ty) -> String {
    let mut defs = Vec::new();
    let text = ty.wit(&mut defs);
    let defs = defs.join("\n");
    format!(
        "package test:tail@0.1.0;
        world tail {{
            export node: interface {{
                {defs}
                record inputs {{ v: {text} }}
                record outputs {{ v: {text} }}
                run: async func(inputs: inputs) -> outputs;
            }}
        }}"
    )
}

/// A component of the only world in `wit`, with nothing behind it.
fn component(wit: &str, world: &str) -> Vec<u8> {
    let mut resolve = Resolve {
        all_features: true,
        ..Resolve::default()
    };
    let package = resolve.push_str("node.wit", wit).unwrap();
    let world = resolve.select_world(&[package], Some(world)).unwrap();
    let mut module = wit_component::dummy_module(
        &resolve,
        world,
        ManglingAndAbi::Legacy(LiftLowerAbi::AsyncCallback),
    );
    wit_component::embed_component_metadata(
        &mut module,
        &resolve,
        world,
        StringEncoding::UTF8,
        false,
    )
    .unwrap();
    ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .encode()
        .unwrap()
}

/// The hub's memory, and how often it grew.
#[derive(Default)]
struct HubMemory {
    size: usize,
    grown: usize,
}

impl wasmtime::ResourceLimiter for HubMemory {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if (current == 0 && desired == 65536) || (self.size != 0 && current == self.size) {
            self.size = desired;
            self.grown += 1;
        }
        Ok(true)
    }

    fn table_growing(&mut self, _: usize, _: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

struct Case {
    store: Store<HubMemory>,
    instance: Instance,
    ports: PortTable,
    /// What each mocked `run` was given.
    given: Arc<Mutex<Vec<Val>>>,
}

/// A member of the graph: its bytes, its WIT, and what it runs.
struct Member {
    bytes: Vec<u8>,
    wit: WitSource,
    id: ContractId,
}

fn compile(ty: &Ty) -> (CompiledGraph, Member, Member) {
    let (head_wit, tail_wit) = (world(ty), tail_world(ty));
    let head_id: ContractId = "head/test:node/head@0.1.0".parse().unwrap();
    let tail_id: ContractId = "tail/test:tail/tail@0.1.0".parse().unwrap();
    let head = witgraph_wit::contract_from_str("head.wit", &head_wit, &head_id).unwrap();
    let tail = witgraph_wit::contract_from_str("tail.wit", &tail_wit, &tail_id).unwrap();
    let compiled = Graph::builder("random")
        .add_component(&head)
        .add_component(&tail)
        .add_node("a", head.id.clone())
        .add_node("b", tail.id.clone())
        .connect("ab", PortRef::new("a", "v"), PortRef::new("b", "v"))
        .build()
        .compile(&[head, tail])
        .unwrap();
    let member = |wit: &str, world: &str, id: ContractId| Member {
        bytes: component(wit, world),
        wit: witgraph_wit::load::load_str("member.wit", wit).unwrap(),
        id,
    };
    (
        compiled,
        member(&head_wit, "head", head_id),
        member(&tail_wit, "tail", tail_id),
    )
}

/// What a `run` returns for what it was given. `optional` says its input is an
/// option around the value.
fn echoing(params: &[Val], optional: bool, given: &Mutex<Vec<Val>>) -> Val {
    let Some(Val::Record(fields)) = params.first() else {
        panic!("`run` takes a record: {params:?}");
    };
    let value = match &fields[0].1 {
        Val::Option(Some(value)) if optional => (**value).clone(),
        value => value.clone(),
    };
    given.lock().unwrap().push(value.clone());
    Val::Record(vec![("v".to_string(), value)])
}

async fn build(engine: &Engine, ty: &Ty) -> Case {
    let (compiled, head, tail) = compile(ty);
    let ports = PortTable::new(&compiled);
    let (a, b) = ("a".into(), "b".into());
    let members = [
        HubMember {
            node: &a,
            bytes: &head.bytes,
            export: "node",
            wit: &head.wit,
            world: &head.id.world,
            mocked: true,
        },
        HubMember {
            node: &b,
            bytes: &tail.bytes,
            export: "node",
            wit: &tail.wit,
            world: &tail.id.world,
            mocked: true,
        },
    ];
    let composed =
        hub::compose_graph(&compiled, &members, hub::HubOptions::default()).expect("composes");
    let component = Component::new(engine, &composed.bytes).expect("compiles");
    let given = Arc::new(Mutex::new(Vec::new()));
    let mut linker = Linker::<HubMemory>::new(engine);
    for node in 0..2 {
        let seen = given.clone();
        linker
            .root()
            .instance(&hub::member_import(node))
            .unwrap()
            .func_new_concurrent("run", move |_, _, params, results| {
                // `a`, numbered first, has the optional input.
                results[0] = echoing(params, node == 0, &seen);
                Box::pin(async { Ok(()) })
            })
            .unwrap();
    }
    let mut store = Store::new(engine, HubMemory::default());
    store.limiter(|memory| memory as &mut dyn wasmtime::ResourceLimiter);
    store.set_fuel(u64::MAX / 2).unwrap();
    store.set_hostcall_fuel(usize::MAX / 2);
    let instance = linker
        .instantiate_async(&mut store, &component)
        .await
        .expect("instantiates");
    Case {
        store,
        instance,
        ports,
        given,
    }
}

impl Case {
    fn func(&mut self, interface: &str, name: &str) -> Func {
        let at = self
            .instance
            .get_export_index(&mut self.store, None, interface)
            .unwrap();
        let at = self
            .instance
            .get_export_index(&mut self.store, Some(&at), name)
            .unwrap_or_else(|| panic!("no `{name}`"));
        self.instance.get_func(&mut self.store, at).unwrap()
    }

    async fn call(&mut self, func: Func, args: Vec<Val>) -> Val {
        let mut results = [Val::Bool(false)];
        self.store
            .run_concurrent(async |acc| func.call_concurrent(acc, &args, &mut results).await)
            .await
            .unwrap()
            .unwrap();
        let [result] = results;
        result
    }

    /// Writes `a.v`, and returns whether that changed it.
    async fn set(&mut self, value: &Val) -> bool {
        let input = self.ports.input(&"a".into(), &"v".into()).unwrap();
        let f = self.func(hub::PORTS_INTERFACE, &hub::set_input(input));
        self.call(f, vec![Val::List(vec![value.clone()])]).await == Val::Bool(true)
    }

    async fn clear(&mut self) -> bool {
        let input = self.ports.input(&"a".into(), &"v".into()).unwrap();
        let f = self.func(hub::CONTROL_INTERFACE, "clear-input");
        let at = u32::try_from(witgraph_ir::Idx::position(input)).unwrap();
        self.call(f, vec![Val::U32(at)]).await == Val::Bool(true)
    }

    async fn getter(&mut self, name: &str) -> Option<Val> {
        let f = self.func(hub::PORTS_INTERFACE, name);
        let mut results = [Val::Bool(false)];
        self.store
            .run_concurrent(async |acc| f.call_concurrent(acc, &[], &mut results).await)
            .await
            .unwrap()
            .unwrap();
        let [result] = results;
        match result {
            Val::Option(value) => value.map(|value| *value),
            other => panic!("a getter returned {other:?}"),
        }
    }

    async fn input_of(&mut self, node: &str) -> Option<Val> {
        let input = self.ports.input(&node.into(), &"v".into()).unwrap();
        self.getter(&hub::get_input(input)).await
    }

    async fn output_of(&mut self, node: &str) -> Option<Val> {
        let output = self.ports.output(&node.into(), &"v".into()).unwrap();
        self.getter(&hub::get_output(output)).await
    }

    async fn run(&mut self, node: &str) {
        let at = self.ports.node(&node.into()).unwrap();
        let at = u32::try_from(witgraph_ir::Idx::position(at)).unwrap();
        let f = self.func(hub::CONTROL_INTERFACE, "run");
        self.call(f, vec![Val::U32(at)]).await;
    }

    fn last_given(&self) -> Val {
        self.given.lock().unwrap().last().unwrap().clone()
    }
}

fn expect_same(what: &str, found: Option<Val>, wanted: &Val) {
    let found = found.unwrap_or_else(|| panic!("{what}: nothing, wanted {wanted:?}"));
    assert!(same(&found, wanted), "{what}: {found:?}, wanted {wanted:?}");
}

/// Everything a value should go through, on the way down the chain.
async fn run_value(case: &mut Case, value: &Val, last: Option<&Val>) {
    let changes = last.is_none_or(|last| !same(last, value));
    assert_eq!(case.set(value).await, changes, "writing {value:?}");
    assert!(!case.set(value).await, "writing {value:?} again");
    expect_same("a.v in", case.input_of("a").await, value);

    case.run("a").await;
    expect_same("what `a` was given", Some(case.last_given()), value);
    expect_same("a.v out", case.output_of("a").await, value);
    expect_same("b.v in", case.input_of("b").await, value);

    case.run("b").await;
    expect_same("what `b` was given", Some(case.last_given()), value);
    expect_same("b.v out", case.output_of("b").await, value);
}

async fn run_case(engine: &Engine, seed: u64) {
    let mut rng = Rng::new(seed);
    // An option at the top makes the port optional, and its connection unwrap it,
    // which `tests/hub.rs` covers: here a value goes through as it is.
    let ty = loop {
        let ty = gen_ty(&mut rng, 3);
        if !matches!(ty, Ty::Option(_)) {
            break ty;
        }
    };
    let values: Vec<Val> = (0..6).map(|_| ty.value(&mut rng)).collect();
    let mut case = build(engine, &ty).await;

    let mut last: Option<&Val> = None;
    for value in &values {
        run_value(&mut case, value, last).await;
        last = Some(value);
    }

    // What is freed is used again: after a warmup, the memory stops growing.
    for _ in 0..3 {
        for value in &values {
            run_value(&mut case, value, last).await;
            last = Some(value);
        }
    }
    let warmed = case.store.data().grown;
    for _ in 0..3 {
        for value in &values {
            run_value(&mut case, value, last).await;
            last = Some(value);
        }
    }
    assert_eq!(case.store.data().grown, warmed, "the hub's memory grew");

    assert!(case.clear().await, "clearing a value");
    assert!(!case.clear().await, "clearing nothing");
    assert!(case.input_of("a").await.is_none(), "a cleared input");
}

#[tokio::test]
async fn generated_hubs_keep_generated_values() {
    let var = |name: &str| std::env::var(name).ok().map(|v| v.parse::<u64>().unwrap());
    let seeds: Vec<u64> = match var("WITGRAPH_RANDOM_SEED") {
        Some(seed) => vec![seed],
        None => (0..var("WITGRAPH_RANDOM_CASES").unwrap_or(16)).collect(),
    };
    let engine = witgraph_runtime::RuntimeConfig::default()
        .new_engine()
        .unwrap();
    for seed in seeds {
        eprintln!("seed {seed}");
        run_case(&engine, seed).await;
    }
}

/// Each row of a nested list counts: the loop over a row starts again for each.
#[tokio::test]
async fn every_row_of_a_nested_list_is_compared_and_freed() {
    let engine = witgraph_runtime::RuntimeConfig::default()
        .new_engine()
        .unwrap();
    let rows = |rows: &[&[&str]]| {
        Val::List(
            rows.iter()
                .map(|row| {
                    Val::List(
                        row.iter()
                            .map(|text| Val::String((*text).to_string()))
                            .collect(),
                    )
                })
                .collect(),
        )
    };
    let ty = Ty::List(Box::new(Ty::List(Box::new(Ty::String))));
    let mut case = build(&engine, &ty).await;
    let (first, second) = (rows(&[&["x"], &["y"]]), rows(&[&["x"], &["z"]]));
    assert!(case.set(&first).await);
    assert!(case.set(&second).await, "they differ in the second row");
    expect_same("a.v", case.input_of("a").await, &second);

    let big = |c: &'static str| {
        let row: Vec<&str> = vec![c; 20];
        let table: Vec<&[&str]> = vec![&row; 50];
        rows(&table)
    };
    let (one, other) = (big("a"), big("b"));
    let mut warmed = 0;
    for round in 0..40 {
        case.set(&one).await;
        case.set(&other).await;
        if round == 3 {
            warmed = case.store.data().grown;
        }
    }
    assert_eq!(case.store.data().grown, warmed, "the hub's memory grew");
}
