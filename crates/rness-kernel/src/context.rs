//! The plugin-facing API. A [`Context`] is handed to [`crate::Plugin::apply`];
//! everything a plugin does goes through it, and everything it does is
//! recorded as a reversible effect (invariant #7: no privileged core —
//! built-ins and dynamic plugins use these same paths).

use std::any::Any;
use std::sync::Arc;

use crate::effects::{Disposer, EffectBag};
use crate::events::{BailEvent, Event, EventBus, Next};
use crate::services::ServiceRegistry;
use crate::KernelError;

pub struct Context<'k> {
    services: &'k Arc<ServiceRegistry>,
    bus: &'k Arc<EventBus>,
    pub(crate) effects: EffectBag,
    pub(crate) provided: Vec<String>,
}

impl<'k> Context<'k> {
    pub(crate) fn new(services: &'k Arc<ServiceRegistry>, bus: &'k Arc<EventBus>) -> Self {
        Self {
            services,
            bus,
            effects: EffectBag::default(),
            provided: Vec::new(),
        }
    }

    // -- services ----------------------------------------------------------

    /// Provide a named service. Its lifetime is tied to this plugin: unload
    /// removes it (and the kernel cascades to consumers).
    pub fn provide<T: Send + Sync + 'static>(
        &mut self,
        name: &str,
        value: Arc<T>,
    ) -> Result<(), KernelError> {
        self.services.set(name, value as Arc<dyn Any + Send + Sync>)?;
        let services = Arc::clone(self.services);
        let owned = name.to_string();
        self.provided.push(owned.clone());
        self.effects.push(Box::new(move || {
            services.remove(&owned);
        }));
        Ok(())
    }

    /// Fetch a service. Plugins should only fetch what they declared in
    /// [`crate::Plugin::inject`] — declared deps are what activation and
    /// cascade are computed from.
    pub fn get<T: Send + Sync + 'static>(&self, name: &str) -> Option<Arc<T>> {
        self.services.get::<T>(name)
    }

    // -- events (all registrations are reversible) -------------------------

    pub fn on<E: Event>(&mut self, f: impl Fn(&E::Payload) + Send + Sync + 'static) {
        let d = self.bus.on::<E>(f);
        self.effects.push(d);
    }

    pub fn on_bail<E: BailEvent>(
        &mut self,
        f: impl Fn(&E::Payload) -> Option<E::Output> + Send + Sync + 'static,
    ) {
        let d = self.bus.on_bail::<E>(f);
        self.effects.push(d);
    }

    pub fn on_waterfall<E: Event>(
        &mut self,
        f: impl Fn(E::Payload, Next<'_, E::Payload>) -> E::Payload + Send + Sync + 'static,
    ) {
        let d = self.bus.on_waterfall::<E>(f);
        self.effects.push(d);
    }

    pub fn on_serial<E: Event>(
        &mut self,
        f: impl Fn(&E::Payload) -> Result<(), String> + Send + Sync + 'static,
    ) {
        let d = self.bus.on_serial::<E>(f);
        self.effects.push(d);
    }

    /// The bus itself, for dispatching (emit/bail/waterfall/serial calls
    /// need no disposer; only registrations do).
    pub fn bus(&self) -> &Arc<EventBus> {
        self.bus
    }

    // -- raw effects -------------------------------------------------------

    /// Record an arbitrary reversible side effect (timer, watcher, mount).
    pub fn effect(&mut self, dispose: Disposer) {
        self.effects.push(dispose);
    }
}
