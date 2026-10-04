//! Checks on WAVE text that WAVE's own typed parser leaves out, and on
//! values a host hands an executor.

use std::borrow::Cow;

use wasm_wave::ast::{Node, NodeType};
use wasm_wave::untyped::UntypedValue;
use wasm_wave::wasm::{WasmType, WasmTypeKind, WasmValue};

/// Checks every label in WAVE `text` against `ty`, for any representation
/// of WAVE types: record fields, cases and flags the type does not have
/// are errors (WAVE's typed parse ignores a record field the type does not
/// have). Fields and flags given twice are rejected by WAVE's own parser,
/// and shape mismatches by the typed parse. Every executor parses WAVE
/// through this, so a value is accepted or rejected the same way on each.
///
/// The elements of a fixed-length list are not checked: neither WAVE type
/// representation exposes their type through [`WasmType`], and lowering
/// keeps fixed-length lists out of port types.
pub fn check_labels<T: WasmType>(ty: &T, text: &str) -> Result<(), String> {
    let parsed = UntypedValue::parse(text).map_err(|e| format!("{e}"))?;
    check_parsed(ty, &parsed)
}

/// Parses WAVE `text` as a value of type `ty`, rejecting what
/// [`check_labels`] rejects: how every executor parses WAVE, so each
/// accepts and rejects the same text (snapshots are shared between them).
pub fn parse<V: WasmValue>(ty: &V::Type, text: &str) -> Result<V, String> {
    let parsed = UntypedValue::parse(text).map_err(|e| format!("{e}"))?;
    check_parsed(ty, &parsed)?;
    parsed.to_wasm_value(ty).map_err(|e| format!("{e}"))
}

/// Checks that `val` has type `ty`, for any representation of WAVE values:
/// how every executor checks a value the host hands it, so each accepts and
/// rejects the same values. The kinds must agree exactly (a list is no
/// fixed-length list), every record field must be there, in order, and
/// every case and flag must be the type's, each flag set once.
///
/// Fixed-length lists and types WAVE does not support are rejected:
/// lowering keeps them out of port types.
pub fn check_value<V: WasmValue>(ty: &V::Type, val: &V) -> Result<(), String> {
    let (kind, found) = (ty.kind(), val.kind());
    if kind != found {
        return Err(format!("expected {kind}, found {found}"));
    }
    match kind {
        WasmTypeKind::List => {
            let element = ty
                .list_element_type()
                .ok_or("the list type has no element type")?;
            val.unwrap_list()
                .try_for_each(|item| check_value(&element, &*item))
        }
        WasmTypeKind::Record => {
            let fields: Vec<_> = ty.record_fields().collect();
            let values: Vec<_> = val.unwrap_record().collect();
            if fields.len() != values.len() {
                return Err(format!(
                    "expected {} fields, found {}",
                    fields.len(),
                    values.len()
                ));
            }
            fields
                .iter()
                .zip(&values)
                .try_for_each(|((field, ty), (name, value))| {
                    if field != name {
                        return Err(format!("expected field `{field}`, found `{name}`"));
                    }
                    check_value(ty, &**value).map_err(|e| format!("field `{name}`: {e}"))
                })
        }
        WasmTypeKind::Tuple => {
            let types: Vec<_> = ty.tuple_element_types().collect();
            let items: Vec<_> = val.unwrap_tuple().collect();
            if types.len() != items.len() {
                return Err(format!(
                    "expected {} elements, found {}",
                    types.len(),
                    items.len()
                ));
            }
            types
                .iter()
                .zip(&items)
                .try_for_each(|(ty, item)| check_value(ty, &**item))
        }
        WasmTypeKind::Variant => {
            let (name, value) = val.unwrap_variant();
            let (_, case) = ty
                .variant_cases()
                .find(|(case, _)| *case == name)
                .ok_or_else(|| format!("the variant has no case `{name}`"))?;
            payload(case, value).map_err(|e| format!("case `{name}`: {e}"))
        }
        WasmTypeKind::Enum => {
            let name = val.unwrap_enum();
            if ty.enum_cases().any(|case| case == name) {
                Ok(())
            } else {
                Err(format!("the enum has no case `{name}`"))
            }
        }
        WasmTypeKind::Option => match val.unwrap_option() {
            Some(value) => payload(ty.option_some_type(), Some(value)),
            None => Ok(()),
        },
        WasmTypeKind::Result => {
            let (ok, err) = ty.result_types().unwrap_or((None, None));
            match val.unwrap_result() {
                Ok(value) => payload(ok, value),
                Err(value) => payload(err, value),
            }
        }
        WasmTypeKind::Flags => {
            let set: Vec<_> = val.unwrap_flags().collect();
            for (i, flag) in set.iter().enumerate() {
                if !ty.flags_names().any(|name| name == *flag) {
                    return Err(format!("the flags have no flag `{flag}`"));
                }
                if set[..i].contains(flag) {
                    return Err(format!("flag `{flag}` is set twice"));
                }
            }
            Ok(())
        }
        WasmTypeKind::FixedLengthList | WasmTypeKind::Unsupported => {
            Err(format!("a {kind} value cannot be checked"))
        }
        _ => Ok(()),
    }
}

/// Checks a case's or result's payload against its type.
fn payload<V: WasmValue>(ty: Option<V::Type>, val: Option<Cow<'_, V>>) -> Result<(), String> {
    match (ty, val) {
        (None, None) => Ok(()),
        (Some(ty), Some(val)) => check_value(&ty, &*val),
        (Some(_), None) => Err("missing payload".into()),
        (None, Some(_)) => Err("unexpected payload".into()),
    }
}

/// [`check_labels`] on text already parsed.
pub fn check_parsed<T: WasmType>(ty: &T, parsed: &UntypedValue<'_>) -> Result<(), String> {
    check_node(ty, parsed.node(), parsed.source())
}

/// Checks one parsed node against `ty`.
fn check_node<T: WasmType>(ty: &T, node: &Node, src: &str) -> Result<(), String> {
    let at = |e: wasm_wave::parser::ParserError| format!("{e}");
    match (ty.kind(), node.ty()) {
        (WasmTypeKind::Record, NodeType::Record) => {
            let fields: Vec<_> = ty.record_fields().collect();
            for (label, value) in node.as_record(src).map_err(at)? {
                let (_, field) = fields
                    .iter()
                    .find(|(name, _)| *name == label)
                    .ok_or_else(|| format!("the record has no field `{label}`"))?;
                check_node(field, value, src).map_err(|e| format!("field `{label}`: {e}"))?;
            }
            Ok(())
        }
        (WasmTypeKind::List | WasmTypeKind::FixedLengthList, NodeType::List) => {
            let Some(element) = ty.list_element_type() else {
                return Ok(());
            };
            node.as_list()
                .map_err(at)?
                .into_iter()
                .try_for_each(|item| check_node(&element, item, src))
        }
        (WasmTypeKind::Tuple, NodeType::Tuple) => ty
            .tuple_element_types()
            .zip(node.as_tuple().map_err(at)?)
            .try_for_each(|(ty, item)| check_node(&ty, item, src)),
        (WasmTypeKind::Option, NodeType::OptionSome | NodeType::OptionNone) => {
            match (node.as_option().map_err(at)?, ty.option_some_type()) {
                (Some(payload), Some(some)) => check_node(&some, payload, src),
                _ => Ok(()),
            }
        }
        // An option's payload may be written bare.
        (WasmTypeKind::Option, _) => match ty.option_some_type() {
            Some(some) => check_node(&some, node, src),
            None => Ok(()),
        },
        (WasmTypeKind::Result, NodeType::ResultOk | NodeType::ResultErr) => {
            let (ok, err) = ty.result_types().unwrap_or((None, None));
            match node.as_result().map_err(at)? {
                Ok(Some(payload)) => ok.map_or(Ok(()), |ty| check_node(&ty, payload, src)),
                Err(Some(payload)) => err.map_or(Ok(()), |ty| check_node(&ty, payload, src)),
                _ => Ok(()),
            }
        }
        // An ok payload may be written bare too (WAVE flattens it).
        (WasmTypeKind::Result, _) => match ty.result_types() {
            Some((Some(ok), _)) => check_node(&ok, node, src),
            _ => Ok(()),
        },
        (WasmTypeKind::Variant, NodeType::Label | NodeType::VariantWithPayload) => {
            let (label, payload) = node.as_variant(src).map_err(at)?;
            let (_, case) = ty
                .variant_cases()
                .find(|(name, _)| *name == label)
                .ok_or_else(|| format!("the variant has no case `{label}`"))?;
            match (case, payload) {
                (Some(ty), Some(payload)) => check_node(&ty, payload, src),
                _ => Ok(()),
            }
        }
        (WasmTypeKind::Enum, NodeType::Label) => {
            let label = node.as_enum(src).map_err(at)?;
            if ty.enum_cases().any(|case| case == label) {
                Ok(())
            } else {
                Err(format!("the enum has no case `{label}`"))
            }
        }
        (WasmTypeKind::Flags, NodeType::Flags) => {
            for flag in node.as_flags(src).map_err(at)? {
                if !ty.flags_names().any(|name| name == flag) {
                    return Err(format!("the flags have no flag `{flag}`"));
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_wave::value::{Type, Value};

    #[test]
    fn values_are_checked_by_kind_and_label() {
        let point = Type::record([("x", Type::U32), ("y", Type::U32)]).unwrap();
        let ok = Value::make_record(&point, [("x", Value::from(1u32)), ("y", Value::from(2u32))])
            .unwrap();
        assert_eq!(check_value(&point, &ok), Ok(()));
        assert_eq!(
            check_value(&Type::U32, &Value::from(1u64)),
            Err("expected u32, found u64".into())
        );
        let other = Type::record([("y", Type::U32), ("x", Type::U32)]).unwrap();
        assert_eq!(
            check_value(&other, &ok),
            Err("expected field `y`, found `x`".into())
        );
        let color = Type::enum_ty(["red", "green"]).unwrap();
        let blue = Type::enum_ty(["blue"]).unwrap();
        let value = Value::make_enum(&blue, "blue").unwrap();
        assert_eq!(
            check_value(&color, &value),
            Err("the enum has no case `blue`".into())
        );
        let maybe = Type::option(Type::U32);
        let some = Value::make_option(&Type::option(Type::U64), Some(Value::from(1u64))).unwrap();
        assert_eq!(
            check_value(&maybe, &some),
            Err("expected u32, found u64".into())
        );
    }

    fn record() -> Type {
        Type::record([("x", Type::U32)]).unwrap()
    }

    #[test]
    fn unknown_fields_are_errors_however_written() {
        let result = Type::result(Some(record()), Some(Type::STRING));
        assert!(check_labels(&result, "ok({x: 1})").is_ok());
        assert!(check_labels(&result, "ok({x: 1, typo: 2})").is_err());
        assert!(
            check_labels(&result, "{x: 1, typo: 2}").is_err(),
            "a bare ok payload is checked too"
        );
        let option = Type::option(record());
        assert!(check_labels(&option, "{x: 1, typo: 2}").is_err());
        // WAVE's own parser rejects labels given twice.
        assert!(check_labels(&record(), "{x: 1, x: 2}").is_err());
        let flags = Type::flags(["a", "b"]).unwrap();
        assert!(check_labels(&flags, "{a, a}").is_err());
    }
}
