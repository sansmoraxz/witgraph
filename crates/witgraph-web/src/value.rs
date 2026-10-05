//! Values of the JavaScript executor, and their jco representation.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use js_sys::{Array, ArrayBuffer, BigInt, JsString, Object, Reflect};
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};
use witgraph_ir::wasm_wave::value::{Type, Value};
use witgraph_ir::wasm_wave::wasm::{WasmType, WasmTypeKind, WasmValue};

/// What a port carries on a JavaScript host.
#[derive(Debug, Clone)]
pub(crate) enum WebValue {
    /// A Value port's value, shared: the scheduler, its consumers and the
    /// live view hold the same one.
    Data(Rc<Value>),
    /// A stream or future, as its producer returned it. Two handles are
    /// equal only when they are the same object.
    Handle(JsValue),
}

/// Values compare as on wasmtime: floats by their bits, except that every
/// NaN equals every other (so `0.0` and `-0.0` differ).
impl PartialEq for WebValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Data(a), Self::Data(b)) => same(a, b),
            (Self::Handle(a), Self::Handle(b)) => a == b,
            _ => false,
        }
    }
}

/// Whether two values are the same, as [`WebValue`]'s equality has it.
fn same(a: &Value, b: &Value) -> bool {
    // Walked side by side, without collecting either.
    fn all<'v, T: 'v>(
        mut a: impl Iterator<Item = T>,
        mut b: impl Iterator<Item = T>,
        eq: impl Fn(&T, &T) -> bool,
    ) -> bool {
        loop {
            match (a.next(), b.next()) {
                (Some(x), Some(y)) if eq(&x, &y) => {}
                (None, None) => return true,
                _ => return false,
            }
        }
    }
    let values = |x: &Cow<'_, Value>, y: &Cow<'_, Value>| same(x, y);
    let payload = |a: Option<Cow<'_, Value>>, b: Option<Cow<'_, Value>>| match (a, b) {
        (Some(a), Some(b)) => same(&a, &b),
        (None, None) => true,
        _ => false,
    };
    if a.kind() != b.kind() {
        return false;
    }
    match a.kind() {
        WasmTypeKind::F32 => {
            let (a, b) = (a.unwrap_f32(), b.unwrap_f32());
            (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
        }
        WasmTypeKind::F64 => {
            let (a, b) = (a.unwrap_f64(), b.unwrap_f64());
            (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
        }
        WasmTypeKind::List | WasmTypeKind::FixedLengthList => {
            all(a.unwrap_list(), b.unwrap_list(), values)
        }
        WasmTypeKind::Tuple => all(a.unwrap_tuple(), b.unwrap_tuple(), values),
        WasmTypeKind::Record => all(
            a.unwrap_record(),
            b.unwrap_record(),
            |(na, va), (nb, vb)| na == nb && same(va, vb),
        ),
        WasmTypeKind::Variant => {
            let ((ca, pa), (cb, pb)) = (a.unwrap_variant(), b.unwrap_variant());
            ca == cb && payload(pa, pb)
        }
        WasmTypeKind::Option => payload(a.unwrap_option(), b.unwrap_option()),
        WasmTypeKind::Result => match (a.unwrap_result(), b.unwrap_result()) {
            (Ok(a), Ok(b)) | (Err(a), Err(b)) => payload(a, b),
            _ => false,
        },
        // Scalars, strings, enums and flags hold no floats.
        _ => a == b,
    }
}

/// `kebab-case` as jco names record fields and flags: heck's
/// `lowerCamelCase`, which lower-cases every word before capitalising all
/// but the first (`HTTP-status` is `httpStatus`, `get-HTTP` is `getHttp`).
pub(crate) fn camel(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, word) in name.split('-').filter(|w| !w.is_empty()).enumerate() {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            if i == 0 {
                out.extend(first.to_lowercase());
            } else {
                out.extend(first.to_uppercase());
            }
            out.extend(chars.flat_map(char::to_lowercase));
        }
    }
    out
}

// The JavaScript calls a conversion makes on a value it was handed, with
// their exceptions caught: the module is built with `panic = "abort"`, so an
// exception thrown through it would skip every `Drop` on its way out (a
// borrow, a guard) and leave the graph unusable.
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(catch, js_namespace = Array, js_name = isArray)]
    fn is_array(value: &JsValue) -> Result<bool, JsValue>;
    #[wasm_bindgen(catch, js_namespace = Array, js_name = from)]
    fn array_from(value: &JsValue) -> Result<Array, JsValue>;
    #[wasm_bindgen(catch, js_name = String)]
    fn string(value: &JsValue) -> Result<JsString, JsValue>;
}

/// `value` as text, for a message: what `String(value)` gives, or a
/// placeholder when even that throws.
pub(crate) fn describe(value: &JsValue) -> String {
    string(value).map_or_else(
        |_| "a value that does not convert to text".into(),
        String::from,
    )
}

thread_local! {
    /// Each WIT name's JavaScript key ([`camel`]), made once: the names
    /// come from contracts, so there are only so many.
    static KEYS: RefCell<HashMap<String, JsValue>> = RefCell::new(HashMap::new());
}

/// Calls `f` with the JavaScript key of WIT name `name` (`burst-size` is
/// `burstSize`).
pub(crate) fn with_key<R>(name: &str, f: impl FnOnce(&JsValue) -> R) -> R {
    KEYS.with(|keys| {
        // `f` may call into JavaScript, and that back here: a nested call
        // that finds the cache borrowed makes its key afresh.
        if let Ok(keys) = keys.try_borrow()
            && let Some(key) = keys.get(name)
        {
            return f(key);
        }
        let key = JsValue::from_str(&camel(name));
        if let Ok(mut keys) = keys.try_borrow_mut() {
            keys.insert(name.to_owned(), key.clone());
        }
        f(&key)
    })
}

/// Whether jco represents option type `ty` as nullable (`undefined` for
/// `none`, the payload itself for `some`), rather than as
/// `{ tag: 'none' }` and `{ tag: 'some', val }`. It is nullable unless its
/// payload is a nullable option itself: `option<u32>` is,
/// `option<option<u32>>` is not, `option<option<option<u32>>>` is again.
fn nullable(ty: &Type) -> bool {
    ty.kind() == WasmTypeKind::Option && !ty.option_some_type().is_some_and(|some| nullable(&some))
}

fn get(object: &JsValue, key: &str) -> JsValue {
    Reflect::get(object, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}

/// The property of `object` that jco names after WIT name `name`.
fn field(object: &JsValue, name: &str) -> JsValue {
    with_key(name, |key| Reflect::get(object, key)).unwrap_or(JsValue::UNDEFINED)
}

pub(crate) fn set(object: &Object, key: &str, value: &JsValue) {
    // Setting a property of a plain object cannot fail.
    let _ = Reflect::set(object, &JsValue::from_str(key), value);
}

/// Sets the property of `object` that jco names after WIT name `name`.
pub(crate) fn set_field(object: &Object, name: &str, value: &JsValue) {
    // Setting a property of a plain object cannot fail.
    let _ = with_key(name, |key| Reflect::set(object, key, value));
}

/// `{ tag, val }`, as jco writes a variant or a nested result.
fn tagged(tag: &str, val: Option<JsValue>) -> JsValue {
    let object = Object::new();
    set(&object, "tag", &JsValue::from_str(tag));
    if let Some(val) = val {
        set(&object, "val", &val);
    }
    object.into()
}

/// A WAVE value of type `ty` as jco hands it to JavaScript.
pub(crate) fn to_js(value: &Value, ty: &Type) -> JsValue {
    match ty.kind() {
        WasmTypeKind::Bool => JsValue::from_bool(value.unwrap_bool()),
        WasmTypeKind::S8 => JsValue::from(value.unwrap_s8()),
        WasmTypeKind::U8 => JsValue::from(value.unwrap_u8()),
        WasmTypeKind::S16 => JsValue::from(value.unwrap_s16()),
        WasmTypeKind::U16 => JsValue::from(value.unwrap_u16()),
        WasmTypeKind::S32 => JsValue::from(value.unwrap_s32()),
        WasmTypeKind::U32 => JsValue::from(value.unwrap_u32()),
        WasmTypeKind::S64 => BigInt::from(value.unwrap_s64()).into(),
        WasmTypeKind::U64 => BigInt::from(value.unwrap_u64()).into(),
        WasmTypeKind::F32 => JsValue::from(value.unwrap_f32()),
        WasmTypeKind::F64 => JsValue::from(value.unwrap_f64()),
        WasmTypeKind::Char => JsValue::from_str(&value.unwrap_char().to_string()),
        WasmTypeKind::String => JsValue::from_str(&value.unwrap_string()),
        WasmTypeKind::List | WasmTypeKind::FixedLengthList => {
            let Some(element) = ty.list_element_type() else {
                return JsValue::UNDEFINED;
            };
            typed_array(value, element.kind()).unwrap_or_else(|| {
                let items: Array = value.unwrap_list().map(|v| to_js(&v, &element)).collect();
                items.into()
            })
        }
        WasmTypeKind::Record => {
            let object = Object::new();
            for ((name, field), (_, v)) in ty.record_fields().zip(value.unwrap_record()) {
                set_field(&object, &name, &to_js(&v, &field));
            }
            object.into()
        }
        WasmTypeKind::Tuple => {
            let items: Array = ty
                .tuple_element_types()
                .zip(value.unwrap_tuple())
                .map(|(element, v)| to_js(&v, &element))
                .collect();
            items.into()
        }
        WasmTypeKind::Variant => {
            let (case, payload) = value.unwrap_variant();
            let payload_type = ty
                .variant_cases()
                .find(|(name, _)| *name == case)
                .and_then(|(_, ty)| ty);
            let val = payload
                .zip(payload_type)
                .map(|(payload, ty)| to_js(&payload, &ty));
            tagged(&case, val)
        }
        WasmTypeKind::Enum => JsValue::from_str(&value.unwrap_enum()),
        WasmTypeKind::Option => {
            let Some(some) = ty.option_some_type() else {
                return JsValue::UNDEFINED;
            };
            match value.unwrap_option() {
                Some(payload) => some_to_js(&payload, &some),
                None => none_to_js(&some),
            }
        }
        WasmTypeKind::Result => {
            let (ok, err) = ty.result_types().unwrap_or((None, None));
            match value.unwrap_result() {
                Ok(payload) => tagged("ok", payload.zip(ok).map(|(v, ty)| to_js(&v, &ty))),
                Err(payload) => tagged("err", payload.zip(err).map(|(v, ty)| to_js(&v, &ty))),
            }
        }
        WasmTypeKind::Flags => {
            let set_flags: Vec<Cow<'_, str>> = value.unwrap_flags().collect();
            let object = Object::new();
            for name in ty.flags_names() {
                let on = set_flags.contains(&name);
                set_field(&object, &name, &JsValue::from_bool(on));
            }
            object.into()
        }
        _ => JsValue::UNDEFINED,
    }
}

/// Checks that `value` has type `ty`, as every executor does
/// ([`witgraph_ir::wave::check_value`]). [`to_js`] relies on it.
pub(crate) fn check_type(value: &Value, ty: &Type) -> Result<(), String> {
    witgraph_ir::wave::check_value(ty, value)
}

/// `some(value)` of an `option<ty>`, `value` being of the payload type
/// `ty`, as jco represents it: the payload itself, or `{ tag: 'some', val }`
/// when the payload is a nullable option (see [`nullable`]).
pub(crate) fn some_to_js(value: &Value, ty: &Type) -> JsValue {
    let js = to_js(value, ty);
    if nullable(ty) {
        tagged("some", Some(js))
    } else {
        js
    }
}

/// `none` of an `option<ty>`, `ty` being the payload type, as jco
/// represents it: `undefined`, or `{ tag: 'none' }` when the payload is a
/// nullable option (see [`nullable`]).
pub(crate) fn none_to_js(ty: &Type) -> JsValue {
    if nullable(ty) {
        tagged("none", None)
    } else {
        JsValue::UNDEFINED
    }
}

/// A list as the typed array jco uses for numeric elements; `None` for
/// other elements.
fn typed_array(list: &Value, element: WasmTypeKind) -> Option<JsValue> {
    // Each copies the elements once, from a Rust slice.
    macro_rules! typed {
        ($array:ident, $unwrap:ident) => {{
            let items: Vec<_> = list.unwrap_list().map(|v| v.$unwrap()).collect();
            js_sys::$array::from(items.as_slice()).into()
        }};
    }
    Some(match element {
        WasmTypeKind::U8 => typed!(Uint8Array, unwrap_u8),
        WasmTypeKind::S8 => typed!(Int8Array, unwrap_s8),
        WasmTypeKind::U16 => typed!(Uint16Array, unwrap_u16),
        WasmTypeKind::S16 => typed!(Int16Array, unwrap_s16),
        WasmTypeKind::U32 => typed!(Uint32Array, unwrap_u32),
        WasmTypeKind::S32 => typed!(Int32Array, unwrap_s32),
        WasmTypeKind::U64 => typed!(BigUint64Array, unwrap_u64),
        WasmTypeKind::S64 => typed!(BigInt64Array, unwrap_s64),
        WasmTypeKind::F32 => typed!(Float32Array, unwrap_f32),
        WasmTypeKind::F64 => typed!(Float64Array, unwrap_f64),
        _ => return None,
    })
}

fn number(js: &JsValue, what: &str) -> Result<f64, String> {
    js.as_f64()
        .ok_or_else(|| format!("expected a number for {what}"))
}

/// An integer of a 32-bit or narrower type, checked to be in range.
fn integer<T: TryFrom<i64>>(js: &JsValue, what: &str) -> Result<T, String> {
    let n = number(js, what)?;
    if n.fract() != 0.0 {
        return Err(format!("{n} is not an integer ({what})"));
    }
    // Every integer a `f64` holds exactly in this range fits an `i64`.
    #[allow(clippy::cast_possible_truncation)]
    T::try_from(n as i64).map_err(|_| format!("{n} is out of range for {what}"))
}

fn big<T: TryFrom<BigInt>>(js: &JsValue, what: &str) -> Result<T, String> {
    let big = match js.clone().dyn_into::<BigInt>() {
        Ok(big) => big,
        // A plain number is accepted where it is an exact integer.
        Err(js) => {
            let n = number(&js, what)?;
            if n.fract() != 0.0 {
                return Err(format!("{n} is not an integer ({what})"));
            }
            BigInt::new(&JsValue::from_f64(n))
                .map_err(|_| format!("{n} is not an integer ({what})"))?
        }
    };
    T::try_from(big).map_err(|_| format!("the value is out of range for {what}"))
}

/// The elements of an array or array-like `js`, in a fresh array.
fn elements(js: &JsValue) -> Result<Array, String> {
    array_from(js).map_err(|e| format!("not an array: {}", describe(&e)))
}

/// The elements of `js` when it is the typed array jco uses for a list of
/// `element`, copied out at once; `None` for anything else.
fn from_typed_array(js: &JsValue, element: WasmTypeKind) -> Option<Vec<Value>> {
    // `isView` sees through no proxy, so what passes is a typed array
    // whose elements copy without calling back into JavaScript.
    if !ArrayBuffer::is_view(js) {
        return None;
    }
    macro_rules! typed {
        ($array:ident, $make:ident) => {
            js.dyn_ref::<js_sys::$array>()
                .map(|array| array.to_vec().into_iter().map(Value::$make).collect())
        };
    }
    match element {
        WasmTypeKind::U8 => typed!(Uint8Array, make_u8),
        WasmTypeKind::S8 => typed!(Int8Array, make_s8),
        WasmTypeKind::U16 => typed!(Uint16Array, make_u16),
        WasmTypeKind::S16 => typed!(Int16Array, make_s16),
        WasmTypeKind::U32 => typed!(Uint32Array, make_u32),
        WasmTypeKind::S32 => typed!(Int32Array, make_s32),
        WasmTypeKind::U64 => typed!(BigUint64Array, make_u64),
        WasmTypeKind::S64 => typed!(BigInt64Array, make_s64),
        WasmTypeKind::F32 => typed!(Float32Array, make_f32),
        WasmTypeKind::F64 => typed!(Float64Array, make_f64),
        _ => None,
    }
}

/// A JavaScript value, in jco's representation, as a WAVE value of type
/// `ty`.
pub(crate) fn from_js(js: &JsValue, ty: &Type) -> Result<Value, String> {
    let made = |made: Result<Value, _>| made.map_err(|e| format!("{e}"));
    Ok(match ty.kind() {
        WasmTypeKind::Bool => Value::make_bool(js.as_bool().ok_or("expected a boolean")?),
        WasmTypeKind::S8 => Value::make_s8(integer(js, "s8")?),
        WasmTypeKind::U8 => Value::make_u8(integer(js, "u8")?),
        WasmTypeKind::S16 => Value::make_s16(integer(js, "s16")?),
        WasmTypeKind::U16 => Value::make_u16(integer(js, "u16")?),
        WasmTypeKind::S32 => Value::make_s32(integer(js, "s32")?),
        WasmTypeKind::U32 => Value::make_u32(integer(js, "u32")?),
        WasmTypeKind::S64 => Value::make_s64(big(js, "s64")?),
        WasmTypeKind::U64 => Value::make_u64(big(js, "u64")?),
        // Narrowing to `f32` is the conversion itself.
        #[allow(clippy::cast_possible_truncation)]
        WasmTypeKind::F32 => Value::make_f32(number(js, "f32")? as f32),
        WasmTypeKind::F64 => Value::make_f64(number(js, "f64")?),
        WasmTypeKind::Char => {
            let text = js.as_string().ok_or("expected a string for a char")?;
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Value::make_char(c),
                _ => return Err(format!("`{text}` is not one character")),
            }
        }
        WasmTypeKind::String => {
            Value::make_string(Cow::Owned(js.as_string().ok_or("expected a string")?))
        }
        WasmTypeKind::List | WasmTypeKind::FixedLengthList => {
            let element = ty
                .list_element_type()
                .ok_or("a list without an element type")?;
            let items = match from_typed_array(js, element.kind()) {
                Some(items) => items,
                None => {
                    let array = is_array(js).unwrap_or(false);
                    if !array && get(js, "length").is_undefined() {
                        return Err("expected an array".into());
                    }
                    elements(js)?
                        .iter()
                        .enumerate()
                        .map(|(i, item)| {
                            from_js(&item, &element).map_err(|e| format!("item {i}: {e}"))
                        })
                        .collect::<Result<Vec<_>, _>>()?
                }
            };
            made(Value::make_list(ty, items))?
        }
        WasmTypeKind::Record => {
            if !js.is_object() {
                return Err("expected an object".into());
            }
            let (names, values): (Vec<_>, Vec<_>) = ty
                .record_fields()
                .map(|(name, field_type)| {
                    let value = from_js(&field(js, &name), &field_type)
                        .map_err(|e| format!("field `{name}`: {e}"))?;
                    Ok((name, value))
                })
                .collect::<Result<Vec<_>, String>>()?
                .into_iter()
                .unzip();
            made(Value::make_record(
                ty,
                names.iter().map(AsRef::as_ref).zip(values),
            ))?
        }
        WasmTypeKind::Tuple => {
            if !is_array(js).unwrap_or(false) {
                return Err("expected an array for a tuple".into());
            }
            let items = ty
                .tuple_element_types()
                .zip(elements(js)?.iter())
                .map(|(element, item)| from_js(&item, &element))
                .collect::<Result<Vec<_>, _>>()?;
            made(Value::make_tuple(ty, items))?
        }
        WasmTypeKind::Variant => {
            let tag = get(js, "tag")
                .as_string()
                .ok_or("expected `{ tag, val }`")?;
            let (_, payload_type) = ty
                .variant_cases()
                .find(|(name, _)| *name == tag)
                .ok_or_else(|| format!("no case `{tag}`"))?;
            let payload = payload_type
                .map(|ty| from_js(&get(js, "val"), &ty).map_err(|e| format!("case `{tag}`: {e}")))
                .transpose()?;
            made(Value::make_variant(ty, &tag, payload))?
        }
        WasmTypeKind::Enum => {
            let case = js.as_string().ok_or("expected a case name")?;
            made(Value::make_enum(ty, &case))?
        }
        WasmTypeKind::Option => {
            let some = ty.option_some_type().ok_or("an option without a type")?;
            let payload = if nullable(ty) {
                if js.is_undefined() || js.is_null() {
                    None
                } else {
                    Some(from_js(js, &some)?)
                }
            } else {
                // A payload that is a nullable option is tagged, so that
                // `none` and `some(none)` stay apart.
                match get(js, "tag").as_string().as_deref() {
                    Some("none") => None,
                    Some("some") => Some(from_js(&get(js, "val"), &some)?),
                    _ => return Err("expected `{ tag: 'none' | 'some', val }`".into()),
                }
            };
            made(Value::make_option(ty, payload))?
        }
        WasmTypeKind::Result => {
            let (ok, err) = ty.result_types().unwrap_or((None, None));
            let payload = |ty: Option<Type>| ty.map(|ty| from_js(&get(js, "val"), &ty)).transpose();
            let value = match get(js, "tag").as_string().as_deref() {
                Some("ok") => Ok(payload(ok)?),
                Some("err") => Err(payload(err)?),
                _ => return Err("expected `{ tag: 'ok' | 'err', val }`".into()),
            };
            made(Value::make_result(ty, value))?
        }
        WasmTypeKind::Flags => {
            if !js.is_object() {
                return Err("expected an object of booleans".into());
            }
            let names: Vec<Cow<'_, str>> = ty.flags_names().collect();
            let on = names
                .iter()
                .filter(|name| field(js, name).is_truthy())
                .map(AsRef::as_ref);
            made(Value::make_flags(ty, on))?
        }
        other => return Err(format!("a {other} cannot be a port value")),
    })
}

/// A value as WAVE text.
pub(crate) fn to_wave(value: &Value) -> String {
    witgraph_ir::wasm_wave::to_string(value).unwrap_or_default()
}

/// WAVE text as a value of type `ty`. As on wasmtime (and unlike WAVE's
/// own parser), record fields, cases and flags the type does not have are
/// errors, and so are fields and flags given twice.
pub(crate) fn parse_wave(ty: &Type, text: &str) -> Result<Value, String> {
    witgraph_ir::wave::parse(ty, text)
}
