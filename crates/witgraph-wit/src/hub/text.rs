//! The hub's world, as WIT text.
//!
//! Members' WIT sources can't be merged into one `Resolve` (two may well
//! declare one package otherwise), and a hub must import each member's
//! `node` interface under a name of its own. So each interface is printed afresh, with its
//! types copied, structurally: only the resources it names stay what they are,
//! through a `use` of the interface declaring them.
//!
//! A member's instance exports the types its interface names, its own and those
//! it `use`s, and no others: the hub's import of it must name no others either.
//! The types of other interfaces it reaches (a field's type, of a type it `use`s)
//! are printed into an interface of the hub's own for each, `hub-types<n>`, once
//! for every member, a definition once however many name it, and `use`d from
//! there. Holding no resource, those are no imports of the hub.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use wit_parser::{Function, Handle, InterfaceId, Resolve, Type, TypeDefKind, TypeId, TypeOwner};
use witgraph_ir::interface::compat_track;

use super::HubError;

/// The resources the hub's interfaces use, by the semver track of the interface
/// declaring them, at the newest version any member names: the one import they
/// all merge into.
#[derive(Debug, Default, Clone)]
struct Resources {
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
    fn packages(&self) -> String {
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

/// What the printers of one hub share: the resources its interfaces use, and the
/// types of other interfaces than members' they reach.
#[derive(Debug, Default, Clone)]
pub(crate) struct Shared {
    resources: Resources,
    /// By the semver track of the interface declaring them.
    types: BTreeMap<String, Types>,
}

impl Shared {
    /// What [`Printer`]s of every member, printing with this, need to know
    /// before printing: the newest version of each resource's interface.
    pub(crate) fn learnt(&self) -> Self {
        Self {
            resources: self.resources.clone(),
            types: BTreeMap::new(),
        }
    }

    /// The packages declaring the resources, as nested WIT packages.
    pub(crate) fn packages(&self) -> String {
        self.resources.packages()
    }

    /// The hub's interfaces of other interfaces' types.
    pub(crate) fn interfaces(&self) -> String {
        let mut out = String::new();
        for types in self.types.values() {
            let _ = writeln!(
                out,
                "/// Types of `{}`.\ninterface {} {{\n{}}}",
                types.of, types.name, types.defs
            );
        }
        out
    }

    fn types(&mut self, interface: &str) -> &mut Types {
        let count = self.types.len();
        self.types
            .entry(compat_track(interface))
            .or_insert_with(|| Types {
                name: format!("hub-types{}", count + 1),
                of: interface.to_string(),
                ..Types::default()
            })
    }
}

/// An interface of the hub's, holding types of another.
#[derive(Debug, Default, Clone)]
struct Types {
    /// Its name in the hub's package.
    name: String,
    /// The interface whose types it holds, as first met.
    of: String,
    taken: HashSet<String>,
    /// The name of each definition, by what it defines: one type, however
    /// many members name it.
    defined: HashMap<String, String>,
    /// The name of each type `use`d into it, by where it is from.
    used: HashMap<String, String>,
    defs: String,
    next: usize,
}

impl Types {
    /// A name for a type here: its own, if nothing here has taken it.
    fn fresh(&mut self, own: Option<&str>) -> String {
        if let Some(own) = own {
            let name = id(own);
            if self.taken.insert(name.clone()) {
                return name;
            }
        }
        self.next += 1;
        format!("hub-t{}", self.next)
    }

    /// The local name of `name` of `from`, `use`d here.
    fn use_(&mut self, from: &str, name: &str, own: Option<&str>) -> String {
        let key = format!("{from}.{name}");
        if let Some(local) = self.used.get(&key) {
            return local.clone();
        }
        let local = self.fresh(own);
        let _ = writeln!(self.defs, "use {from}.{{{name} as {local}}};");
        self.used.insert(key, local.clone());
        local
    }
}

/// Prints types of one `Resolve` into one interface.
pub(crate) struct Printer<'a> {
    /// What this printer's names start with, so several can share an interface.
    prefix: String,
    resolve: &'a Resolve,
    shared: &'a mut Shared,
    named: HashMap<TypeId, String>,
    taken: HashSet<String>,
    /// The `use`s and type definitions so far.
    defs: String,
    next: usize,
    /// The member interface printed, whose types of other interfaces go to
    /// the hub's interfaces of them.
    member: Option<InterfaceId>,
    /// The track of the hub's interface of types printed into now, if not
    /// this printer's own.
    into: Option<String>,
    /// Each type of another interface: the track of the hub's interface
    /// holding it, and its name there.
    elsewhere: HashMap<TypeId, (String, String)>,
}

/// A type's definition, but for its name: `record`, say, and its body.
struct Definition {
    keyword: &'static str,
    body: String,
}

impl<'a> Printer<'a> {
    pub(crate) fn new(prefix: &str, resolve: &'a Resolve, shared: &'a mut Shared) -> Self {
        Self {
            prefix: prefix.to_string(),
            resolve,
            shared,
            named: HashMap::new(),
            taken: HashSet::new(),
            defs: String::new(),
            next: 0,
            member: None,
            into: None,
            elsewhere: HashMap::new(),
        }
    }

    /// A printer of the member interface `own`.
    pub(crate) fn member(resolve: &'a Resolve, shared: &'a mut Shared, own: InterfaceId) -> Self {
        let mut printer = Self::new("", resolve, shared);
        printer.member = Some(own);
        printer
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

    /// The interface declaring `id_`, if it is a named type a member's
    /// interface is to `use` from the hub's interface of it.
    fn elsewhere_of(&self, id_: TypeId) -> Option<String> {
        let member = self.member?;
        let def = &self.resolve.types[id_];
        let TypeOwner::Interface(owner) = def.owner else {
            return None;
        };
        let named = matches!(
            def.kind,
            TypeDefKind::Record(_)
                | TypeDefKind::Variant(_)
                | TypeDefKind::Enum(_)
                | TypeDefKind::Flags(_)
        );
        if owner == member || def.name.is_none() || !named {
            return None;
        }
        self.resolve.id_of(owner)
    }

    /// The type of another interface an alias of the member's (a `use`, say)
    /// stands for, if there is one.
    fn aliased_elsewhere(&self, id_: TypeId) -> Option<TypeId> {
        let member = self.member?;
        let def = &self.resolve.types[id_];
        if self.into.is_some() || def.owner != TypeOwner::Interface(member) {
            return None;
        }
        let mut at = id_;
        while let TypeDefKind::Type(Type::Id(inner)) = self.resolve.types[at].kind {
            at = inner;
        }
        (at != id_ && self.elsewhere_of(at).is_some()).then_some(at)
    }

    fn def(&mut self, id_: TypeId) -> Result<String, HubError> {
        if self.into.is_none()
            && let Some(name) = self.named.get(&id_)
        {
            return Ok(name.clone());
        }
        let resolve = self.resolve;
        let def = &resolve.types[id_];
        if let Some(target) = self.aliased_elsewhere(id_) {
            return self.use_elsewhere(id_, target, def.name.as_deref());
        }
        if self.elsewhere_of(id_).is_some() {
            return self.use_elsewhere(id_, id_, None);
        }
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
            TypeDefKind::Record(_)
            | TypeDefKind::Variant(_)
            | TypeDefKind::Enum(_)
            | TypeDefKind::Flags(_) => {
                let Definition { keyword, body } = self.definition(id_)?;
                let name = self.fresh("t", def.name.as_deref());
                self.named.insert(id_, name.clone());
                let _ = writeln!(self.defs, "{keyword} {name} {{ {body} }}");
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

    /// The definition of a named type, but for its name, its types written
    /// where it is printed.
    fn definition(&mut self, id_: TypeId) -> Result<Definition, HubError> {
        let resolve = self.resolve;
        let mut body = String::new();
        let keyword = match &resolve.types[id_].kind {
            TypeDefKind::Record(record) => {
                for field in &record.fields {
                    let _ = write!(body, "{}: {}, ", id(&field.name), self.ty(&field.ty)?);
                }
                "record"
            }
            TypeDefKind::Variant(variant) => {
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
                "variant"
            }
            TypeDefKind::Enum(enum_) => {
                let cases: Vec<String> = enum_.cases.iter().map(|c| id(&c.name)).collect();
                body = cases.join(", ");
                "enum"
            }
            TypeDefKind::Flags(flags) => {
                let names: Vec<String> = flags.flags.iter().map(|f| id(&f.name)).collect();
                body = names.join(", ");
                "flags"
            }
            other => {
                return Err(HubError::Unsupported(format!(
                    "a named type of kind {}",
                    other.as_str()
                )));
            }
        };
        Ok(Definition { keyword, body })
    }

    /// `target`, a type of another interface, defined in the hub's interface
    /// of that one, and its name where it is printed: a `use` of it under
    /// `local` (or its own name), unless it is printed there.
    fn use_elsewhere(
        &mut self,
        id_: TypeId,
        target: TypeId,
        local: Option<&str>,
    ) -> Result<String, HubError> {
        let (track, name) = self.define_elsewhere(target)?;
        let own = local.or(self.resolve.types[target].name.as_deref());
        if self.into.as_ref() == Some(&track) {
            return Ok(name);
        }
        let from = self.shared.types[&track].name.clone();
        if let Some(into) = self.into.clone() {
            let types = self.shared.types.get_mut(&into).ok_or_else(missing)?;
            return Ok(types.use_(&from, &name, own));
        }
        let local = self.fresh("t", own);
        let _ = writeln!(self.defs, "use {from}.{{{name} as {local}}};");
        self.named.insert(id_, local.clone());
        Ok(local)
    }

    /// Defines `id_`, of another interface, in the hub's interface of that
    /// one, once: its track, and its name there.
    fn define_elsewhere(&mut self, id_: TypeId) -> Result<(String, String), HubError> {
        if let Some(found) = self.elsewhere.get(&id_) {
            return Ok(found.clone());
        }
        let interface = self.elsewhere_of(id_).ok_or_else(missing)?;
        let track = compat_track(&interface);
        self.shared.types(&interface);
        let outer = self.into.replace(track.clone());
        let definition = self.definition(id_);
        self.into = outer;
        let Definition { keyword, body } = definition?;
        let types = self.shared.types.get_mut(&track).ok_or_else(missing)?;
        let key = format!("{keyword} {body}");
        let name = match types.defined.get(&key) {
            Some(name) => name.clone(),
            None => {
                let name = types.fresh(self.resolve.types[id_].name.as_deref());
                let _ = writeln!(types.defs, "{keyword} {name} {{ {body} }}");
                types.defined.insert(key, name.clone());
                name
            }
        };
        self.elsewhere.insert(id_, (track.clone(), name.clone()));
        Ok((track, name))
    }

    /// The local name of a resource: `use`d from the interface declaring it.
    pub(crate) fn resource_name(&mut self, id_: TypeId) -> Result<String, HubError> {
        self.resource(id_)
    }

    fn resource(&mut self, id_: TypeId) -> Result<String, HubError> {
        if self.into.is_none()
            && let Some(name) = self.named.get(&id_)
        {
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
        if self.into.is_none()
            && let Some(name) = self.named.get(&at)
        {
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
        self.shared.resources.note(&interface, &resource);
        let chosen = self.shared.resources.chosen(&interface);
        if let Some(into) = self.into.clone() {
            let types = self.shared.types.get_mut(&into).ok_or_else(missing)?;
            return Ok(types.use_(&chosen, &id(&resource), Some(&resource)));
        }
        let name = self.fresh("r", local.as_deref());
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

/// A hub's interface of types missing where it was made: a bug here.
fn missing() -> HubError {
    HubError::Generate("a hub interface of types went missing".into())
}
