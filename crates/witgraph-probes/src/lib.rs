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

use std::collections::HashMap;
use std::convert::Infallible;

use gimli::{ColumnType, Dwarf, EndianSlice, LittleEndian, SectionId};
use wasm_encoder::reencode::{self, Reencode};
use wasm_encoder::{
    BlockType, CodeSection, EntityType, ExportKind, ExportSection, Function, ImportSection,
    Instruction, MemArg, MemArgTarget, MemorySection, MemoryType, TypeSection, ValType,
};
use wasmparser::{FunctionBody, Parser, Payload, TypeRef};

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

/// Which source lines to probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lines {
    /// The guest's own: not the standard library's, or a dependency's. Stepping stays out of
    /// those, and a probe in a tight loop of theirs costs nothing.
    Own,
    /// Every line the debug info has.
    All,
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
}

/// What a line table says of one address.
struct Row {
    address: u64,
    file: String,
    line: u32,
    column: u32,
}

/// Paths a guest's own code does not live under.
const NOT_OWN: [&str; 6] = [
    "/rustc/",
    "/rustlib/",
    "/rust/deps/",
    "/.cargo/",
    "/cargo/registry/",
    "/cargo/git/",
];

/// Whether `file` is the guest's own, not the standard library's or a dependency's.
fn is_own(file: &str) -> bool {
    !NOT_OWN.iter().any(|path| file.contains(path))
}

/// The first row of each run of one line, by address.
fn line_rows(module: &[u8]) -> Result<Vec<Row>, ProbeError> {
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
    let mut rows = Vec::new();
    let mut units = dwarf.units();
    while let Some(header) = units.next()? {
        let unit = dwarf.unit(header)?;
        let Some(program) = unit.line_program.clone() else {
            continue;
        };
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
                files.insert(row.file_index(), path.clone());
                path
            };
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
                    let count = group?.types().len();
                    shape.types += u32::try_from(count).unwrap_or(u32::MAX);
                }
            }
            Payload::ImportSection(section) => {
                shape.has_imports = true;
                for import in section.into_imports() {
                    match import?.ty {
                        TypeRef::Func(_) | TypeRef::FuncExact(_) => shape.imported_functions += 1,
                        TypeRef::Memory(_) => shape.imported_memories += 1,
                        _ => {}
                    }
                }
            }
            Payload::MemorySection(section) => {
                shape.has_memories = true;
                shape.defined_memories = section.count();
            }
            Payload::ExportSection(_) => shape.has_exports = true,
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

    fn add_flags(&self, memories: &mut MemorySection) {
        let pages = u64::try_from(self.sites.len())
            .unwrap_or(u64::MAX)
            .div_ceil(65536)
            .max(1);
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

    /// `if (flags[id]) probe(id)`, which leaves the stack as it was.
    fn emit_probe(&self, function: &mut Function, id: u32) {
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
                );
                self.functions.push(this_function);
                self.next_site += 1;
            }
            function.instruction(&self.instruction(operator)?);
        }
        code.function(&function);
        Ok(())
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
fn rewrite(module: &[u8], shape: Shape, sites: &[u32]) -> Result<(Vec<u8>, Vec<u32>), ProbeError> {
    let first_function = shape.imported_functions;
    let mut rewriter = Rewriter {
        shape,
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
    Ok((out.finish(), rewriter.functions))
}

/// Puts a probe on each of `lines` in `module`. `None` if it has none: it has no debug info,
/// or only of code that is not its own, and can run as it is.
pub fn instrument(module: &[u8], lines: Lines) -> Result<Option<Instrumented>, ProbeError> {
    let shape = shape_of(module)?;
    let Some(code_start) = shape.code_start else {
        return Ok(None);
    };
    let mut rows = line_rows(module)?;
    if lines == Lines::Own {
        rows.retain(|row| is_own(&row.file));
    }
    if rows.is_empty() {
        return Ok(None);
    }
    let offset = |row: &Row| u32::try_from(row.address + code_start).unwrap_or(u32::MAX);
    let sites: Vec<u32> = rows.iter().map(offset).collect();
    let (module, functions) = rewrite(module, shape, &sites)?;
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
    Ok(Some(Instrumented { module, probes }))
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
    use wasmparser::{KnownCustom, Name, Validator};

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

    #[test]
    fn probes_stand_on_the_guests_own_lines() {
        let instrumented = instrument(&line_walk(), Lines::Own).unwrap().unwrap();
        validate(&instrumented.module);
        assert!(
            instrumented
                .probes
                .iter()
                .all(|probe| probe.file.ends_with("guests/line-walk/src/lib.rs")),
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

    #[test]
    fn all_lines_reach_into_what_is_not_the_guests() {
        let module = line_walk();
        let own = instrument(&module, Lines::Own).unwrap().unwrap();
        let all = instrument(&module, Lines::All).unwrap().unwrap();
        validate(&all.module);
        assert!(all.probes.len() > own.probes.len() * 2);
        assert!(all.probes.iter().any(|probe| !is_own(&probe.file)));
    }

    #[test]
    fn the_rewrite_keeps_memories_and_names_where_they_mean_the_same() {
        let module = line_walk();
        let instrumented = instrument(&module, Lines::Own).unwrap().unwrap();
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
        assert!(named.contains("walk"), "{named}");

        for payload in Parser::new(0).parse_all(&instrumented.module) {
            if let Payload::CustomSection(section) = payload.unwrap() {
                assert!(!section.name().starts_with(".debug_"), "{}", section.name());
            }
        }
    }

    #[test]
    fn a_module_with_no_debug_info_runs_as_it_is() {
        let module = wat::parse_str("(module (func (export \"f\")))").unwrap();
        assert!(instrument(&module, Lines::All).unwrap().is_none());
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
        let (rewritten, functions) =
            rewrite(&module, shape, &[operators[1], operators[3]]).unwrap();
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
        let (rewritten, _) = rewrite(&module, shape, &[operators[0]]).unwrap();
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
