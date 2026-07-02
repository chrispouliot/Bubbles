mod gtk_bridge;
mod image;
mod persist;
mod power;
mod protocol;
mod retry;
mod runtime;
mod setup;
mod store;
mod text_scale;
mod time_format;
mod tray;
mod ui;
mod window_state;
mod attachment_cache;
mod demo;
mod video;

#[cfg(feature = "rustpush")]
mod api;

#[cfg(feature = "rustpush")]
mod sync;

// The Mac-hardware protobuf, compiled by build.rs into OUT_DIR. Referenced as
// `crate::bbhwinfo` by the vendored api subset.
#[cfg(feature = "rustpush")]
pub mod bbhwinfo {
    include!(concat!(env!("OUT_DIR"), "/bbhwinfo.rs"));
}

use std::sync::Arc;

use adw::prelude::*;

use power::PowerMonitor;
use protocol::Backend;

const APP_ID: &str = "io.github.chrispouliot.Bubbles";

fn main() -> glib::ExitCode {
    // rustls 0.23 won't auto-pick a crypto provider when more than one is linked.
    // reqwest's rustls-tls pulls aws-lc-rs; our IMD fetch (ureq) pulls ring. Select
    // one explicitly before any TLS happens (session restore, Apple calls, IMD fetch).
    
    let _ = rustls::crypto::ring::default_provider().install_default();
    // NAC self-test: set NAC_SELFTEST=<path-to-base64-blob> (plus OPEN_ABSINTHE_IMD)
    // to generate validation data once from the command line and exit, skipping the
    // GUI and any Apple ID login. Lets you verify the emulator / a hardware identity.
    #[cfg(feature = "rustpush")]
    if let Ok(path) = std::env::var("NAC_SELFTEST") {
        nac_selftest(&path);
        return glib::ExitCode::SUCCESS;
    }

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .build();

    app.connect_activate(|app| {
        if let Some(existing) = app.windows().into_iter().next() {
            existing.present();
            return;
        }
        // App-scoped PowerMonitor: created once so D-Bus logind subscription
        // lives for the process lifetime, not per-view. Threaded through to
        // enter_messaging for wake callbacks (sync, APS refresh, re-subscribe).
        let monitor = Arc::new(PowerMonitor::new());
        #[cfg(target_os = "linux")]
        crate::power::spawn_dbus_power_monitor(Arc::clone(&monitor));

        let window = if std::env::var_os("BUBBLES_DEMO").is_some() {
            demo::build_demo_window(app, monitor)
        } else {
            setup::view::build_window(app, make_backend(), make_store(), monitor)
        };
        window.present();
        tray::install(app, &window);
    });

    app.run()
}

/// Generate validation data once from a hardware blob and print it, then exit.
/// Exercises the full native NAC path — cert fetch, `nacInit`, Apple's
/// `initializeValidation` round-trip, key establishment, and `nacSign` — with no
/// GUI and no Apple ID login. Set `OPEN_ABSINTHE_IMD` so the emulator can find
/// `IMDAppleServices`. `NAC_SELFTEST` is a path to a file containing the base64
/// hardware blob (the same string you'd paste into "Local Mac Hardware").
#[cfg(feature = "rustpush")]
fn nac_selftest(blob_path: &str) {
    let raw = match std::fs::read_to_string(blob_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("NAC selftest: cannot read {blob_path}: {e}");
            std::process::exit(2);
        }
    };
    let decoded = glib::base64_decode(raw.trim());
    // Strip the OABS magic (4) + flag (1) if present, same as the sign-in path.
    let inner = if decoded.len() > 5 && decoded.starts_with(b"OABS") {
        decoded[5..].to_vec()
    } else {
        decoded.to_vec()
    };
    let config = match api::config_from_encoded(inner) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("NAC selftest: failed to decode hardware blob: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("NAC selftest: decoded identity; requesting validation data from Apple...");
    match runtime::runtime().block_on(config.generate_validation_data()) {
        Ok(data) => {
            println!("NAC OK: {} bytes of validation data", data.len());
            println!("{}", glib::base64_encode(&data));
        }
        Err(e) => {
            eprintln!("NAC FAILED: {e}");
            std::process::exit(1);
        }
    }
}

/// Open (creating if needed) the message store under the app data dir.
fn make_store() -> store::Store {
    let dir = glib::user_data_dir().join("bubbles");
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("messages.db");
    runtime::runtime()
        .block_on(store::Store::open(path))
        .expect("failed to open message store")
}

/// Real backend: initialises the rustpush state dir + logger, then hands the
/// onboarding flow a live `RustpushBackend`.
#[cfg(feature = "rustpush")]
fn make_backend() -> Arc<dyn Backend> {
    let dir = glib::user_data_dir().join("bubbles");
    std::fs::create_dir_all(&dir).ok();
    let path = dir.to_string_lossy().into_owned();
    api::do_first_time_init(path.clone());
    Arc::new(protocol::rustpush_backend::RustpushBackend::new(path))
}

/// Default backend: canned values so the flow is click-through-able without
/// rustpush linked.
#[cfg(not(feature = "rustpush"))]
fn make_backend() -> Arc<dyn Backend> {
    Arc::new(protocol::stub::StubBackend::default())
}
