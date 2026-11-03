//! The canonical ABI of the hub's types: layout and classification, read off
//! the hub's own `Resolve` once.

use wit_parser::abi::{FlatTypes, WasmType};
use wit_parser::{Int, Resolve, SizeAlign, Type, TypeDefKind};

use super::module::{Offset, Size};

/// A type with aliases seen through.
#[derive(Clone)]
pub(crate) enum Shape<'a> {
    Prim(Prim),
    String,
    List(Type),
    Record(Vec<Type>),
    /// A variant, option or result: its tag, and each case's payload.
    Cases(Int, Vec<Option<Type>>),
    /// An enum or flags: bits of this many bytes.
    Bits(Int),
    Own(wit_parser::TypeId),
    Borrow,
    Stream,
    Future,
    #[allow(dead_code)]
    Other(&'a str),
}

/// A number or character.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Prim {
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
}

/// The layout of the hub's types.
pub(crate) struct Abi<'a> {
    pub(crate) resolve: &'a Resolve,
    sizes: SizeAlign,
}

impl<'a> Abi<'a> {
    pub(crate) fn new(resolve: &'a Resolve) -> Self {
        let mut sizes = SizeAlign::default();
        sizes.fill(resolve).ok();
        Self { resolve, sizes }
    }

    pub(crate) fn size(&self, ty: &Type) -> Size {
        Size(self.sizes.size(ty).size_wasm32() as u32)
    }

    /// Where each of `types` is in a record of them.
    pub(crate) fn offsets(&self, types: &[Type]) -> Vec<Offset> {
        self.sizes
            .field_offsets(types.iter())
            .into_iter()
            .map(|(at, _)| Offset(at.size_wasm32() as u32))
            .collect()
    }

    /// Where the payload is in a value with these cases.
    pub(crate) fn payload_offset(&self, tag: Int, cases: &[Option<Type>]) -> Offset {
        let at = self
            .sizes
            .payload_offset(tag, cases.iter().map(|case| case.as_ref()));
        Offset(at.size_wasm32() as u32)
    }

    /// `ty`, aliases seen through.
    pub(crate) fn shape(&self, ty: &Type) -> Shape<'a> {
        match ty {
            Type::Bool | Type::U8 | Type::S8 => Shape::Prim(Prim::U8),
            Type::U16 | Type::S16 => Shape::Prim(Prim::U16),
            Type::U32 | Type::S32 | Type::Char => Shape::Prim(Prim::U32),
            Type::U64 | Type::S64 => Shape::Prim(Prim::U64),
            Type::F32 => Shape::Prim(Prim::F32),
            Type::F64 => Shape::Prim(Prim::F64),
            Type::String => Shape::String,
            Type::ErrorContext => Shape::Other("error-context"),
            Type::Id(id) => match &self.resolve.types[*id].kind {
                TypeDefKind::Type(inner) => self.shape(inner),
                TypeDefKind::Record(record) => {
                    Shape::Record(record.fields.iter().map(|f| f.ty).collect())
                }
                TypeDefKind::Tuple(tuple) => Shape::Record(tuple.types.clone()),
                TypeDefKind::List(inner) => Shape::List(*inner),
                TypeDefKind::Option(inner) => Shape::Cases(Int::U8, vec![None, Some(*inner)]),
                TypeDefKind::Result(result) => Shape::Cases(Int::U8, vec![result.ok, result.err]),
                TypeDefKind::Variant(variant) => Shape::Cases(
                    variant.tag(),
                    variant.cases.iter().map(|case| case.ty).collect(),
                ),
                TypeDefKind::Enum(enum_) => Shape::Bits(enum_.tag()),
                TypeDefKind::Flags(flags) => Shape::Bits(match flags.repr() {
                    wit_parser::FlagsRepr::U8 => Int::U8,
                    wit_parser::FlagsRepr::U16 => Int::U16,
                    wit_parser::FlagsRepr::U32(_) => Int::U32,
                }),
                TypeDefKind::Handle(wit_parser::Handle::Own(resource)) => Shape::Own(*resource),
                TypeDefKind::Handle(wit_parser::Handle::Borrow(_)) => Shape::Borrow,
                TypeDefKind::Stream(_) => Shape::Stream,
                TypeDefKind::Future(_) => Shape::Future,
                other => Shape::Other(other.as_str()),
            },
        }
    }

    /// Whether two values of `ty` are equal exactly when their bytes are: no
    /// padding, no pointers, no floats.
    pub(crate) fn dense(&self, ty: &Type) -> bool {
        match self.shape(ty) {
            Shape::Prim(Prim::F32 | Prim::F64) => false,
            Shape::Prim(_) | Shape::Bits(_) => true,
            Shape::Record(types) => {
                let mut end = 0;
                let offsets = self.offsets(&types);
                let packed = types.iter().zip(&offsets).all(|(ty, at)| {
                    let packed = at.0 == end && self.dense(ty);
                    end = at.0 + self.size(ty).0;
                    packed
                });
                packed && end == self.record_size(&types).0
            }
            _ => false,
        }
    }

    /// The size of a record of `types`.
    fn record_size(&self, types: &[Type]) -> Size {
        Size(self.sizes.record(types.iter()).size.size_wasm32() as u32)
    }

    /// Whether a value of `ty` owns anything that must be freed.
    pub(crate) fn owns(&self, ty: &Type) -> bool {
        match self.shape(ty) {
            Shape::String | Shape::List(_) | Shape::Own(_) => true,
            Shape::Record(types) => types.iter().any(|ty| self.owns(ty)),
            Shape::Cases(_, cases) => cases.iter().flatten().any(|ty| self.owns(ty)),
            _ => false,
        }
    }

    /// The core values `ty` flattens to.
    pub(crate) fn flat(&self, ty: &Type) -> Vec<WasmType> {
        let mut storage = [WasmType::I32; 32];
        let mut flat = FlatTypes::new(&mut storage);
        self.resolve.push_flat(ty, &mut flat);
        flat.to_vec()
    }

    /// A string that is the same for structurally equal types.
    pub(crate) fn key(&self, ty: &Type) -> String {
        match self.shape(ty) {
            Shape::Prim(p) => match ty {
                Type::F32 => "f32".into(),
                Type::F64 => "f64".into(),
                _ => format!("b{}", p as u8),
            },
            Shape::String => "s".into(),
            Shape::List(e) => format!("l[{}]", self.key(&e)),
            Shape::Record(types) => {
                let keys: Vec<String> = types.iter().map(|t| self.key(t)).collect();
                format!("r[{}]", keys.join(","))
            }
            Shape::Cases(tag, cases) => {
                let keys: Vec<String> = cases
                    .iter()
                    .map(|c| c.as_ref().map_or("_".to_string(), |t| self.key(t)))
                    .collect();
                format!("c{tag:?}[{}]", keys.join(","))
            }
            Shape::Bits(tag) => format!("i{tag:?}"),
            Shape::Own(resource) => format!("o{}", resource.index()),
            Shape::Borrow => "bw".into(),
            Shape::Stream => "st".into(),
            Shape::Future => "fu".into(),
            Shape::Other(name) => name.to_string(),
        }
    }
}
