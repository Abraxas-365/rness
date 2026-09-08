//! Typed event bus with four dispatch modes. The mode is part of each
//! event's public contract:
//!
//! - `emit`: fire-and-forget notification; all listeners run in order
//! - `bail`: first listener returning `Some` short-circuits (veto/answer)
//! - `waterfall`: around-middleware — each listener wraps `next`, may
//!   modify the payload before/after, or short-circuit by not calling it
//!   (policy plugins wrap any decision)
//! - `serial`: in-order, fallible; stops at the first error
//!
//! Events are declared as zero-sized types implementing [`Event`] (plus
//! [`BailEvent`] for bail outputs), so Rust callers get typed payloads.
//! Listener registration returns a [`Disposer`] so subscriptions are
//! reversible effects.

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::effects::Disposer;

/// A named event with a payload type. `NAME` is the wire identity —
/// dynamic (Lua) listeners will key on it.
pub trait Event: 'static {
    const NAME: &'static str;
    type Payload: Send + Sync + 'static;
}

/// Extra contract for events dispatched with [`EventBus::bail`].
pub trait BailEvent: Event {
    type Output: Send + 'static;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Emit,
    Bail,
    Waterfall,
    Serial,
}

struct Entry {
    id: u64,
    kind: Kind,
    handler: Arc<dyn Any + Send + Sync>,
}

type EmitFn<P> = Box<dyn Fn(&P) + Send + Sync>;
type BailFn<P, R> = Box<dyn Fn(&P) -> Option<R> + Send + Sync>;
type WaterFn<P> = Box<dyn for<'a> Fn(P, Next<'a, P>) -> P + Send + Sync>;
type SerialFn<P> = Box<dyn Fn(&P) -> Result<(), String> + Send + Sync>;

struct EmitH<P>(EmitFn<P>);
struct BailH<P, R>(BailFn<P, R>);
struct WaterH<P>(WaterFn<P>);
struct SerialH<P>(SerialFn<P>);

/// The continuation handed to a waterfall listener. Calling [`Next::run`]
/// invokes the rest of the chain; not calling it short-circuits.
pub struct Next<'a, P> {
    rest: &'a [Arc<WaterH<P>>],
}

impl<'a, P> Next<'a, P> {
    pub fn run(self, payload: P) -> P {
        match self.rest.split_first() {
            None => payload,
            Some((h, rest)) => (h.0)(payload, Next { rest }),
        }
    }
}

type Listeners = Arc<RwLock<HashMap<&'static str, Vec<Entry>>>>;

/// The bus. Cheap to share; the kernel owns one per application.
#[derive(Default)]
pub struct EventBus {
    listeners: Listeners,
    next_id: AtomicU64,
}

impl EventBus {
    fn register(&self, name: &'static str, kind: Kind, handler: Arc<dyn Any + Send + Sync>) -> Disposer {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.listeners
            .write()
            .unwrap()
            .entry(name)
            .or_default()
            .push(Entry { id, kind, handler });
        let listeners = Arc::clone(&self.listeners);
        Box::new(move || {
            if let Some(v) = listeners.write().unwrap().get_mut(name) {
                v.retain(|e| e.id != id);
            }
        })
    }

    /// Snapshot the handlers of one kind, in registration order.
    fn collect<H: Send + Sync + 'static>(&self, name: &'static str, kind: Kind) -> Vec<Arc<H>> {
        self.listeners
            .read()
            .unwrap()
            .get(name)
            .map(|v| {
                v.iter()
                    .filter(|e| e.kind == kind)
                    .filter_map(|e| Arc::clone(&e.handler).downcast::<H>().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    // -- registration ------------------------------------------------------

    pub fn on<E: Event>(&self, f: impl Fn(&E::Payload) + Send + Sync + 'static) -> Disposer {
        self.register(E::NAME, Kind::Emit, Arc::new(EmitH::<E::Payload>(Box::new(f))))
    }

    pub fn on_bail<E: BailEvent>(
        &self,
        f: impl Fn(&E::Payload) -> Option<E::Output> + Send + Sync + 'static,
    ) -> Disposer {
        self.register(E::NAME, Kind::Bail, Arc::new(BailH::<E::Payload, E::Output>(Box::new(f))))
    }

    pub fn on_waterfall<E: Event>(
        &self,
        f: impl Fn(E::Payload, Next<'_, E::Payload>) -> E::Payload + Send + Sync + 'static,
    ) -> Disposer {
        self.register(E::NAME, Kind::Waterfall, Arc::new(WaterH::<E::Payload>(Box::new(f))))
    }

    pub fn on_serial<E: Event>(
        &self,
        f: impl Fn(&E::Payload) -> Result<(), String> + Send + Sync + 'static,
    ) -> Disposer {
        self.register(E::NAME, Kind::Serial, Arc::new(SerialH::<E::Payload>(Box::new(f))))
    }

    // -- dispatch ----------------------------------------------------------

    pub fn emit<E: Event>(&self, payload: &E::Payload) {
        for h in self.collect::<EmitH<E::Payload>>(E::NAME, Kind::Emit) {
            (h.0)(payload);
        }
    }

    pub fn bail<E: BailEvent>(&self, payload: &E::Payload) -> Option<E::Output> {
        for h in self.collect::<BailH<E::Payload, E::Output>>(E::NAME, Kind::Bail) {
            if let Some(out) = (h.0)(payload) {
                return Some(out);
            }
        }
        None
    }

    pub fn waterfall<E: Event>(&self, payload: E::Payload) -> E::Payload {
        let handlers = self.collect::<WaterH<E::Payload>>(E::NAME, Kind::Waterfall);
        Next { rest: &handlers }.run(payload)
    }

    pub fn serial<E: Event>(&self, payload: &E::Payload) -> Result<(), String> {
        for h in self.collect::<SerialH<E::Payload>>(E::NAME, Kind::Serial) {
            (h.0)(payload)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Ping;
    impl Event for Ping {
        const NAME: &'static str = "test/ping";
        type Payload = String;
    }
    impl BailEvent for Ping {
        type Output = u32;
    }

    #[test]
    fn emit_reaches_all_and_disposer_removes() {
        let bus = EventBus::default();
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let s1 = Arc::clone(&seen);
        let d1 = bus.on::<Ping>(move |p| s1.lock().unwrap().push(format!("a:{p}")));
        let s2 = Arc::clone(&seen);
        let _d2 = bus.on::<Ping>(move |p| s2.lock().unwrap().push(format!("b:{p}")));

        bus.emit::<Ping>(&"x".into());
        d1();
        bus.emit::<Ping>(&"y".into());

        assert_eq!(*seen.lock().unwrap(), vec!["a:x", "b:x", "b:y"]);
    }

    #[test]
    fn bail_short_circuits() {
        let bus = EventBus::default();
        let _a = bus.on_bail::<Ping>(|p| if p == "hit" { Some(1) } else { None });
        let _b = bus.on_bail::<Ping>(|_| Some(2));
        assert_eq!(bus.bail::<Ping>(&"hit".into()), Some(1));
        assert_eq!(bus.bail::<Ping>(&"miss".into()), Some(2));
    }

    #[test]
    fn waterfall_wraps_and_can_short_circuit() {
        let bus = EventBus::default();
        // outer wraps: before + after
        let _a = bus.on_waterfall::<Ping>(|p, next| format!("<{}>", next.run(p)));
        // inner transforms
        let _b = bus.on_waterfall::<Ping>(|p, next| next.run(p.to_uppercase()));
        assert_eq!(bus.waterfall::<Ping>("hi".into()), "<HI>");

        // short-circuit: never calls next
        let _c = bus.on_waterfall::<Ping>(|_, _next| "blocked".into());
        assert_eq!(bus.waterfall::<Ping>("hi".into()), "<blocked>");
    }

    #[test]
    fn serial_stops_on_error() {
        let bus = EventBus::default();
        let ran: Arc<Mutex<u32>> = Arc::default();
        let _a = bus.on_serial::<Ping>(|_| Err("nope".into()));
        let r = Arc::clone(&ran);
        let _b = bus.on_serial::<Ping>(move |_| {
            *r.lock().unwrap() += 1;
            Ok(())
        });
        assert_eq!(bus.serial::<Ping>(&"x".into()), Err("nope".into()));
        assert_eq!(*ran.lock().unwrap(), 0);
    }
}
