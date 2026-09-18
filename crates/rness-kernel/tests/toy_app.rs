//! M0 exit criteria: a toy app of three plugins with inject dependencies,
//! waterfall interception, and clean unload — proving derived boot order,
//! cascade deactivation, reversible effects, and hot reload.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use rness_kernel::{Context, Event, Kernel, Plugin, PluginStatus};

// -- the toy domain: a greeter service, a policy wrapper, a consumer -------

/// Service provided by `GreeterPlugin`.
struct Greeter {
    calls: AtomicU32,
}
impl Greeter {
    fn greet(&self, who: &str) -> String {
        self.calls.fetch_add(1, Ordering::Relaxed);
        format!("hello {who}")
    }
}

/// Waterfall event: transform the outgoing greeting.
struct GreetingOut;
impl Event for GreetingOut {
    const NAME: &'static str = "toy/greeting-out";
    type Payload = String;
}

struct GreeterPlugin;
impl Plugin for GreeterPlugin {
    fn name(&self) -> &str {
        "greeter"
    }
    fn apply(&self, ctx: &mut Context<'_>) -> Result<(), String> {
        ctx.provide(
            "greeter",
            Arc::new(Greeter {
                calls: AtomicU32::new(0),
            }),
        )
        .map_err(|e| e.to_string())
    }
}

/// Policy plugin: wraps every greeting via waterfall (shouts it).
struct ShoutPolicy;
impl Plugin for ShoutPolicy {
    fn name(&self) -> &str {
        "shout-policy"
    }
    fn apply(&self, ctx: &mut Context<'_>) -> Result<(), String> {
        ctx.on_waterfall::<GreetingOut>(|g, next| next.run(g.to_uppercase()));
        Ok(())
    }
}

/// Consumer: injects the greeter service, produces greetings through the bus.
struct AppPlugin {
    log: Arc<Mutex<Vec<String>>>,
}
impl Plugin for AppPlugin {
    fn name(&self) -> &str {
        "app"
    }
    fn inject(&self) -> &[&str] {
        &["greeter"]
    }
    fn apply(&self, ctx: &mut Context<'_>) -> Result<(), String> {
        let greeter: Arc<Greeter> = ctx.get("greeter").ok_or("greeter missing")?;
        let out = ctx.bus().waterfall::<GreetingOut>(greeter.greet("world"));
        self.log.lock().unwrap().push(out);
        Ok(())
    }
}

#[test]
fn toy_app_boot_cascade_and_reload() {
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut kernel = Kernel::new();

    // Mount in "wrong" order on purpose: consumer first, provider last.
    // Boot order is derived, not written.
    kernel
        .mount(AppPlugin {
            log: Arc::clone(&log),
        })
        .unwrap();
    assert_eq!(kernel.status("app"), Some(PluginStatus::Waiting));

    kernel.mount(ShoutPolicy).unwrap();
    kernel.mount(GreeterPlugin).unwrap();

    // Provider arrival activated the waiting consumer; policy intercepted.
    assert_eq!(kernel.status("app"), Some(PluginStatus::Active));
    assert_eq!(*log.lock().unwrap(), vec!["HELLO WORLD"]);

    // Unload the POLICY: app is untouched (doesn't inject it), but the
    // waterfall listener is gone — proven on reload of app.
    kernel.unmount("shout-policy").unwrap();
    kernel.reload("app").unwrap();
    assert_eq!(*log.lock().unwrap(), vec!["HELLO WORLD", "hello world"]);

    // Unload the PROVIDER: cascade deactivates the consumer, service is gone.
    kernel.unmount("greeter").unwrap();
    assert_eq!(kernel.status("app"), Some(PluginStatus::Waiting));
    assert!(!kernel.services().has("greeter"));

    // Remount the provider: the waiting consumer comes back by itself.
    kernel.mount(GreeterPlugin).unwrap();
    assert_eq!(kernel.status("app"), Some(PluginStatus::Active));
    assert_eq!(
        *log.lock().unwrap(),
        vec!["HELLO WORLD", "hello world", "hello world"]
    );

    // Full teardown leaves no services behind.
    kernel.unmount("app").unwrap();
    kernel.unmount("greeter").unwrap();
    assert!(kernel.services().is_empty());
    assert!(kernel.active().is_empty());
}

#[test]
fn failed_apply_unwinds_partial_effects_and_can_retry() {
    struct Flaky {
        attempts: Arc<AtomicU32>,
    }
    impl Plugin for Flaky {
        fn name(&self) -> &str {
            "flaky"
        }
        fn apply(&self, ctx: &mut Context<'_>) -> Result<(), String> {
            // Registers a service, THEN fails on the first attempt — the
            // partial registration must be unwound.
            ctx.provide("flaky-svc", Arc::new(1u8))
                .map_err(|e| e.to_string())?;
            if self.attempts.fetch_add(1, Ordering::Relaxed) == 0 {
                return Err("first attempt fails".into());
            }
            Ok(())
        }
    }

    let attempts = Arc::new(AtomicU32::new(0));
    let mut kernel = Kernel::new();
    kernel
        .mount(Flaky {
            attempts: Arc::clone(&attempts),
        })
        .unwrap();

    assert_eq!(kernel.status("flaky"), Some(PluginStatus::Failed));
    assert!(
        !kernel.services().has("flaky-svc"),
        "partial effects of a failed apply must be unwound"
    );

    // Explicit reload retries; second attempt succeeds.
    kernel.reload("flaky").unwrap();
    assert_eq!(kernel.status("flaky"), Some(PluginStatus::Active));
    assert!(kernel.services().has("flaky-svc"));
}

#[test]
fn duplicate_service_fails_second_provider_not_first() {
    struct P(&'static str);
    impl Plugin for P {
        fn name(&self) -> &str {
            self.0
        }
        fn apply(&self, ctx: &mut Context<'_>) -> Result<(), String> {
            ctx.provide("the-service", Arc::new(0u8))
                .map_err(|e| e.to_string())
        }
    }

    let mut kernel = Kernel::new();
    kernel.mount(P("first")).unwrap();
    kernel.mount(P("second")).unwrap();
    assert_eq!(kernel.status("first"), Some(PluginStatus::Active));
    assert_eq!(kernel.status("second"), Some(PluginStatus::Failed));

    // First provider unloads -> service freed -> explicit reload of the
    // second succeeds (takeover).
    kernel.unmount("first").unwrap();
    kernel.reload("second").unwrap();
    assert_eq!(kernel.status("second"), Some(PluginStatus::Active));
}
