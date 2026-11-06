//! Puts a probe on each source line of a core WebAssembly module, from its DWARF.
//!
//! A browser gives a page no way to set a breakpoint in WebAssembly, so a debugger has to
//! build them in. Each probe reads one byte of a shared memory the module defines, and
//! calls an import only when that byte is set. Reading the byte costs next to nothing, and
//! as the memory is shared, another thread can set it while the module runs, even in a
//! loop that never returns to its host.
//!
//! The module's memories keep their indices, as engines reach the first one fastest, so
//! the flags come after them.
//!
//! A probe that fires first writes out, after the flags, the locals and globals the debug
//! info reads variables from, as nothing outside a function can read its locals:
//! [`ModuleDebug`] finds its variables from there. Only those: reading a local at all keeps it
//! alive across each probe, which slows a loop even though no probe fires.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::Infallible;

use gimli::{ColumnType, Dwarf, EndianSlice, LittleEndian, SectionId};
use wasm_encoder::reencode::{self, Reencode};
use wasm_encoder::{
    BlockType, CodeSection, EntityType, ExportKind, ExportSection, Function, ImportSection,
    Instruction, MemArg, MemArgTarget, MemorySection, MemoryType, TypeSection, ValType,
};
use wasmparser::{
    CompositeInnerType, FunctionBody, KnownCustom, Name, Operator, Parser, Payload, TypeRef,
};

mod variables;
use variables::ReadFrom;
pub use variables::{Frame, ModuleDebug};
pub use witgraph_traits::lines::{
    OwnLines, OwnRules, PathPattern, SourceFile, SourceLanguage, SourceVariable as Variable,
};

/// The module of the import a probe calls.
pub const IMPORT_MODULE: &str = "witgraph:debug";
/// The import a probe calls, with its [`ProbeId`], when its flag is set.
pub const IMPORT_PROBE: &str = "probe";
/// The export of the shared memory of flags, one byte for each [`ProbeId`].
pub const EXPORT_FLAGS: &str = "witgraph:debug/flags";

/// Why a module could not be given probes.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// The module does not parse.
    #[error("the module does not parse: {0}")]
    Parse(#[from] wasmparser::BinaryReaderError),
    /// Its debug info does not parse.
    #[error("the module's debug info does not parse: {0}")]
    Dwarf(#[from] gimli::Error),
    /// It could not be written back.
    #[error("the module could not be rewritten: {0}")]
    Rewrite(String),
    /// Its debug info names code that is not there.
    #[error("{unplaced} of the module's {sites} source lines are not in its code")]
    Unplaced {
        /// The lines not placed.
        unplaced: usize,
        /// The lines there were.
        sites: usize,
    },
}

/// Where a probe's flag is, and what it passes to the import.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProbeId(pub u32);

/// A byte offset from the start of a module, as a debugger's addresses are given.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModuleOffset(pub u32);

/// A source line a probe stands at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    /// Where it is in the original module.
    pub offset: ModuleOffset,
    /// The function it is in, by its index in the original module.
    pub function: u32,
    /// The source file, as the debug info has it.
    pub file: String,
    /// The line, from 1.
    pub line: u32,
    /// The column, from 1, or 0 for no particular one.
    pub column: u32,
}

/// A module with its probes in.
#[derive(Clone, Debug)]
pub struct Instrumented {
    /// The new module, with no debug info: it would not describe this one. Read what you
    /// need of the original.
    pub module: Vec<u8>,
    /// The probes, each at its [`ProbeId`].
    pub probes: Vec<Probe>,
    /// The calls in functions with probes, for finding the line of a stack frame.
    pub calls: Vec<CallSite>,
    /// Where a firing probe writes out what it can see, in the memory of flags.
    pub spill: Spill,
}

/// A call, in the module with probes and in the original.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallSite {
    /// Where it is in the new module, as a stack trace of that gives it.
    pub offset: ModuleOffset,
    /// Where it is in the original.
    pub original: ModuleOffset,
    /// The function it is in, by its index in the original.
    pub function: u32,
}

/// Where a firing probe writes out locals and globals: 8 bytes for each, little-endian, the
/// globals first, each at its own index whether or not it is written. Those that are not
/// numbers are left out, as are locals past [`MOST_SPILLED_LOCALS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spill {
    /// The offset of the first, after the flags.
    pub base: u32,
    /// How many slots the globals take.
    pub globals: u32,
}

impl Spill {
    /// The bytes of `local`, in `spilled`, the memory from [`Spill::base`] on.
    #[must_use]
    pub fn local(&self, spilled: &[u8], local: u32) -> Option<[u8; 8]> {
        slot(spilled, u64::from(self.globals) + u64::from(local))
    }

    /// The bytes of `global`, in `spilled`, the memory from [`Spill::base`] on.
    #[must_use]
    pub fn global(&self, spilled: &[u8], global: u32) -> Option<[u8; 8]> {
        (global < self.globals)
            .then(|| slot(spilled, u64::from(global)))
            .flatten()
    }
}

fn slot(spilled: &[u8], at: u64) -> Option<[u8; 8]> {
    let start = usize::try_from(at.checked_mul(8)?).ok()?;
    spilled.get(start..start.checked_add(8)?)?.try_into().ok()
}

/// The language of a compile unit's code, if it is one witgraph knows.
fn language_of<R: gimli::Reader>(
    unit: &gimli::Unit<R>,
) -> Result<Option<SourceLanguage>, ProbeError> {
    use gimli::{
        DW_LANG_C, DW_LANG_C_plus_plus, DW_LANG_C_plus_plus_03, DW_LANG_C_plus_plus_11,
        DW_LANG_C_plus_plus_14, DW_LANG_C_plus_plus_17, DW_LANG_C_plus_plus_20, DW_LANG_C11,
        DW_LANG_C17, DW_LANG_C89, DW_LANG_C99, DW_LANG_Rust,
    };
    let root = unit.entry(unit.header.root_offset())?;
    let Some(gimli::AttributeValue::Language(language)) = root.attr_value(gimli::DW_AT_language)
    else {
        return Ok(None);
    };
    let c = [
        DW_LANG_C89,
        DW_LANG_C,
        DW_LANG_C99,
        DW_LANG_C11,
        DW_LANG_C17,
    ];
    let cpp = [
        DW_LANG_C_plus_plus,
        DW_LANG_C_plus_plus_03,
        DW_LANG_C_plus_plus_11,
        DW_LANG_C_plus_plus_14,
        DW_LANG_C_plus_plus_17,
        DW_LANG_C_plus_plus_20,
    ];
    Ok(if language == DW_LANG_Rust {
        Some(SourceLanguage::Rust)
    } else if c.contains(&language) {
        Some(SourceLanguage::C)
    } else if cpp.contains(&language) {
        Some(SourceLanguage::Cpp)
    } else {
        None
    })
}

/// `path` with its `.` and `..` worked out, as far as its text says.
fn normalized(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "." => {}
            ".." if parts
                .last()
                .is_some_and(|last| !last.is_empty() && *last != "..") =>
            {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    let joined = parts.join("/");
    // `a//b` is `a/b`, but a leading `/` stays.
    let mut out = String::with_capacity(joined.len());
    for (at, part) in joined.split('/').enumerate() {
        if at > 0 && part.is_empty() {
            continue;
        }
        if at > 0 {
            out.push('/');
        }
        out.push_str(part);
    }
    out
}

/// The directory of the guest's project: that of the compile unit its `run` is in, or the
/// first with debug info that `run` calls, as a C guest's bindings have none. Of the directory
/// the unit was compiled in and that of its first file, what they have in common, so it is the
/// project's whether a compiler ran in it, or in a build directory of it. `None` for a module
/// with no `run`, or none that comes to debug info.
fn anchor_root<R: gimli::Reader>(
    module: &[u8],
    dwarf: &Dwarf<R>,
) -> Result<Option<String>, ProbeError> {
    let shape = shape_of(module)?;
    let Some(code_start) = shape.code_start else {
        return Ok(None);
    };
    let mut run = None;
    // By defined function, where its body is and the functions it calls.
    let mut bodies: Vec<(std::ops::Range<u64>, Vec<u32>)> = Vec::new();
    for payload in Parser::new(0).parse_all(module) {
        match payload? {
            Payload::ExportSection(section) => {
                for export in section {
                    let export = export?;
                    // `[async-lift]node#run`, and not its callback.
                    let named =
                        export.name.ends_with("#run") && !export.name.starts_with("[callback]");
                    if named && export.kind == wasmparser::ExternalKind::Func {
                        run = Some(export.index);
                    }
                }
            }
            Payload::CodeSectionEntry(body) => {
                let mut calls = Vec::new();
                let mut reader = body.get_operators_reader()?;
                while !reader.eof() {
                    if let Operator::Call { function_index } = reader.read()? {
                        calls.push(function_index);
                    }
                }
                let range = body.range();
                bodies.push((range.start..range.end, calls));
            }
            _ => {}
        }
    }
    let Some(run) = run else {
        return Ok(None);
    };
    let mut units = Vec::new();
    let mut headers = dwarf.units();
    while let Some(header) = headers.next()? {
        let unit = dwarf.unit(header)?;
        let mut ranges = Vec::new();
        let mut found = dwarf.unit_ranges(&unit)?;
        while let Some(range) = found.next()? {
            ranges.push(range.begin..range.end);
        }
        units.push((unit, ranges));
    }
    let mut seen = HashSet::from([run]);
    let mut waiting = std::collections::VecDeque::from([run]);
    while let Some(function) = waiting.pop_front() {
        // Not far: past a few calls, it is not the guest's `run` any more.
        if seen.len() > MOST_ANCHOR_CALLS {
            break;
        }
        let Some(defined) = function.checked_sub(shape.imported_functions) else {
            continue;
        };
        let Some((body, calls)) = bodies.get(defined as usize) else {
            continue;
        };
        let (begin, end) = (
            body.start.saturating_sub(code_start),
            body.end.saturating_sub(code_start),
        );
        let covering = units.iter().find(|(_, ranges)| {
            ranges
                .iter()
                .any(|range| range.start < end && begin < range.end)
        });
        if let Some((unit, _)) = covering {
            return Ok(project_root(unit));
        }
        for call in calls {
            if seen.insert(*call) {
                waiting.push_back(*call);
            }
        }
    }
    Ok(None)
}

/// How many functions [`anchor_root`] looks at, from `run`, for one with debug info.
const MOST_ANCHOR_CALLS: usize = 64;

/// The directory of the project a compile unit is of: see [`anchor_root`].
fn project_root<R: gimli::Reader>(unit: &gimli::Unit<R>) -> Option<String> {
    let built_in = unit
        .comp_dir
        .clone()
        .and_then(|dir| dir.to_string_lossy().ok().map(|dir| dir.into_owned()));
    let name = unit
        .name
        .clone()
        .and_then(|name| name.to_string_lossy().ok().map(|name| name.into_owned()));
    // Rust names a unit `{file}/@/{codegen unit}`.
    let name = name.map(|name| match name.split_once("/@/") {
        Some((file, _)) => file.to_string(),
        None => name,
    });
    let file = match (name, &built_in) {
        (Some(name), Some(dir)) if !name.starts_with('/') => Some(format!("{dir}/{name}")),
        (name, _) => name,
    };
    let file_dir = file.map(|file| {
        let file = normalized(&file);
        match file.rsplit_once('/') {
            Some((dir, _)) => dir.to_string(),
            None => file,
        }
    });
    let root = match (built_in.map(|dir| normalized(&dir)), file_dir) {
        (Some(built), Some(file_dir)) => common_directory(&built, &file_dir),
        (Some(dir), None) | (None, Some(dir)) => dir,
        (None, None) => return None,
    };
    // A root of everything says nothing.
    (root.starts_with('/') && !root.trim_end_matches('/').is_empty()).then_some(root)
}

/// The deepest directory both `a` and `b` are in, or are.
fn common_directory(a: &str, b: &str) -> String {
    let common: Vec<&str> = a
        .split('/')
        .zip(b.split('/'))
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a)
        .collect();
    common.join("/")
}

/// How many of a function's locals a probe writes out.
pub const MOST_SPILLED_LOCALS: u32 = 2048;

/// What a line table says of one address.
struct Row {
    address: u64,
    file: String,
    line: u32,
    column: u32,
}

/// The first row of each run of one line, by address, of the files `own` counts as the
/// guest's own.
fn own_rows(module: &[u8], own: &OwnLines) -> Result<Vec<Row>, ProbeError> {
    let mut sections: HashMap<&str, &[u8]> = HashMap::new();
    for payload in Parser::new(0).parse_all(module) {
        if let Payload::CustomSection(section) = payload?
            && section.name().starts_with(".debug_")
        {
            sections.insert(section.name(), section.data());
        }
    }
    let load = |id: SectionId| -> Result<_, gimli::Error> {
        let data = sections.get(id.name()).copied().unwrap_or(&[]);
        Ok(EndianSlice::new(data, LittleEndian))
    };
    let dwarf = Dwarf::load(load)?;
    let root = anchor_root(module, &dwarf)?;
    let mut rows = Vec::new();
    let mut units = dwarf.units();
    while let Some(header) = units.next()? {
        let unit = dwarf.unit(header)?;
        let Some(program) = unit.line_program.clone() else {
            continue;
        };
        let language = language_of(&unit)?;
        // Whether each file counts, decided once.
        let mut counts: HashMap<String, bool> = HashMap::new();
        let mut lines = program.rows();
        let mut files: HashMap<u64, String> = HashMap::new();
        let mut last: Option<(u64, u64)> = None;
        // A sequence at address 0 is what the linker threw away.
        let mut discarded = false;
        while let Some((header, row)) = lines.next_row()? {
            if row.end_sequence() {
                last = None;
                discarded = false;
                continue;
            }
            if last.is_none() && row.address() == 0 {
                discarded = true;
            }
            if discarded || !row.is_stmt() {
                continue;
            }
            let Some(line) = row.line() else { continue };
            let key = (row.file_index(), line.get());
            if last == Some(key) {
                continue;
            }
            last = Some(key);
            let file = if let Some(file) = files.get(&row.file_index()) {
                file.clone()
            } else {
                let mut path = String::new();
                if let Some(entry) = row.file(header) {
                    if let Some(directory) = entry.directory(header)
                        && let Ok(directory) = dwarf.attr_string(&unit, directory)
                    {
                        path.push_str(&directory.to_string_lossy());
                        path.push('/');
                    }
                    if let Ok(name) = dwarf.attr_string(&unit, entry.path_name()) {
                        path.push_str(&name.to_string_lossy());
                    }
                    // A path in the crate being built is relative to where it was built.
                    if !path.starts_with('/')
                        && let Some(directory) = unit.comp_dir
                    {
                        path = format!("{}/{path}", directory.to_string_lossy());
                    }
                }
                let path = normalized(&path);
                files.insert(row.file_index(), path.clone());
                path
            };
            let counted = *counts.entry(file.clone()).or_insert_with(|| {
                own.covers(&SourceFile {
                    path: &file,
                    language,
                    root: root.as_deref(),
                })
            });
            if !counted {
                continue;
            }
            let column = match row.column() {
                ColumnType::LeftEdge => 0,
                ColumnType::Column(column) => u32::try_from(column.get()).unwrap_or(0),
            };
            rows.push(Row {
                address: row.address(),
                file,
                line: u32::try_from(line.get()).unwrap_or(u32::MAX),
                column,
            });
        }
    }
    rows.sort_by_key(|row| row.address);
    rows.dedup_by_key(|row| row.address);
    Ok(rows)
}

/// What the rewrite needs to know of a module before it starts.
#[derive(Default)]
struct Shape {
    code_start: Option<u64>,
    types: u32,
    imported_functions: u32,
    imported_memories: u32,
    defined_memories: u32,
    /// By type, a function's parameters.
    params: Vec<Vec<wasmparser::ValType>>,
    /// By defined function, its type.
    function_types: Vec<u32>,
    /// By global, its type.
    globals: Vec<wasmparser::ValType>,
    /// The most locals of any function, parameters included.
    most_locals: u32,
    /// How many function bodies have been looked at.
    seen_bodies: usize,
    has_types: bool,
    has_imports: bool,
    has_memories: bool,
    has_exports: bool,
}

fn shape_of(module: &[u8]) -> Result<Shape, ProbeError> {
    let mut shape = Shape::default();
    for payload in Parser::new(0).parse_all(module) {
        match payload? {
            Payload::TypeSection(section) => {
                shape.has_types = true;
                for group in section {
                    for ty in group?.types() {
                        shape.types += 1;
                        shape.params.push(match &ty.composite_type.inner {
                            CompositeInnerType::Func(function) => function.params().to_vec(),
                            _ => Vec::new(),
                        });
                    }
                }
            }
            Payload::ImportSection(section) => {
                shape.has_imports = true;
                for import in section.into_imports() {
                    match import?.ty {
                        TypeRef::Func(_) | TypeRef::FuncExact(_) => shape.imported_functions += 1,
                        TypeRef::Memory(_) => shape.imported_memories += 1,
                        TypeRef::Global(global) => shape.globals.push(global.content_type),
                        _ => {}
                    }
                }
            }
            Payload::MemorySection(section) => {
                shape.has_memories = true;
                shape.defined_memories = section.count();
            }
            Payload::ExportSection(_) => shape.has_exports = true,
            Payload::FunctionSection(section) => {
                for ty in section {
                    shape.function_types.push(ty?);
                }
            }
            Payload::GlobalSection(section) => {
                for global in section {
                    shape.globals.push(global?.ty.content_type);
                }
            }
            Payload::CodeSectionEntry(body) => {
                let at = shape.seen_bodies;
                shape.seen_bodies += 1;
                let params = shape
                    .function_types
                    .get(at)
                    .and_then(|ty| shape.params.get(*ty as usize))
                    .map_or(0, Vec::len);
                let mut count = u64::try_from(params).unwrap_or(u64::MAX);
                for locals in body.get_locals_reader()? {
                    count += u64::from(locals?.0);
                }
                let count = u32::try_from(count).unwrap_or(u32::MAX);
                shape.most_locals = shape.most_locals.max(count.min(MOST_SPILLED_LOCALS));
            }
            Payload::CodeSectionStart {
                unchecked_range, ..
            } => shape.code_start = Some(unchecked_range.start),
            _ => {}
        }
    }
    Ok(shape)
}

/// The order sections come in.
fn section_rank(id: wasm_encoder::SectionId) -> u8 {
    use wasm_encoder::SectionId as S;
    match id {
        S::Custom => 0,
        S::Type => 1,
        S::Import => 2,
        S::Function => 3,
        S::Table => 4,
        S::Memory => 5,
        S::Tag => 6,
        S::Global => 7,
        S::Export => 8,
        S::Start => 9,
        S::Element => 10,
        S::DataCount => 11,
        S::Code => 12,
        S::Data => 13,
    }
}

struct Rewriter<'a> {
    shape: Shape,
    /// What to write out. With no debug info to say, as when testing, nothing.
    read_from: ReadFrom,
    /// Offsets in the original module to put a probe at, sorted.
    sites: &'a [u32],
    /// The next of `sites` to place.
    next_site: usize,
    /// The next defined function, by its index in the original module.
    next_function: u32,
    /// By site, the function it went in.
    functions: Vec<u32>,
    added_types: bool,
    added_imports: bool,
    added_memories: bool,
    added_exports: bool,
}

type Rewritten<T = ()> = Result<T, reencode::Error<Infallible>>;

impl Rewriter<'_> {
    /// The new import comes last of the imported functions: what comes after moves up.
    fn probe_index(&self) -> u32 {
        self.shape.imported_functions
    }

    fn flags_index(&self) -> u32 {
        self.shape.imported_memories + self.shape.defined_memories
    }

    /// The locals of a function a probe writes out.
    fn locals_of(
        &self,
        function: u32,
        body: &FunctionBody<'_>,
    ) -> Rewritten<Vec<(u32, wasmparser::ValType)>> {
        let defined = function - self.shape.imported_functions;
        let mut locals: Vec<wasmparser::ValType> = self
            .shape
            .function_types
            .get(defined as usize)
            .and_then(|ty| self.shape.params.get(*ty as usize))
            .cloned()
            .unwrap_or_default();
        for declared in body.get_locals_reader()? {
            let (count, ty) = declared?;
            let room = (MOST_SPILLED_LOCALS as usize).saturating_sub(locals.len());
            locals.extend(std::iter::repeat_n(ty, room.min(count as usize)));
        }
        locals.truncate(MOST_SPILLED_LOCALS as usize);
        let read = self.read_from.locals_in(body.range());
        Ok(read
            .into_iter()
            .filter_map(|index| locals.get(index as usize).map(|ty| (index, *ty)))
            .collect())
    }

    fn add_probe_type(&self, types: &mut TypeSection) {
        types.ty().function([ValType::I32], []);
    }

    fn add_probe_import(&self, imports: &mut ImportSection) {
        imports.import(
            IMPORT_MODULE,
            IMPORT_PROBE,
            EntityType::Function(self.shape.types),
        );
    }

    /// Where a firing probe writes out what it can see: on a boundary, after the flags.
    fn spill(&self) -> Spill {
        let flags = u32::try_from(self.sites.len()).unwrap_or(u32::MAX);
        Spill {
            base: flags.div_ceil(8).saturating_mul(8),
            globals: u32::try_from(self.shape.globals.len()).unwrap_or(u32::MAX),
        }
    }

    fn add_flags(&self, memories: &mut MemorySection) {
        let spill = self.spill();
        let slots = u64::from(spill.globals) + u64::from(self.shape.most_locals);
        let pages = (u64::from(spill.base) + slots * 8).div_ceil(65536).max(1);
        memories.memory(MemoryType {
            minimum: pages,
            maximum: Some(pages),
            memory64: false,
            shared: true,
            page_size_log2: None,
        });
    }

    fn add_flags_export(&self, exports: &mut ExportSection) {
        exports.export(EXPORT_FLAGS, ExportKind::Memory, self.flags_index());
    }

    /// Stores a value in a slot of the spill, if it is a number. Leaves the stack as it was.
    fn emit_store(
        &self,
        function: &mut Function,
        slot: u64,
        ty: wasmparser::ValType,
        value: Instruction<'_>,
    ) {
        use wasmparser::ValType as V;
        let argument = MemArg {
            offset: u64::from(self.spill().base) + slot * 8,
            align: 0,
            target: MemArgTarget::Memory(self.flags_index()),
            ordering: Default::default(),
        };
        let store = match ty {
            V::I32 => Instruction::I32Store(argument),
            V::I64 => Instruction::I64Store(argument),
            V::F32 => Instruction::F32Store(argument),
            V::F64 => Instruction::F64Store(argument),
            _ => return,
        };
        function.instruction(&Instruction::I32Const(0));
        function.instruction(&value);
        function.instruction(&store);
    }

    /// `if (flags[id]) { write out locals and globals; probe(id) }`, which leaves the stack as
    /// it was.
    fn emit_probe(&self, function: &mut Function, id: u32, locals: &[(u32, wasmparser::ValType)]) {
        let id = i32::try_from(id).unwrap_or(i32::MAX);
        // Atomic, or an engine could read it once and hoist that out of a loop.
        function.instruction(&Instruction::I32Const(id));
        function.instruction(&Instruction::I32AtomicLoad8U(MemArg {
            offset: 0,
            align: 0,
            target: MemArgTarget::Memory(self.flags_index()),
            ordering: Default::default(),
        }));
        function.instruction(&Instruction::If(BlockType::Empty));
        let globals = self.shape.globals.len();
        for index in &self.read_from.globals {
            if let Some(ty) = self.shape.globals.get(*index as usize) {
                self.emit_store(
                    function,
                    u64::from(*index),
                    *ty,
                    Instruction::GlobalGet(*index),
                );
            }
        }
        for (index, ty) in locals {
            let slot = u64::try_from(globals).unwrap_or(u64::MAX) + u64::from(*index);
            self.emit_store(function, slot, *ty, Instruction::LocalGet(*index));
        }
        function.instruction(&Instruction::I32Const(id));
        function.instruction(&Instruction::Call(self.probe_index()));
        function.instruction(&Instruction::End);
    }
}

impl Reencode for Rewriter<'_> {
    type Error = Infallible;

    fn function_index(&mut self, function: u32) -> Rewritten<u32> {
        Ok(if function >= self.shape.imported_functions {
            function + 1
        } else {
            function
        })
    }

    fn parse_type_section(
        &mut self,
        types: &mut TypeSection,
        section: wasmparser::TypeSectionReader<'_>,
    ) -> Rewritten {
        reencode::utils::parse_type_section(self, types, section)?;
        self.add_probe_type(types);
        self.added_types = true;
        Ok(())
    }

    fn parse_import_section(
        &mut self,
        imports: &mut ImportSection,
        section: wasmparser::ImportSectionReader<'_>,
    ) -> Rewritten {
        reencode::utils::parse_import_section(self, imports, section)?;
        self.add_probe_import(imports);
        self.added_imports = true;
        Ok(())
    }

    fn parse_memory_section(
        &mut self,
        memories: &mut MemorySection,
        section: wasmparser::MemorySectionReader<'_>,
    ) -> Rewritten {
        reencode::utils::parse_memory_section(self, memories, section)?;
        self.add_flags(memories);
        self.added_memories = true;
        Ok(())
    }

    fn parse_export_section(
        &mut self,
        exports: &mut ExportSection,
        section: wasmparser::ExportSectionReader<'_>,
    ) -> Rewritten {
        reencode::utils::parse_export_section(self, exports, section)?;
        self.add_flags_export(exports);
        self.added_exports = true;
        Ok(())
    }

    /// Makes the sections a module did not have, in their places.
    fn intersperse_section_hook(
        &mut self,
        module: &mut wasm_encoder::Module,
        _after: Option<wasm_encoder::SectionId>,
        before: Option<wasm_encoder::SectionId>,
    ) -> Rewritten {
        let next = before.map_or(u8::MAX, section_rank);
        if !self.shape.has_types && !self.added_types && next > 1 {
            let mut types = TypeSection::new();
            self.add_probe_type(&mut types);
            module.section(&types);
            self.added_types = true;
        }
        if !self.shape.has_imports && !self.added_imports && next > 2 {
            let mut imports = ImportSection::new();
            self.add_probe_import(&mut imports);
            module.section(&imports);
            self.added_imports = true;
        }
        if !self.shape.has_memories && !self.added_memories && next > 5 {
            let mut memories = MemorySection::new();
            self.add_flags(&mut memories);
            module.section(&memories);
            self.added_memories = true;
        }
        if !self.shape.has_exports && !self.added_exports && next > 8 {
            let mut exports = ExportSection::new();
            self.add_flags_export(&mut exports);
            module.section(&exports);
            self.added_exports = true;
        }
        Ok(())
    }

    fn parse_function_body(&mut self, code: &mut CodeSection, body: FunctionBody<'_>) -> Rewritten {
        let this_function = self.next_function;
        self.next_function += 1;
        let mut function = self.new_function_with_parsed_locals(&body)?;
        let locals = self.locals_of(this_function, &body)?;
        let mut reader = body.get_operators_reader()?;
        while !reader.eof() {
            let at = u32::try_from(reader.original_position()).unwrap_or(u32::MAX);
            let operator = reader.read()?;
            // All that comes up to this operator: a line may start among the locals.
            while let Some(site) = self.sites.get(self.next_site)
                && *site <= at
            {
                self.emit_probe(
                    &mut function,
                    u32::try_from(self.next_site).unwrap_or(u32::MAX),
                    &locals,
                );
                self.functions.push(this_function);
                self.next_site += 1;
            }
            function.instruction(&self.instruction(operator)?);
        }
        code.function(&function);
        Ok(())
    }

    /// Names as a person reads them, for a stack trace to show: with no module name to
    /// come before each, and Rust's demangled.
    fn parse_custom_name_subsection(
        &mut self,
        names: &mut wasm_encoder::NameSection,
        section: wasmparser::Name<'_>,
    ) -> Rewritten {
        match section {
            wasmparser::Name::Module { .. } => Ok(()),
            wasmparser::Name::Function(map) => {
                let mut functions = wasm_encoder::NameMap::new();
                for naming in map {
                    let naming = naming?;
                    let index = self.function_index(naming.index)?;
                    let readable = format!("{:#}", rustc_demangle::demangle(naming.name));
                    functions.append(index, &readable);
                }
                names.functions(&functions);
                Ok(())
            }
            other => reencode::utils::parse_custom_name_subsection(self, names, other),
        }
    }

    fn parse_custom_section(
        &mut self,
        module: &mut wasm_encoder::Module,
        section: wasmparser::CustomSectionReader<'_>,
    ) -> Rewritten {
        // It describes the original. Names come through, for stack traces.
        if section.name().starts_with(".debug_") {
            return Ok(());
        }
        reencode::utils::parse_custom_section(self, module, section)
    }
}

/// Puts a probe before the operators at `sites`, offsets in `module`, sorted. Returns the new
/// module, and by site the function it went in.
fn rewrite(
    module: &[u8],
    shape: Shape,
    read_from: ReadFrom,
    sites: &[u32],
) -> Result<(Vec<u8>, Vec<u32>, Spill), ProbeError> {
    let first_function = shape.imported_functions;
    let mut rewriter = Rewriter {
        shape,
        read_from,
        sites,
        next_site: 0,
        next_function: first_function,
        functions: Vec::with_capacity(sites.len()),
        added_types: false,
        added_imports: false,
        added_memories: false,
        added_exports: false,
    };
    let mut out = wasm_encoder::Module::new();
    rewriter
        .parse_core_module(&mut out, Parser::new(0), module)
        .map_err(|error| ProbeError::Rewrite(format!("{error:?}")))?;
    if rewriter.next_site != sites.len() {
        return Err(ProbeError::Unplaced {
            unplaced: sites.len() - rewriter.next_site,
            sites: sites.len(),
        });
    }
    let spill = rewriter.spill();
    Ok((out.finish(), rewriter.functions, spill))
}

/// Puts a probe on each of `lines` in `module`. `None` if it has none: it has no debug info,
/// or only of code that is not its own, and can run as it is.
pub fn instrument(module: &[u8], lines: &OwnLines) -> Result<Option<Instrumented>, ProbeError> {
    let shape = shape_of(module)?;
    let Some(code_start) = shape.code_start else {
        return Ok(None);
    };
    let rows = own_rows(module, lines)?;
    if rows.is_empty() {
        return Ok(None);
    }
    let offset = |row: &Row| u32::try_from(row.address + code_start).unwrap_or(u32::MAX);
    let sites: Vec<u32> = rows.iter().map(offset).collect();
    let read_from = match ModuleDebug::new(module)? {
        Some(debug) => debug.read_from()?,
        None => ReadFrom::default(),
    };
    let imported = shape.imported_functions;
    let (rewritten, functions, spill) = rewrite(module, shape, read_from, &sites)?;
    let with_probes: HashSet<u32> = functions.iter().copied().collect();
    let calls = call_sites(module, &rewritten, imported, &with_probes)?;
    let module = rewritten;
    let probes = rows
        .into_iter()
        .zip(functions)
        .zip(sites)
        .map(|((row, function), site)| Probe {
            offset: ModuleOffset(site),
            function,
            file: row.file,
            line: row.line,
            column: row.column,
        })
        .collect();
    Ok(Some(Instrumented {
        module,
        probes,
        calls,
        spill,
    }))
}

/// The offsets of the calls in each function of `module`, but for those of `leaving_out`.
fn calls_by_function(module: &[u8], leaving_out: Option<u32>) -> Result<Vec<Vec<u32>>, ProbeError> {
    let mut calls = Vec::new();
    for payload in Parser::new(0).parse_all(module) {
        if let Payload::CodeSectionEntry(body) = payload? {
            let mut here = Vec::new();
            let mut reader = body.get_operators_reader()?;
            while !reader.eof() {
                let at = u32::try_from(reader.original_position()).unwrap_or(u32::MAX);
                match reader.read()? {
                    Operator::Call { function_index } if Some(function_index) == leaving_out => {}
                    Operator::Call { .. }
                    | Operator::CallIndirect { .. }
                    | Operator::CallRef { .. } => here.push(at),
                    _ => {}
                }
            }
            calls.push(here);
        }
    }
    Ok(calls)
}

/// Pairs up the calls of a module and of what [`rewrite`] made of it, whose only new ones call
/// the probe.
fn call_sites(
    original: &[u8],
    rewritten: &[u8],
    imported: u32,
    with_probes: &HashSet<u32>,
) -> Result<Vec<CallSite>, ProbeError> {
    let before = calls_by_function(original, None)?;
    let after = calls_by_function(rewritten, Some(imported))?;
    let mut sites = Vec::new();
    for (at, (before, after)) in before.iter().zip(&after).enumerate() {
        let function = imported + u32::try_from(at).unwrap_or(u32::MAX);
        if with_probes.contains(&function) {
            for (old, new) in before.iter().zip(after) {
                sites.push(CallSite {
                    offset: ModuleOffset(*new),
                    original: ModuleOffset(*old),
                    function,
                });
            }
        }
    }
    Ok(sites)
}

/// `module`'s source lines, with no change to it, for a host that can set breakpoints itself:
/// each [`Probe`] says where it stands, moved up to the operator a breakpoint can be set on.
pub fn source_lines(module: &[u8], lines: &OwnLines) -> Result<Vec<Probe>, ProbeError> {
    let shape = shape_of(module)?;
    let Some(code_start) = shape.code_start else {
        return Ok(Vec::new());
    };
    let mut rows = own_rows(module, lines)?.into_iter().peekable();
    let mut function = shape.imported_functions;
    let mut probes = Vec::new();
    for payload in Parser::new(0).parse_all(module) {
        if let Payload::CodeSectionEntry(body) = payload? {
            let mut reader = body.get_operators_reader()?;
            while !reader.eof() {
                let at = reader.original_position();
                reader.read()?;
                while let Some(row) = rows.next_if(|row| row.address + code_start <= at) {
                    probes.push(Probe {
                        offset: ModuleOffset(u32::try_from(at).unwrap_or(u32::MAX)),
                        function,
                        file: row.file,
                        line: row.line,
                        column: row.column,
                    });
                }
            }
            function += 1;
        }
    }
    Ok(probes)
}

/// How many functions `module` imports: a defined function's index comes after those.
pub fn imported_functions(module: &[u8]) -> Result<u32, ProbeError> {
    Ok(shape_of(module)?.imported_functions)
}

/// By a defined function's index among those a module defines, its name as a person reads it.
pub fn defined_function_names(module: &[u8]) -> Result<HashMap<u32, String>, ProbeError> {
    let shape = shape_of(module)?;
    let mut names = HashMap::new();
    for payload in Parser::new(0).parse_all(module) {
        if let Payload::CustomSection(section) = payload?
            && let KnownCustom::Name(subsections) = section.as_known()
        {
            for subsection in subsections {
                if let Name::Function(map) = subsection? {
                    for naming in map {
                        let naming = naming?;
                        if let Some(index) = naming.index.checked_sub(shape.imported_functions) {
                            let readable = format!("{:#}", rustc_demangle::demangle(naming.name));
                            names.insert(index, readable);
                        }
                    }
                }
            }
        }
    }
    Ok(names)
}

/// How a step is to end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// At another line, in a call this one makes if it makes one.
    Into,
    /// At another line of this function, or of its caller if it returns.
    Over,
    /// Once this function has returned to one with a line to stop on.
    Out,
}

/// A module's lines, to go between them, their code and what a step should do.
#[derive(Clone, Debug, Default)]
pub struct LineIndex {
    probes: Vec<Probe>,
    by_offset: HashMap<u32, ProbeId>,
    /// By file and line, where stopping on it stops: the first place in each function.
    sites: BTreeMap<String, BTreeMap<u32, Vec<ProbeId>>>,
    /// The functions all on one line: what a macro generated, usually.
    one_line: HashSet<u32>,
    /// The probe each function starts at: before it has stored its parameters.
    opening: HashSet<ProbeId>,
}

impl LineIndex {
    /// Indexes `probes`, each at its [`ProbeId`].
    #[must_use]
    pub fn new(probes: Vec<Probe>) -> Self {
        let mut index = Self::default();
        let mut line_of: HashMap<u32, Option<u32>> = HashMap::new();
        let mut opened = HashSet::new();
        for (at, probe) in probes.iter().enumerate() {
            let id = ProbeId(u32::try_from(at).unwrap_or(u32::MAX));
            index.by_offset.entry(probe.offset.0).or_insert(id);
            index
                .sites
                .entry(probe.file.clone())
                .or_default()
                .entry(probe.line)
                .or_default()
                .push(id);
            if opened.insert(probe.function) {
                index.opening.insert(id);
            }
            line_of
                .entry(probe.function)
                .and_modify(|line| {
                    if *line != Some(probe.line) {
                        *line = None;
                    }
                })
                .or_insert(Some(probe.line));
        }
        index.one_line = line_of
            .into_iter()
            .filter_map(|(function, line)| line.map(|_| function))
            .collect();
        index.probes = probes;
        index
    }

    /// Whether there are no lines at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.probes.is_empty()
    }

    /// The source files with lines.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        self.sites.keys().map(String::as_str)
    }

    /// The lines of `file` with code, in order.
    pub fn lines(&self, file: &str) -> impl Iterator<Item = u32> {
        self.sites
            .get(file)
            .into_iter()
            .flat_map(BTreeMap::keys)
            .copied()
    }

    /// Where stopping on a line of a file stops: all of it, as which way the code gets to a
    /// line says nothing of where it starts. An async function is a state machine, with a line
    /// in several places, and the first by address may be taken once or never.
    /// [`LeavingLine`] keeps that to one stop each time the line runs.
    #[must_use]
    pub fn sites(&self, file: &str, line: u32) -> &[ProbeId] {
        self.sites
            .get(file)
            .and_then(|lines| lines.get(&line))
            .map_or(&[], Vec::as_slice)
    }

    /// What to watch for once a guest stopped at `stopped` goes on, if that line would stop it
    /// again before it has left: when `armed` has more than one site on the line.
    #[must_use]
    pub fn leaving_line(
        &self,
        stopped: ProbeId,
        depth: usize,
        armed: impl Fn(ProbeId) -> bool,
    ) -> Option<LeavingLine> {
        let probe = self.probe(stopped)?;
        let sites = self.sites(&probe.file, probe.line);
        (sites.len() > 1 && sites.iter().any(|site| armed(*site))).then(|| LeavingLine {
            from: stopped,
            seen: HashSet::from([stopped]),
            depth,
        })
    }

    /// Whether a line being left is still armed.
    #[must_use]
    pub fn still_armed(&self, leaving: &LeavingLine, armed: impl Fn(ProbeId) -> bool) -> bool {
        self.probe(leaving.from).is_some_and(|probe| {
            self.sites(&probe.file, probe.line)
                .iter()
                .any(|site| armed(*site))
        })
    }

    /// What reaching `here` says of the line being left.
    pub fn passing(&self, leaving: &mut LeavingLine, here: ProbeId, depth: usize) -> Passing {
        let (Some(from), Some(probe)) = (self.probe(leaving.from), self.probe(here)) else {
            return Passing::Left;
        };
        if probe.file == from.file && probe.line == from.line {
            // Back at a place it has been: this is the line running again.
            return if leaving.seen.insert(here) {
                Passing::OnTheLine
            } else {
                Passing::Left
            };
        }
        if probe.function != from.function && depth > leaving.depth {
            return Passing::InACall;
        }
        Passing::Left
    }

    /// The line a module offset starts, if one does.
    #[must_use]
    pub fn at(&self, offset: u32) -> Option<ProbeId> {
        self.by_offset.get(&offset).copied()
    }

    /// The line `offset` is on, in `function`: the last to start at or before it there.
    #[must_use]
    pub fn line_at(&self, function: u32, offset: u32) -> Option<&Probe> {
        let after = self
            .probes
            .partition_point(|probe| probe.offset.0 <= offset);
        self.probes
            .get(after.checked_sub(1)?)
            .filter(|probe| probe.function == function)
    }

    /// A line, by its id.
    #[must_use]
    pub fn probe(&self, id: ProbeId) -> Option<&Probe> {
        self.probes.get(id.0 as usize)
    }

    /// Whether a step from `from`, at a stack depth of `from_depth`, ends at `here`, at `depth`.
    /// It does not end in what has no lines to step through, nor on a function's opening line,
    /// where its parameters are not yet what they will be.
    #[must_use]
    pub fn step_ends(
        &self,
        how: StepKind,
        from: ProbeId,
        from_depth: usize,
        here: ProbeId,
        depth: usize,
    ) -> bool {
        let (Some(from), Some(probe)) = (self.probe(from), self.probe(here)) else {
            return false;
        };
        if self.one_line.contains(&probe.function) || self.opening.contains(&here) {
            return false;
        }
        let other_line = probe.file != from.file || probe.line != from.line;
        match how {
            StepKind::Into => depth > from_depth || other_line,
            StepKind::Over => depth < from_depth || (depth == from_depth && other_line),
            StepKind::Out => depth < from_depth,
        }
    }
}

/// A guest going on from a stop, until it has left the line: a line can be many sites, and
/// passing through them is one time running it, so one stop.
#[derive(Clone, Debug)]
pub struct LeavingLine {
    from: ProbeId,
    seen: HashSet<ProbeId>,
    depth: usize,
}

/// What reaching a probe says of a line being left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Passing {
    /// Still on it, somewhere else: a breakpoint here is the one it stopped for.
    OnTheLine,
    /// In a call the line makes, which it comes back from. Breakpoints there stop.
    InACall,
    /// It has left, or is running the line again.
    Left,
}

/// The core modules inside a component, nested ones too, as their bytes.
pub fn core_modules(component: &[u8]) -> Result<Vec<&[u8]>, ProbeError> {
    let mut modules = Vec::new();
    for payload in Parser::new(0).parse_all(component) {
        if let Payload::ModuleSection {
            unchecked_range, ..
        } = payload?
            && let Ok(start) = usize::try_from(unchecked_range.start)
            && let Ok(end) = usize::try_from(unchecked_range.end)
            && let Some(module) = component.get(start..end)
        {
            modules.push(module);
        }
    }
    Ok(modules)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use wasmparser::Validator;

    fn validate(module: &[u8]) {
        Validator::new()
            .validate_all(module)
            .expect("a valid module");
    }

    /// The core module of `line-walk`, which has debug info.
    fn line_walk() -> Vec<u8> {
        let modules = core_modules(test_components::LINE_WALK.wasm).unwrap();
        let module = modules.iter().max_by_key(|module| module.len()).unwrap();
        module.to_vec()
    }

    /// The line of the `// mark: {name}` comment in `line-walk`'s source.
    fn marked(name: &str) -> u32 {
        let source =
            include_str!("../../../test-fixtures/test-components/guests/line-walk/src/lib.rs");
        let wanted = format!("// mark: {name}");
        let at = source
            .lines()
            .position(|line| line.trim_end().ends_with(&wanted))
            .unwrap_or_else(|| panic!("no mark `{name}`"));
        u32::try_from(at).unwrap() + 1
    }

    fn exports(module: &[u8]) -> Vec<(String, wasmparser::ExternalKind, u32)> {
        let mut found = Vec::new();
        for payload in Parser::new(0).parse_all(module) {
            if let Payload::ExportSection(section) = payload.unwrap() {
                for export in section {
                    let export = export.unwrap();
                    found.push((export.name.to_string(), export.kind, export.index));
                }
            }
        }
        found
    }

    /// `line-walk`'s `lib.rs`, of the two files of its own: `scale.rs` is the other.
    fn lib_rs(index: &LineIndex) -> &str {
        let files: Vec<&str> = index.files().collect();
        assert_eq!(files.len(), 2, "{files:?}");
        files
            .into_iter()
            .find(|file| file.ends_with("guests/line-walk/src/lib.rs"))
            .expect("its `lib.rs`")
    }

    #[test]
    fn probes_stand_on_the_guests_own_lines() {
        let instrumented = instrument(&line_walk(), &OwnLines::default())
            .unwrap()
            .unwrap();
        validate(&instrumented.module);
        assert!(
            instrumented
                .probes
                .iter()
                .all(|probe| probe.file.ends_with("guests/line-walk/src/lib.rs")
                    || probe.file.ends_with("guests/line-walk/src/scale.rs")),
            "{:?}",
            instrumented.probes.first(),
        );
        for mark in [
            "add-body",
            "add-end",
            "walk-first",
            "walk-second",
            "walk-end",
            "run-walk",
        ] {
            let line = marked(mark);
            assert!(
                instrumented.probes.iter().any(|probe| probe.line == line),
                "no probe on `{mark}`, line {line}",
            );
        }
        assert!(
            !instrumented
                .probes
                .iter()
                .any(|probe| probe.line == marked("above-walk")),
            "a comment has no code",
        );
    }

    /// The root a module's `run` anchors its own lines to.
    fn root_of(module: &[u8]) -> Option<String> {
        let mut sections: HashMap<&str, &[u8]> = HashMap::new();
        for payload in Parser::new(0).parse_all(module) {
            if let Payload::CustomSection(section) = payload.unwrap()
                && section.name().starts_with(".debug_")
            {
                sections.insert(section.name(), section.data());
            }
        }
        let load = |id: SectionId| -> Result<_, gimli::Error> {
            let data = sections.get(id.name()).copied().unwrap_or(&[]);
            Ok(EndianSlice::new(data, LittleEndian))
        };
        anchor_root(module, &Dwarf::load(load).unwrap()).unwrap()
    }

    #[test]
    fn run_anchors_a_guest_to_its_project_in_rust_and_c() {
        let guests = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../test-fixtures/test-components/guests"
        );
        let guests = normalized(guests);
        // Cargo compiles a workspace's crates in its directory.
        assert_eq!(root_of(&line_walk()).as_deref(), Some(guests.as_str()));
        // The bindings, with no debug info, call the guest's `node_run`.
        let modules = core_modules(test_components::C_WALK.wasm).unwrap();
        let c_walk = modules.iter().max_by_key(|module| module.len()).unwrap();
        assert_eq!(root_of(c_walk), Some(format!("{guests}/c-walk")));
        // No `run`, no anchor.
        let plain = wat::parse_str("(module (func (export \"other\")))").unwrap();
        assert_eq!(root_of(&plain), None);
    }

    #[test]
    fn paths_are_worked_out_as_their_text_says() {
        assert_eq!(normalized("/a/./b/../c//d.c"), "/a/c/d.c");
        assert_eq!(normalized("a/../../b"), "../b");
        assert_eq!(common_directory("/p/build", "/p/src"), "/p");
        assert_eq!(common_directory("/p/x", "/p/x"), "/p/x");
        assert_eq!(common_directory("/p/xy", "/p/x"), "/p");
    }

    #[test]
    fn all_lines_reach_into_what_is_not_the_guests() {
        let module = line_walk();
        let own = instrument(&module, &OwnLines::default()).unwrap().unwrap();
        let all = instrument(&module, &OwnLines::All).unwrap().unwrap();
        validate(&all.module);
        assert!(all.probes.len() > own.probes.len() * 2);
        assert!(
            all.probes
                .iter()
                .any(|probe| probe.file.contains("/rustc/"))
        );
    }

    #[test]
    fn the_rewrite_keeps_memories_and_names_where_they_mean_the_same() {
        let module = line_walk();
        let instrumented = instrument(&module, &OwnLines::default()).unwrap().unwrap();
        let shape = shape_of(&module).unwrap();

        let found = exports(&instrumented.module);
        let flags = found
            .iter()
            .find(|(name, ..)| name == EXPORT_FLAGS)
            .unwrap();
        assert_eq!(flags.1, wasmparser::ExternalKind::Memory);
        assert_eq!(flags.2, shape.imported_memories + shape.defined_memories);
        let memory = found.iter().find(|(name, ..)| name == "memory").unwrap();
        assert_eq!(memory.2, 0, "the guest's memory stays the first");

        // The function a probe says it is in is the one its name says.
        let walk = instrumented
            .probes
            .iter()
            .find(|probe| probe.line == marked("walk-first"))
            .unwrap();
        let mut named = None;
        for payload in Parser::new(0).parse_all(&instrumented.module) {
            if let Payload::CustomSection(section) = payload.unwrap()
                && let KnownCustom::Name(names) = section.as_known()
            {
                for subsection in names {
                    if let Name::Function(map) = subsection.unwrap() {
                        for naming in map {
                            let naming = naming.unwrap();
                            if naming.index == walk.function + 1 {
                                named = Some(naming.name.to_string());
                            }
                        }
                    }
                }
            }
        }
        let named = named.expect("its function has a name");
        assert_eq!(
            named, "line_walk_guest::walk",
            "a name as a person reads it"
        );

        for payload in Parser::new(0).parse_all(&instrumented.module) {
            if let Payload::CustomSection(section) = payload.unwrap() {
                assert!(!section.name().starts_with(".debug_"), "{}", section.name());
            }
        }
    }

    #[test]
    fn a_host_can_say_what_counts_as_its_own() {
        let module = line_walk();
        let guessed = instrument(&module, &OwnLines::default()).unwrap().unwrap();
        let file = guessed.probes[0].file.clone();
        let directory = file.strip_suffix("lib.rs").unwrap().to_string();

        let under = OwnLines::under([directory]);
        let mine = instrument(&module, &under).unwrap().unwrap();
        assert_eq!(mine.probes.len(), guessed.probes.len());

        // Not what a Rust guess would pick: only the standard library's.
        let library = OwnLines::under(["/rustc/".to_string()]);
        let all = instrument(&module, &OwnLines::All).unwrap().unwrap();
        let rustc = all
            .probes
            .iter()
            .find(|probe| probe.file.starts_with("/rustc/"));
        let theirs = instrument(&module, &library).unwrap();
        assert_eq!(rustc.is_some(), theirs.is_some());

        let nothing = OwnLines::under(["/nowhere/".to_string()]);
        assert!(instrument(&module, &nothing).unwrap().is_none());
    }

    #[test]
    fn calls_are_paired_up_past_the_probes_and_find_their_lines() {
        let module = line_walk();
        let instrumented = instrument(&module, &OwnLines::default()).unwrap().unwrap();
        let index = LineIndex::new(source_lines(&module, &OwnLines::default()).unwrap());
        let file = lib_rs(&index);
        let first = index
            .probe(index.sites(file, marked("walk-first"))[0])
            .unwrap();
        // `walk` calls `add` from its first line.
        let call = instrumented
            .calls
            .iter()
            .find(|call| {
                call.function == first.function
                    && index
                        .line_at(call.function, call.original.0)
                        .map(|p| p.line)
                        == Some(marked("walk-first"))
            })
            .expect("a call on `walk`'s first line");
        assert_ne!(call.offset, call.original, "the probes moved it");
        let mut read = Parser::new(0).parse_all(&instrumented.module);
        let is_call = read.any(|payload| match payload.unwrap() {
            Payload::CodeSectionEntry(body) => {
                let mut reader = body.get_operators_reader().unwrap();
                let mut found = false;
                while !reader.eof() {
                    let at = u32::try_from(reader.original_position()).unwrap();
                    let operator = reader.read().unwrap();
                    found |= at == call.offset.0 && matches!(operator, Operator::Call { .. });
                }
                found
            }
            _ => false,
        });
        assert!(is_call, "the offset is that of a call in the new module");
        assert_eq!(
            index.line_at(first.function + 1, first.offset.0),
            None,
            "in that function"
        );
    }

    #[test]
    fn a_line_is_all_its_sites_and_passing_through_them_is_one_time_running_it() {
        let probe = |function, line| Probe {
            offset: ModuleOffset(0),
            function,
            file: "f".into(),
            line,
            column: 0,
        };
        // Line 2 is in three places in function 0. Line 3 is one, and line 9 is in a callee.
        let index = LineIndex::new(vec![
            probe(0, 2),
            probe(0, 2),
            probe(0, 3),
            probe(0, 2),
            probe(1, 9),
        ]);
        let id = ProbeId;
        assert_eq!(index.sites("f", 2), [id(0), id(1), id(3)]);
        let armed = |site: ProbeId| index.probe(site).is_some_and(|probe| probe.line == 2);
        let Some(mut leaving) = index.leaving_line(id(0), 1, armed) else {
            panic!("line 2 has more than one site");
        };
        assert!(index.leaving_line(id(2), 1, |_| true).is_none(), "one site");
        assert!(
            index.leaving_line(id(0), 1, |_| false).is_none(),
            "not armed"
        );

        assert_eq!(index.passing(&mut leaving, id(1), 1), Passing::OnTheLine);
        assert_eq!(index.passing(&mut leaving, id(4), 2), Passing::InACall);
        assert_eq!(index.passing(&mut leaving, id(3), 1), Passing::OnTheLine);
        assert_eq!(
            index.passing(&mut leaving, id(1), 1),
            Passing::Left,
            "around again"
        );
        let mut leaving = index.leaving_line(id(0), 1, armed).unwrap();
        assert_eq!(index.passing(&mut leaving, id(2), 1), Passing::Left);
        let mut leaving = index.leaving_line(id(0), 1, armed).unwrap();
        assert_eq!(
            index.passing(&mut leaving, id(4), 1),
            Passing::Left,
            "returned to a caller"
        );
        assert!(index.still_armed(&leaving, armed));
        assert!(!index.still_armed(&leaving, |_| false));
    }

    #[test]
    fn lines_stand_on_operators_and_say_how_a_step_ends() {
        let module = line_walk();
        let index = LineIndex::new(source_lines(&module, &OwnLines::default()).unwrap());
        let file = lib_rs(&index);
        let first = index.sites(file, marked("walk-first"))[0];
        let second = index.sites(file, marked("walk-second"))[0];
        let body = index.sites(file, marked("add-body"))[0];
        assert_eq!(index.at(index.probe(first).unwrap().offset.0), Some(first));

        // At a depth of 3 in `walk`, say.
        assert!(index.step_ends(StepKind::Over, first, 3, second, 3));
        assert!(
            !index.step_ends(StepKind::Over, first, 3, body, 4),
            "over the call"
        );
        assert!(index.step_ends(StepKind::Into, second, 3, body, 4));
        assert!(!index.step_ends(StepKind::Out, body, 4, body, 4));
        assert!(index.step_ends(StepKind::Out, body, 4, second, 3));

        // `fn add`'s opening line is not stopped at, nor is what a macro generated.
        let opening = index.sites(file, marked("add-body") - 1)[0];
        assert!(!index.step_ends(StepKind::Into, second, 3, opening, 4));
        let generated = index.sites(file, 1)[0];
        assert!(!index.step_ends(StepKind::Into, second, 3, generated, 4));
    }

    #[test]
    fn names_are_found_by_a_functions_place_among_those_defined() {
        let module = line_walk();
        let shape = shape_of(&module).unwrap();
        let names = defined_function_names(&module).unwrap();
        let index = LineIndex::new(source_lines(&module, &OwnLines::default()).unwrap());
        let file = lib_rs(&index);
        let first = index
            .probe(index.sites(file, marked("walk-first"))[0])
            .unwrap();
        assert_eq!(
            names
                .get(&(first.function - shape.imported_functions))
                .map(String::as_str),
            Some("line_walk_guest::walk"),
        );
    }

    #[test]
    fn only_what_the_debug_info_reads_is_written_out() {
        let module = line_walk();
        let debug = ModuleDebug::new(&module).unwrap().unwrap();
        let read = debug.read_from().unwrap();
        // Rust keeps each variable in memory, behind a frame pointer, at this level.
        let mut body = None;
        let walk = instrument(&module, &OwnLines::default())
            .unwrap()
            .unwrap()
            .probes
            .into_iter()
            .find(|probe| probe.line == marked("walk-first"))
            .unwrap();
        for payload in Parser::new(0).parse_all(&module) {
            if let Payload::CodeSectionEntry(entry) = payload.unwrap() {
                let range = entry.range();
                if range.contains(&u64::from(walk.offset.0)) {
                    body = Some(range);
                }
            }
        }
        let body = body.unwrap();
        let locals = read.locals_in(body);
        assert_eq!(locals.len(), 1, "only its frame pointer: {locals:?}");
    }

    #[test]
    fn a_module_with_no_debug_info_runs_as_it_is() {
        let module = wat::parse_str("(module (func (export \"f\")))").unwrap();
        assert!(instrument(&module, &OwnLines::All).unwrap().is_none());
        let component = wat::parse_str("(component)").unwrap();
        assert!(core_modules(&component).unwrap().is_empty());
    }

    /// The offset of each of `count` operators in the only function of `module`.
    fn operator_offsets(module: &[u8]) -> Vec<u32> {
        let mut offsets = Vec::new();
        for payload in Parser::new(0).parse_all(module) {
            if let Payload::CodeSectionEntry(body) = payload.unwrap() {
                let mut reader = body.get_operators_reader().unwrap();
                while !reader.eof() {
                    offsets.push(u32::try_from(reader.original_position()).unwrap());
                    reader.read().unwrap();
                }
            }
        }
        offsets
    }

    #[test]
    fn a_module_lacking_sections_gets_them_in_their_places() {
        let module = wat::parse_str(
            "(module
               (func (result i32)
                 i32.const 1
                 i32.const 2
                 i32.add))",
        )
        .unwrap();
        // Between the two constants, with one on the stack, and at the end.
        let operators = operator_offsets(&module);
        let shape = shape_of(&module).unwrap();
        let (rewritten, functions, _) = rewrite(
            &module,
            shape,
            ReadFrom::default(),
            &[operators[1], operators[3]],
        )
        .unwrap();
        validate(&rewritten);
        assert_eq!(functions, [0, 0]);
        let found = exports(&rewritten);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, EXPORT_FLAGS);
        assert_eq!(found[0].2, 0, "the only memory");
    }

    #[test]
    fn indices_of_what_moves_up_are_kept_true() {
        let module = wat::parse_str(
            r#"(module
                 (rec (type $a (func)) (type $b (func (param i32))))
                 (import "m" "f" (func $f (type $a)))
                 (table 1 funcref)
                 (memory (export "memory") 1)
                 (func $g (export "g") (type $a)
                   call $f
                   call $h)
                 (func $h (type $a))
                 (elem (i32.const 0) $h)
                 (start $h))"#,
        )
        .unwrap();
        let operators = operator_offsets(&module);
        let shape = shape_of(&module).unwrap();
        assert_eq!(shape.types, 2, "a recursion group's types each count");
        let (rewritten, ..) =
            rewrite(&module, shape, ReadFrom::default(), &[operators[0]]).unwrap();
        validate(&rewritten);
        let found = exports(&rewritten);
        let g = found.iter().find(|(name, ..)| name == "g").unwrap();
        assert_eq!(g.2, 2, "`g` moved up past the probe's import");
        let mut start = None;
        for payload in Parser::new(0).parse_all(&rewritten) {
            if let Payload::StartSection { func, .. } = payload.unwrap() {
                start = Some(func);
            }
        }
        assert_eq!(start, Some(3));
    }
}
