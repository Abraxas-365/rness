//! The [`Kernel`]: plugin registration, activation, and lifecycle.
//!
//! A plugin declares the services it injects; the kernel activates it only
//! when all of them exist, and deactivates it (running its disposers) when
//! any goes away. Boot order is never written down — it is a fixpoint:
//! activate whatever is satisfied, repeat until nothing changes. Cascade
//! works the same way in reverse.

use std::collections::HashMap;
use std::sync::Arc;

use crate::context::Context;
use crate::effects::EffectBag;
use crate::events::EventBus;
use crate::services::ServiceRegistry;
use crate::KernelError;

/// A unit of composition. `apply` is called on activation; everything it
/// registers through [`Context`] is undone on deactivation. Plugins must be
/// re-appliable (activate → deactivate → activate again).
pub trait Plugin: Send + Sync + 'static {
    /// Unique name; also the key for reload/unload.
    fn name(&self) -> &str;

    /// Service names this plugin requires before it can run.
    fn inject(&self) -> &[&str] {
        &[]
    }

    fn apply(&self, ctx: &mut Context<'_>) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginStatus {
    /// Registered, waiting for injected services.
    Waiting,
    Active,
    /// `apply` returned an error; will not retry until reload.
    Failed,
}

struct Slot {
    plugin: Arc<dyn Plugin>,
    status: PluginStatus,
    effects: EffectBag,
    /// Services this instance provided while active (for cascade).
    provided: Vec<String>,
}

/// The application kernel. Owns the service registry, the event bus, and
/// every mounted plugin.
pub struct Kernel {
    services: Arc<ServiceRegistry>,
    bus: Arc<EventBus>,
    slots: HashMap<String, Slot>,
    /// Registration order, for deterministic fixpoint sweeps.
    order: Vec<String>,
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

impl Kernel {
    pub fn new() -> Self {
        Self {
            services: Arc::new(ServiceRegistry::default()),
            bus: Arc::new(EventBus::default()),
            slots: HashMap::new(),
            order: Vec::new(),
        }
    }

    pub fn services(&self) -> &Arc<ServiceRegistry> {
        &self.services
    }

    pub fn bus(&self) -> &Arc<EventBus> {
        &self.bus
    }

    pub fn status(&self, name: &str) -> Option<PluginStatus> {
        self.slots.get(name).map(|s| s.status)
    }

    /// Mount a plugin. It activates immediately if its injections are
    /// satisfied — and its activation may satisfy others (fixpoint sweep).
    pub fn mount(&mut self, plugin: impl Plugin) -> Result<(), KernelError> {
        self.mount_arc(Arc::new(plugin))
    }

    pub fn mount_arc(&mut self, plugin: Arc<dyn Plugin>) -> Result<(), KernelError> {
        let name = plugin.name().to_string();
        if self.slots.contains_key(&name) {
            return Err(KernelError::DuplicatePlugin(name));
        }
        self.slots.insert(
            name.clone(),
            Slot {
                plugin,
                status: PluginStatus::Waiting,
                effects: EffectBag::default(),
                provided: Vec::new(),
            },
        );
        self.order.push(name);
        self.settle();
        Ok(())
    }

    /// Unload a plugin: run its disposers, cascade-deactivate consumers of
    /// its services, then re-settle (another provider may take over).
    pub fn unmount(&mut self, name: &str) -> Result<(), KernelError> {
        if !self.slots.contains_key(name) {
            return Err(KernelError::UnknownPlugin(name.to_string()));
        }
        self.deactivate(name);
        self.order.retain(|n| n != name);
        self.slots.remove(name);
        self.settle();
        Ok(())
    }

    /// Deactivate + reactivate one plugin (hot reload). Consumers of its
    /// services bounce with it.
    pub fn reload(&mut self, name: &str) -> Result<(), KernelError> {
        if !self.slots.contains_key(name) {
            return Err(KernelError::UnknownPlugin(name.to_string()));
        }
        self.deactivate(name);
        // Failed plugins get a retry on explicit reload.
        if let Some(slot) = self.slots.get_mut(name) {
            slot.status = PluginStatus::Waiting;
        }
        self.settle();
        Ok(())
    }

    /// Names of active plugins, in activation-eligible order.
    pub fn active(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|n| self.slots[*n].status == PluginStatus::Active)
            .cloned()
            .collect()
    }

    // -- internals ---------------------------------------------------------

    /// Activate every Waiting plugin whose injections are satisfied, until
    /// a full pass makes no progress. Derived boot order, no manual sequencing.
    fn settle(&mut self) {
        loop {
            let mut progressed = false;
            for name in self.order.clone() {
                let slot = &self.slots[&name];
                if slot.status != PluginStatus::Waiting {
                    continue;
                }
                let satisfied = slot.plugin.inject().iter().all(|s| self.services.has(s));
                if satisfied {
                    self.activate(&name);
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }
    }

    fn activate(&mut self, name: &str) {
        let plugin = Arc::clone(&self.slots[name].plugin);
        let mut ctx = Context::new(&self.services, &self.bus);
        let result = plugin.apply(&mut ctx);
        let Context {
            effects, provided, ..
        } = ctx;
        let slot = self.slots.get_mut(name).unwrap();
        match result {
            Ok(()) => {
                slot.effects = effects;
                slot.provided = provided;
                slot.status = PluginStatus::Active;
                tracing::debug!(plugin = name, "activated");
            }
            Err(reason) => {
                // effects drop here -> partial registrations are unwound.
                slot.status = PluginStatus::Failed;
                tracing::error!(plugin = name, %reason, "apply failed");
            }
        }
    }

    /// Deactivate one plugin and every active plugin that injected a
    /// service it provided, recursively. Reverse-dependency cascade.
    fn deactivate(&mut self, name: &str) {
        let Some(slot) = self.slots.get_mut(name) else {
            return;
        };
        if slot.status != PluginStatus::Active {
            return;
        }
        let provided = std::mem::take(&mut slot.provided);
        slot.status = PluginStatus::Waiting;

        // Cascade FIRST: consumers must unwind while the services they hold
        // are still notionally theirs; then this plugin's disposers remove
        // the services themselves.
        for svc in &provided {
            let consumers: Vec<String> = self
                .order
                .iter()
                .filter(|n| {
                    n.as_str() != name
                        && self.slots[*n].status == PluginStatus::Active
                        && self.slots[*n].plugin.inject().contains(&svc.as_str())
                })
                .cloned()
                .collect();
            for c in consumers {
                self.deactivate(&c);
            }
        }

        self.slots.get_mut(name).unwrap().effects.dispose_all();
        tracing::debug!(plugin = name, "deactivated");
    }
}
