//! Values of the JavaScript executor, and their jco representation.

use std::borrow::Cow;
use std::cell::{OnceCell, RefCell};
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
    Data(Rc<Data>),
    /// A stream or future, as its producer returned it. Two handles are
    /// equal only when they are the same object.
    Handle(JsValue),
    /// What a handle port carries: an object the host provides, which the
    /// scheduler, its readers and the live view share. Equal only to
    /// itself.
    Resource(Rc<Resource>),
}

/// An object a handle port carries, as its maker returned it: the host's
/// (whatever it holds, a buffer on a device, say) until no latch or view
/// holds it any more, when its `[Symbol.dispose]()` is called, if it has
/// one, so what it holds is freed then rather than when it is collected.
#[derive(Debug)]
pub(crate) struct Resource(JsValue);

impl Resource {
    pub(crate) fn new(js: JsValue) -> Self {
        Self(js)
    }

    pub(crate) fn js(&self) -> &JsValue {
        &self.0
    }
}

impl Drop for Resource {
    fn drop(&mut self) {
        // `Symbol.dispose`, where the engine has it; else the symbol jco
        // falls back to.
        let symbol = js_sys::Reflect::get(&js_sys::global(), &"Symbol".into())
            .and_then(|symbol| js_sys::Reflect::get(&symbol, &"dispose".into()))
            .ok()
            .filter(JsValue::is_symbol)
            .unwrap_or_else(|| js_sys::Symbol::for_("dispose").into());
        if let Ok(dispose) = js_sys::Reflect::get(&self.0, &symbol)
            && let Some(dispose) = dispose.dyn_ref::<js_sys::Function>()
        {
            // Best effort: a provider's failure to free is its own.
            let _ = dispose.call0(&self.0);
        }
    }
}

/// Values compare as on wasmtime: floats by their bits, except that every
/// NaN equals every other (so `0.0` and `-0.0` differ).
impl PartialEq for WebValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Data(a), Self::Data(b)) => Rc::ptr_eq(a, b) || a.equals(b),
            (Self::Handle(a), Self::Handle(b)) => a == b,
            (Self::Resource(a), Self::Resource(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// A Value port's value of type `ty`, in the forms it has been needed in:
/// as jco represents it, as a WAVE value, or both, each made from the other
/// the first time it is needed.
///
/// A guest's output stays as jco handed it over: passed on to its
/// consumers, compared and read as it is, so moving it between nodes
/// converts nothing. Only WAVE text, snapshots and type checks need the
/// WAVE value, and convert it once. The JavaScript value is shared, by
/// every consumer and every read: nothing may change it.
#[derive(Debug)]
pub(crate) struct Data {
    ty: Type,
    js: OnceCell<JsValue>,
    value: OnceCell<Value>,
}

impl Data {
    /// `value`, of type `ty`.
    pub(crate) fn from_value(value: Value, ty: Type) -> Self {
        Self {
            ty,
            js: OnceCell::new(),
            value: OnceCell::from(value),
        }
    }

    /// What a guest returned for a port of type `ty`: kept as it is when it
    /// is exactly what jco makes of a value of `ty` (see [`exact`]),
    /// converted otherwise (a mock's output, say), which checks it and
    /// puts it in that form.
    pub(crate) fn from_output(js: JsValue, ty: Type) -> Result<Self, String> {
        if exact(&js, &ty) {
            Ok(Self {
                ty,
                js: OnceCell::from(js),
                value: OnceCell::new(),
            })
        } else {
            Ok(Self::from_value(from_js(&js, &ty)?, ty))
        }
    }

    /// The value as jco represents it.
    pub(crate) fn js(&self) -> &JsValue {
        self.js.get_or_init(|| match self.value.get() {
            Some(value) => to_js(value, &self.ty),
            // Every `Data` is made with one form or the other.
            None => JsValue::UNDEFINED,
        })
    }

    /// The value as a WAVE value. Converting an exact JavaScript value
    /// cannot fail; the error is for what should not happen.
    pub(crate) fn value(&self) -> Result<&Value, String> {
        if let Some(value) = self.value.get() {
            return Ok(value);
        }
        let js = self.js.get().ok_or("a value with no form")?;
        affordable(js, &self.ty)?;
        let value = from_js(js, &self.ty)?;
        Ok(self.value.get_or_init(|| value))
    }

    /// The value as WAVE text. A value that has no WAVE value yet is
    /// converted for the text alone, and the WAVE value dropped again: it
    /// can take many times the value's own size (a `list<u8>` takes tens of
    /// bytes a byte), so only what needs it to stay (a type check) keeps it.
    pub(crate) fn wave(&self) -> Result<String, String> {
        match self.value.get() {
            Some(value) => Ok(to_wave(value)),
            None => {
                let js = self.js.get().ok_or("a value with no form")?;
                affordable(js, &self.ty)?;
                Ok(to_wave(&from_js(js, &self.ty)?))
            }
        }
    }

    /// Whether `self` and `other` are the same value, as [`WebValue`]'s
    /// equality has it: compared as JavaScript values when both are, so a
    /// guest's outputs compare without converting.
    fn equals(&self, other: &Self) -> bool {
        match (self.js.get(), other.js.get()) {
            (Some(a), Some(b)) => same_js(a, b, &self.ty),
            _ => match (self.value(), other.value()) {
                (Ok(a), Ok(b)) => same(a, b),
                _ => false,
            },
        }
    }

    /// For an option, what its `some` holds (as a value of the payload
    /// type), or `None` for `none`; `Err` for anything else.
    pub(crate) fn option_payload(&self) -> Result<Option<Self>, ()> {
        let Some(some) = self.ty.option_some_type() else {
            return Err(());
        };
        if let Some(value) = self.value.get() {
            return Ok(value
                .unwrap_option()
                .map(|payload| Self::from_value(payload.into_owned(), some)));
        }
        let js = self.js();
        let payload = if nullable(&self.ty) {
            (!js.is_undefined() && !js.is_null()).then(|| js.clone())
        } else {
            match get(js, "tag").as_string().as_deref() {
                Some("some") => Some(get(js, "val")),
                _ => None,
            }
        };
        Ok(payload.map(|js| Self {
            ty: some,
            js: OnceCell::from(js),
            value: OnceCell::new(),
        }))
    }
}

/// The most bytes converting one value to a WAVE value, and that to text,
/// may take. A conversion that does not fit the module's memory traps, and
/// a trap leaves the graph unusable (it skips every `Drop`, a borrow of the
/// graph's among them): one that would not fit is refused instead, with an
/// error. A 32-bit memory (4 GiB, shared with everything else) affords a
/// quarter of it; a 64-bit one, as much as a host's memory reasonably holds.
#[cfg(target_pointer_width = "32")]
const CONVERSION_BUDGET: usize = 1 << 30;
#[cfg(not(target_pointer_width = "32"))]
const CONVERSION_BUDGET: usize = 1 << 34;

/// `Ok` when converting `js`, of type `ty`, fits the
/// [budget](CONVERSION_BUDGET); otherwise how much it would take.
fn affordable(js: &JsValue, ty: &Type) -> Result<(), String> {
    let cost = conversion_cost(js, ty, CONVERSION_BUDGET);
    if cost <= CONVERSION_BUDGET {
        return Ok(());
    }
    let mib = |bytes: usize| bytes >> 20;
    Err(format!(
        "too large to convert: it would take over {} MiB as a WAVE value (at most {} MiB)",
        mib(cost),
        mib(CONVERSION_BUDGET)
    ))
}

/// About how many bytes converting `js`, of type `ty`, to a WAVE value and
/// to text takes: a `Value` and a few characters per element, and its
/// strings' bytes. Counting stops once past `limit`.
fn conversion_cost(js: &JsValue, ty: &Type, limit: usize) -> usize {
    // A `Value`, its slot in its parent, and its text.
    let node = std::mem::size_of::<Value>() + 16;
    match ty.kind() {
        WasmTypeKind::String | WasmTypeKind::Char => {
            node + js.as_string().map_or(0, |text| text.len() * 3)
        }
        WasmTypeKind::List | WasmTypeKind::FixedLengthList => {
            let Some(element) = ty.list_element_type() else {
                return node;
            };
            if numeric(element.kind()) && ArrayBuffer::is_view(js) {
                let length = get(js, "length").as_f64().unwrap_or(0.0);
                // A typed array's length fits a `usize` on any host it has.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                return node.saturating_add((length as usize).saturating_mul(node));
            }
            let mut cost = node;
            for item in js.dyn_ref::<Array>().into_iter().flat_map(Array::iter) {
                cost = cost.saturating_add(conversion_cost(&item, &element, limit));
                if cost > limit {
                    break;
                }
            }
            cost
        }
        WasmTypeKind::Record => {
            let mut cost = node;
            for (name, field_type) in ty.record_fields() {
                cost = cost.saturating_add(conversion_cost(&field(js, &name), &field_type, limit));
                if cost > limit {
                    break;
                }
            }
            cost
        }
        WasmTypeKind::Tuple => {
            let items = js.dyn_ref::<Array>().map(Array::to_vec).unwrap_or_default();
            ty.tuple_element_types()
                .zip(items)
                .fold(node, |cost, (t, item)| {
                    cost.saturating_add(conversion_cost(&item, &t, limit))
                })
        }
        WasmTypeKind::Variant | WasmTypeKind::Result => {
            let tag = get(js, "tag").as_string();
            let payload = match ty.kind() {
                WasmTypeKind::Variant => ty
                    .variant_cases()
                    .find(|(name, _)| Some(name.as_ref()) == tag.as_deref())
                    .and_then(|(_, t)| t),
                _ => {
                    let (ok, err) = ty.result_types().unwrap_or((None, None));
                    if tag.as_deref() == Some("ok") {
                        ok
                    } else {
                        err
                    }
                }
            };
            node + payload.map_or(0, |t| conversion_cost(&get(js, "val"), &t, limit))
        }
        WasmTypeKind::Option => {
            let Some(some) = ty.option_some_type() else {
                return node;
            };
            let payload = if nullable(ty) {
                js.clone()
            } else {
                get(js, "val")
            };
            if payload.is_undefined() || payload.is_null() {
                node
            } else {
                node + conversion_cost(&payload, &some, limit)
            }
        }
        _ => node,
    }
}

/// The value of an optional input whose payload is `data`, of type `ty`,
/// as jco represents `some` of it (see [`some_to_js`]).
pub(crate) fn some_js(data: &Data, ty: &Type) -> JsValue {
    if nullable(ty) {
        tagged("some", Some(data.js().clone()))
    } else {
        data.js().clone()
    }
}

/// Whether jco hands over a list of `element` as a typed array.
fn numeric(element: WasmTypeKind) -> bool {
    matches!(
        element,
        WasmTypeKind::U8
            | WasmTypeKind::S8
            | WasmTypeKind::U16
            | WasmTypeKind::S16
            | WasmTypeKind::U32
            | WasmTypeKind::S32
            | WasmTypeKind::U64
            | WasmTypeKind::S64
            | WasmTypeKind::F32
            | WasmTypeKind::F64
    )
}

/// The typed array class jco uses for a list of `element`, as a check of
/// `js` and its bytes; `None` when it uses a plain array.
macro_rules! typed_bytes {
    ($js:expr, $element:expr) => {{
        macro_rules! view {
            ($array:ident) => {
                $js.dyn_ref::<js_sys::$array>().map(|a| {
                    js_sys::Uint8Array::new_with_byte_offset_and_length(
                        &a.buffer(),
                        a.byte_offset(),
                        a.byte_length(),
                    )
                })
            };
        }
        match $element {
            WasmTypeKind::U8 => Some(view!(Uint8Array)),
            WasmTypeKind::S8 => Some(view!(Int8Array)),
            WasmTypeKind::U16 => Some(view!(Uint16Array)),
            WasmTypeKind::S16 => Some(view!(Int16Array)),
            WasmTypeKind::U32 => Some(view!(Uint32Array)),
            WasmTypeKind::S32 => Some(view!(Int32Array)),
            WasmTypeKind::U64 => Some(view!(BigUint64Array)),
            WasmTypeKind::S64 => Some(view!(BigInt64Array)),
            WasmTypeKind::F32 => Some(view!(Float32Array)),
            WasmTypeKind::F64 => Some(view!(Float64Array)),
            _ => None,
        }
    }};
}

/// Whether `js` is exactly what jco makes of a value of type `ty`, as a
/// guest returns one: every integer of 32 bits or fewer a number in range,
/// every 64-bit one a `BigInt`, every list of numbers the typed array jco
/// uses, `none` `undefined` (or tagged), and so on. Such a value converts
/// to a WAVE value without error, and can be handed to another guest as it
/// is. Anything else is `false`, even a value [`from_js`] would accept.
pub(crate) fn exact(js: &JsValue, ty: &Type) -> bool {
    let integer = |min: f64, max: f64| {
        js.as_f64()
            .is_some_and(|n| n.fract() == 0.0 && (min..=max).contains(&n))
    };
    match ty.kind() {
        WasmTypeKind::Bool => js.as_bool().is_some(),
        WasmTypeKind::S8 => integer(i8::MIN.into(), i8::MAX.into()),
        WasmTypeKind::U8 => integer(0.0, u8::MAX.into()),
        WasmTypeKind::S16 => integer(i16::MIN.into(), i16::MAX.into()),
        WasmTypeKind::U16 => integer(0.0, u16::MAX.into()),
        WasmTypeKind::S32 => integer(i32::MIN.into(), i32::MAX.into()),
        WasmTypeKind::U32 => integer(0.0, u32::MAX.into()),
        WasmTypeKind::S64 => js
            .dyn_ref::<BigInt>()
            .is_some_and(|b| i64::try_from(b.clone()).is_ok()),
        WasmTypeKind::U64 => js
            .dyn_ref::<BigInt>()
            .is_some_and(|b| u64::try_from(b.clone()).is_ok()),
        WasmTypeKind::F32 | WasmTypeKind::F64 => js.as_f64().is_some(),
        WasmTypeKind::Char => js.as_string().is_some_and(|t| t.chars().count() == 1),
        WasmTypeKind::String => js.is_string(),
        WasmTypeKind::List | WasmTypeKind::FixedLengthList => {
            let Some(element) = ty.list_element_type() else {
                return false;
            };
            if numeric(element.kind()) {
                // `isView` first: it sees through no proxy, so what passes
                // is a typed array whose class is checked without calling
                // back into JavaScript.
                return ArrayBuffer::is_view(js)
                    && typed_bytes!(js, element.kind()).flatten().is_some();
            }
            is_array(js).unwrap_or(false)
                && js
                    .dyn_ref::<Array>()
                    .is_some_and(|items| items.iter().all(|item| exact(&item, &element)))
        }
        WasmTypeKind::Record => {
            js.is_object()
                && ty
                    .record_fields()
                    .all(|(name, field_type)| exact(&field(js, &name), &field_type))
        }
        WasmTypeKind::Tuple => {
            let types: Vec<Type> = ty.tuple_element_types().collect();
            is_array(js).unwrap_or(false)
                && js.dyn_ref::<Array>().is_some_and(|items| {
                    items.length() as usize == types.len()
                        && types
                            .iter()
                            .zip(items.iter())
                            .all(|(t, item)| exact(&item, t))
                })
        }
        WasmTypeKind::Variant => {
            let Some(tag) = get(js, "tag").as_string() else {
                return false;
            };
            ty.variant_cases()
                .find(|(name, _)| *name == tag)
                .is_some_and(|(_, payload)| payload.is_none_or(|t| exact(&get(js, "val"), &t)))
        }
        WasmTypeKind::Enum => js
            .as_string()
            .is_some_and(|case| ty.enum_cases().any(|name| name == case)),
        WasmTypeKind::Option => {
            let Some(some) = ty.option_some_type() else {
                return false;
            };
            if nullable(ty) {
                js.is_undefined() || exact(js, &some)
            } else {
                match get(js, "tag").as_string().as_deref() {
                    Some("none") => true,
                    Some("some") => exact(&get(js, "val"), &some),
                    _ => false,
                }
            }
        }
        WasmTypeKind::Result => {
            let (ok, err) = ty.result_types().unwrap_or((None, None));
            let payload = match get(js, "tag").as_string().as_deref() {
                Some("ok") => ok,
                Some("err") => err,
                _ => return false,
            };
            payload.is_none_or(|t| exact(&get(js, "val"), &t))
        }
        WasmTypeKind::Flags => {
            js.is_object()
                && ty
                    .flags_names()
                    .all(|name| field(js, &name).as_bool().is_some())
        }
        _ => false,
    }
}

/// Whether two exact values of type `ty` (see [`exact`]) are the same, as
/// [`WebValue`]'s equality has it: floats by their bits, every NaN equal to
/// every other. Typed arrays compare by their bytes.
fn same_js(a: &JsValue, b: &JsValue, ty: &Type) -> bool {
    let float = |x: f64, y: f64| (x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits();
    match ty.kind() {
        // Narrowing to `f32` is the comparison itself.
        #[allow(clippy::cast_possible_truncation)]
        WasmTypeKind::F32 => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => {
                let (x, y) = (x as f32, y as f32);
                (x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits()
            }
            _ => false,
        },
        WasmTypeKind::F64 => a.as_f64().zip(b.as_f64()).is_some_and(|(x, y)| float(x, y)),
        WasmTypeKind::List | WasmTypeKind::FixedLengthList => {
            let Some(element) = ty.list_element_type() else {
                return false;
            };
            if let (Some(Some(x)), Some(Some(y))) = (
                typed_bytes!(a, element.kind()),
                typed_bytes!(b, element.kind()),
            ) {
                if x.length() != y.length() {
                    return false;
                }
                let (x, y) = (x.to_vec(), y.to_vec());
                if x == y {
                    return true;
                }
                // Unequal bytes are unequal values, but for NaNs.
                return match element.kind() {
                    WasmTypeKind::F32 => {
                        x.as_chunks::<4>()
                            .0
                            .iter()
                            .zip(y.as_chunks::<4>().0)
                            .all(|(p, q)| {
                                let (p, q) = (f32::from_le_bytes(*p), f32::from_le_bytes(*q));
                                (p.is_nan() && q.is_nan()) || p.to_bits() == q.to_bits()
                            })
                    }
                    WasmTypeKind::F64 => x
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .zip(y.as_chunks::<8>().0)
                        .all(|(p, q)| float(f64::from_le_bytes(*p), f64::from_le_bytes(*q))),
                    _ => false,
                };
            }
            match (a.dyn_ref::<Array>(), b.dyn_ref::<Array>()) {
                (Some(x), Some(y)) => {
                    x.length() == y.length()
                        && x.iter()
                            .zip(y.iter())
                            .all(|(p, q)| same_js(&p, &q, &element))
                }
                _ => false,
            }
        }
        WasmTypeKind::Record => ty
            .record_fields()
            .all(|(name, t)| same_js(&field(a, &name), &field(b, &name), &t)),
        WasmTypeKind::Tuple => match (a.dyn_ref::<Array>(), b.dyn_ref::<Array>()) {
            (Some(x), Some(y)) => ty
                .tuple_element_types()
                .zip(x.iter().zip(y.iter()))
                .all(|(t, (p, q))| same_js(&p, &q, &t)),
            _ => false,
        },
        WasmTypeKind::Variant => {
            let tag = get(a, "tag").as_string();
            tag.is_some()
                && tag == get(b, "tag").as_string()
                && ty
                    .variant_cases()
                    .find(|(name, _)| Some(name.as_ref()) == tag.as_deref())
                    .is_some_and(|(_, payload)| {
                        payload.is_none_or(|t| same_js(&get(a, "val"), &get(b, "val"), &t))
                    })
        }
        WasmTypeKind::Option => {
            let Some(some) = ty.option_some_type() else {
                return false;
            };
            if nullable(ty) {
                match (a.is_undefined(), b.is_undefined()) {
                    (true, true) => true,
                    (false, false) => same_js(a, b, &some),
                    _ => false,
                }
            } else {
                let tag = get(a, "tag").as_string();
                tag == get(b, "tag").as_string()
                    && (tag.as_deref() != Some("some")
                        || same_js(&get(a, "val"), &get(b, "val"), &some))
            }
        }
        WasmTypeKind::Result => {
            let (ok, err) = ty.result_types().unwrap_or((None, None));
            let tag = get(a, "tag").as_string();
            if tag.is_none() || tag != get(b, "tag").as_string() {
                return false;
            }
            let payload = if tag.as_deref() == Some("ok") {
                ok
            } else {
                err
            };
            payload.is_none_or(|t| same_js(&get(a, "val"), &get(b, "val"), &t))
        }
        WasmTypeKind::Flags => ty
            .flags_names()
            .all(|name| field(a, &name).as_bool() == field(b, &name).as_bool()),
        // Numbers, `BigInt`s, booleans and strings: equal as JavaScript has
        // them (`===`).
        _ => a == b,
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
