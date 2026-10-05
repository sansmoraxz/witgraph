//! Capability plugins: a [`Host`] assembled from providers that each
//! declare what they serve.
//!
//! A [`CapabilityPlugin`] names the capabilities it provides and links one
//! of them into a node's linker on request. [`Plugins`] is the [`Host`]
//! over a set of them: it routes every capability a node imports to one
//! plugin, and answers [`Host::check`] from the plugins' declarations, so a
//! capability nothing provides fails the load by name
//! ([`LoadError::MissingCapability`](crate::LoadError::MissingCapability))
//! rather than as a linker error.
//!
//! # Routing
//!
//! A capability is wanted as an interface: its own name, or, for a labelled
//! import (`import primary: clock;`), the interface the label stands for.
//! A plugin serves it when it provides that name, or the same interface at
//! a semver-compatible version no older than the one wanted (a newer minor
//! has everything an older one has; an older one may lack what the import
//! uses).
//!
//! - A labelled import whose label is a plugin's [id](CapabilityPlugin::id)
//!   goes to that plugin, which must serve the interface.
//! - Anything else goes to the one plugin serving it. None is
//!   [`CapabilityGap::Missing`]; several are [`CapabilityGap::Ambiguous`],
//!   which a label naming one of them resolves.
//!
//! So a world importing one interface under two labels reaches two plugins,
//! each backing the interface its own way.

use std::any::{Any, TypeId};
use std::collections::HashMap;

use wasmtime::component::Linker;
use witgraph_ir::{Capability, ComponentContract, NodeId};

use crate::engine::{CapabilityGap, Host, HostState, IslandData};
use witgraph_ir::interface::semver_serves;
use witgraph_sched::RuntimeError;

/// One provider of capabilities, for [`Plugins`].
pub trait CapabilityPlugin<D: IslandData = PluginData>: Send + Sync + 'static {
    /// The plugin's id, unique among a [`Plugins`]' plugins. A labelled
    /// import whose label equals it is routed to this plugin.
    fn id(&self) -> &str;

    /// The capabilities the plugin provides, named as components import
    /// them ([`Capability::interface`]): full interface ids
    /// (`namespace:name/interface@version`, which also serves every older
    /// semver-compatible version), inline interface import names,
    /// `func:`-prefixed functions and `resource:`-prefixed resources.
    /// Asked once, when the plugin is added to a [`Plugins`].
    fn provides(&self) -> Vec<String>;

    /// Adds `capability` to `node`'s linker under the name the node
    /// imports it by, [`Capability::link_name`]: an interface id, inline
    /// import name or label as an instance, a bare function or world
    /// resource at the linker's root. Called once per node and capability
    /// routed to this plugin.
    fn link(
        &self,
        node: &NodeId,
        capability: &Capability,
        linker: &mut Linker<D>,
    ) -> wasmtime::Result<()>;
}

/// Per-Store state of any type, one value per type: where plugins sharing
/// a Store keep what belongs to them. An island has one Store, or several
/// when members joined only by streams the host can pump each get their
/// own (see [`engine`](crate::engine#islands)); state kept here is not
/// shared between an island's Stores.
#[derive(Default)]
pub struct Extensions(HashMap<TypeId, Box<dyn Any + Send>>);

impl Extensions {
    /// The value of type `T`, if one was stored.
    pub fn get<T: Send + 'static>(&self) -> Option<&T> {
        self.0.get(&TypeId::of::<T>())?.downcast_ref()
    }

    /// The value of type `T`, mutably, if one was stored.
    pub fn get_mut<T: Send + 'static>(&mut self) -> Option<&mut T> {
        self.0.get_mut(&TypeId::of::<T>())?.downcast_mut()
    }

    /// Stores `value`, returning the value of its type it replaces.
    pub fn insert<T: Send + 'static>(&mut self, value: T) -> Option<T> {
        let old = self.0.insert(TypeId::of::<T>(), Box::new(value))?;
        old.downcast().ok().map(|old| *old)
    }
}

impl std::fmt::Debug for Extensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extensions")
            .field("len", &self.0.len())
            .finish()
    }
}

/// The default island Store data of [`Plugins`]: witgraph's [`HostState`]
/// plus [`Extensions`] for the plugins' own state in that Store. It is
/// created empty for every island Store, at load and on every rebuild.
#[derive(Debug)]
pub struct PluginData {
    state: HostState,
    extensions: Extensions,
}

impl PluginData {
    /// The plugins' state in this Store.
    pub fn extensions(&mut self) -> &mut Extensions {
        &mut self.extensions
    }
}

impl IslandData for PluginData {
    fn host_state(&mut self) -> &mut HostState {
        &mut self.state
    }
}

/// How [`Plugins`] makes the data of an island Store.
type DataFactory<D> = Box<dyn Fn(&[NodeId], HostState) -> wasmtime::Result<D> + Send + Sync>;

/// A [`Host`] made of [`CapabilityPlugin`]s. See the [module docs](self)
/// for how a capability is routed to one of them.
pub struct Plugins<D: IslandData = PluginData> {
    /// Each plugin, with what it provides.
    plugins: Vec<(Box<dyn CapabilityPlugin<D>>, Vec<String>)>,
    data: DataFactory<D>,
}

impl Plugins<PluginData> {
    /// No plugins yet, with [`PluginData`] as the island Store data.
    pub fn new() -> Self {
        Self::with_data(|_, state| {
            Ok(PluginData {
                state,
                extensions: Extensions::default(),
            })
        })
    }
}

impl Default for Plugins<PluginData> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D: IslandData> Plugins<D> {
    /// No plugins yet, with island Store data made by `data` (as
    /// [`Host::island_data`] makes it), for plugins that need a data type
    /// of their own.
    pub fn with_data(
        data: impl Fn(&[NodeId], HostState) -> wasmtime::Result<D> + Send + Sync + 'static,
    ) -> Self {
        Self {
            plugins: Vec::new(),
            data: Box::new(data),
        }
    }

    /// Adds a plugin. Its id must be new
    /// ([`RuntimeError::InvalidConfig`]).
    pub fn with(mut self, plugin: impl CapabilityPlugin<D>) -> Result<Self, RuntimeError> {
        if self.ids().any(|id| id == plugin.id()) {
            return Err(RuntimeError::InvalidConfig {
                message: format!("plugin id `{}` is registered twice", plugin.id()),
            });
        }
        let provides = plugin.provides();
        self.plugins.push((Box::new(plugin), provides));
        Ok(self)
    }

    /// The ids of the plugins, in the order they were added.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.plugins.iter().map(|(p, _)| p.id())
    }

    /// The plugin `capability` is routed to.
    fn route(&self, capability: &Capability) -> Result<&dyn CapabilityPlugin<D>, CapabilityGap> {
        let wanted = capability
            .implements
            .as_deref()
            .unwrap_or(&capability.interface);
        let serves = |provides: &[String]| provides.iter().any(|name| semver_serves(name, wanted));
        if capability.implements.is_some()
            && let Some((named, provides)) = self
                .plugins
                .iter()
                .find(|(p, _)| p.id() == capability.interface)
        {
            return if serves(provides) {
                Ok(named.as_ref())
            } else {
                Err(CapabilityGap::Missing)
            };
        }
        let serving: Vec<&dyn CapabilityPlugin<D>> = self
            .plugins
            .iter()
            .filter(|(_, provides)| serves(provides))
            .map(|(p, _)| p.as_ref())
            .collect();
        match serving.as_slice() {
            [] => Err(CapabilityGap::Missing),
            [one] => Ok(*one),
            several => Err(CapabilityGap::Ambiguous(
                several.iter().map(|p| p.id().to_string()).collect(),
            )),
        }
    }
}

impl<D: IslandData> Host for Plugins<D> {
    type Data = D;

    fn link(
        &self,
        node: &NodeId,
        contract: &ComponentContract,
        linker: &mut Linker<D>,
    ) -> wasmtime::Result<()> {
        for capability in &contract.capabilities {
            let plugin = self.route(capability).map_err(|gap| {
                wasmtime::format_err!(
                    "no plugin for capability `{}`: {gap:?}",
                    capability.interface
                )
            })?;
            plugin.link(node, capability, linker).map_err(|e| {
                e.context(format!(
                    "plugin `{}` linking `{}`",
                    plugin.id(),
                    capability.interface
                ))
            })?;
        }
        Ok(())
    }

    fn island_data(&self, members: &[NodeId], state: HostState) -> wasmtime::Result<D> {
        (self.data)(members, state)
    }

    fn check(&self, _: &NodeId, capability: &Capability) -> Result<(), CapabilityGap> {
        self.route(capability).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(&'static str, &'static [&'static str]);

    impl CapabilityPlugin for Fake {
        fn id(&self) -> &str {
            self.0
        }

        fn provides(&self) -> Vec<String> {
            self.1.iter().map(ToString::to_string).collect()
        }

        fn link(
            &self,
            _: &NodeId,
            _: &Capability,
            _: &mut Linker<PluginData>,
        ) -> wasmtime::Result<()> {
            Ok(())
        }
    }

    fn labelled(label: &str, interface: &str) -> Capability {
        Capability {
            implements: Some(interface.into()),
            ..Capability::new(label)
        }
    }

    fn routed(plugins: &Plugins, capability: &Capability) -> Result<String, CapabilityGap> {
        plugins.route(capability).map(|p| p.id().to_string())
    }

    #[test]
    fn a_capability_goes_to_the_one_plugin_serving_it() {
        let plugins = Plugins::new()
            .with(Fake("clock", &["a:b/clock@0.1.7"]))
            .unwrap()
            .with(Fake("misc", &["config", "func:blink", "resource:r"]))
            .unwrap();
        let route = |name: &str| routed(&plugins, &Capability::new(name));
        assert_eq!(route("a:b/clock@0.1.7").as_deref(), Ok("clock"));
        assert_eq!(
            route("a:b/clock@0.1.0").as_deref(),
            Ok("clock"),
            "an older semver-compatible version is served too"
        );
        assert_eq!(
            route("a:b/clock@0.1.9"),
            Err(CapabilityGap::Missing),
            "a newer one may use what the plugin lacks"
        );
        assert_eq!(route("config").as_deref(), Ok("misc"));
        assert_eq!(route("func:blink").as_deref(), Ok("misc"));
        assert_eq!(route("a:b/clock@0.2.0"), Err(CapabilityGap::Missing));
        assert_eq!(route("a:b/other@0.1.0"), Err(CapabilityGap::Missing));
    }

    #[test]
    fn a_label_picks_among_plugins_serving_one_interface() {
        let plugins = Plugins::new()
            .with(Fake("primary", &["a:b/kv@1.2.0"]))
            .unwrap()
            .with(Fake("backup", &["a:b/kv@1.2.0"]))
            .unwrap();
        assert_eq!(
            routed(&plugins, &Capability::new("a:b/kv@1.0.0")),
            Err(CapabilityGap::Ambiguous(vec![
                "primary".into(),
                "backup".into()
            ]))
        );
        for label in ["primary", "backup"] {
            assert_eq!(
                routed(&plugins, &labelled(label, "a:b/kv@1.1.0")).as_deref(),
                Ok(label)
            );
        }
        assert_eq!(
            routed(&plugins, &labelled("other", "a:b/kv@1.0.0")),
            Err(CapabilityGap::Ambiguous(vec![
                "primary".into(),
                "backup".into()
            ])),
            "a label naming no plugin picks none"
        );
    }

    #[test]
    fn a_label_naming_a_plugin_needs_it_to_serve_the_interface() {
        let plugins = Plugins::new()
            .with(Fake("primary", &["a:b/kv@1.0.0"]))
            .unwrap()
            .with(Fake("clock", &["a:b/clock@0.1.0"]))
            .unwrap();
        assert_eq!(
            routed(&plugins, &labelled("primary", "a:b/clock@0.1.0")),
            Err(CapabilityGap::Missing)
        );
        assert_eq!(
            routed(&plugins, &labelled("anything", "a:b/clock@0.1.0")).as_deref(),
            Ok("clock"),
            "a label naming no plugin still reaches the only one serving it"
        );
    }

    #[test]
    fn a_plugin_id_is_registered_once() {
        let twice = Plugins::new()
            .with(Fake("clock", &[]))
            .unwrap()
            .with(Fake("clock", &[]));
        assert!(matches!(twice, Err(RuntimeError::InvalidConfig { .. })));
    }

    #[test]
    fn extensions_keep_one_value_per_type() {
        let mut extensions = Extensions::default();
        assert_eq!(extensions.get::<u32>(), None);
        assert_eq!(extensions.insert(2u32), None);
        if let Some(n) = extensions.get_mut::<u32>() {
            *n += 3;
        }
        assert_eq!(extensions.get::<u32>(), Some(&5));
        assert_eq!(extensions.insert(7u32), Some(5));
        assert_eq!(extensions.insert(String::from("x")), None);
        assert_eq!(extensions.get_mut::<String>().map(|s| s.len()), Some(1));
    }
}
