//! The core payload type system.
//!
//! Types are fully structural: equality is derived, so two identically-shaped
//! records from different packages are compatible. Declared WIT type names
//! are not part of [`Type`]; they live in a contract's
//! [`TypeDecl`](crate::component::TypeDecl) registry for rendering.
//!
//! `future`/`stream`/event are deliberately NOT payload types — they are
//! [`PortKind`](crate::port::PortKind)s. Nested async types (e.g.
//! `stream<stream<u8>>`) are rejected at WIT lowering. WIT resources and
//! handles (`own<T>`, `borrow<T>`) have no representation here at all:
//! payloads are plain data, and contracts using resources in port types are
//! rejected at lowering.

use core::fmt;

/// A structural payload type, mirroring the WIT data type grammar.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum Type {
    /// WIT `bool`.
    Bool,
    /// WIT `u8`.
    U8,
    /// WIT `u16`.
    U16,
    /// WIT `u32`.
    U32,
    /// WIT `u64`.
    U64,
    /// WIT `s8`.
    S8,
    /// WIT `s16`.
    S16,
    /// WIT `s32`.
    S32,
    /// WIT `s64`.
    S64,
    /// WIT `f32`.
    F32,
    /// WIT `f64`.
    F64,
    /// WIT `char` (a Unicode scalar value).
    Char,
    /// WIT `string`.
    String,
    /// WIT `list<T>`.
    List(Box<Type>),
    /// WIT `option<T>`.
    Option(Box<Type>),
    /// WIT `tuple<...>`. `Tuple(vec![])` doubles as the unit payload of a
    /// bare `future`.
    Tuple(Vec<Type>),
    /// WIT `result<ok, err>`; either side may be absent.
    Result {
        /// The success payload, if any.
        ok: Option<Box<Type>>,
        /// The error payload, if any.
        err: Option<Box<Type>>,
    },
    /// A WIT `record`.
    Record(Record),
    /// A WIT `variant`.
    Variant(Variant),
    /// A WIT `enum`.
    Enum(EnumType),
    /// A WIT `flags` set.
    Flags(FlagsType),
}

/// The shape of a [`Type::Record`]. Field names and order are structural.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Record {
    /// The record's fields, in declaration order.
    pub fields: Vec<Field>,
}

/// One field of a [`Record`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Field {
    /// The field's kebab-case name.
    pub name: String,
    /// The field's type.
    #[cfg_attr(feature = "serde", serde(rename = "type"))]
    pub ty: Type,
}

/// The shape of a [`Type::Variant`]. Case names and order are structural.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Variant {
    /// The variant's cases, in declaration order.
    pub cases: Vec<Case>,
}

/// One case of a [`Variant`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Case {
    /// The case's kebab-case name.
    pub name: String,
    /// The case's payload, if it carries one.
    #[cfg_attr(feature = "serde", serde(rename = "type"))]
    pub ty: Option<Type>,
}

/// The shape of a [`Type::Enum`]: a variant with no payloads.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EnumType {
    /// The case names, in declaration order.
    pub cases: Vec<String>,
}

/// The shape of a [`Type::Flags`]: a set of named booleans.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FlagsType {
    /// The flag names, in declaration order.
    pub flags: Vec<String>,
}

/// Renders WIT-like syntax (`list<u32>`, `record { x: f32 }`) for
/// diagnostics and editor metadata.
///
/// This rendering is also the canonical type spelling inside component
/// content hashes, so any change to it changes every component identity.
impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Bool => f.write_str("bool"),
            Type::U8 => f.write_str("u8"),
            Type::U16 => f.write_str("u16"),
            Type::U32 => f.write_str("u32"),
            Type::U64 => f.write_str("u64"),
            Type::S8 => f.write_str("s8"),
            Type::S16 => f.write_str("s16"),
            Type::S32 => f.write_str("s32"),
            Type::S64 => f.write_str("s64"),
            Type::F32 => f.write_str("f32"),
            Type::F64 => f.write_str("f64"),
            Type::Char => f.write_str("char"),
            Type::String => f.write_str("string"),
            Type::List(ty) => write!(f, "list<{ty}>"),
            Type::Option(ty) => write!(f, "option<{ty}>"),
            Type::Tuple(items) => {
                f.write_str("tuple<")?;
                for (i, ty) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{ty}")?;
                }
                f.write_str(">")
            }
            Type::Result { ok, err } => match (ok, err) {
                (None, None) => f.write_str("result"),
                (Some(ok), None) => write!(f, "result<{ok}>"),
                (None, Some(err)) => write!(f, "result<_, {err}>"),
                (Some(ok), Some(err)) => write!(f, "result<{ok}, {err}>"),
            },
            Type::Record(record) => {
                if record.fields.is_empty() {
                    return f.write_str("record {}");
                }
                f.write_str("record { ")?;
                for (i, field) in record.fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}: {}", field.name, field.ty)?;
                }
                f.write_str(" }")
            }
            Type::Variant(variant) => {
                if variant.cases.is_empty() {
                    return f.write_str("variant {}");
                }
                f.write_str("variant { ")?;
                for (i, case) in variant.cases.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    match &case.ty {
                        Some(ty) => write!(f, "{}({ty})", case.name)?,
                        None => f.write_str(&case.name)?,
                    }
                }
                f.write_str(" }")
            }
            Type::Enum(e) => {
                if e.cases.is_empty() {
                    return f.write_str("enum {}");
                }
                f.write_str("enum { ")?;
                for (i, case) in e.cases.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(case)?;
                }
                f.write_str(" }")
            }
            Type::Flags(flags) => {
                if flags.flags.is_empty() {
                    return f.write_str("flags {}");
                }
                f.write_str("flags { ")?;
                for (i, flag) in flags.flags.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(flag)?;
                }
                f.write_str(" }")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(field_names: [&str; 2]) -> Type {
        Type::Record(Record {
            fields: field_names
                .iter()
                .map(|name| Field {
                    name: (*name).into(),
                    ty: Type::F32,
                })
                .collect(),
        })
    }

    #[test]
    fn structural_equality() {
        assert_eq!(point(["x", "y"]), point(["x", "y"]));
        assert_ne!(
            point(["x", "y"]),
            point(["x", "z"]),
            "field names are structural"
        );
        assert_ne!(
            point(["x", "y"]),
            point(["y", "x"]),
            "field order is structural"
        );
    }

    #[test]
    fn display_renders_wit_syntax() {
        assert_eq!(Type::List(Box::new(Type::U32)).to_string(), "list<u32>");
        assert_eq!(
            Type::Option(Box::new(Type::String)).to_string(),
            "option<string>"
        );
        assert_eq!(
            Type::Tuple(vec![Type::U8, Type::Char]).to_string(),
            "tuple<u8, char>"
        );
        assert_eq!(
            Type::Result {
                ok: Some(Box::new(Type::U32)),
                err: Some(Box::new(Type::String)),
            }
            .to_string(),
            "result<u32, string>"
        );
        assert_eq!(
            Type::Result {
                ok: None,
                err: None
            }
            .to_string(),
            "result"
        );
        assert_eq!(point(["x", "y"]).to_string(), "record { x: f32, y: f32 }");
        assert_eq!(
            Type::Record(Record { fields: vec![] }).to_string(),
            "record {}"
        );
        assert_eq!(
            Type::Variant(Variant {
                cases: vec![
                    Case {
                        name: "none".into(),
                        ty: None
                    },
                    Case {
                        name: "some".into(),
                        ty: Some(Type::U64)
                    },
                ],
            })
            .to_string(),
            "variant { none, some(u64) }"
        );
        assert_eq!(
            Type::Enum(EnumType {
                cases: vec!["a".into(), "b".into()]
            })
            .to_string(),
            "enum { a, b }"
        );
        assert_eq!(
            Type::Flags(FlagsType {
                flags: vec!["read".into(), "write".into()]
            })
            .to_string(),
            "flags { read, write }"
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_round_trip() {
        let ty = Type::Tuple(vec![
            point(["x", "y"]),
            Type::List(Box::new(Type::Option(Box::new(Type::Bool)))),
        ]);
        let json = serde_json::to_string(&ty).unwrap();
        let back: Type = serde_json::from_str(&json).unwrap();
        assert_eq!(ty, back);
    }
}
