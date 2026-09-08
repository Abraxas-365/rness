//! Named service slots — the seam through which plugins share capabilities.
//!
//! One plugin provides `"tools"`, consumers declare `inject: ["tools"]`.
//! The kernel activates a consumer only when every injected service exists,
//! and deactivates it when one goes away (see [`crate::plugin::Kernel`]).
//! Service lifetime is tied to its provider through a disposer: unloading
//! the provider removes the service automatically.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::KernelError;

/// Type-erased, name-keyed service registry. Values are `Arc<T>` behind
/// `dyn Any`; [`ServiceRegistry::get`] downcasts back to the concrete type.
#[derive(Default)]
pub struct ServiceRegistry {
    map: RwLock<HashMap<String, Arc<dyn Any + Send + Sync>>>,
}

impl ServiceRegistry {
    /// Register a service. Fails if the name is already claimed — two
    /// providers for one service is a composition error, not a race to win.
    pub fn set(&self, name: &str, value: Arc<dyn Any + Send + Sync>) -> Result<(), KernelError> {
        let mut map = self.map.write().unwrap();
        if map.contains_key(name) {
            return Err(KernelError::DuplicateService(name.to_string()));
        }
        map.insert(name.to_string(), value);
        Ok(())
    }

    pub fn remove(&self, name: &str) -> bool {
        self.map.write().unwrap().remove(name).is_some()
    }

    /// Fetch a service by name, downcast to its concrete type.
    /// `None` if absent or if `T` does not match what was provided.
    pub fn get<T: Send + Sync + 'static>(&self, name: &str) -> Option<Arc<T>> {
        let entry = self.map.read().unwrap().get(name)?.clone();
        entry.downcast::<T>().ok()
    }

    pub fn has(&self, name: &str) -> bool {
        self.map.read().unwrap().contains_key(name)
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<_> = self.map.read().unwrap().keys().cloned().collect();
        v.sort();
        v
    }

    pub fn len(&self) -> usize {
        self.map.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.read().unwrap().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_remove_roundtrip() {
        let reg = ServiceRegistry::default();
        reg.set("num", Arc::new(42u32)).unwrap();
        assert_eq!(*reg.get::<u32>("num").unwrap(), 42);
        assert!(reg.get::<String>("num").is_none(), "wrong type downcast");
        assert!(reg.remove("num"));
        assert!(!reg.has("num"));
    }

    #[test]
    fn duplicate_provider_is_an_error() {
        let reg = ServiceRegistry::default();
        reg.set("x", Arc::new(1u8)).unwrap();
        assert!(matches!(
            reg.set("x", Arc::new(2u8)),
            Err(KernelError::DuplicateService(_))
        ));
    }
}
