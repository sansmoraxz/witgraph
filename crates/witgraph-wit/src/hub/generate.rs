//! Generating a hub for a graph: its world, its core module, its component.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use wasm_encoder::{Instruction as I, ValType};
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::abi::{AbiVariant, WasmSignature, WasmType};
use wit_parser::{
    Function, FutureIntrinsic, Int, LiftLowerAbi, Mangling, ManglingAndAbi, Resolve,
    ResourceIntrinsic, StreamIntrinsic, Type, TypeDefKind, TypeId, WasmExport, WasmExportKind,
    WasmImport, WorldId, WorldItem, WorldKey,
};
use witgraph_ir::{
    CompiledGraph, ComponentContract, FeedbackIdx, Idx, InputIdx, NodeId, OutputIdx, PortKind,
    PortName, PortTable,
};

use super::abi::{Abi, Prim, Shape};
use super::module::{
    AddingFuncs, Body, EqFn, FuncIdx, HEAP_TOP, Helper, Import, Local, ModuleBuilder, Offset,
    Place, Sig, Size, Static,
};
use super::text::{Printer, Resources};
use super::{
    CONTROL_INTERFACE, ControlFunc, HubError, HubMember, LEND_INTERFACE, PORTS_INTERFACE,
    get_feedback, get_input, get_output, give_stream, lend_handle, read_handle, set_feedback,
    set_input, set_output, take_stream,
};

/// The fixed part of a hub's world.
const CONTROL_WIT: &str = "
interface control {
    record returned {
        outputs-changed: list<u32>,
        inputs-changed: list<u32>,
        inputs-recorded: list<u32>,
        inputs-cleared: list<u32>,
        feedback-buffered: list<u32>,
    }
    enum latched { unchanged, changed, cleared }
    run: async func(member: u32) -> returned;
    clear-input: func(input: u32) -> bool;
    latch-feedback: func(connection: u32) -> latched;
    drop-feedback: func(connection: u32);
}
";

/// A generated hub.
pub(super) struct Generated<'a> {
    pub(super) bytes: Vec<u8>,
    /// The members, each `m<position>` to the hub.
    pub(super) order: Vec<HubMember<'a>>,
}

/// A member's WIT, as its node manifest's WIT declares its world.
struct Decoded<'a> {
    resolve: &'a Resolve,
    run: Function,
}

fn read_member<'a>(member: &HubMember<'a>) -> Result<Decoded<'a>, HubError> {
    let failed = |message: String| HubError::Member {
        node: member.node.clone(),
        message,
    };
    let world = crate::lower::find_world(member.wit, member.world)
        .ok_or_else(|| failed(format!("its WIT has no world `{}`", member.world)))?;
    let resolve = &member.wit.resolve;
    let run = resolve.worlds[world]
        .exports
        .iter()
        .find_map(|(key, item)| match item {
            WorldItem::Interface { id, .. } if resolve.name_world_key(key) == member.export => {
                resolve.interfaces[*id].functions.get("run").cloned()
            }
            _ => None,
        })
        .ok_or_else(|| failed(format!("`{}` exports no `run`", member.export)))?;
    Ok(Decoded { resolve, run })
}

/// The fields of a `run`'s `inputs` record, in `resolve`.
fn input_fields<'r>(resolve: &'r Resolve, run: &Function) -> Vec<(&'r str, Type)> {
    run.params
        .first()
        .map_or_else(Vec::new, |param| record_fields(resolve, &param.ty))
}

/// The fields of a `run`'s `outputs` record, in `resolve`.
fn output_fields<'r>(resolve: &'r Resolve, run: &Function) -> Vec<(&'r str, Type)> {
    run.result
        .as_ref()
        .map_or_else(Vec::new, |ty| record_fields(resolve, ty))
}

fn record_fields<'r>(resolve: &'r Resolve, ty: &Type) -> Vec<(&'r str, Type)> {
    let Type::Id(id) = ty else {
        return Vec::new();
    };
    match &resolve.types[*id].kind {
        TypeDefKind::Record(record) => record
            .fields
            .iter()
            .map(|field| (field.name.as_str(), field.ty))
            .collect(),
        TypeDefKind::Type(inner) => record_fields(resolve, inner),
        _ => Vec::new(),
    }
}

/// What a field's port carries, when it is declared `option<T>` and its port
/// optional: `T`.
fn payload_of(resolve: &Resolve, ty: &Type) -> Option<Type> {
    let Type::Id(id) = ty else {
        return None;
    };
    match &resolve.types[*id].kind {
        TypeDefKind::Option(inner) => Some(*inner),
        TypeDefKind::Type(inner) => payload_of(resolve, inner),
        _ => None,
    }
}

/// The resource an `own` type is a handle of.
fn owned_resource(resolve: &Resolve, ty: &Type) -> Option<TypeId> {
    let Type::Id(id) = ty else {
        return None;
    };
    match &resolve.types[*id].kind {
        TypeDefKind::Handle(wit_parser::Handle::Own(resource)) => Some(*resource),
        TypeDefKind::Type(inner) => owned_resource(resolve, inner),
        _ => None,
    }
}

fn contract_of<'c>(
    compiled: &'c CompiledGraph,
    node: &NodeId,
) -> Result<&'c ComponentContract, HubError> {
    compiled
        .contract_for(node)
        .ok_or_else(|| HubError::MissingMember(node.clone()))
}

/// The hub's world, as WIT text.
fn world_text(
    compiled: &CompiledGraph,
    ports: &PortTable,
    order: &[HubMember<'_>],
    decoded: &[Decoded<'_>],
) -> Result<String, HubError> {
    let mut resources = Resources::default();
    // Once to find every resource's newest version, then for real.
    for member in decoded {
        Printer::new("", member.resolve, &mut resources).run(&member.run)?;
    }

    let mut imports = String::new();
    let mut port_defs = String::new();
    let mut port_funcs = String::new();
    let mut lend_defs = String::new();
    let mut lend_funcs = String::new();
    // Each feedback connection's type, once its source's output is printed.
    let mut feedback: BTreeMap<usize, String> = BTreeMap::new();
    for (at, (member, node)) in decoded.iter().zip(order).enumerate() {
        let resolve = &member.resolve;
        let mut printer = Printer::new("", resolve, &mut resources);
        let run = printer.run(&member.run)?;
        let _ = writeln!(
            imports,
            "  import m{at}: interface {{\n{}{run}\n  }}",
            printer.defs()
        );

        let contract = contract_of(compiled, node.node)?;
        let mut printer = Printer::new(&format!("m{at}-"), resolve, &mut resources);
        for (name, ty) in input_fields(resolve, &member.run) {
            let port = PortName::from(name);
            let Some(def) = contract.inputs.iter().find(|p| p.name == port) else {
                continue;
            };
            let Some(input) = ports.input(node.node, &port) else {
                continue;
            };
            if def.kind() == PortKind::Handle {
                continue;
            }
            let payload = if def.is_optional() {
                payload_of(resolve, &ty).unwrap_or(ty)
            } else {
                ty
            };
            let text = printer.ty(&payload)?;
            if def.kind().is_async() {
                let _ = writeln!(
                    port_funcs,
                    "  {}: func() -> option<{text}>;\n  {}: func(value: {text});",
                    take_stream(input),
                    give_stream(input),
                );
                continue;
            }
            let _ = writeln!(
                port_funcs,
                "  {}: func() -> option<{text}>;\n  {}: func(value: list<{text}>) -> bool;",
                get_input(input),
                set_input(input),
            );
        }
        let mut handles = Vec::new();
        for (name, ty) in output_fields(resolve, &member.run) {
            let port = PortName::from(name);
            let Some(output) = ports.output(node.node, &port) else {
                continue;
            };
            match ports.outputs()[output].kind {
                PortKind::Value => {
                    let text = printer.ty(&ty)?;
                    let _ = writeln!(
                        port_funcs,
                        "  {}: func() -> option<{text}>;\n  {}: func(value: list<{text}>) -> bool;",
                        get_output(output),
                        set_output(output),
                    );
                    for (conn, entry) in ports.feedback().iter_enumerated() {
                        if entry.from == output {
                            feedback.insert(conn.position(), text.clone());
                        }
                    }
                }
                PortKind::Handle => {
                    let resource = owned_resource(resolve, &ty).ok_or_else(|| {
                        HubError::Unsupported(format!("handle output `{}.{port}`", node.node))
                    })?;
                    handles.push((output, resource));
                    let _ = writeln!(port_funcs, "  {}: func();", read_handle(output));
                }
                PortKind::Stream | PortKind::Future => {}
            }
        }
        port_defs.push_str(printer.defs());
        let mut lender = Printer::new(&format!("l{at}-"), resolve, &mut resources);
        for (output, resource) in handles {
            let name = lender.resource_name(resource)?;
            let _ = writeln!(
                lend_funcs,
                "  {}: func(it: borrow<{name}>);",
                lend_handle(output)
            );
        }
        lend_defs.push_str(lender.defs());
    }
    for (conn, text) in &feedback {
        let conn = FeedbackIdx::at(*conn);
        let _ = writeln!(
            port_funcs,
            "  {}: func() -> option<{text}>;\n  {}: func(value: list<{text}>) -> bool;",
            get_feedback(conn),
            set_feedback(conn),
        );
    }
    let lend = !lend_funcs.is_empty();
    let mut world = String::from("world hub {\n");
    world.push_str(&imports);
    if lend {
        world.push_str("  import lend;\n");
    }
    world.push_str("  export control;\n  export ports;\n}\n");
    let mut text = String::from("package witgraph:hub@0.1.0;\n");
    text.push_str(CONTROL_WIT);
    let _ = writeln!(text, "interface ports {{\n{port_defs}{port_funcs}}}");
    if lend {
        let _ = writeln!(text, "interface lend {{\n{lend_defs}{lend_funcs}}}");
    }
    text.push_str(&world);
    text.push_str(&resources.packages());
    Ok(text)
}

/// Generates the hub of `compiled`, with each of `members`.
pub(super) fn hub<'a>(
    compiled: &CompiledGraph,
    members: &[HubMember<'a>],
) -> Result<Generated<'a>, HubError> {
    let ports = PortTable::new(compiled);
    let order = (0..ports.node_count())
        .map(|at| {
            let node = ports.node_id(witgraph_ir::NodeIdx::at(at));
            members
                .iter()
                .find(|member| member.node == node)
                .copied()
                .ok_or_else(|| HubError::MissingMember(node.clone()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let decoded = order
        .iter()
        .map(read_member)
        .collect::<Result<Vec<_>, _>>()?;
    let text = world_text(compiled, &ports, &order, &decoded)?;
    let generate = |e: String| HubError::Generate(e);
    let mut resolve = Resolve::default();
    let package = resolve
        .push_str("hub.wit", &text)
        .map_err(|e| generate(format!("{e:#}\n{text}")))?;
    let world = resolve
        .select_world(&[package], Some("hub"))
        .map_err(|e| generate(format!("{e:#}")))?;
    let mut core = Core::new(compiled, &ports, &order, &resolve, world)?.build()?;
    embed_component_metadata(&mut core, &resolve, world, StringEncoding::UTF8, false)
        .map_err(|e| generate(format!("{e:#}")))?;
    let bytes = ComponentEncoder::default()
        .validate(true)
        .module(&core)
        .map_err(|e| generate(format!("{e:#}")))?
        .encode()
        .map_err(|e| generate(format!("{e:#}")))?;
    Ok(Generated { bytes, order })
}

const CALLBACK_EXIT: u32 = 0;
const CALLBACK_WAIT: u32 = 2;
const EVENT_SUBTASK: u32 = 1;
const STATUS_RETURNED: u32 = 2;

const FREE_LISTS: u32 = 1024;
const SIZE_CLASSES: u32 = 128;
/// A slot holds a block's header address (0 for no value), then the value's.
const SLOT_SIZE: u32 = 8;
const BLOCK_HEADER: u32 = 8;
const CALL_SIZE: u32 = 12;
/// `outputs-changed`, and so on: five lists.
const RETURNED_SIZE: u32 = 40;

/// A field of a member's `inputs` or `outputs` record, in the hub's `Resolve`.
#[derive(Clone)]
struct Field {
    offset: Offset,
    ty: Type,
    kind: PortKind,
    /// A declared `option<T>` of an optional port: `T`, and where it is.
    optional: Option<(Type, Offset)>,
}

/// One connection out of an output.
#[derive(Clone, Copy)]
struct Edge {
    to: Target,
    /// Delivers the payload at this offset, when the value is `some`.
    unwrap: Option<Offset>,
}

#[derive(Clone, Copy)]
enum Target {
    /// Inside the producer's stream group: recorded, owing nothing.
    Internal(InputIdx),
    /// Anywhere else, owing a run if it changes.
    External(InputIdx),
    /// Buffered until the end of the iteration.
    Feedback(FeedbackIdx),
}

struct Member {
    key: WorldKey,
    func: Function,
    sig: WasmSignature,
    inputs: Vec<(Option<InputIdx>, Field)>,
    outputs: Vec<(Option<OutputIdx>, Field)>,
    inputs_ty: Option<Type>,
    outputs_ty: Option<Type>,
}

/// What a block of memory holds, so it can be freed.
#[derive(Clone)]
enum BlockKind {
    /// A `run`'s `outputs` record.
    Outputs(usize),
    /// One value the host wrote.
    Boxed(Type),
    /// A stream or future the host gave an input, until its reader starts.
    Given(InputIdx),
}

/// The canonical built-ins and helpers every function may call.
#[derive(Clone, Copy)]
struct Runtime {
    alloc: FuncIdx<Helper>,
    dealloc: FuncIdx<Helper>,
    memeq: FuncIdx<Helper>,
    alloc_block: FuncIdx<Helper>,
    retain: FuncIdx<Helper>,
    release: FuncIdx<Helper>,
    free_block: FuncIdx<Helper>,
    slot_set: FuncIdx<Helper>,
    slot_clear: FuncIdx<Helper>,
}

/// The imports, once they are closed.
struct Imports {
    runs: Vec<FuncIdx<Import>>,
    /// Per member and output field: how to drop a reader of its stream or
    /// future.
    drops: Vec<HashMap<usize, FuncIdx<Import>>>,
    resource_drops: HashMap<TypeId, FuncIdx<Import>>,
    lends: HashMap<OutputIdx, FuncIdx<Import>>,
    /// Per stream or future input: how to drop one its reader never took.
    given_drops: HashMap<InputIdx, FuncIdx<Import>>,
    task_return: FuncIdx<Import>,
    set_new: FuncIdx<Import>,
    set_drop: FuncIdx<Import>,
    join: FuncIdx<Import>,
    subtask_drop: FuncIdx<Import>,
    context_get: FuncIdx<Import>,
    context_set: FuncIdx<Import>,
}

struct Core<'a> {
    abi: Abi<'a>,
    resolve: &'a Resolve,
    world: WorldId,
    ports: &'a PortTable,
    members: Vec<Member>,
    /// Per output: where it is delivered.
    edges: HashMap<OutputIdx, Vec<Edge>>,
    /// Each input's payload type, each output's type.
    input_ty: BTreeMap<InputIdx, Type>,
    /// Each stream or future input's type.
    stream_ty: BTreeMap<InputIdx, Type>,
    output_ty: BTreeMap<OutputIdx, Type>,
    feedback_ty: BTreeMap<FeedbackIdx, Type>,
    control: WorldKey,
    ports_key: WorldKey,
    lend_key: Option<WorldKey>,
    return_area: Static,
    return_area_size: u32,
    inputs_at: Static,
    outputs_at: Static,
    feedback_at: Static,
    calls_at: Static,
    pins_at: Static,
    /// Per member, where its pins start.
    pin_offsets: Vec<u32>,
    /// The five lists of a `returned`.
    lists: [Static; 5],
    heap_base: u32,
}

fn wasm_val(ty: WasmType) -> ValType {
    match ty {
        WasmType::I32 | WasmType::Pointer | WasmType::Length => ValType::I32,
        WasmType::I64 | WasmType::PointerOrI64 => ValType::I64,
        WasmType::F32 => ValType::F32,
        WasmType::F64 => ValType::F64,
    }
}

fn sig_of(sig: &WasmSignature) -> Sig {
    Sig {
        params: sig.params.iter().copied().map(wasm_val).collect(),
        results: sig.results.iter().copied().map(wasm_val).collect(),
    }
}

fn int_load(tag: Int) -> fn(wasm_encoder::MemArg) -> I<'static> {
    match tag {
        Int::U8 => I::I32Load8U,
        Int::U16 => I::I32Load16U,
        Int::U32 => I::I32Load,
        Int::U64 => I::I32Load,
    }
}

fn slot(base: Static, index: usize) -> Static {
    Static(base.0 + SLOT_SIZE * index as u32)
}

impl<'a> Core<'a> {
    fn new(
        compiled: &CompiledGraph,
        ports: &'a PortTable,
        order: &[HubMember<'_>],
        resolve: &'a Resolve,
        world: WorldId,
    ) -> Result<Self, HubError> {
        let abi = Abi::new(resolve);
        let interface_key = |id: &str| -> Result<WorldKey, HubError> {
            resolve.worlds[world]
                .exports
                .iter()
                .chain(resolve.worlds[world].imports.iter())
                .map(|(key, _)| key)
                .find(|key| resolve.name_world_key(key) == id)
                .cloned()
                .ok_or_else(|| HubError::Generate(format!("the hub's world has no `{id}`")))
        };
        let control = interface_key(CONTROL_INTERFACE)?;
        let ports_key = interface_key(PORTS_INTERFACE)?;
        let lend_key = interface_key(LEND_INTERFACE).ok();

        let mut members = Vec::with_capacity(order.len());
        let mut input_ty = BTreeMap::new();
        let mut stream_ty = BTreeMap::new();
        let mut output_ty = BTreeMap::new();
        for (at, member) in order.iter().enumerate() {
            let key = WorldKey::Name(format!("m{at}"));
            let Some(WorldItem::Interface { id, .. }) = resolve.worlds[world].imports.get(&key)
            else {
                return Err(HubError::Generate(format!("the hub imports no `m{at}`")));
            };
            let func = resolve.interfaces[*id]
                .functions
                .get("run")
                .cloned()
                .ok_or_else(|| HubError::Generate(format!("`m{at}` has no `run`")))?;
            let sig = resolve.wasm_signature(AbiVariant::GuestImportAsync, &func);
            let contract = contract_of(compiled, member.node)?;
            let in_fields = input_fields(resolve, &func);
            let in_types: Vec<Type> = in_fields.iter().map(|(_, ty)| *ty).collect();
            let in_offsets = abi.offsets(&in_types);
            let mut inputs = Vec::with_capacity(in_fields.len());
            for ((name, ty), offset) in in_fields.iter().zip(in_offsets) {
                let port = PortName::from(*name);
                let def = contract.inputs.iter().find(|p| p.name == port);
                let index = ports.input(member.node, &port);
                let kind = def.map_or(PortKind::Value, |d| d.kind());
                let optional = def.filter(|d| d.is_optional()).and_then(|_| {
                    let payload = payload_of(resolve, ty)?;
                    let at = abi.payload_offset(Int::U8, &[None, Some(payload)]);
                    Some((payload, at))
                });
                if let Some(index) = index
                    && kind == PortKind::Value
                {
                    input_ty.insert(index, optional.map_or(*ty, |(payload, _)| payload));
                }
                if let Some(index) = index
                    && kind.is_async()
                {
                    stream_ty.insert(index, optional.map_or(*ty, |(payload, _)| payload));
                }
                inputs.push((
                    index,
                    Field {
                        offset,
                        ty: *ty,
                        kind,
                        optional,
                    },
                ));
            }
            let out_fields = output_fields(resolve, &func);
            let out_types: Vec<Type> = out_fields.iter().map(|(_, ty)| *ty).collect();
            let out_offsets = abi.offsets(&out_types);
            let mut outputs = Vec::with_capacity(out_fields.len());
            for ((name, ty), offset) in out_fields.iter().zip(out_offsets) {
                let port = PortName::from(*name);
                let index = ports.output(member.node, &port);
                let kind = index.map_or(PortKind::Value, |i| ports.outputs()[i].kind);
                if kind.is_async() && abi.owns(ty) {
                    return Err(HubError::Unsupported(format!(
                        "`{}.{port}` holds a stream or future inside a value",
                        member.node
                    )));
                }
                if let Some(index) = index {
                    output_ty.insert(index, *ty);
                }
                outputs.push((
                    index,
                    Field {
                        offset,
                        ty: *ty,
                        kind,
                        optional: None,
                    },
                ));
            }
            members.push(Member {
                key,
                inputs_ty: func.params.first().map(|param| param.ty),
                outputs_ty: func.result,
                func,
                sig,
                inputs,
                outputs,
            });
        }

        let group_of: Vec<Option<usize>> = compiled.nodes().map(|n| n.group).collect();
        let mut edges: HashMap<OutputIdx, Vec<Edge>> = HashMap::new();
        let mut feedback_ty = BTreeMap::new();
        for resolved in compiled.connections() {
            let conn = resolved.connection;
            let (Some(output), Some(input)) = (
                ports.output(&conn.from.node, &conn.from.port),
                ports.input(&conn.to.node, &conn.to.port),
            ) else {
                continue;
            };
            let unwrap = if resolved.unwraps_option {
                output_ty.get(&output).and_then(|ty| {
                    let payload = payload_of(resolve, ty)?;
                    Some(abi.payload_offset(Int::U8, &[None, Some(payload)]))
                })
            } else {
                None
            };
            let to = match ports.feedback_index(&conn.id) {
                Some(feedback) => {
                    if let Some(ty) = output_ty.get(&output) {
                        feedback_ty.insert(feedback, *ty);
                    }
                    Target::Feedback(feedback)
                }
                None if group_of[resolved.from_node].is_some()
                    && group_of[resolved.from_node] == group_of[resolved.to_node] =>
                {
                    Target::Internal(input)
                }
                None => Target::External(input),
            };
            edges.entry(output).or_default().push(Edge { to, unwrap });
        }

        // Static data, after what the low addresses are kept free for.
        let mut at = FREE_LISTS + SIZE_CLASSES * 4;
        let mut take = |bytes: u32| {
            at = at.next_multiple_of(8);
            let here = at;
            at += bytes;
            Static(here)
        };
        let mut return_area_size = RETURNED_SIZE;
        let option_size = |ty: &Type| {
            let payload = abi.payload_offset(Int::U8, &[None, Some(*ty)]);
            payload.0 + abi.size(ty).0
        };
        for ty in input_ty
            .values()
            .chain(output_ty.values())
            .chain(feedback_ty.values())
        {
            return_area_size = return_area_size.max(option_size(ty));
        }
        let return_area = take(return_area_size);
        let inputs_at = take(SLOT_SIZE * ports.inputs().len() as u32);
        let outputs_at = take(SLOT_SIZE * ports.outputs().len() as u32);
        let feedback_at = take(SLOT_SIZE * ports.feedback().len() as u32);
        let calls_at = take(CALL_SIZE * members.len() as u32);
        let mut pin_offsets = Vec::with_capacity(members.len());
        let mut pins = 0;
        for member in &members {
            pin_offsets.push(pins);
            pins += member.inputs.len() as u32;
        }
        let pins_at = take(4 * pins);
        let (i, o, f) = (
            ports.inputs().len() as u32,
            ports.outputs().len() as u32,
            ports.feedback().len() as u32,
        );
        let lists = [
            take(4 * o),
            take(4 * i),
            take(4 * i),
            take(4 * i),
            take(4 * f),
        ];
        let heap_base = at.next_multiple_of(8);

        Ok(Self {
            abi,
            resolve,
            world,
            ports,
            members,
            edges,
            input_ty,
            stream_ty,
            output_ty,
            feedback_ty,
            control,
            ports_key,
            lend_key,
            return_area,
            return_area_size,
            inputs_at,
            outputs_at,
            feedback_at,
            calls_at,
            pins_at,
            pin_offsets,
            lists,
            heap_base,
        })
    }

    fn input_slot(&self, input: InputIdx) -> Static {
        slot(self.inputs_at, input.position())
    }

    fn output_slot(&self, output: OutputIdx) -> Static {
        slot(self.outputs_at, output.position())
    }

    fn feedback_slot(&self, connection: FeedbackIdx) -> Static {
        slot(self.feedback_at, connection.position())
    }

    fn call_state(&self, member: usize) -> Static {
        Static(self.calls_at.0 + CALL_SIZE * member as u32)
    }

    fn pin(&self, member: usize, field: usize) -> Static {
        Static(self.pins_at.0 + 4 * (self.pin_offsets[member] + field as u32))
    }

    fn func_of(&self, key: &WorldKey, name: &str) -> Result<&'a Function, HubError> {
        let Some(WorldItem::Interface { id, .. }) = self.resolve.worlds[self.world]
            .exports
            .get(key)
            .or_else(|| self.resolve.worlds[self.world].imports.get(key))
        else {
            return Err(HubError::Generate(format!("no interface for `{name}`")));
        };
        self.resolve.interfaces[*id]
            .functions
            .get(name)
            .ok_or_else(|| HubError::Generate(format!("no function `{name}`")))
    }

    fn imports(
        &self,
        module: &mut ModuleBuilder<super::module::AddingImports>,
    ) -> Result<Imports, HubError> {
        let callback = ManglingAndAbi::Legacy(LiftLowerAbi::AsyncCallback);
        let sync = ManglingAndAbi::Legacy(LiftLowerAbi::Sync);
        let mut runs = Vec::new();
        let mut drops = Vec::new();
        let mut resource_drops = HashMap::new();
        for member in &self.members {
            let (from, name) = self.resolve.wasm_import_name(
                callback,
                WasmImport::Func {
                    interface: Some(&member.key),
                    func: &member.func,
                },
            );
            runs.push(module.import(&from, &name, &sig_of(&member.sig)));
            let mut member_drops = HashMap::new();
            for (at, (_, field)) in member.outputs.iter().enumerate() {
                let Type::Id(id) = self.deref(&field.ty) else {
                    continue;
                };
                let intrinsic = match self.resolve.types[id].kind {
                    TypeDefKind::Stream(_) => WasmImport::StreamIntrinsic {
                        interface: Some(&member.key),
                        func: &member.func,
                        ty: Some(id),
                        intrinsic: StreamIntrinsic::DropReadable,
                        exported: false,
                        async_: false,
                    },
                    TypeDefKind::Future(_) => WasmImport::FutureIntrinsic {
                        interface: Some(&member.key),
                        func: &member.func,
                        ty: Some(id),
                        intrinsic: FutureIntrinsic::DropReadable,
                        exported: false,
                        async_: false,
                    },
                    _ => continue,
                };
                let (from, name) = self.resolve.wasm_import_name(callback, intrinsic);
                member_drops.insert(at, module.import(&from, &name, &Sig::i32s(1, 0)));
            }
            drops.push(member_drops);
            for (_, field) in &member.outputs {
                self.resources_owned(&field.ty, &mut |resource| {
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        resource_drops.entry(resource)
                    {
                        let (owner, name) = self.resource_import(resource, sync);
                        entry.insert(module.import(&owner, &name, &Sig::i32s(1, 0)));
                    }
                });
            }
        }
        let mut given_drops = HashMap::new();
        for member in &self.members {
            for (index, field) in &member.inputs {
                let Some(index) = index else { continue };
                let ty = field.optional.map_or(field.ty, |(payload, _)| payload);
                let Type::Id(id) = self.deref(&ty) else {
                    continue;
                };
                let intrinsic = match self.resolve.types[id].kind {
                    TypeDefKind::Stream(_) => WasmImport::StreamIntrinsic {
                        interface: Some(&member.key),
                        func: &member.func,
                        ty: Some(id),
                        intrinsic: StreamIntrinsic::DropReadable,
                        exported: false,
                        async_: false,
                    },
                    TypeDefKind::Future(_) => WasmImport::FutureIntrinsic {
                        interface: Some(&member.key),
                        func: &member.func,
                        ty: Some(id),
                        intrinsic: FutureIntrinsic::DropReadable,
                        exported: false,
                        async_: false,
                    },
                    _ => continue,
                };
                let (from, name) = self.resolve.wasm_import_name(callback, intrinsic);
                given_drops.insert(*index, module.import(&from, &name, &Sig::i32s(1, 0)));
            }
        }
        let mut lends = HashMap::new();
        if let Some(key) = &self.lend_key {
            let Some(WorldItem::Interface { id, .. }) =
                self.resolve.worlds[self.world].imports.get(key)
            else {
                return Err(HubError::Generate("no `lend` interface".into()));
            };
            for (output, _) in self.ports.outputs().iter_enumerated() {
                if let Some(func) = self.resolve.interfaces[*id]
                    .functions
                    .get(&lend_handle(output))
                {
                    let (from, name) = self.resolve.wasm_import_name(
                        sync,
                        WasmImport::Func {
                            interface: Some(key),
                            func,
                        },
                    );
                    lends.insert(output, module.import(&from, &name, &Sig::i32s(1, 0)));
                }
            }
        }
        let run = self.func_of(&self.control, ControlFunc::Run.name())?;
        let (from, name, sig) =
            run.task_return_import(self.resolve, Some(&self.control), Mangling::Legacy);
        let task_return = module.import(&from, &name, &sig_of(&sig));
        let root = |name: &str| ("$root", name.to_string());
        let mut intrinsic = |name: &str, sig: Sig| {
            let (from, name) = root(name);
            module.import(from, &name, &sig)
        };
        Ok(Imports {
            runs,
            drops,
            resource_drops,
            lends,
            given_drops,
            task_return,
            set_new: intrinsic("[waitable-set-new]", Sig::i32s(0, 1)),
            set_drop: intrinsic("[waitable-set-drop]", Sig::i32s(1, 0)),
            join: intrinsic("[waitable-join]", Sig::i32s(2, 0)),
            subtask_drop: intrinsic("[subtask-drop]", Sig::i32s(1, 0)),
            context_get: intrinsic("[context-get-0]", Sig::i32s(0, 1)),
            context_set: intrinsic("[context-set-0]", Sig::i32s(1, 0)),
        })
    }

    /// `ty`, aliases seen through.
    fn deref(&self, ty: &Type) -> Type {
        if let Type::Id(id) = ty
            && let TypeDefKind::Type(inner) = self.resolve.types[*id].kind
        {
            return self.deref(&inner);
        }
        *ty
    }

    /// Calls `found` with every resource a value of `ty` owns a handle of.
    fn resources_owned(&self, ty: &Type, found: &mut impl FnMut(TypeId)) {
        match self.abi.shape(ty) {
            Shape::Own(resource) => found(self.resource_def(resource)),
            Shape::List(inner) => self.resources_owned(&inner, found),
            Shape::Record(types) => types.iter().for_each(|ty| self.resources_owned(ty, found)),
            Shape::Cases(_, cases) => cases
                .iter()
                .flatten()
                .for_each(|ty| self.resources_owned(ty, found)),
            _ => {}
        }
    }

    /// The resource's own definition, aliases seen through.
    fn resource_def(&self, resource: TypeId) -> TypeId {
        match self.resolve.types[resource].kind {
            TypeDefKind::Type(Type::Id(inner)) => self.resource_def(inner),
            _ => resource,
        }
    }

    /// The name a resource's `drop` is imported under.
    fn resource_import(&self, resource: TypeId, mangling: ManglingAndAbi) -> (String, String) {
        let owner = match self.resolve.types[resource].owner {
            wit_parser::TypeOwner::Interface(id) => self.resolve.worlds[self.world]
                .imports
                .keys()
                .find(|key| {
                    matches!(
                        self.resolve.worlds[self.world].imports.get(*key),
                        Some(WorldItem::Interface { id: other, .. }) if *other == id
                    )
                })
                .cloned(),
            _ => None,
        };
        self.resolve.wasm_import_name(
            mangling,
            WasmImport::ResourceIntrinsic {
                interface: owner.as_ref(),
                resource,
                intrinsic: ResourceIntrinsic::ImportedDrop,
            },
        )
    }

    fn build(self) -> Result<Vec<u8>, HubError> {
        let mut module = ModuleBuilder::new();
        let imports = self.imports(&mut module)?;
        let mut module = module.seal();

        let rt = Runtime {
            alloc: module.declare(&Sig::i32s(1, 1)),
            dealloc: module.declare(&Sig::i32s(1, 0)),
            memeq: module.declare(&Sig::i32s(3, 1)),
            alloc_block: module.declare(&Sig::i32s(2, 1)),
            retain: module.declare(&Sig::i32s(1, 0)),
            release: module.declare(&Sig::i32s(1, 0)),
            free_block: module.declare(&Sig::i32s(1, 0)),
            slot_set: module.declare(&Sig::i32s(3, 0)),
            slot_clear: module.declare(&Sig::i32s(1, 1)),
        };
        self.write_allocator(&mut module, rt);
        self.write_memeq(&mut module, rt);
        self.write_slots(&mut module, rt);

        // What frees each kind of block, and compares each type.
        let mut kinds: Vec<BlockKind> = (0..self.members.len()).map(BlockKind::Outputs).collect();
        let mut boxed: HashMap<String, u32> = HashMap::new();
        for ty in self
            .input_ty
            .values()
            .chain(self.output_ty.values())
            .chain(self.feedback_ty.values())
        {
            let key = self.abi.key(ty);
            if !boxed.contains_key(&key) && self.is_value(ty) {
                boxed.insert(key, kinds.len() as u32);
                kinds.push(BlockKind::Boxed(*ty));
            }
        }
        let mut given: HashMap<InputIdx, u32> = HashMap::new();
        for input in self.stream_ty.keys() {
            given.insert(*input, kinds.len() as u32);
            kinds.push(BlockKind::Given(*input));
        }
        self.write_free_block(&mut module, rt, &imports, &kinds);
        let mut eqs: HashMap<String, FuncIdx<EqFn>> = HashMap::new();
        for ty in self
            .input_ty
            .values()
            .chain(self.output_ty.values())
            .chain(self.feedback_ty.values())
        {
            let key = self.abi.key(ty);
            if eqs.contains_key(&key) || !self.is_value(ty) {
                continue;
            }
            let f = self.write_eq(&mut module, rt, ty);
            eqs.insert(key, f);
        }
        let types = TypeOps { eqs, boxed, given };

        let completes = (0..self.members.len())
            .map(|member| self.write_complete(&mut module, rt, &imports, &types, member))
            .collect::<Vec<_>>();
        let starts = (0..self.members.len())
            .map(|member| self.write_start(&mut module, rt, &imports, member))
            .collect::<Vec<_>>();
        self.write_run(&mut module, &imports, &starts, &completes)?;
        self.write_control(&mut module, rt, &types)?;
        self.write_ports(&mut module, rt, &imports, &types)?;

        let realloc: FuncIdx<Helper> = module.declare(&Sig::i32s(4, 1));
        let (mut body, params) = Body::new(&Sig::i32s(4, 1));
        self.write_realloc(&mut body, &params, rt);
        module.define(realloc, body);
        let mangling = ManglingAndAbi::Legacy(LiftLowerAbi::Sync);
        module.export(
            &self.resolve.wasm_export_name(mangling, WasmExport::Realloc),
            realloc,
        );

        let memory = self.resolve.wasm_export_name(mangling, WasmExport::Memory);
        let pages = u64::from(self.heap_base / 65536) + 1;
        module
            .finish(&memory, pages, self.heap_base)
            .map_err(|e| HubError::Generate(e.to_string()))
    }

    /// Whether `ty` is a value the hub can compare and box: no stream, future
    /// or handle in it.
    fn is_value(&self, ty: &Type) -> bool {
        match self.abi.shape(ty) {
            Shape::Own(_) | Shape::Borrow | Shape::Stream | Shape::Future | Shape::Other(_) => {
                false
            }
            Shape::List(inner) => self.is_value(&inner),
            Shape::Record(types) => types.iter().all(|ty| self.is_value(ty)),
            Shape::Cases(_, cases) => cases.iter().flatten().all(|ty| self.is_value(ty)),
            _ => true,
        }
    }

    fn write_allocator(&self, module: &mut ModuleBuilder<AddingFuncs>, rt: Runtime) {
        // alloc(size) -> pointer, 8-aligned, with its size class in the 8
        // bytes before it. Nothing is ever given back to the engine: a freed
        // block waits on its class's list.
        let (mut b, params) = Body::new(&Sig::i32s(1, 1));
        let size = params[0];
        let (total, class, bytes, head, top) = (b.int(), b.int(), b.int(), b.int(), b.int());
        b.get(size).op(I::I32Eqz).when(|b| {
            // An empty list points nowhere: `dealloc` knows low addresses.
            b.int_const(8).ret();
        });
        b.get(size).int_const(BLOCK_HEADER).op(I::I32Add).set(total);
        b.get(total).get(size).op(I::I32LtU).when(|b| {
            b.op(I::Unreachable);
        });
        b.get(total).int_const(16).op(I::I32LeU).when_else(
            |b| {
                b.int_const(0).set(class).int_const(16).set(bytes);
            },
            |b| {
                // Four classes per doubling: at most a quarter is wasted.
                let (x, e, sub) = (b.int(), b.int(), b.int());
                b.get(total).int_const(1).op(I::I32Sub).set(x);
                b.int_const(31).get(x).op(I::I32Clz).op(I::I32Sub).set(e);
                b.get(x)
                    .get(e)
                    .int_const(2)
                    .op(I::I32Sub)
                    .op(I::I32ShrU)
                    .int_const(3)
                    .op(I::I32And)
                    .set(sub);
                b.get(e)
                    .int_const(4)
                    .op(I::I32Sub)
                    .int_const(4)
                    .op(I::I32Mul)
                    .get(sub)
                    .op(I::I32Add)
                    .int_const(1)
                    .op(I::I32Add)
                    .set(class);
                b.get(sub)
                    .int_const(5)
                    .op(I::I32Add)
                    .get(e)
                    .int_const(2)
                    .op(I::I32Sub)
                    .op(I::I32Shl)
                    .int_const(7)
                    .op(I::I32Add)
                    .int_const(!7)
                    .op(I::I32And)
                    .set(bytes);
            },
        );
        let list = b.int();
        b.int_const(FREE_LISTS)
            .get(class)
            .int_const(4)
            .op(I::I32Mul)
            .op(I::I32Add)
            .set(list);
        b.load32(list).set(head);
        b.get(head).when(|b| {
            b.store32(list, |b| {
                b.load32(head);
            });
            b.get(head).ret();
        });
        b.op(I::GlobalGet(HEAP_TOP)).set(head);
        b.get(head).get(bytes).op(I::I32Add).set(top);
        b.get(top).get(head).op(I::I32LtU).when(|b| {
            b.op(I::Unreachable);
        });
        // Grown as 64-bit, as 65536 pages do not fit 32 bits of bytes.
        let pages = b.int();
        b.get(top)
            .op(I::I64ExtendI32U)
            .op(I::I64Const(65535))
            .op(I::I64Add)
            .op(I::I64Const(16))
            .op(I::I64ShrU)
            .op(I::I32WrapI64)
            .op(I::MemorySize(0))
            .op(I::I32Sub)
            .set(pages);
        b.get(pages).int_const(0).op(I::I32GtS).when(|b| {
            b.get(pages)
                .op(I::MemoryGrow(0))
                .int_const(u32::MAX)
                .op(I::I32Eq)
                .when(|b| {
                    b.op(I::Unreachable);
                });
        });
        b.get(top).op(I::GlobalSet(HEAP_TOP));
        b.store32(head, |b| {
            b.get(class);
        });
        b.get(head).int_const(BLOCK_HEADER).op(I::I32Add);
        module.define(rt.alloc, b);

        // dealloc(pointer): what `alloc` gave, or a low address it did not.
        let (mut b, params) = Body::new(&Sig::i32s(1, 0));
        let ptr = params[0];
        b.get(ptr)
            .int_const(self.heap_base)
            .op(I::I32LtU)
            .when(|b| {
                b.ret();
            });
        let list = b.int();
        let header = b.int();
        b.get(ptr).int_const(BLOCK_HEADER).op(I::I32Sub).set(header);
        b.int_const(FREE_LISTS)
            .load32(header)
            .int_const(4)
            .op(I::I32Mul)
            .op(I::I32Add)
            .set(list);
        b.store32(ptr, |b| {
            b.load32(list);
        });
        b.store32(list, |b| {
            b.get(ptr);
        });
        module.define(rt.dealloc, b);
    }

    fn write_realloc(&self, b: &mut Body, params: &[Local], rt: Runtime) {
        let (old, old_size, new_size) = (params[0], params[1], params[3]);
        let new = b.int();
        b.get(new_size).call(rt.alloc).set(new);
        b.get(old).when(|b| {
            let keep = b.int();
            b.get(old_size)
                .get(new_size)
                .get(old_size)
                .get(new_size)
                .op(I::I32LtU)
                .op(I::Select)
                .set(keep);
            b.get(new).get(old).get(keep).op(I::MemoryCopy {
                src_mem: 0,
                dst_mem: 0,
            });
            b.get(old).call(rt.dealloc);
        });
        b.get(new);
    }

    fn write_memeq(&self, module: &mut ModuleBuilder<AddingFuncs>, rt: Runtime) {
        // memeq(a, b, len) -> bool: eight bytes at a time.
        let (mut f, params) = Body::new(&Sig::i32s(3, 1));
        let (a, c, len) = (params[0], params[1], params[2]);
        let i = f.int();
        f.op(I::Block(wasm_encoder::BlockType::Empty));
        f.op(I::Loop(wasm_encoder::BlockType::Empty));
        f.get(i)
            .int_const(8)
            .op(I::I32Add)
            .get(len)
            .op(I::I32GtU)
            .op(I::BrIf(1));
        let (pa, pc) = (f.int(), f.int());
        f.get(a).get(i).op(I::I32Add).set(pa);
        f.get(c).get(i).op(I::I32Add).set(pc);
        f.load(pa, I::I64Load)
            .load(pc, I::I64Load)
            .op(I::I64Ne)
            .when(|f| {
                f.int_const(0).ret();
            });
        f.get(i).int_const(8).op(I::I32Add).set(i);
        f.op(I::Br(0)).op(I::End).op(I::End);
        f.op(I::Block(wasm_encoder::BlockType::Empty));
        f.op(I::Loop(wasm_encoder::BlockType::Empty));
        f.get(i).get(len).op(I::I32GeU).op(I::BrIf(1));
        f.get(a).get(i).op(I::I32Add).set(pa);
        f.get(c).get(i).op(I::I32Add).set(pc);
        f.load(pa, I::I32Load8U)
            .load(pc, I::I32Load8U)
            .op(I::I32Ne)
            .when(|f| {
                f.int_const(0).ret();
            });
        f.get(i).int_const(1).op(I::I32Add).set(i);
        f.op(I::Br(0)).op(I::End).op(I::End);
        f.int_const(1);
        module.define(rt.memeq, f);
    }

    fn write_slots(&self, module: &mut ModuleBuilder<AddingFuncs>, rt: Runtime) {
        // alloc_block(kind, size) -> header, holding one reference.
        let (mut b, params) = Body::new(&Sig::i32s(2, 1));
        let (kind, size) = (params[0], params[1]);
        let block = b.int();
        b.get(size)
            .int_const(BLOCK_HEADER)
            .op(I::I32Add)
            .call(rt.alloc)
            .set(block);
        b.store32(block, |b| {
            b.int_const(1);
        });
        b.store32(Place::At(block, Offset(4)), |b| {
            b.get(kind);
        });
        b.get(block);
        module.define(rt.alloc_block, b);

        let (mut b, params) = Body::new(&Sig::i32s(1, 0));
        let block = params[0];
        b.store32(block, |b| {
            b.load32(block).int_const(1).op(I::I32Add);
        });
        module.define(rt.retain, b);

        let (mut b, params) = Body::new(&Sig::i32s(1, 0));
        let block = params[0];
        b.store32(block, |b| {
            b.load32(block).int_const(1).op(I::I32Sub);
        });
        b.load32(block).op(I::I32Eqz).when(|b| {
            b.get(block).call(rt.free_block);
        });
        module.define(rt.release, b);

        // slot_set(slot, block, address): the new value is held before the
        // old one is let go, which may be the same block.
        let (mut b, params) = Body::new(&Sig::i32s(3, 0));
        let (at, block, addr) = (params[0], params[1], params[2]);
        let old = b.int();
        b.load32(at).set(old);
        b.get(block).call(rt.retain);
        b.store32(at, |b| {
            b.get(block);
        });
        b.store32(Place::At(at, Offset(4)), |b| {
            b.get(addr);
        });
        b.get(old).when(|b| {
            b.get(old).call(rt.release);
        });
        module.define(rt.slot_set, b);

        // slot_clear(slot) -> whether it held a value.
        let (mut b, params) = Body::new(&Sig::i32s(1, 1));
        let at = params[0];
        let old = b.int();
        b.load32(at).set(old);
        b.get(old).op(I::I32Eqz).when(|b| {
            b.int_const(0).ret();
        });
        b.store32(at, |b| {
            b.int_const(0);
        });
        b.get(old).call(rt.release);
        b.int_const(1);
        module.define(rt.slot_clear, b);
    }

    fn write_free_block(
        &self,
        module: &mut ModuleBuilder<AddingFuncs>,
        rt: Runtime,
        imports: &Imports,
        kinds: &[BlockKind],
    ) {
        let (mut b, params) = Body::new(&Sig::i32s(1, 0));
        let block = params[0];
        let kind = b.int();
        b.load32(Place::At(block, Offset(4))).set(kind);
        let payload = Offset(BLOCK_HEADER);
        for (at, what) in kinds.iter().enumerate() {
            b.get(kind)
                .int_const(at as u32)
                .op(I::I32Eq)
                .when(|b| match what {
                    BlockKind::Outputs(member) => {
                        for (field_at, (_, field)) in
                            self.members[*member].outputs.iter().enumerate()
                        {
                            let place = Place::At(block, payload + field.offset);
                            if let Some(drop) = imports.drops[*member].get(&field_at) {
                                // Zero once it has moved to its reader.
                                let handle = b.int();
                                b.load32(place).set(handle);
                                b.get(handle).when(|b| {
                                    b.get(handle).call(*drop);
                                });
                            } else {
                                self.emit_free(b, rt, imports, &field.ty, place);
                            }
                        }
                    }
                    BlockKind::Boxed(ty) => {
                        self.emit_free(b, rt, imports, ty, Place::At(block, payload));
                    }
                    BlockKind::Given(input) => {
                        if let Some(drop) = imports.given_drops.get(input) {
                            // Zero once it has moved to its reader.
                            let handle = b.int();
                            b.load32(Place::At(block, payload)).set(handle);
                            b.get(handle).when(|b| {
                                b.get(handle).call(*drop);
                            });
                        }
                    }
                });
        }
        // The block came from `alloc` as a whole.
        b.get(block).call(rt.dealloc);
        module.define(rt.free_block, b);
    }

    /// Frees what the value at `place` owns.
    fn emit_free(&self, b: &mut Body, rt: Runtime, imports: &Imports, ty: &Type, place: Place) {
        if !self.abi.owns(ty) {
            return;
        }
        match self.abi.shape(ty) {
            Shape::String => {
                b.load32(place).call(rt.dealloc);
            }
            Shape::List(element) => {
                let (ptr, len) = (b.int(), b.int());
                b.load32(place).set(ptr);
                b.load32(place.offset(Offset(4))).set(len);
                if self.abi.owns(&element) {
                    let size = self.abi.size(&element);
                    let item = b.int();
                    b.count_up(len, |b, i| {
                        b.get(ptr)
                            .get(i)
                            .int_const(size.0)
                            .op(I::I32Mul)
                            .op(I::I32Add)
                            .set(item);
                        self.emit_free(b, rt, imports, &element, Place::At(item, Offset(0)));
                    });
                }
                b.get(ptr).call(rt.dealloc);
            }
            Shape::Own(resource) => {
                if let Some(drop) = imports.resource_drops.get(&self.resource_def(resource)) {
                    let handle = b.int();
                    b.load32(place).set(handle);
                    b.get(handle).when(|b| {
                        b.get(handle).call(*drop);
                    });
                }
            }
            Shape::Record(types) => {
                let offsets = self.abi.offsets(&types);
                for (ty, at) in types.iter().zip(offsets) {
                    self.emit_free(b, rt, imports, ty, place.offset(at));
                }
            }
            Shape::Cases(tag, cases) => {
                let payload = self.abi.payload_offset(tag, &cases);
                let which = b.int();
                b.load(place, int_load(tag)).set(which);
                for (at, case) in cases.iter().enumerate() {
                    let Some(case) = case else { continue };
                    if !self.abi.owns(case) {
                        continue;
                    }
                    b.get(which).int_const(at as u32).op(I::I32Eq).when(|b| {
                        self.emit_free(b, rt, imports, case, place.offset(payload));
                    });
                }
            }
            _ => {}
        }
    }

    fn write_eq(
        &self,
        module: &mut ModuleBuilder<AddingFuncs>,
        rt: Runtime,
        ty: &Type,
    ) -> FuncIdx<EqFn> {
        let sig = Sig::i32s(2, 1);
        let f: FuncIdx<EqFn> = module.declare(&sig);
        let (mut b, params) = Body::new(&sig);
        self.emit_eq(&mut b, rt, ty, params[0].into(), params[1].into());
        b.int_const(1);
        module.define(f, b);
        f
    }

    /// Returns 0 from the function if the values at `a` and `c` differ.
    fn emit_eq(&self, b: &mut Body, rt: Runtime, ty: &Type, a: Place, c: Place) {
        let differ = |b: &mut Body| {
            b.when(|b| {
                b.int_const(0).ret();
            });
        };
        if self.abi.dense(ty) {
            let size = self.abi.size(ty);
            match (size.0, self.abi.shape(ty)) {
                (1, _) => {
                    b.load(a, I::I32Load8U).load(c, I::I32Load8U).op(I::I32Ne);
                    differ(b);
                }
                (2, _) => {
                    b.load(a, I::I32Load16U).load(c, I::I32Load16U).op(I::I32Ne);
                    differ(b);
                }
                (4, _) => {
                    b.load32(a).load32(c).op(I::I32Ne);
                    differ(b);
                }
                (8, _) => {
                    b.load(a, I::I64Load).load(c, I::I64Load).op(I::I64Ne);
                    differ(b);
                }
                _ => {
                    b.addr(a)
                        .addr(c)
                        .int_const(size.0)
                        .call(rt.memeq)
                        .op(I::I32Eqz);
                    differ(b);
                }
            }
            return;
        }
        match self.abi.shape(ty) {
            Shape::Prim(Prim::F32) => {
                // All NaNs are one value; 0.0 and -0.0 are two.
                let (x, y) = (b.local(ValType::F32), b.local(ValType::F32));
                b.load(a, I::F32Load).set(x).load(c, I::F32Load).set(y);
                b.get(x)
                    .op(I::I32ReinterpretF32)
                    .get(y)
                    .op(I::I32ReinterpretF32)
                    .op(I::I32Ne)
                    .when(|b| {
                        b.get(x)
                            .get(x)
                            .op(I::F32Ne)
                            .get(y)
                            .get(y)
                            .op(I::F32Ne)
                            .op(I::I32And);
                        b.op(I::I32Eqz);
                        b.when(|b| {
                            b.int_const(0).ret();
                        });
                    });
            }
            Shape::Prim(Prim::F64) => {
                let (x, y) = (b.local(ValType::F64), b.local(ValType::F64));
                b.load(a, I::F64Load).set(x).load(c, I::F64Load).set(y);
                b.get(x)
                    .op(I::I64ReinterpretF64)
                    .get(y)
                    .op(I::I64ReinterpretF64)
                    .op(I::I64Ne)
                    .when(|b| {
                        b.get(x)
                            .get(x)
                            .op(I::F64Ne)
                            .get(y)
                            .get(y)
                            .op(I::F64Ne)
                            .op(I::I32And);
                        b.op(I::I32Eqz);
                        b.when(|b| {
                            b.int_const(0).ret();
                        });
                    });
            }
            Shape::String => {
                b.load32(a.offset(Offset(4)))
                    .load32(c.offset(Offset(4)))
                    .op(I::I32Ne);
                differ(b);
                b.load32(a)
                    .load32(c)
                    .load32(a.offset(Offset(4)))
                    .call(rt.memeq)
                    .op(I::I32Eqz);
                differ(b);
            }
            Shape::List(element) => {
                let (len, pa, pc) = (b.int(), b.int(), b.int());
                b.load32(a.offset(Offset(4))).set(len);
                b.get(len).load32(c.offset(Offset(4))).op(I::I32Ne);
                differ(b);
                b.load32(a).set(pa).load32(c).set(pc);
                let size = self.abi.size(&element);
                if self.abi.dense(&element) {
                    b.get(pa)
                        .get(pc)
                        .get(len)
                        .int_const(size.0)
                        .op(I::I32Mul)
                        .call(rt.memeq)
                        .op(I::I32Eqz);
                    differ(b);
                } else {
                    let (x, y) = (b.int(), b.int());
                    b.count_up(len, |b, i| {
                        b.get(pa)
                            .get(i)
                            .int_const(size.0)
                            .op(I::I32Mul)
                            .op(I::I32Add)
                            .set(x);
                        b.get(pc)
                            .get(i)
                            .int_const(size.0)
                            .op(I::I32Mul)
                            .op(I::I32Add)
                            .set(y);
                        self.emit_eq(b, rt, &element, x.into(), y.into());
                    });
                }
            }
            Shape::Record(types) => {
                let offsets = self.abi.offsets(&types);
                for (ty, at) in types.iter().zip(offsets) {
                    self.emit_eq(b, rt, ty, a.offset(at), c.offset(at));
                }
            }
            Shape::Cases(tag, cases) => {
                let payload = self.abi.payload_offset(tag, &cases);
                let which = b.int();
                b.load(a, int_load(tag)).set(which);
                b.get(which).load(c, int_load(tag)).op(I::I32Ne);
                differ(b);
                for (at, case) in cases.iter().enumerate() {
                    let Some(case) = case else { continue };
                    b.get(which).int_const(at as u32).op(I::I32Eq).when(|b| {
                        self.emit_eq(b, rt, case, a.offset(payload), c.offset(payload));
                    });
                }
            }
            // Dense, handled above; a handle or stream is always new.
            _ => {
                b.int_const(0).ret();
            }
        }
    }

    /// Stores `ty`'s flat form at `targets`, from the value at `place`.
    fn emit_flat(&self, b: &mut Body, ty: &Type, place: Place, targets: &[(Local, WasmType)]) {
        let put = |b: &mut Body, natural: WasmType, target: (Local, WasmType)| {
            use WasmType as W;
            match (natural, target.1) {
                (W::I32 | W::Pointer | W::Length, W::I64 | W::PointerOrI64) => {
                    b.op(I::I64ExtendI32U);
                }
                (W::F32, W::I32 | W::Pointer | W::Length) => {
                    b.op(I::I32ReinterpretF32);
                }
                (W::F32, W::I64 | W::PointerOrI64) => {
                    b.op(I::I32ReinterpretF32).op(I::I64ExtendI32U);
                }
                (W::F64, W::I64 | W::PointerOrI64) => {
                    b.op(I::I64ReinterpretF64);
                }
                _ => {}
            }
            b.set(target.0);
        };
        match self.abi.shape(ty) {
            Shape::Prim(Prim::U8) => {
                b.load(place, I::I32Load8U);
                put(b, WasmType::I32, targets[0]);
            }
            Shape::Prim(Prim::U16) => {
                b.load(place, I::I32Load16U);
                put(b, WasmType::I32, targets[0]);
            }
            Shape::Prim(Prim::U32)
            | Shape::Own(_)
            | Shape::Borrow
            | Shape::Stream
            | Shape::Future => {
                b.load32(place);
                put(b, WasmType::I32, targets[0]);
            }
            Shape::Prim(Prim::U64) => {
                b.load(place, I::I64Load);
                put(b, WasmType::I64, targets[0]);
            }
            Shape::Prim(Prim::F32) => {
                b.load(place, I::F32Load);
                put(b, WasmType::F32, targets[0]);
            }
            Shape::Prim(Prim::F64) => {
                b.load(place, I::F64Load);
                put(b, WasmType::F64, targets[0]);
            }
            Shape::Bits(tag) => {
                b.load(place, int_load(tag));
                put(b, WasmType::I32, targets[0]);
            }
            Shape::String | Shape::List(_) => {
                b.load32(place);
                put(b, WasmType::Pointer, targets[0]);
                b.load32(place.offset(Offset(4)));
                put(b, WasmType::Length, targets[1]);
            }
            Shape::Record(types) => {
                let offsets = self.abi.offsets(&types);
                let mut at = 0;
                for (ty, offset) in types.iter().zip(offsets) {
                    let count = self.abi.flat(ty).len();
                    self.emit_flat(b, ty, place.offset(offset), &targets[at..at + count]);
                    at += count;
                }
            }
            Shape::Cases(tag, cases) => {
                let payload = self.abi.payload_offset(tag, &cases);
                let which = b.int();
                b.load(place, int_load(tag)).set(which);
                b.get(which);
                put(b, WasmType::I32, targets[0]);
                for (at, case) in cases.iter().enumerate() {
                    let Some(case) = case else { continue };
                    b.get(which).int_const(at as u32).op(I::I32Eq).when(|b| {
                        self.emit_flat(b, case, place.offset(payload), &targets[1..]);
                    });
                }
            }
            Shape::Other(_) => {
                b.op(I::Unreachable);
            }
        }
    }

    /// `start_<member>() -> status`: lays out its inputs, holds what they
    /// borrow from, and makes the call.
    fn write_start(
        &self,
        module: &mut ModuleBuilder<AddingFuncs>,
        rt: Runtime,
        imports: &Imports,
        at: usize,
    ) -> FuncIdx<Helper> {
        let member = &self.members[at];
        let sig = Sig::i32s(0, 1);
        let f: FuncIdx<Helper> = module.declare(&sig);
        let (mut b, _) = Body::new(&sig);
        let call = self.call_state(at);
        let (params, results) = (b.int(), b.int());
        let size = member
            .inputs_ty
            .as_ref()
            .map_or(Size(0), |ty| self.abi.size(ty));
        b.int_const(size.0).call(rt.alloc).set(params);
        b.store32(call, |b| {
            b.get(params);
        });
        for (field_at, (index, field)) in member.inputs.iter().enumerate() {
            let to = Place::At(params, field.offset);
            b.store32(self.pin(at, field_at), |b| {
                b.int_const(0);
            });
            let Some(index) = index else { continue };
            let held = self.input_slot(*index);
            let (block, value) = (b.int(), b.int());
            b.load32(held).set(block);
            b.load32(Place::Fixed(held).offset(Offset(4))).set(value);
            let tag_some = |b: &mut Body, some: u32| {
                b.store(
                    to,
                    |b| {
                        b.int_const(some);
                    },
                    I::I32Store8,
                );
            };
            let payload_to = field.optional.map_or(to, |(_, at)| to.offset(at));
            let payload_ty = field.optional.map_or(field.ty, |(ty, _)| ty);
            b.get(block).when_else(
                |b| {
                    if field.optional.is_some() {
                        tag_some(b, 1);
                    }
                    let size = self.abi.size(&payload_ty);
                    b.copy(payload_to, value, size);
                    if field.kind.is_async() {
                        // Moved to its reader: the slot and the record it came
                        // from no longer have it.
                        b.store32(value, |b| {
                            b.int_const(0);
                        });
                        b.addr(held).call(rt.slot_clear).op(I::Drop);
                    } else {
                        // Lent until the call returns.
                        b.get(block).call(rt.retain);
                        b.store32(self.pin(at, field_at), |b| {
                            b.get(block);
                        });
                    }
                },
                |b| {
                    if field.optional.is_some() {
                        tag_some(b, 0);
                    } else {
                        // The scheduler checks required inputs first.
                        b.op(I::Unreachable);
                    }
                },
            );
        }
        if let Some(ty) = &member.outputs_ty {
            b.int_const(at as u32)
                .int_const(self.abi.size(ty).0)
                .call(rt.alloc_block)
                .set(results);
        }
        b.store32(Place::Fixed(call).offset(Offset(4)), |b| {
            b.get(results);
        });
        // Up to four core values are passed flat, more by pointer.
        if member.sig.indirect_params {
            b.get(params);
        } else if let Some(ty) = &member.inputs_ty {
            let flat = self.abi.flat(ty);
            let targets: Vec<(Local, WasmType)> = flat
                .iter()
                .map(|ty| (b.local(wasm_val(*ty)), *ty))
                .collect();
            self.emit_flat(&mut b, ty, Place::At(params, Offset(0)), &targets);
            for (local, _) in &targets {
                b.get(*local);
            }
        }
        if member.outputs_ty.is_some() {
            b.get(results).int_const(BLOCK_HEADER).op(I::I32Add);
        }
        b.call(imports.runs[at]);
        module.define(f, b);
        f
    }

    /// Delivers the value at (`block`, `value`) along `edge`.
    fn emit_edge(
        &self,
        b: &mut Body,
        rt: Runtime,
        counts: &ListCounts,
        edge: Edge,
        block: Local,
        value: Local,
    ) {
        match edge.to {
            Target::Feedback(conn) => {
                b.addr(self.feedback_slot(conn))
                    .get(block)
                    .get(value)
                    .call(rt.slot_set);
                self.emit_push(b, counts, List::Feedback, conn.position() as u32);
            }
            Target::Internal(input) | Target::External(input) => {
                let list = match edge.to {
                    Target::External(_) => List::Changed,
                    _ => List::Recorded,
                };
                let held = self.input_slot(input);
                let put = |b: &mut Body, at: Local| {
                    b.addr(held).get(block).get(at).call(rt.slot_set);
                    self.emit_push(b, counts, list, input.position() as u32);
                };
                match edge.unwrap {
                    None => put(b, value),
                    Some(payload) => {
                        let at = b.int();
                        b.get(value).int_const(payload.0).op(I::I32Add).set(at);
                        b.load(value, I::I32Load8U).when_else(
                            |b| put(b, at),
                            |b| {
                                b.addr(held).call(rt.slot_clear).when(|b| {
                                    self.emit_push(
                                        b,
                                        counts,
                                        List::Cleared,
                                        input.position() as u32,
                                    );
                                });
                            },
                        );
                    }
                }
            }
        }
    }

    /// Appends `value` to one of the lists of a `returned`.
    fn emit_push(&self, b: &mut Body, counts: &ListCounts, list: List, value: u32) {
        let count = counts.0[list as usize];
        let base = self.lists[list as usize];
        let at = b.int();
        b.get(count)
            .int_const(4)
            .op(I::I32Mul)
            .int_const(base.0)
            .op(I::I32Add)
            .set(at);
        b.store32(at, |b| {
            b.int_const(value);
        });
        b.get(count).int_const(1).op(I::I32Add).set(count);
    }

    /// `complete_<member>()`: a `run` returned. Its outputs are compared,
    /// latched and delivered, and the task returns what changed.
    fn write_complete(
        &self,
        module: &mut ModuleBuilder<AddingFuncs>,
        rt: Runtime,
        imports: &Imports,
        types: &TypeOps,
        at: usize,
    ) -> FuncIdx<Helper> {
        let member = &self.members[at];
        let sig = Sig::i32s(0, 0);
        let f: FuncIdx<Helper> = module.declare(&sig);
        let (mut b, _) = Body::new(&sig);
        let counts = ListCounts::new(&mut b);
        let call = self.call_state(at);
        let (params, results) = (b.int(), b.int());
        b.load32(call).set(params);
        b.load32(Place::Fixed(call).offset(Offset(4))).set(results);

        for (field_at, (index, field)) in member.outputs.iter().enumerate() {
            let Some(index) = index else { continue };
            let value = b.int();
            b.get(results)
                .int_const(BLOCK_HEADER + field.offset.0)
                .op(I::I32Add)
                .set(value);
            let edges = self.edges.get(index).cloned().unwrap_or_default();
            if field.kind.is_async() {
                match edges.first() {
                    Some(edge) => {
                        self.emit_edge(&mut b, rt, &counts, *edge, results, value);
                    }
                    None => {
                        // Nothing reads it: its writes fail rather than block.
                        if let Some(drop) = imports.drops[at].get(&field_at) {
                            b.load32(value).call(*drop);
                            b.store32(value, |b| {
                                b.int_const(0);
                            });
                        }
                    }
                }
                continue;
            }
            let held = self.output_slot(*index);
            let (old_block, old_value) = (b.int(), b.int());
            b.load32(held).set(old_block);
            b.load32(Place::Fixed(held).offset(Offset(4)))
                .set(old_value);
            let same = b.int();
            if let Some(eq) = types.eqs.get(&self.abi.key(&field.ty)) {
                b.get(old_block).when(|b| {
                    b.get(old_value).get(value).call(*eq).set(same);
                });
            }
            b.get(same).when_else(
                |b| {
                    // Unchanged: the old value stands. Feedback is still due,
                    // and an input that lost its value gets it back.
                    for edge in &edges {
                        match edge.to {
                            Target::Feedback(_) => {
                                self.emit_edge(b, rt, &counts, *edge, old_block, old_value);
                            }
                            Target::Internal(input) | Target::External(input) => {
                                b.load32(self.input_slot(input)).op(I::I32Eqz).when(|b| {
                                    self.emit_edge(b, rt, &counts, *edge, old_block, old_value);
                                });
                            }
                        }
                    }
                },
                |b| {
                    b.addr(held).get(results).get(value).call(rt.slot_set);
                    self.emit_push(b, &counts, List::Outputs, index.position() as u32);
                    for edge in &edges {
                        self.emit_edge(b, rt, &counts, *edge, results, value);
                    }
                },
            );
        }

        // The call is over: what it borrowed is let go.
        for field_at in 0..member.inputs.len() {
            let pin = b.int();
            b.load32(self.pin(at, field_at)).set(pin);
            b.get(pin).when(|b| {
                b.get(pin).call(rt.release);
            });
        }
        b.get(params).call(rt.dealloc);
        if member.outputs_ty.is_some() {
            b.get(results).call(rt.release);
        }
        for list in 0..5 {
            b.int_const(self.lists[list].0).get(counts.0[list]);
        }
        b.call(imports.task_return);
        module.define(f, b);
        f
    }

    /// `witgraph:hub/control#run`, and its callback.
    fn write_run(
        &self,
        module: &mut ModuleBuilder<AddingFuncs>,
        imports: &Imports,
        starts: &[FuncIdx<Helper>],
        completes: &[FuncIdx<Helper>],
    ) -> Result<(), HubError> {
        let mangling = ManglingAndAbi::Legacy(LiftLowerAbi::AsyncCallback);
        let run = self.func_of(&self.control, ControlFunc::Run.name())?;
        let export = |kind| {
            self.resolve.wasm_export_name(
                mangling,
                WasmExport::Func {
                    interface: Some(&self.control),
                    func: run,
                    kind,
                },
            )
        };
        let set_at = |member: Local, b: &mut Body, set: Local| {
            let at = b.int();
            b.get(member)
                .int_const(CALL_SIZE)
                .op(I::I32Mul)
                .int_const(self.calls_at.0 + 8)
                .op(I::I32Add)
                .set(at);
            b.store32(at, |b| {
                b.get(set);
            });
        };

        let sig = Sig::i32s(1, 1);
        let f: FuncIdx<Helper> = module.declare(&sig);
        let (mut b, params) = Body::new(&sig);
        let member = params[0];
        let status = b.int();
        for (at, start) in starts.iter().enumerate() {
            b.get(member).int_const(at as u32).op(I::I32Eq).when(|b| {
                b.call(*start).set(status);
            });
        }
        b.get(status)
            .int_const(15)
            .op(I::I32And)
            .int_const(STATUS_RETURNED)
            .op(I::I32Eq)
            .when(|b| {
                for (at, complete) in completes.iter().enumerate() {
                    b.get(member).int_const(at as u32).op(I::I32Eq).when(|b| {
                        b.call(*complete);
                    });
                }
                b.int_const(CALLBACK_EXIT).ret();
            });
        let set = b.int();
        b.call(imports.set_new).set(set);
        b.get(status)
            .int_const(4)
            .op(I::I32ShrU)
            .get(set)
            .call(imports.join);
        set_at(member, &mut b, set);
        b.get(member).call(imports.context_set);
        b.get(set)
            .int_const(4)
            .op(I::I32Shl)
            .int_const(CALLBACK_WAIT)
            .op(I::I32Or);
        module.define(f, b);
        module.export(&export(WasmExportKind::Normal), f);

        let sig = Sig::i32s(3, 1);
        let f: FuncIdx<Helper> = module.declare(&sig);
        let (mut b, params) = Body::new(&sig);
        let (event, subtask, state) = (params[0], params[1], params[2]);
        let (member, set, at) = (b.int(), b.int(), b.int());
        b.call(imports.context_get).set(member);
        b.get(member)
            .int_const(CALL_SIZE)
            .op(I::I32Mul)
            .int_const(self.calls_at.0 + 8)
            .op(I::I32Add)
            .set(at);
        b.load32(at).set(set);
        let done = b.int();
        b.get(event)
            .int_const(EVENT_SUBTASK)
            .op(I::I32Eq)
            .get(state)
            .int_const(STATUS_RETURNED)
            .op(I::I32Eq)
            .op(I::I32And)
            .set(done);
        b.get(done).op(I::I32Eqz).when(|b| {
            b.get(set)
                .int_const(4)
                .op(I::I32Shl)
                .int_const(CALLBACK_WAIT)
                .op(I::I32Or)
                .ret();
        });
        b.get(subtask).int_const(0).call(imports.join);
        b.get(subtask).call(imports.subtask_drop);
        b.get(set).call(imports.set_drop);
        for (index, complete) in completes.iter().enumerate() {
            b.get(member)
                .int_const(index as u32)
                .op(I::I32Eq)
                .when(|b| {
                    b.call(*complete);
                });
        }
        b.int_const(CALLBACK_EXIT);
        module.define(f, b);
        module.export(&export(WasmExportKind::Callback), f);
        Ok(())
    }

    /// The rest of `witgraph:hub/control`.
    fn write_control(
        &self,
        module: &mut ModuleBuilder<AddingFuncs>,
        rt: Runtime,
        types: &TypeOps,
    ) -> Result<(), HubError> {
        let mangling = ManglingAndAbi::Legacy(LiftLowerAbi::Sync);
        let export = |func: &Function| {
            self.resolve.wasm_export_name(
                mangling,
                WasmExport::Func {
                    interface: Some(&self.control),
                    func,
                    kind: WasmExportKind::Normal,
                },
            )
        };

        // clear-input(input) -> bool
        let sig = Sig::i32s(1, 1);
        let f: FuncIdx<Helper> = module.declare(&sig);
        let (mut b, params) = Body::new(&sig);
        b.get(params[0])
            .int_const(SLOT_SIZE)
            .op(I::I32Mul)
            .int_const(self.inputs_at.0)
            .op(I::I32Add)
            .call(rt.slot_clear);
        module.define(f, b);
        module.export(
            &export(self.func_of(&self.control, ControlFunc::ClearInput.name())?),
            f,
        );

        // drop-feedback(connection)
        let sig = Sig::i32s(1, 0);
        let f: FuncIdx<Helper> = module.declare(&sig);
        let (mut b, params) = Body::new(&sig);
        b.get(params[0])
            .int_const(SLOT_SIZE)
            .op(I::I32Mul)
            .int_const(self.feedback_at.0)
            .op(I::I32Add)
            .call(rt.slot_clear)
            .op(I::Drop);
        module.define(f, b);
        module.export(
            &export(self.func_of(&self.control, ControlFunc::DropFeedback.name())?),
            f,
        );

        // latch-feedback(connection) -> latched: 0 unchanged, 1 changed, 2 cleared.
        let sig = Sig::i32s(1, 1);
        let f: FuncIdx<Helper> = module.declare(&sig);
        let (mut b, params) = Body::new(&sig);
        let connection = params[0];
        for (conn, entry) in self.ports.feedback().iter_enumerated() {
            b.get(connection)
                .int_const(conn.position() as u32)
                .op(I::I32Eq)
                .when(|b| {
                    let held = self.feedback_slot(conn);
                    let (block, value) = (b.int(), b.int());
                    b.load32(held).set(block);
                    b.load32(Place::Fixed(held).offset(Offset(4))).set(value);
                    b.get(block).op(I::I32Eqz).when(|b| {
                        b.int_const(0).ret();
                    });
                    b.get(block).call(rt.retain);
                    b.addr(held).call(rt.slot_clear).op(I::Drop);
                    let input = self.input_slot(entry.to);
                    let at = b.int();
                    b.get(value).set(at);
                    let unwrap = entry.unwraps_option.then(|| {
                        self.feedback_ty.get(&conn).and_then(|ty| {
                            let payload = payload_of(self.resolve, ty)?;
                            Some(self.abi.payload_offset(Int::U8, &[None, Some(payload)]))
                        })
                    });
                    if let Some(Some(payload)) = unwrap {
                        b.load(value, I::I32Load8U).op(I::I32Eqz).when(|b| {
                            let had = b.int();
                            b.addr(input).call(rt.slot_clear).set(had);
                            b.get(block).call(rt.release);
                            b.get(had).when_else(
                                |b| {
                                    b.int_const(2).ret();
                                },
                                |b| {
                                    b.int_const(0).ret();
                                },
                            );
                        });
                        b.get(value).int_const(payload.0).op(I::I32Add).set(at);
                    }
                    let same = b.int();
                    if let Some(eq) = self
                        .input_ty
                        .get(&entry.to)
                        .and_then(|ty| types.eqs.get(&self.abi.key(ty)))
                    {
                        b.load32(input).when(|b| {
                            b.load32(Place::Fixed(input).offset(Offset(4)))
                                .get(at)
                                .call(*eq)
                                .set(same);
                        });
                    }
                    b.get(same).when(|b| {
                        b.get(block).call(rt.release);
                        b.int_const(0).ret();
                    });
                    b.addr(input).get(block).get(at).call(rt.slot_set);
                    b.get(block).call(rt.release);
                    b.int_const(1).ret();
                });
        }
        b.int_const(0);
        module.define(f, b);
        module.export(
            &export(self.func_of(&self.control, ControlFunc::LatchFeedback.name())?),
            f,
        );
        Ok(())
    }

    /// `witgraph:hub/ports`.
    fn write_ports(
        &self,
        module: &mut ModuleBuilder<AddingFuncs>,
        rt: Runtime,
        imports: &Imports,
        types: &TypeOps,
    ) -> Result<(), HubError> {
        debug_assert!(self.return_area_size >= RETURNED_SIZE);
        for (input, ty) in &self.input_ty {
            let held = self.input_slot(*input);
            self.write_getter(module, &get_input(*input), held, ty)?;
            self.write_setter(module, rt, types, &set_input(*input), (held, ty), true)?;
        }
        for (input, ty) in &self.stream_ty {
            self.write_take(module, rt, *input, ty)?;
            self.write_give(module, rt, types, *input)?;
        }
        for (output, ty) in &self.output_ty {
            match self.ports.outputs()[*output].kind {
                PortKind::Value => {
                    let held = self.output_slot(*output);
                    self.write_getter(module, &get_output(*output), held, ty)?;
                    self.write_setter(module, rt, types, &set_output(*output), (held, ty), false)?;
                }
                PortKind::Handle => {
                    let sig = Sig::i32s(0, 0);
                    let f: FuncIdx<Helper> = module.declare(&sig);
                    let (mut b, _) = Body::new(&sig);
                    let held = self.output_slot(*output);
                    if let Some(lend) = imports.lends.get(output) {
                        b.load32(held).when(|b| {
                            let value = b.int();
                            b.load32(Place::Fixed(held).offset(Offset(4))).set(value);
                            b.load32(value).call(*lend);
                        });
                    }
                    module.define(f, b);
                    self.export_port(module, &read_handle(*output), f)?;
                }
                PortKind::Stream | PortKind::Future => {}
            }
        }
        // A handle in feedback is the host's, not text: it has no getter or setter.
        for (conn, ty) in self.feedback_ty.iter().filter(|(_, ty)| self.is_value(ty)) {
            let held = self.feedback_slot(*conn);
            self.write_getter(module, &get_feedback(*conn), held, ty)?;
            self.write_setter(module, rt, types, &set_feedback(*conn), (held, ty), false)?;
        }
        Ok(())
    }

    fn export_port<K: super::module::FuncKind>(
        &self,
        module: &mut ModuleBuilder<AddingFuncs>,
        name: &str,
        f: FuncIdx<K>,
    ) -> Result<(), HubError> {
        let func = self.func_of(&self.ports_key, name)?;
        module.export(
            &self.resolve.wasm_export_name(
                ManglingAndAbi::Legacy(LiftLowerAbi::Sync),
                WasmExport::Func {
                    interface: Some(&self.ports_key),
                    func,
                    kind: WasmExportKind::Normal,
                },
            ),
            f,
        );
        Ok(())
    }

    /// `() -> option<T>`, pointing into the slot's value: the engine copies it
    /// out as the call returns.
    fn write_getter(
        &self,
        module: &mut ModuleBuilder<AddingFuncs>,
        name: &str,
        held: Static,
        ty: &Type,
    ) -> Result<(), HubError> {
        let sig = Sig::i32s(0, 1);
        let f: FuncIdx<Helper> = module.declare(&sig);
        let (mut b, _) = Body::new(&sig);
        let area = self.return_area;
        let payload = self.abi.payload_offset(Int::U8, &[None, Some(*ty)]);
        let (block, value) = (b.int(), b.int());
        b.load32(held).set(block);
        b.load32(Place::Fixed(held).offset(Offset(4))).set(value);
        b.store(
            area,
            |b| {
                b.get(block).int_const(0).op(I::I32Ne);
            },
            I::I32Store8,
        );
        b.get(block).when(|b| {
            b.copy(Place::Fixed(area).offset(payload), value, self.abi.size(ty));
        });
        b.int_const(area.0);
        module.define(f, b);
        self.export_port(module, name, f)
    }

    /// `() -> option<stream<T>>`: the handle leaves the hub, and its slot.
    fn write_take(
        &self,
        module: &mut ModuleBuilder<AddingFuncs>,
        rt: Runtime,
        input: InputIdx,
        ty: &Type,
    ) -> Result<(), HubError> {
        let sig = Sig::i32s(0, 1);
        let f: FuncIdx<Helper> = module.declare(&sig);
        let (mut b, _) = Body::new(&sig);
        let area = self.return_area;
        let held = self.input_slot(input);
        let payload = self.abi.payload_offset(Int::U8, &[None, Some(*ty)]);
        let (block, value) = (b.int(), b.int());
        b.load32(held).set(block);
        b.load32(Place::Fixed(held).offset(Offset(4))).set(value);
        b.store(
            area,
            |b| {
                b.get(block).int_const(0).op(I::I32Ne);
            },
            I::I32Store8,
        );
        b.get(block).when(|b| {
            b.copy(Place::Fixed(area).offset(payload), value, self.abi.size(ty));
            // Whatever held it no longer does.
            b.store32(value, |b| {
                b.int_const(0);
            });
            b.addr(held).call(rt.slot_clear).op(I::Drop);
        });
        b.int_const(area.0);
        module.define(f, b);
        self.export_port(module, &take_stream(input), f)
    }

    /// `(value: stream<T>)`: the input holds it until its reader starts. A handle
    /// it held already is dropped.
    fn write_give(
        &self,
        module: &mut ModuleBuilder<AddingFuncs>,
        rt: Runtime,
        types: &TypeOps,
        input: InputIdx,
    ) -> Result<(), HubError> {
        let sig = Sig::i32s(1, 0);
        let f: FuncIdx<Helper> = module.declare(&sig);
        let (mut b, params) = Body::new(&sig);
        let held = self.input_slot(input);
        let kind = types.given.get(&input).copied().unwrap_or(0);
        let block = b.int();
        b.int_const(kind)
            .int_const(4)
            .call(rt.alloc_block)
            .set(block);
        let value = b.int();
        b.get(block)
            .int_const(BLOCK_HEADER)
            .op(I::I32Add)
            .set(value);
        b.store32(value, |b| {
            b.get(params[0]);
        });
        b.addr(held).get(block).get(value).call(rt.slot_set);
        // The slot holds it now; `alloc_block`'s reference is done.
        b.get(block).call(rt.release);
        module.define(f, b);
        self.export_port(module, &give_stream(input), f)
    }

    /// `(value: list<T>) -> bool`: the list holds the one value, in memory as
    /// the canonical ABI lays it out, whatever `T` flattens to. With `compare`,
    /// a value equal to the current one changes nothing.
    fn write_setter(
        &self,
        module: &mut ModuleBuilder<AddingFuncs>,
        rt: Runtime,
        types: &TypeOps,
        name: &str,
        (held, ty): (Static, &Type),
        compare: bool,
    ) -> Result<(), HubError> {
        let sig = Sig::i32s(2, 1);
        let f: FuncIdx<Helper> = module.declare(&sig);
        let (mut b, params) = Body::new(&sig);
        let list = params[0];
        let size = self.abi.size(ty);
        let (block, value) = (b.int(), b.int());
        let key = self.abi.key(ty);
        let kind = types.boxed.get(&key).copied().unwrap_or(0);
        b.int_const(kind)
            .int_const(size.0)
            .call(rt.alloc_block)
            .set(block);
        b.get(block)
            .int_const(BLOCK_HEADER)
            .op(I::I32Add)
            .set(value);
        b.copy(value, list, size);
        b.get(list).call(rt.dealloc);
        if compare && let Some(eq) = types.eqs.get(&key) {
            let same = b.int();
            b.load32(held).when(|b| {
                b.load32(Place::Fixed(held).offset(Offset(4)))
                    .get(value)
                    .call(*eq)
                    .set(same);
            });
            b.get(same).when(|b| {
                b.get(block).call(rt.release);
                b.int_const(0).ret();
            });
        }
        b.addr(held).get(block).get(value).call(rt.slot_set);
        // The slot holds it now; `alloc_block`'s reference is done.
        b.get(block).call(rt.release);
        b.int_const(1);
        module.define(f, b);
        self.export_port(module, name, f)
    }
}

/// What compares and boxes each type, by its key.
struct TypeOps {
    /// What frees a stream or future given to each input, as a block kind.
    given: HashMap<InputIdx, u32>,
    eqs: HashMap<String, FuncIdx<EqFn>>,
    boxed: HashMap<String, u32>,
}

/// The five lists of a `returned`, in its order.
#[derive(Clone, Copy)]
enum List {
    Outputs = 0,
    Changed = 1,
    Recorded = 2,
    Cleared = 3,
    Feedback = 4,
}

/// Locals counting what each list holds so far.
struct ListCounts([Local; 5]);

impl ListCounts {
    fn new(b: &mut Body) -> Self {
        Self([b.int(), b.int(), b.int(), b.int(), b.int()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_control_interface_declares_every_function() {
        for func in ControlFunc::ALL {
            let declared = format!("{}: ", func.name());
            assert!(CONTROL_WIT.contains(&declared), "{declared}");
        }
    }
}
