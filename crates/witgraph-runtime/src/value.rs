//! Values as the runtime keeps them between nodes.
//!
//! wasmtime's dynamic [`Val`] holds a list as one `Val` per element, 48
//! bytes each: a `list<u8>` of 16 MiB is a 768 MiB tree, which the
//! scheduler would hold, copy at every generation start and compare. A
//! [`Value`] holds the same value with its lists of numbers packed (a byte a
//! `u8`) and its containers shared, so copying one copies a pointer. A
//! `run`'s results are made into `Value`s as they are returned, and a
//! `Value` is made into a `Val` only to call a `run`, or to be read or
//! written as one.

use std::borrow::Cow;
use std::sync::Arc;

use wasmtime::component::Val;
use witgraph_sched::OptionPayload;

use crate::shared::Shared;

/// A Value port's value as the runtime keeps it: see the [module](self).
#[derive(Debug, Clone)]
pub(crate) enum Value {
    /// Anything that is not a container: a number, a string, an enum case,
    /// flags, a handle.
    Scalar(Val),
    /// A non-empty list of numbers, packed; `fixed` for a fixed-length one.
    Numbers {
        fixed: bool,
        numbers: Numbers,
    },
    /// Any other list; `fixed` for a fixed-length one.
    List {
        fixed: bool,
        items: Arc<[Value]>,
    },
    Record(Arc<[(String, Value)]>),
    Tuple(Arc<[Value]>),
    Variant(String, Option<Arc<Value>>),
    Option(Option<Arc<Value>>),
    Result(Result<Option<Arc<Value>>, Option<Arc<Value>>>),
    Map(Arc<[(Value, Value)]>),
    /// What a handle port carries, kept by the host: never a `Val`, which
    /// would be a handle of one Store. `run`'s arguments lend it, and its
    /// results give it up ([`crate::engine`]).
    Handle(Shared),
}

/// A list of numbers, packed.
#[derive(Debug, Clone)]
pub(crate) enum Numbers {
    U8(Arc<[u8]>),
    S8(Arc<[i8]>),
    U16(Arc<[u16]>),
    S16(Arc<[i16]>),
    U32(Arc<[u32]>),
    S32(Arc<[i32]>),
    U64(Arc<[u64]>),
    S64(Arc<[i64]>),
    F32(Arc<[f32]>),
    F64(Arc<[f64]>),
}

impl Value {
    /// `val`, as the runtime keeps it.
    pub(crate) fn from_val(val: Val) -> Self {
        let shared = |payload: Option<Box<Val>>| payload.map(|v| Arc::new(Self::from_val(*v)));
        match val {
            Val::List(items) => list(false, items),
            Val::FixedLengthList(items) => list(true, items),
            Val::Record(fields) => Self::Record(
                fields
                    .into_iter()
                    .map(|(name, v)| (name, Self::from_val(v)))
                    .collect(),
            ),
            Val::Tuple(items) => Self::Tuple(items.into_iter().map(Self::from_val).collect()),
            Val::Variant(case, payload) => Self::Variant(case, shared(payload)),
            Val::Option(payload) => Self::Option(shared(payload)),
            Val::Result(Ok(payload)) => Self::Result(Ok(shared(payload))),
            Val::Result(Err(payload)) => Self::Result(Err(shared(payload))),
            Val::Map(pairs) => Self::Map(
                pairs
                    .into_iter()
                    .map(|(k, v)| (Self::from_val(k), Self::from_val(v)))
                    .collect(),
            ),
            other => Self::Scalar(other),
        }
    }

    /// The value as a `Val`, taking what is not shared rather than copying
    /// it.
    pub(crate) fn into_val(self) -> Val {
        let own = |shared: Arc<Value>| {
            Arc::try_unwrap(shared).map_or_else(|shared| shared.to_val(), Self::into_val)
        };
        let boxed = |payload: Option<Arc<Value>>| payload.map(|v| Box::new(own(v)));
        match self {
            Self::Scalar(val) => val,
            Self::Variant(case, payload) => Val::Variant(case, boxed(payload)),
            Self::Option(payload) => Val::Option(boxed(payload)),
            Self::Result(Ok(payload)) => Val::Result(Ok(boxed(payload))),
            Self::Result(Err(payload)) => Val::Result(Err(boxed(payload))),
            // A container's elements are built afresh either way (and a
            // handle is never made a `Val`: see `to_val`).
            other => other.to_val(),
        }
    }

    /// The value as a `Val`, built afresh. A handle has none: every caller
    /// keeps handles out (`run`'s arguments lend them, and reading or
    /// writing a port as a `Val` or WAVE refuses a handle port), and one
    /// that slipped through would be an empty tuple, which no handle
    /// type-checks as.
    pub(crate) fn to_val(&self) -> Val {
        let boxed = |payload: &Option<Arc<Value>>| payload.as_ref().map(|v| Box::new(v.to_val()));
        match self {
            Self::Scalar(val) => val.clone(),
            Self::Numbers { fixed, numbers } => {
                let items = numbers.to_vals();
                if *fixed {
                    Val::FixedLengthList(items)
                } else {
                    Val::List(items)
                }
            }
            Self::List { fixed, items } => {
                let items = items.iter().map(Self::to_val).collect();
                if *fixed {
                    Val::FixedLengthList(items)
                } else {
                    Val::List(items)
                }
            }
            Self::Record(fields) => Val::Record(
                fields
                    .iter()
                    .map(|(name, v)| (name.clone(), v.to_val()))
                    .collect(),
            ),
            Self::Tuple(items) => Val::Tuple(items.iter().map(Self::to_val).collect()),
            Self::Variant(case, payload) => Val::Variant(case.clone(), boxed(payload)),
            Self::Option(payload) => Val::Option(boxed(payload)),
            Self::Result(Ok(payload)) => Val::Result(Ok(boxed(payload))),
            Self::Result(Err(payload)) => Val::Result(Err(boxed(payload))),
            Self::Map(pairs) => Val::Map(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_val(), v.to_val()))
                    .collect(),
            ),
            Self::Handle(_) => Val::Tuple(Vec::new()),
        }
    }

    /// What a connection that unwraps an option delivers for the value.
    pub(crate) fn option_payload(&self) -> OptionPayload<'_, Self> {
        match self {
            Self::Option(Some(payload)) => OptionPayload::Some(Cow::Borrowed(&**payload)),
            Self::Option(None) => OptionPayload::None,
            _ => OptionPayload::NotOption,
        }
    }
}

/// A list as the runtime keeps it: packed when its elements are numbers.
fn list(fixed: bool, items: Vec<Val>) -> Value {
    macro_rules! packed {
        ($variant:ident, $make:ident) => {
            if let Some(numbers) = items
                .iter()
                .map(|v| match v {
                    Val::$variant(n) => Some(*n),
                    _ => None,
                })
                .collect::<Option<Arc<[_]>>>()
            {
                return Value::Numbers {
                    fixed,
                    numbers: Numbers::$make(numbers),
                };
            }
        };
    }
    match items.first() {
        Some(Val::U8(_)) => packed!(U8, U8),
        Some(Val::S8(_)) => packed!(S8, S8),
        Some(Val::U16(_)) => packed!(U16, U16),
        Some(Val::S16(_)) => packed!(S16, S16),
        Some(Val::U32(_)) => packed!(U32, U32),
        Some(Val::S32(_)) => packed!(S32, S32),
        Some(Val::U64(_)) => packed!(U64, U64),
        Some(Val::S64(_)) => packed!(S64, S64),
        Some(Val::Float32(_)) => packed!(Float32, F32),
        Some(Val::Float64(_)) => packed!(Float64, F64),
        _ => {}
    }
    Value::List {
        fixed,
        items: items.into_iter().map(Value::from_val).collect(),
    }
}

impl Numbers {
    fn to_vals(&self) -> Vec<Val> {
        match self {
            Self::U8(n) => n.iter().map(|&x| Val::U8(x)).collect(),
            Self::S8(n) => n.iter().map(|&x| Val::S8(x)).collect(),
            Self::U16(n) => n.iter().map(|&x| Val::U16(x)).collect(),
            Self::S16(n) => n.iter().map(|&x| Val::S16(x)).collect(),
            Self::U32(n) => n.iter().map(|&x| Val::U32(x)).collect(),
            Self::S32(n) => n.iter().map(|&x| Val::S32(x)).collect(),
            Self::U64(n) => n.iter().map(|&x| Val::U64(x)).collect(),
            Self::S64(n) => n.iter().map(|&x| Val::S64(x)).collect(),
            Self::F32(n) => n.iter().map(|&x| Val::Float32(x)).collect(),
            Self::F64(n) => n.iter().map(|&x| Val::Float64(x)).collect(),
        }
    }
}

/// Floats compare as `Val`'s do: by value, but zeros by their bits (`0.0`
/// and `-0.0` differ) and every NaN equal to every other.
fn same_float(a: f64, b: f64, bits: impl Fn(f64) -> u64) -> bool {
    (a != 0.0 && a == b) || (a == 0.0 && bits(a) == bits(b)) || (a.is_nan() && b.is_nan())
}

impl PartialEq for Numbers {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::U8(a), Self::U8(b)) => Arc::ptr_eq(a, b) || a == b,
            (Self::S8(a), Self::S8(b)) => Arc::ptr_eq(a, b) || a == b,
            (Self::U16(a), Self::U16(b)) => Arc::ptr_eq(a, b) || a == b,
            (Self::S16(a), Self::S16(b)) => Arc::ptr_eq(a, b) || a == b,
            (Self::U32(a), Self::U32(b)) => Arc::ptr_eq(a, b) || a == b,
            (Self::S32(a), Self::S32(b)) => Arc::ptr_eq(a, b) || a == b,
            (Self::U64(a), Self::U64(b)) => Arc::ptr_eq(a, b) || a == b,
            (Self::S64(a), Self::S64(b)) => Arc::ptr_eq(a, b) || a == b,
            (Self::F32(a), Self::F32(b)) => {
                Arc::ptr_eq(a, b)
                    || (a.len() == b.len()
                        && a.iter().zip(b.iter()).all(|(&x, &y)| {
                            same_float(x.into(), y.into(), |v| u64::from((v as f32).to_bits()))
                        }))
            }
            (Self::F64(a), Self::F64(b)) => {
                Arc::ptr_eq(a, b)
                    || (a.len() == b.len()
                        && a.iter()
                            .zip(b.iter())
                            .all(|(&x, &y)| same_float(x, y, f64::to_bits)))
            }
            _ => false,
        }
    }
}

/// Values compare as the `Val`s they stand for do (see [`same_float`]).
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        fn shared<T: ?Sized + PartialEq>(a: &Arc<T>, b: &Arc<T>) -> bool {
            Arc::ptr_eq(a, b) || **a == **b
        }
        fn payload(a: &Option<Arc<Value>>, b: &Option<Arc<Value>>) -> bool {
            match (a, b) {
                (Some(a), Some(b)) => shared(a, b),
                (None, None) => true,
                _ => false,
            }
        }
        match (self, other) {
            (Self::Scalar(a), Self::Scalar(b)) => a == b,
            (
                Self::Numbers { fixed, numbers },
                Self::Numbers {
                    fixed: other_fixed,
                    numbers: other_numbers,
                },
            ) => fixed == other_fixed && numbers == other_numbers,
            (
                Self::List { fixed, items },
                Self::List {
                    fixed: other_fixed,
                    items: other_items,
                },
            ) => fixed == other_fixed && shared(items, other_items),
            (Self::Record(a), Self::Record(b)) => shared(a, b),
            (Self::Tuple(a), Self::Tuple(b)) => shared(a, b),
            (Self::Variant(case, a), Self::Variant(other_case, b)) => {
                case == other_case && payload(a, b)
            }
            (Self::Option(a), Self::Option(b)) => payload(a, b),
            (Self::Result(Ok(a)), Self::Result(Ok(b)))
            | (Self::Result(Err(a)), Self::Result(Err(b))) => payload(a, b),
            (Self::Map(a), Self::Map(b)) => shared(a, b),
            (Self::Handle(a), Self::Handle(b)) => a == b,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(val: Val) {
        let value = Value::from_val(val.clone());
        assert_eq!(value.to_val(), val);
        assert_eq!(value.clone().into_val(), val);
        assert_eq!(Value::from_val(val.clone()), value);
    }

    #[test]
    fn values_round_trip() {
        round_trip(Val::List((0..=255).map(Val::U8).collect()));
        round_trip(Val::FixedLengthList(vec![
            Val::Float32(1.5),
            Val::Float32(-0.0),
        ]));
        round_trip(Val::List(Vec::new()));
        round_trip(Val::List(vec![
            Val::String("a".into()),
            Val::String("b".into()),
        ]));
        round_trip(Val::Record(vec![
            ("width".into(), Val::U32(2)),
            ("pixels".into(), Val::List(vec![Val::U8(1), Val::U8(2)])),
        ]));
        round_trip(Val::Option(Some(Box::new(Val::List(vec![Val::S64(-1)])))));
        round_trip(Val::Result(Err(None)));
        round_trip(Val::Variant(
            "c".into(),
            Some(Box::new(Val::Tuple(vec![Val::Bool(true)]))),
        ));
        round_trip(Val::Map(vec![(Val::String("k".into()), Val::U16(3))]));
    }

    #[test]
    fn numbers_are_packed() {
        let value = Value::from_val(Val::List(vec![Val::U8(1), Val::U8(2)]));
        assert!(matches!(
            value,
            Value::Numbers {
                numbers: Numbers::U8(_),
                ..
            }
        ));
        let value = Value::from_val(Val::List(vec![Val::String("x".into())]));
        assert!(matches!(value, Value::List { .. }));
    }

    #[test]
    fn equality_is_val_equality() {
        let floats =
            |xs: &[f64]| Value::from_val(Val::List(xs.iter().map(|&x| Val::Float64(x)).collect()));
        assert_eq!(floats(&[f64::NAN, 1.0]), floats(&[f64::NAN, 1.0]));
        assert_ne!(floats(&[0.0]), floats(&[-0.0]));
        assert_ne!(floats(&[1.0]), floats(&[1.0, 2.0]));
        let bytes =
            |xs: &[u8]| Value::from_val(Val::List(xs.iter().map(|&x| Val::U8(x)).collect()));
        assert_eq!(bytes(&[1, 2, 3]), bytes(&[1, 2, 3]));
        assert_ne!(bytes(&[1, 2, 3]), bytes(&[1, 2, 4]));
        // The same in a record.
        let record = |b: Value| Value::Record(Arc::from([("b".to_owned(), b)]));
        assert_eq!(record(bytes(&[9])), record(bytes(&[9])));
        assert_ne!(record(bytes(&[9])), record(bytes(&[8])));
        // Every packed float compares as its `Val` does.
        for (x, y) in [(f32::NAN, f32::NAN), (0.0, -0.0), (1.0, 1.0), (2.0, 3.0)] {
            let one = |v: f32| Value::from_val(Val::List(vec![Val::Float32(v)]));
            assert_eq!(
                one(x) == one(y),
                Val::Float32(x) == Val::Float32(y),
                "{x} {y}"
            );
        }
    }
}
