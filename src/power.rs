//! Power management integration.
//!
//! [`PowerMonitor`] owns a registry of "on resume" callbacks. When the OS
//! reports a resume-from-sleep event (via D-Bus in a follow-up unit), every
//! registered callback is invoked.

use std::sync::{Arc, Mutex};

/// A registry of callbacks to invoke when the OS resumes from sleep.
///
/// Callbacks are registered via [`on_resume`](Self::on_resume) and all fire
/// (in registration order) when [`handle_wake`](Self::handle_wake) is called.
pub struct PowerMonitor {
    callbacks: Mutex<Vec<Arc<dyn Fn() + Send + Sync + 'static>>>,
}

/// Pure bridge from the D-Bus `logind.PrepareForSleep(bool)` signal to the
/// `PowerMonitor`. When `sleeping == false` (the OS just resumed), invokes
/// `monitor.handle_wake()` to fire all registered callbacks. When
/// `sleeping == true` (the OS is about to sleep), does nothing.
///
/// Used by tests directly and by the Linux D-Bus subscription at runtime.
pub fn handle_prepare_for_sleep(monitor: &PowerMonitor, sleeping: bool) {
    if !sleeping {
        monitor.handle_wake();
    }
}

// ─── Linux D-Bus subscription ───────────────────────────────────────────────

/// Subscribe to `logind`'s `PrepareForSleep` signal on the system D-Bus bus
/// and route resulting wake events to the given [`PowerMonitor`].
///
/// Spawns a background tokio task that holds the D-Bus connection and signal
/// subscription alive. Errors (e.g. no system D-Bus in a sandboxed or
/// headless environment) are logged; the function never panics.
///
/// Spawned once at app scope from `main.rs` (the activation closure, before
/// `build_window`). The monitor is threaded through to `enter_messaging` where
/// wake callbacks (receive-loop kick, threshold-gated sync, and APS refresh)
/// are registered. The D-Bus subscription runs for the lifetime of the process.
#[cfg(target_os = "linux")]
pub fn spawn_dbus_power_monitor(monitor: Arc<PowerMonitor>) {
    crate::runtime::runtime().spawn(async move {
        use zbus::export::futures_util::StreamExt;
        use zbus::message::Type;
        use zbus::{Connection, MatchRule, MessageStream};

        let conn = match Connection::system().await {
            Ok(c) => c,
            Err(e) => {
                log::error!("Failed to connect to system D-Bus: {e}");
                return;
            }
        };

        let build_rule = || -> Result<MatchRule, zbus::Error> {
            Ok(MatchRule::builder()
                .msg_type(Type::Signal)
                .sender("org.freedesktop.login1")?
                .interface("org.freedesktop.login1.Manager")?
                .member("PrepareForSleep")?
                .path("/org/freedesktop/login1")?
                .build())
        };

        let rule = match build_rule() {
            Ok(r) => r,
            Err(e) => {
                log::error!("Failed to build D-Bus match rule: {e}");
                return;
            }
        };

        let mut stream = match MessageStream::for_match_rule(rule, &conn, None).await {
            Ok(s) => s,
            Err(e) => {
                log::error!("Failed to subscribe to PrepareForSleep signal: {e}");
                return;
            }
        };

        // Drop the connection handle — the stream keeps the inner Arc alive.
        drop(conn);

        while let Some(msg_result) = stream.next().await {
            match msg_result {
                Ok(msg) => {
                    match msg.body().deserialize::<(bool,)>() {
                        Ok((sleeping,)) => {
                            handle_prepare_for_sleep(&monitor, sleeping);
                        }
                        Err(e) => {
                            log::error!(
                                "Failed to deserialize PrepareForSleep body: {e}"
                            );
                        }
                    }
                }
                Err(e) => {
                    log::error!("D-Bus stream error: {e}");
                }
            }
        }
    });
}

impl PowerMonitor {
    /// Construct an empty monitor with no registered callbacks.
    pub fn new() -> Self {
        Self {
            callbacks: Mutex::new(Vec::new()),
        }
    }

    /// Register a callback to be invoked on every resume event.
    ///
    /// Multiple callbacks may be registered; all fire on each event. The
    /// callback must outlive the monitor (use `'static`).
    pub fn on_resume(&self, callback: impl Fn() + Send + Sync + 'static) {
        self.callbacks
            .lock()
            .expect("PowerMonitor mutex poisoned")
            .push(Arc::new(callback));
    }

    /// Invoke all registered callbacks as if the OS had just signalled a
    /// resume event.
    ///
    /// The callback list is cloned under the lock so that the lock is
    /// released before invoking any callback, preventing deadlock if a
    /// callback re-enters [`on_resume`](Self::on_resume) or
    /// [`handle_wake`](Self::handle_wake).
    pub(crate) fn handle_wake(&self) {
        let callbacks = {
            let guard = self
                .callbacks
                .lock()
                .expect("PowerMonitor mutex poisoned");
            guard.clone()
        };
        for cb in &callbacks {
            (cb)();
        }
    }
}

/// Connect a `PowerMonitor`'s resume callbacks to a receive-loop kick signal.
///
/// Registers a callback with `monitor` such that any subsequent resume event
/// (signalled by `handle_prepare_for_sleep(monitor, false)`) calls
/// `notify_one()` on `kick`. This is the wiring that makes a wake-from-sleep
/// event re-subscribe the APNs receive loop.
///
/// `handle_prepare_for_sleep(monitor, true)` (the about-to-sleep case) does
/// NOT signal the kick.
pub fn wire_wake_to_receive_loop(monitor: &PowerMonitor, kick: Arc<tokio::sync::Notify>) {
    monitor.on_resume(move || kick.notify_one());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Notify;

    #[test]
    fn callbacks_accumulate_and_all_fire_on_resume() {
        let monitor = PowerMonitor::new();

        // First callback: increment counter1.
        let counter1 = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&counter1);
        monitor.on_resume(move || {
            c1.fetch_add(1, Ordering::Relaxed);
        });

        // Trigger first resume — only callback1 is registered.
        monitor.handle_wake();
        assert_eq!(
            counter1.load(Ordering::Relaxed),
            1,
            "counter1 should be 1 after first resume"
        );

        // Register a second callback: increment counter2.
        let counter2 = Arc::new(AtomicUsize::new(0));
        let c2 = Arc::clone(&counter2);
        monitor.on_resume(move || {
            c2.fetch_add(1, Ordering::Relaxed);
        });

        // Trigger second resume — both callbacks fire.
        monitor.handle_wake();
        assert_eq!(
            counter1.load(Ordering::Relaxed),
            2,
            "counter1 should be 2 after second resume"
        );
        assert_eq!(
            counter2.load(Ordering::Relaxed),
            1,
            "counter2 should be 1 after its first fire"
        );
    }

    #[test]
    fn handle_prepare_for_sleep_invokes_callbacks_only_on_resume() {
        let monitor = PowerMonitor::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        monitor.on_resume(move || {
            c.fetch_add(1, Ordering::Relaxed);
        });

        // Resume from sleep: callback fires.
        handle_prepare_for_sleep(&monitor, false);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "callback should fire on resume from sleep"
        );

        // About to sleep: callback does NOT fire.
        handle_prepare_for_sleep(&monitor, true);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "callback should NOT fire when going to sleep"
        );

        // Another resume: callback fires again.
        handle_prepare_for_sleep(&monitor, false);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "callback should fire on every resume event"
        );
    }

    #[tokio::test]
    async fn wire_wake_to_receive_loop_signals_kick_only_on_resume() {
        // Resume event signals the kick.
        let monitor = PowerMonitor::new();
        let kick = Arc::new(Notify::new());

        wire_wake_to_receive_loop(&monitor, Arc::clone(&kick));
        handle_prepare_for_sleep(&monitor, false);

        assert!(
            tokio::time::timeout(Duration::from_millis(100), kick.notified())
                .await
                .is_ok(),
            "kick should be notified on resume from sleep"
        );

        // Sleep event does NOT signal the kick (fresh monitor, fresh notify).
        let monitor2 = PowerMonitor::new();
        let kick2 = Arc::new(Notify::new());

        wire_wake_to_receive_loop(&monitor2, Arc::clone(&kick2));
        handle_prepare_for_sleep(&monitor2, true);

        assert!(
            tokio::time::timeout(Duration::from_millis(50), kick2.notified())
                .await
                .is_err(),
            "kick should NOT be notified when going to sleep"
        );
    }
}
