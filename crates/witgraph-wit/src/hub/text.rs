//! The hub's world, as WIT text.
//!
//! Decoded components can't be merged into one `Resolve` (every one is
//! `root:component`), and a hub must import each member's `node` interface
//! under a name of its own. So each interface is printed afresh, with its
//! types copied, structurally: only the resources it names stay what they are,
//! through a `use` of the interface declaring them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use wit_parser::{Function, Handle, Resolve, Type, TypeDefKind, TypeId, TypeOwner};
use witgraph_ir::interface::compat_track;

use super::HubError;

/// The resources the hub's interfaces use, by the semver track of the interface
/// declaring them, at the newest version any member names: the one import they
/// all merge into.
#[derive(Debug, Default, Clone)]
pub(crate) struct Resources {
    /// By track: the newest id, and the resources used from it.
    by_track: BTreeMap<String, (String, BTreeMap<String, ()>)>,
}

impl Resources {
    fn note(&mut self, interface: &str, resource: &str) {
        let entry = self
            .by_track
            .entry(compat_track(interface))
            .or_insert_with(|| (interface.to_string(), BTreeMap::new()));
        if newer(interface, &entry.0) {
            entry.0 = interface.to_string();
        }
        entry.1.insert(resource.to_string(), ());
    }

    /// The id to use for the interface `interface`, as first printed.
    fn chosen(&self, interface: &str) -> String {
        self.by_track
            .get(&compat_track(interface))
            .map_or_else(|| interface.to_string(), |(id, _)| id.clone())
    }

    /// The packages declaring the resources, as nested WIT packages.
    pub(crate) fn packages(&self) -> String {
        let mut out = String::new();
        for (id, resources) in self.by_track.values() {
            let Some((package, interface)) = split_id(id) else {
                continue;
            };
            let _ = writeln!(out, "package {package} {{\n  interface {interface} {{");
            for resource in resources.keys() {
                let _ = writeln!(out, "    resource %{resource};");
            }
            let _ = writeln!(out, "  }}\n}}");
        }
        out
    }
}

/// `namespace:name@version` and `interface` of a full interface id.
fn split_id(id: &str) -> Option<(String, String)> {
    let (package, rest) = id.split_once('/')?;
    match rest.split_once('@') {
        Some((interface, version)) => Some((format!("{package}@{version}"), interface.to_string())),
        None => Some((package.to_string(), rest.to_string())),
    }
}

/// Whether id `a` names a newer version than id `b`, of the same track.
fn newer(a: &str, b: &str) -> bool {
    let version = |id: &str| {
        id.split_once('@')
            .and_then(|(_, version)| semver::Version::parse(version).ok())
    };
    match (version(a), version(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// A WIT identifier, escaped so that no name can be taken for a keyword.
fn id(name: &str) -> String {
    format!("%{name}")
}

/// Prints types of one `Resolve` into one interface.
pub(crate) struct Printer<'a> {
    /// What this printer's names start with, so several can share an interface.
    prefix: String,
    resolve: &'a Resolve,
    resources: &'a mut Resources,
    named: HashMap<TypeId, String>,
    taken: HashSet<String>,
    /// The `use`s and type definitions so far.
    defs: String,
    next: usize,
}

impl<'a> Printer<'a> {
    pub(crate) fn new(prefix: &str, resolve: &'a Resolve, resources: &'a mut Resources) -> Self {
        Self {
            prefix: prefix.to_string(),
            resolve,
            resources,
            named: HashMap::new(),
            taken: HashSet::new(),
            defs: String::new(),
            next: 0,
        }
    }

    /// What has been defined so far, for the interface's body.
    pub(crate) fn defs(&self) -> &str {
        &self.defs
    }

    /// The name a type is defined under here. A member's interface is matched
    /// by its exports' names, so with no prefix a type keeps its own, if it has
    /// one nothing here has taken.
    fn fresh(&mut self, kind: &str, own: Option<&str>) -> String {
        if self.prefix.is_empty()
            && let Some(own) = own
        {
            let name = id(own);
            if !self.taken.contains(&name) {
                self.taken.insert(name.clone());
                return name;
            }
        }
        self.next += 1;
        format!("hub-{}{kind}{}", self.prefix, self.next)
    }

    /// `ty`, as it is written where it is used.
    pub(crate) fn ty(&mut self, ty: &Type) -> Result<String, HubError> {
        Ok(match ty {
            Type::Bool => "bool".into(),
            Type::U8 => "u8".into(),
            Type::U16 => "u16".into(),
            Type::U32 => "u32".into(),
            Type::U64 => "u64".into(),
            Type::S8 => "s8".into(),
            Type::S16 => "s16".into(),
            Type::S32 => "s32".into(),
            Type::S64 => "s64".into(),
            Type::F32 => "f32".into(),
            Type::F64 => "f64".into(),
            Type::Char => "char".into(),
            Type::String => "string".into(),
            Type::ErrorContext => {
                return Err(HubError::Unsupported("an `error-context` port".into()));
            }
            Type::Id(id) => self.def(*id)?,
        })
    }

    fn optional(&mut self, ty: Option<&Type>) -> Result<String, HubError> {
        match ty {
            Some(ty) => self.ty(ty),
            None => Ok("_".into()),
        }
    }

    fn def(&mut self, id_: TypeId) -> Result<String, HubError> {
        if let Some(name) = self.named.get(&id_) {
            return Ok(name.clone());
        }
        let resolve = self.resolve;
        let def = &resolve.types[id_];
        let text = match &def.kind {
            TypeDefKind::Type(inner) => return self.ty(inner),
            TypeDefKind::Option(inner) => format!("option<{}>", self.ty(inner)?),
            TypeDefKind::Result(result) => match (&result.ok, &result.err) {
                (None, None) => "result".to_string(),
                (Some(ok), None) => format!("result<{}>", self.ty(ok)?),
                (ok, err) => format!(
                    "result<{}, {}>",
                    self.optional(ok.as_ref())?,
                    self.optional(err.as_ref())?
                ),
            },
            TypeDefKind::List(inner) => format!("list<{}>", self.ty(inner)?),
            TypeDefKind::Tuple(tuple) => {
                let types = tuple
                    .types
                    .iter()
                    .map(|ty| self.ty(ty))
                    .collect::<Result<Vec<_>, _>>()?;
                format!("tuple<{}>", types.join(", "))
            }
            TypeDefKind::Stream(inner) => match inner {
                Some(inner) => format!("stream<{}>", self.ty(inner)?),
                None => "stream".to_string(),
            },
            TypeDefKind::Future(inner) => match inner {
                Some(inner) => format!("future<{}>", self.ty(inner)?),
                None => "future".to_string(),
            },
            TypeDefKind::Handle(Handle::Own(resource)) => {
                return Ok(format!("own<{}>", self.resource(*resource)?));
            }
            TypeDefKind::Handle(Handle::Borrow(resource)) => {
                return Ok(format!("borrow<{}>", self.resource(*resource)?));
            }
            TypeDefKind::Resource => return self.resource(id_),
            TypeDefKind::Record(record) => {
                let name = self.fresh("t", def.name.as_deref());
                self.named.insert(id_, name.clone());
                let mut body = String::new();
                for field in &record.fields {
                    let _ = write!(body, "{}: {}, ", id(&field.name), self.ty(&field.ty)?);
                }
                let _ = writeln!(self.defs, "record {name} {{ {body} }}");
                return Ok(name);
            }
            TypeDefKind::Variant(variant) => {
                let name = self.fresh("t", def.name.as_deref());
                self.named.insert(id_, name.clone());
                let mut body = String::new();
                for case in &variant.cases {
                    match &case.ty {
                        Some(ty) => {
                            let _ = write!(body, "{}({}), ", id(&case.name), self.ty(ty)?);
                        }
                        None => {
                            let _ = write!(body, "{}, ", id(&case.name));
                        }
                    }
                }
                let _ = writeln!(self.defs, "variant {name} {{ {body} }}");
                return Ok(name);
            }
            TypeDefKind::Enum(enum_) => {
                let name = self.fresh("t", def.name.as_deref());
                self.named.insert(id_, name.clone());
                let cases: Vec<String> = enum_.cases.iter().map(|c| id(&c.name)).collect();
                let _ = writeln!(self.defs, "enum {name} {{ {} }}", cases.join(", "));
                return Ok(name);
            }
            TypeDefKind::Flags(flags) => {
                let name = self.fresh("t", def.name.as_deref());
                self.named.insert(id_, name.clone());
                let names: Vec<String> = flags.flags.iter().map(|f| id(&f.name)).collect();
                let _ = writeln!(self.defs, "flags {name} {{ {} }}", names.join(", "));
                return Ok(name);
            }
            other => {
                return Err(HubError::Unsupported(format!(
                    "a port of type {}",
                    other.as_str()
                )));
            }
        };
        Ok(text)
    }

    /// The local name of a resource: `use`d from the interface declaring it.
    pub(crate) fn resource_name(&mut self, id_: TypeId) -> Result<String, HubError> {
        self.resource(id_)
    }

    fn resource(&mut self, id_: TypeId) -> Result<String, HubError> {
        if let Some(name) = self.named.get(&id_) {
            return Ok(name.clone());
        }
        let resolve = self.resolve;
        let local = resolve.types[id_].name.clone();
        let mut def = &resolve.types[id_];
        let mut at = id_;
        while let TypeDefKind::Type(Type::Id(inner)) = def.kind {
            at = inner;
            def = &resolve.types[inner];
        }
        if let Some(name) = self.named.get(&at) {
            let name = name.clone();
            self.named.insert(id_, name.clone());
            return Ok(name);
        }
        let TypeOwner::Interface(owner) = def.owner else {
            return Err(HubError::Unsupported(
                "a handle of a resource no named interface declares".into(),
            ));
        };
        let interface = resolve
            .id_of(owner)
            .ok_or_else(|| HubError::Unsupported("a handle of an unnamed interface".into()))?;
        let resource = def
            .name
            .clone()
            .ok_or_else(|| HubError::Unsupported("a resource with no name".into()))?;
        self.resources.note(&interface, &resource);
        let name = self.fresh("r", local.as_deref());
        let chosen = self.resources.chosen(&interface);
        let _ = writeln!(self.defs, "use {chosen}.{{{} as {name}}};", id(&resource));
        self.named.insert(id_, name.clone());
        self.named.insert(at, name.clone());
        Ok(name)
    }

    /// `func`, as a member's `run` is declared, under its own parameter name.
    pub(crate) fn run(&mut self, func: &Function) -> Result<String, HubError> {
        let mut params = Vec::new();
        for param in &func.params {
            params.push(format!("{}: {}", id(&param.name), self.ty(&param.ty)?));
        }
        let result = match &func.result {
            Some(ty) => format!(" -> {}", self.ty(ty)?),
            None => String::new(),
        };
        Ok(format!("run: async func({}){result};", params.join(", ")))
    }
}
