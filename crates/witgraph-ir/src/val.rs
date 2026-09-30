//! Runtime values: the concrete-value counterpart of [`Type`].
//!
//! [`Val`] carries actual data through channels; [`Type`] describes its
//! shape statically. The two are structurally parallel — every `Type`
//! variant has a corresponding `Val` variant.

use crate::types::{self as ir_types, Type};

/// A concrete runtime value.
///
/// Mirrors [`Type`] with actual payloads. Wasmtime-level
/// `Stream`, `Future`, and `Resource` values are handled by the engine
/// and never appear in channels.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Val {
    /// A boolean value.
    Bool(bool),
    /// An unsigned 8-bit integer.
    U8(u8),
    /// An unsigned 16-bit integer.
    U16(u16),
    /// An unsigned 32-bit integer.
    U32(u32),
    /// An unsigned 64-bit integer.
    U64(u64),
    /// A signed 8-bit integer.
    S8(i8),
    /// A signed 16-bit integer.
    S16(i16),
    /// A signed 32-bit integer.
    S32(i32),
    /// A signed 64-bit integer.
    S64(i64),
    /// A 32-bit floating-point value.
    F32(f32),
    /// A 64-bit floating-point value.
    F64(f64),
    /// A Unicode scalar value.
    Char(char),
    /// A UTF-8 string.
    String(String),
    /// An ordered list of homogeneous values.
    List(Vec<Val>),
    /// An optional value.
    Option(Option<Box<Val>>),
    /// A fixed-length heterogeneous tuple.
    Tuple(Vec<Val>),
    /// A result carrying an optional ok or err payload.
    Result(Result<Option<Box<Val>>, Option<Box<Val>>>),
    /// A named-field record.
    Record(Vec<(String, Val)>),
    /// A tagged union with an optional payload.
    Variant {
        /// The active case name.
        case: String,
        /// The case's payload, if it carries one.
        payload: Option<Box<Val>>,
    },
    /// A case label from an enum (variant with no payloads).
    Enum(String),
    /// A set of active flag names.
    Flags(Vec<String>),
}


impl Val {
    /// Returns `true` if this value structurally matches the given type.
    ///
    /// Used for debug assertions. Checks recursively: container values
    /// must have contents matching the inner types.
    pub fn matches_type(&self, ty: &Type) -> bool {
        match (self, ty) {
            (Val::Bool(_), Type::Bool)
            | (Val::U8(_), Type::U8)
            | (Val::U16(_), Type::U16)
            | (Val::U32(_), Type::U32)
            | (Val::U64(_), Type::U64)
            | (Val::S8(_), Type::S8)
            | (Val::S16(_), Type::S16)
            | (Val::S32(_), Type::S32)
            | (Val::S64(_), Type::S64)
            | (Val::F32(_), Type::F32)
            | (Val::F64(_), Type::F64)
            | (Val::Char(_), Type::Char)
            | (Val::String(_), Type::String) => true,

            (Val::List(items), Type::List(inner)) => {
                items.iter().all(|item| item.matches_type(inner))
            }

            (Val::Option(None), Type::Option(_)) => true,
            (Val::Option(Some(v)), Type::Option(inner)) => v.matches_type(inner),

            (Val::Tuple(items), Type::Tuple(types)) => {
                items.len() == types.len()
                    && items
                        .iter()
                        .zip(types.iter())
                        .all(|(v, t)| v.matches_type(t))
            }

            (Val::Result(Ok(v)), Type::Result { ok, .. }) => match (v, ok) {
                (None, None) => true,
                (Some(v), Some(t)) => v.matches_type(t),
                _ => false,
            },
            (Val::Result(Err(v)), Type::Result { err, .. }) => match (v, err) {
                (None, None) => true,
                (Some(v), Some(t)) => v.matches_type(t),
                _ => false,
            },

            (Val::Record(fields), Type::Record(ir_types::Record { fields: type_fields })) => {
                fields.len() == type_fields.len()
                    && fields.iter().zip(type_fields.iter()).all(
                        |((name, val), ir_types::Field { name: field_name, ty })| {
                            name == field_name && val.matches_type(ty)
                        },
                    )
            }

            (
                Val::Variant { case, payload },
                Type::Variant(ir_types::Variant { cases: type_cases }),
            ) => type_cases.iter().any(|c| {
                c.name == *case
                    && match (payload, &c.ty) {
                        (None, None) => true,
                        (Some(v), Some(t)) => v.matches_type(t),
                        _ => false,
                    }
            }),

            (Val::Enum(case), Type::Enum(ir_types::EnumType { cases })) => cases.contains(case),

            (Val::Flags(flags), Type::Flags(ir_types::FlagsType { flags: type_flags })) => {
                flags.iter().all(|f| type_flags.contains(f))
            }

            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_match() {
        assert!(Val::Bool(true).matches_type(&Type::Bool));
        assert!(Val::U32(42).matches_type(&Type::U32));
        assert!(Val::F64(3.14).matches_type(&Type::F64));
        assert!(Val::Char('a').matches_type(&Type::Char));
        assert!(Val::String("hello".into()).matches_type(&Type::String));
        assert!(Val::S8(-1).matches_type(&Type::S8));
        assert!(Val::S16(-1).matches_type(&Type::S16));
        assert!(Val::S32(-1).matches_type(&Type::S32));
        assert!(Val::S64(-1).matches_type(&Type::S64));
        assert!(Val::U8(1).matches_type(&Type::U8));
        assert!(Val::U16(1).matches_type(&Type::U16));
        assert!(Val::U64(1).matches_type(&Type::U64));
        assert!(Val::F32(1.0).matches_type(&Type::F32));
    }

    #[test]
    fn primitives_reject_wrong_type() {
        assert!(!Val::Bool(true).matches_type(&Type::U32));
        assert!(!Val::U32(42).matches_type(&Type::F64));
        assert!(!Val::String("hi".into()).matches_type(&Type::Bool));
    }

    #[test]
    fn list_matches() {
        let val = Val::List(vec![Val::U32(1), Val::U32(2)]);
        assert!(val.matches_type(&Type::List(Box::new(Type::U32))));
        assert!(!val.matches_type(&Type::List(Box::new(Type::F64))));
    }

    #[test]
    fn empty_list_matches_any_inner_type() {
        let val = Val::List(vec![]);
        assert!(val.matches_type(&Type::List(Box::new(Type::U32))));
        assert!(val.matches_type(&Type::List(Box::new(Type::String))));
    }

    #[test]
    fn option_matches() {
        let none = Val::Option(None);
        let some = Val::Option(Some(Box::new(Val::U32(5))));
        assert!(none.matches_type(&Type::Option(Box::new(Type::U32))));
        assert!(some.matches_type(&Type::Option(Box::new(Type::U32))));
        assert!(!some.matches_type(&Type::Option(Box::new(Type::F64))));
    }

    #[test]
    fn tuple_matches() {
        let val = Val::Tuple(vec![Val::U32(1), Val::String("hi".into())]);
        assert!(val.matches_type(&Type::Tuple(vec![Type::U32, Type::String])));
        assert!(!val.matches_type(&Type::Tuple(vec![Type::U32])));
        assert!(!val.matches_type(&Type::Tuple(vec![Type::String, Type::U32])));
    }

    #[test]
    fn result_matches() {
        let ok_val = Val::Result(Ok(Some(Box::new(Val::U32(1)))));
        let err_val = Val::Result(Err(Some(Box::new(Val::String("oops".into())))));
        let ty = Type::Result {
            ok: Some(Box::new(Type::U32)),
            err: Some(Box::new(Type::String)),
        };
        assert!(ok_val.matches_type(&ty));
        assert!(err_val.matches_type(&ty));

        let unit_result = Val::Result(Ok(None));
        assert!(unit_result.matches_type(&Type::Result {
            ok: None,
            err: None,
        }));
    }

    #[test]
    fn record_matches() {
        let val = Val::Record(vec![
            ("x".into(), Val::F32(1.0)),
            ("y".into(), Val::F32(2.0)),
        ]);
        let ty = Type::Record(ir_types::Record {
            fields: vec![
                ir_types::Field {
                    name: "x".into(),
                    ty: Type::F32,
                },
                ir_types::Field {
                    name: "y".into(),
                    ty: Type::F32,
                },
            ],
        });
        assert!(val.matches_type(&ty));
    }

    #[test]
    fn record_rejects_wrong_field_name() {
        let val = Val::Record(vec![("z".into(), Val::F32(1.0))]);
        let ty = Type::Record(ir_types::Record {
            fields: vec![ir_types::Field {
                name: "x".into(),
                ty: Type::F32,
            }],
        });
        assert!(!val.matches_type(&ty));
    }

    #[test]
    fn variant_matches() {
        let val = Val::Variant {
            case: "some".into(),
            payload: Some(Box::new(Val::U64(42))),
        };
        let ty = Type::Variant(ir_types::Variant {
            cases: vec![
                ir_types::Case {
                    name: "none".into(),
                    ty: None,
                },
                ir_types::Case {
                    name: "some".into(),
                    ty: Some(Type::U64),
                },
            ],
        });
        assert!(val.matches_type(&ty));

        let no_payload = Val::Variant {
            case: "none".into(),
            payload: None,
        };
        assert!(no_payload.matches_type(&ty));
    }

    #[test]
    fn enum_matches() {
        let val = Val::Enum("red".into());
        let ty = Type::Enum(ir_types::EnumType {
            cases: vec!["red".into(), "green".into(), "blue".into()],
        });
        assert!(val.matches_type(&ty));
        assert!(!Val::Enum("yellow".into()).matches_type(&ty));
    }

    #[test]
    fn flags_matches() {
        let val = Val::Flags(vec!["read".into(), "write".into()]);
        let ty = Type::Flags(ir_types::FlagsType {
            flags: vec!["read".into(), "write".into(), "exec".into()],
        });
        assert!(val.matches_type(&ty));
        assert!(!Val::Flags(vec!["unknown".into()]).matches_type(&ty));
    }
}
