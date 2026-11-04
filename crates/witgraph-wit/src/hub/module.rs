//! A core module under construction, typed so that it cannot be put together
//! wrong in ways wasm validation would not catch.
//!
//! Core wasm numbers imports before functions, so a function's index only
//! means something once the imports are closed: [`ModuleBuilder`] hands out no
//! function index before that. A [`FuncIdx`] says what a function is for, so
//! an equality function cannot be called where a free function is meant, and a
//! [`Place`] is the only way to touch memory, so every access names its base
//! and offset.

use std::collections::HashMap;
use std::marker::PhantomData;

use wasm_encoder::{
    CodeSection, ConstExpr, EntityType, ExportKind, ExportSection, Function, FunctionSection,
    GlobalSection, GlobalType, ImportSection, Instruction, MemArg, MemorySection, MemoryType,
    Module, TypeSection, ValType,
};

/// What a function is for.
pub(crate) trait FuncKind: Copy {}

macro_rules! func_kinds {
    ($($(#[$doc:meta])* $name:ident),* $(,)?) => {
        $($(#[$doc])* #[derive(Debug, Clone, Copy, PartialEq, Eq)] pub(crate) struct $name;
        impl FuncKind for $name {})*
    };
}

func_kinds! {
    /// An import: a member's `run`, or a canonical built-in.
    Import,
    /// Compares two values of one type: `(a, b) -> bool`.
    EqFn,
    /// Internal plumbing, or an export.
    Helper,
}

/// A function of the module being built.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FuncIdx<K> {
    index: u32,
    kind: PhantomData<K>,
}

impl<K> Clone for FuncIdx<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> Copy for FuncIdx<K> {}

/// A signature.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Sig {
    pub(crate) params: Vec<ValType>,
    pub(crate) results: Vec<ValType>,
}

impl Sig {
    /// `(i32 × params) -> (i32 × results)`.
    pub(crate) fn i32s(params: usize, results: usize) -> Self {
        Self {
            params: vec![ValType::I32; params],
            results: vec![ValType::I32; results],
        }
    }
}

/// A byte offset from some base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(transparent)]
pub(crate) struct Offset(pub(crate) u32);

impl std::ops::Add for Offset {
    type Output = Offset;

    fn add(self, other: Offset) -> Offset {
        Offset(self.0 + other.0)
    }
}

/// A size in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(transparent)]
pub(crate) struct Size(pub(crate) u32);

/// A local of the function being written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub(crate) struct Local(u32);

/// An address known when the module is built: the hub's static data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub(crate) struct Static(pub(crate) u32);

/// Where in linear memory a value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    /// At an offset from the address in a local.
    At(Local, Offset),
    /// At a static address.
    Fixed(Static),
}

impl Place {
    /// The same place, `by` further on.
    pub(crate) fn offset(self, by: Offset) -> Place {
        match self {
            Place::At(local, at) => Place::At(local, at + by),
            Place::Fixed(Static(at)) => Place::Fixed(Static(at + by.0)),
        }
    }
}

impl From<Local> for Place {
    fn from(local: Local) -> Place {
        Place::At(local, Offset(0))
    }
}

impl From<Static> for Place {
    fn from(at: Static) -> Place {
        Place::Fixed(at)
    }
}

/// Unaligned access to the one memory, at `offset`.
fn mem_arg(offset: u64) -> MemArg {
    MemArg {
        offset,
        align: 0,
        target: wasm_encoder::MemArgTarget::Memory(0),
        ordering: None,
    }
}

/// The body of a function, as it is written.
pub(crate) struct Body {
    params: u32,
    locals: Vec<ValType>,
    code: Vec<Instruction<'static>>,
}

impl Body {
    /// A body for a function of `sig`, with its parameters as locals.
    pub(crate) fn new(sig: &Sig) -> (Self, Vec<Local>) {
        let params = sig.params.len() as u32;
        let body = Self {
            params,
            locals: Vec::new(),
            code: Vec::new(),
        };
        (body, (0..params).map(Local).collect())
    }

    /// A new local.
    pub(crate) fn local(&mut self, ty: ValType) -> Local {
        self.locals.push(ty);
        Local(self.params + self.locals.len() as u32 - 1)
    }

    /// A new `i32` local.
    pub(crate) fn int(&mut self) -> Local {
        self.local(ValType::I32)
    }

    /// Any instruction.
    pub(crate) fn op(&mut self, op: Instruction<'static>) -> &mut Self {
        self.code.push(op);
        self
    }

    /// Pushes a local.
    pub(crate) fn get(&mut self, local: Local) -> &mut Self {
        self.op(Instruction::LocalGet(local.0))
    }

    /// Pops into a local.
    pub(crate) fn set(&mut self, local: Local) -> &mut Self {
        self.op(Instruction::LocalSet(local.0))
    }

    /// Pushes an `i32`.
    pub(crate) fn int_const(&mut self, value: u32) -> &mut Self {
        self.op(Instruction::I32Const(value as i32))
    }

    /// Calls a function.
    pub(crate) fn call<K>(&mut self, f: FuncIdx<K>) -> &mut Self {
        self.op(Instruction::Call(f.index))
    }

    /// Pushes the address of `place`.
    pub(crate) fn addr(&mut self, place: impl Into<Place>) -> &mut Self {
        match place.into() {
            Place::At(local, Offset(0)) => self.get(local),
            Place::At(local, Offset(at)) => self.get(local).int_const(at).op(Instruction::I32Add),
            Place::Fixed(Static(at)) => self.int_const(at),
        }
    }

    fn base(&mut self, place: Place) -> (u64, &mut Self) {
        match place {
            Place::At(local, Offset(at)) => {
                self.get(local);
                (u64::from(at), self)
            }
            Place::Fixed(Static(at)) => {
                self.int_const(at);
                (0, self)
            }
        }
    }

    /// Pushes what `make` loads from `place`, given its memory argument.
    pub(crate) fn load(
        &mut self,
        place: impl Into<Place>,
        make: impl FnOnce(MemArg) -> Instruction<'static>,
    ) -> &mut Self {
        let (offset, body) = self.base(place.into());
        body.op(make(mem_arg(offset)))
    }

    /// Pushes the `i32` at `place`.
    pub(crate) fn load32(&mut self, place: impl Into<Place>) -> &mut Self {
        self.load(place, Instruction::I32Load)
    }

    /// Stores what `value` pushes at `place`, with `make`.
    pub(crate) fn store(
        &mut self,
        place: impl Into<Place>,
        value: impl FnOnce(&mut Body),
        make: impl FnOnce(MemArg) -> Instruction<'static>,
    ) -> &mut Self {
        let (offset, body) = self.base(place.into());
        value(body);
        body.op(make(mem_arg(offset)))
    }

    /// Stores the `i32` `value` pushes at `place`.
    pub(crate) fn store32(
        &mut self,
        place: impl Into<Place>,
        value: impl FnOnce(&mut Body),
    ) -> &mut Self {
        self.store(place, value, Instruction::I32Store)
    }

    /// Copies `size` bytes from `from` to `to`.
    pub(crate) fn copy(
        &mut self,
        to: impl Into<Place>,
        from: impl Into<Place>,
        size: Size,
    ) -> &mut Self {
        self.addr(to)
            .addr(from)
            .int_const(size.0)
            .op(Instruction::MemoryCopy {
                src_mem: 0,
                dst_mem: 0,
            })
    }

    /// `if (popped) { then }`.
    pub(crate) fn when(&mut self, then: impl FnOnce(&mut Body)) -> &mut Self {
        self.op(Instruction::If(wasm_encoder::BlockType::Empty));
        then(self);
        self.op(Instruction::End)
    }

    /// `if (popped) { then } else { otherwise }`.
    pub(crate) fn when_else(
        &mut self,
        then: impl FnOnce(&mut Body),
        otherwise: impl FnOnce(&mut Body),
    ) -> &mut Self {
        self.op(Instruction::If(wasm_encoder::BlockType::Empty));
        then(self);
        self.op(Instruction::Else);
        otherwise(self);
        self.op(Instruction::End)
    }

    /// `for (i = 0; i != count; i++) { step(i) }`, `count` a local.
    pub(crate) fn count_up(
        &mut self,
        count: Local,
        step: impl FnOnce(&mut Body, Local),
    ) -> &mut Self {
        let i = self.int();
        // A local holds its value from the last time round an enclosing loop.
        self.int_const(0).set(i);
        self.op(Instruction::Block(wasm_encoder::BlockType::Empty));
        self.op(Instruction::Loop(wasm_encoder::BlockType::Empty));
        self.get(i)
            .get(count)
            .op(Instruction::I32GeU)
            .op(Instruction::BrIf(1));
        step(self, i);
        self.get(i).int_const(1).op(Instruction::I32Add).set(i);
        self.op(Instruction::Br(0));
        self.op(Instruction::End);
        self.op(Instruction::End)
    }

    /// Returns.
    pub(crate) fn ret(&mut self) -> &mut Self {
        self.op(Instruction::Return)
    }

    fn encode(self) -> Function {
        let mut function = Function::new_with_locals_types(self.locals);
        for op in &self.code {
            function.instruction(op);
        }
        function.instruction(&Instruction::End);
        function
    }
}

/// Imports may still be added: no function index exists yet.
pub(crate) struct AddingImports;
/// The imports are closed: functions may be declared and written.
pub(crate) struct AddingFuncs;

/// A core module under construction.
pub(crate) struct ModuleBuilder<S> {
    types: Vec<Sig>,
    type_index: HashMap<Sig, u32>,
    imports: Vec<(String, String, u32)>,
    funcs: Vec<(u32, Option<Body>)>,
    exports: Vec<(String, u32)>,
    state: PhantomData<S>,
}

impl<S> ModuleBuilder<S> {
    fn type_of(&mut self, sig: &Sig) -> u32 {
        if let Some(index) = self.type_index.get(sig) {
            return *index;
        }
        let index = self.types.len() as u32;
        self.types.push(sig.clone());
        self.type_index.insert(sig.clone(), index);
        index
    }
}

impl ModuleBuilder<AddingImports> {
    /// A module with nothing in it.
    pub(crate) fn new() -> Self {
        Self {
            types: Vec::new(),
            type_index: HashMap::new(),
            imports: Vec::new(),
            funcs: Vec::new(),
            exports: Vec::new(),
            state: PhantomData,
        }
    }

    /// Imports a function.
    pub(crate) fn import(&mut self, module: &str, name: &str, sig: &Sig) -> FuncIdx<Import> {
        let ty = self.type_of(sig);
        self.imports
            .push((module.to_string(), name.to_string(), ty));
        FuncIdx {
            index: self.imports.len() as u32 - 1,
            kind: PhantomData,
        }
    }

    /// Closes the imports.
    pub(crate) fn seal(self) -> ModuleBuilder<AddingFuncs> {
        ModuleBuilder {
            types: self.types,
            type_index: self.type_index,
            imports: self.imports,
            funcs: self.funcs,
            exports: self.exports,
            state: PhantomData,
        }
    }
}

impl ModuleBuilder<AddingFuncs> {
    /// Declares a function, to be written later.
    pub(crate) fn declare<K: FuncKind>(&mut self, sig: &Sig) -> FuncIdx<K> {
        let ty = self.type_of(sig);
        self.funcs.push((ty, None));
        FuncIdx {
            index: self.imports.len() as u32 + self.funcs.len() as u32 - 1,
            kind: PhantomData,
        }
    }

    /// Writes a declared function.
    pub(crate) fn define<K: FuncKind>(&mut self, f: FuncIdx<K>, body: Body) {
        let at = (f.index - self.imports.len() as u32) as usize;
        self.funcs[at].1 = Some(body);
    }

    /// Exports a function.
    pub(crate) fn export<K: FuncKind>(&mut self, name: &str, f: FuncIdx<K>) {
        self.exports.push((name.to_string(), f.index));
    }

    /// The finished module: one memory of at least `pages` pages exported as
    /// `memory`, and one mutable `i32` global, the heap's top, starting at
    /// `heap_base`. A function declared and never written is a bug here, and
    /// fails.
    pub(crate) fn finish(
        self,
        memory: &str,
        pages: u64,
        heap_base: u32,
    ) -> Result<Vec<u8>, &'static str> {
        let mut module = Module::new();
        let mut types = TypeSection::new();
        for sig in &self.types {
            types.ty().function(sig.params.clone(), sig.results.clone());
        }
        module.section(&types);
        let mut imports = ImportSection::new();
        for (from, name, ty) in &self.imports {
            imports.import(from, name, EntityType::Function(*ty));
        }
        module.section(&imports);
        let mut functions = FunctionSection::new();
        for (ty, _) in &self.funcs {
            functions.function(*ty);
        }
        module.section(&functions);
        let mut memories = MemorySection::new();
        memories.memory(MemoryType {
            minimum: pages,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        module.section(&memories);
        let mut globals = GlobalSection::new();
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(heap_base as i32),
        );
        module.section(&globals);
        let mut exports = ExportSection::new();
        exports.export(memory, ExportKind::Memory, 0);
        for (name, index) in &self.exports {
            exports.export(name, ExportKind::Func, *index);
        }
        module.section(&exports);
        let mut code = CodeSection::new();
        for (_, body) in self.funcs {
            let body = body.ok_or("a function of the hub was never written")?;
            code.function(&body.encode());
        }
        module.section(&code);
        Ok(module.finish())
    }
}

/// The one global of the module: the heap's top.
pub(crate) const HEAP_TOP: u32 = 0;
