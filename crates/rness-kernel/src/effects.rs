//! Reversible effects. Every side effect a plugin makes (service
//! registration, listener, timer, mount) is recorded as a [`Disposer`];
//! unloading the plugin runs them in reverse registration order, leaving
//! no residue. This is invariant #6: hot reload is safe by construction.

/// Undoes one side effect. Runs at most once.
pub type Disposer = Box<dyn FnOnce() + Send>;

/// The disposers owned by one plugin instance.
#[derive(Default)]
pub struct EffectBag {
    disposers: Vec<Disposer>,
}

impl EffectBag {
    pub fn push(&mut self, d: Disposer) {
        self.disposers.push(d);
    }

    /// Run all disposers in reverse registration order.
    pub fn dispose_all(&mut self) {
        for d in self.disposers.drain(..).rev() {
            d();
        }
    }

    pub fn len(&self) -> usize {
        self.disposers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.disposers.is_empty()
    }
}

impl Drop for EffectBag {
    fn drop(&mut self) {
        self.dispose_all();
    }
}
