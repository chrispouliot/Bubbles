//! The onboarding UI: an `AdwNavigationView` walking through
//! hardware -> login -> 2FA -> finalize, mirroring the upstream Dart pages.
//!
//! NOTE: this layer links gtk4/libadwaita and is not compiled in the offline
//! verification harness. Build it in the devshell (`cargo run`). The logic it
//! drives ([`super::flow`], [`crate::protocol`], [`crate::gtk_bridge`]) is
//! compile-checked.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;

use crate::gtk_bridge;
use crate::power::PowerMonitor;
use crate::protocol::{Backend, LoginState};
use crate::setup::{flow, SetupState};
use crate::store::Store;

// Relay host + token, overridable at runtime so the client can point at a
// self-hosted validation sidecar without recompiling:
//   BUBBLES_RELAY_HOST=http://nas:8085   (no token needed for the sidecar)
// Defaults to OpenBubbles' hosted relay.
const DEFAULT_RELAY_HOST: &str = "https://hw.openbubbles.app";
const DEFAULT_RELAY_TOKEN: &str = "5c175851953ecaf5209185d897591badb6c3e712";

fn relay_host() -> String {
    std::env::var("BUBBLES_RELAY_HOST").unwrap_or_else(|_| DEFAULT_RELAY_HOST.to_string())
}

/// The beeper/relay token. Sent only when targeting the default hosted relay;
/// for a custom host (the sidecar) it's omitted unless BUBBLES_RELAY_TOKEN
/// is set explicitly.
fn relay_token() -> Option<String> {
    if let Ok(tok) = std::env::var("BUBBLES_RELAY_TOKEN") {
        return Some(tok);
    }
    if std::env::var("BUBBLES_RELAY_HOST").is_ok() {
        None // custom host (sidecar): no token by default
    } else {
        Some(DEFAULT_RELAY_TOKEN.to_string())
    }
}

type Shared = Rc<RefCell<SetupState>>;

/// Build the onboarding window. Hand it the stub today, the real rustpush
/// backend later — nothing else changes.
pub fn build_window(
    app: &adw::Application,
    backend: Arc<dyn Backend>,
    store: Store,
    monitor: Arc<PowerMonitor>,
) -> adw::ApplicationWindow {
    let state: Shared = Rc::new(RefCell::new(SetupState::default()));
    state.borrow_mut().store = Some(store);
    let nav = adw::NavigationView::new();

    // Try to restore a saved session before showing onboarding. While
    // the async restore runs we show a placeholder; on completion we replace the
    // stack with either the messaging UI (restored) or the hardware page.
    nav.push(&restoring_page());

    let fut = flow::restore(backend.clone());
    let nav_cb = nav.clone();
    let state_cb = state.clone();
    let backend_cb = backend.clone();
    gtk_bridge::spawn(fut, move |result| match result {
        Ok(Some(restored)) => {
            {
                let mut s = state_cb.borrow_mut();
                s.config = Some(restored.config);
                s.connection = Some(restored.connection);
                s.identity = Some(restored.identity);
                s.client = Some(restored.client);
                s.handles = restored.handles.clone();
            }
            go_messaging(&nav_cb, &state_cb, &backend_cb, &monitor);
        }
        Ok(None) => {
            nav_cb.replace(&[hardware_page(&nav_cb, &state_cb, &backend_cb, &monitor)]);
        }
        Err(e) => {
            eprintln!("session restore failed, onboarding instead: {e:#}");
            nav_cb.replace(&[hardware_page(&nav_cb, &state_cb, &backend_cb, &monitor)]);
        }
    });

   let (width, height) = crate::window_state::read().unwrap_or((460i32, 560i32));
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Bubbles")
        .default_width(width)
        .default_height(height)
        .content(&nav)
        .build();
    crate::window_state::install(&window);
    window
}

/// Hand off to the messaging UI, pulling the live session out of `state`.
fn go_messaging(nav: &adw::NavigationView, state: &Shared, backend: &Arc<dyn Backend>, monitor: &Arc<PowerMonitor>) {
    let (store, connection, client, handles) = {
        let s = state.borrow();
        (
            s.store.clone().expect("store set at startup"),
            s.connection.clone().expect("connection set"),
            s.client.clone().expect("client set"),
            s.handles.clone(),
        )
    };
    crate::ui::enter_messaging(nav, backend, store, connection, client, handles, monitor);
}
/// Brief placeholder shown while [`flow::restore`] checks for a saved session (spinner only, no text)
/// Brief placeholder shown while [`flow::restore`] checks for a saved session.
fn restoring_page() -> adw::NavigationPage {
    let content = column();
    content.set_valign(gtk::Align::Center);

    let spinner = gtk::Spinner::new();
    spinner.set_size_request(32, 32);
    spinner.start();
    content.append(&spinner);


    nav_page("Bubbles", &content)
}

// --- small scaffolding helpers ---

fn column() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build()
}

fn nav_page(title: &str, content: &impl IsA<gtk::Widget>) -> adw::NavigationPage {
    let clamp = adw::Clamp::builder()
        .maximum_size(420)
        .child(content)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&clamp));
    adw::NavigationPage::builder()
        .title(title)
        .child(&toolbar)
        .build()
}

fn error_label() -> gtk::Label {
    let label = gtk::Label::builder()
        .wrap(true)
        .visible(false)
        .xalign(0.0)
        .build();
    label.add_css_class("error");
    label
}

fn show_error(label: &gtk::Label, err: &anyhow::Error) {
    eprintln!("setup error: {err:#}");
    label.set_text(&format!("{err}"));
    label.set_visible(true);
}

fn pill(label: &str) -> gtk::Button {
    let b = gtk::Button::with_label(label);
    b.add_css_class("suggested-action");
    b.add_css_class("pill");
    b.set_halign(gtk::Align::Center);
    b
}

// --- page 1: hardware token (local Mac) or relay pairing ---

/// Shared success/failure handler for both hardware paths: stash the connected
/// state and advance to the login page, or surface the error.
fn finish_hardware(
    result: anyhow::Result<flow::HardwareReady>,
    nav: &adw::NavigationView,
    state: &Shared,
    backend: &Arc<dyn Backend>,
    errors: &gtk::Label,
    btn: &gtk::Button,
    monitor: &Arc<PowerMonitor>,
) {
    btn.set_sensitive(true);
    match result {
        Ok(ready) => {
            {
                let mut s = state.borrow_mut();
                s.config = Some(ready.config);
                s.identity = Some(ready.connected.identity);
                s.connection = Some(ready.connected.connection);
                s.anisette = Some(ready.connected.anisette);
                s.login_state = LoginState::NeedsLogin;
            }
            nav.push(&login_page(nav, state, backend, &ready.device, monitor));
        }
        Err(e) => show_error(errors, &e),
    }
}

fn hardware_page(
    nav: &adw::NavigationView,
    state: &Shared,
    backend: &Arc<dyn Backend>,
    monitor: &Arc<PowerMonitor>,
) -> adw::NavigationPage {
    let content = column();

    let intro = gtk::Label::builder()
        .label("Pair with your own Mac's hardware, or use the OpenBubbles hosted relay.")
        .wrap(true)
        .xalign(0.0)
        .build();
    content.append(&intro);

    let errors = error_label();

    // -- local macOS hardware: no relay, no reservation --
    let local_group = adw::PreferencesGroup::builder()
        .title("Local Mac hardware")
        .description(
            "Paste validation data exported once from a Mac, or a hardware blob \
             exported from a previous setup. After this the app regenerates its \
             own validation data — the Mac isn't needed again.",
        )
        .build();
    let blob = adw::EntryRow::builder()
        .title("Validation data / hardware blob (base64)")
        .build();
    local_group.add(&blob);
    let use_local = pill("Use local Mac hardware");
    content.append(&local_group);
    content.append(&use_local);

    {
        let nav = nav.clone();
        let state = state.clone();
        let backend = backend.clone();
        let blob = blob.clone();
        let errors = errors.clone();
        let btn = use_local.clone();
        let monitor = Arc::clone(monitor);
        use_local.connect_clicked(move |_| {
            let text = blob.text().to_string();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return;
            }
            let decoded = gtk::glib::base64_decode(trimmed);
            // Same discrimination as upstream hw_inp.dart: an "OABS"-prefixed
            // blob is a cached bbhwinfo export (strip the 4-byte magic + 1 flag
            // byte); a 517-byte payload starting 0x02 is raw validation data.
            let fut: std::pin::Pin<
                Box<dyn std::future::Future<Output = anyhow::Result<flow::HardwareReady>> + Send>,
            > = if decoded.len() > 5 && decoded.starts_with(b"OABS") {
                Box::pin(flow::connect_encoded(backend.clone(), decoded[5..].to_vec()))
            } else if decoded.len() == 517 && decoded[0] == 0x02 {
                Box::pin(flow::connect_local(backend.clone(), decoded))
            } else {
                show_error(
                    &errors,
                    &anyhow::anyhow!(
                        "Unrecognized data. Expected base64 macOS validation data, \
                         or a hardware blob exported from OpenBubbles."
                    ),
                );
                return;
            };
            btn.set_sensitive(false);
            errors.set_visible(false);

            let nav = nav.clone();
            let state = state.clone();
            let backend = backend.clone();
            let errors = errors.clone();
            let btn2 = btn.clone();
            let monitor = monitor.clone();
            gtk_bridge::spawn(fut, move |result| {
                finish_hardware(result, &nav, &state, &backend, &errors, &btn2, &monitor);
            });
        });
    }

    // -- hosted relay: requires an active OpenBubbles reservation --
    let relay_group = adw::PreferencesGroup::builder()
        .title("Hosted relay")
        .description("Requires an active OpenBubbles device reservation.")
        .build();
    let code = adw::EntryRow::builder().title("Pairing code").build();
    relay_group.add(&code);
    let connect = pill("Connect via relay");
    content.append(&relay_group);
    content.append(&connect);

    content.append(&errors);

    {
        let nav = nav.clone();
        let state = state.clone();
        let backend = backend.clone();
        let code = code.clone();
        let errors = errors.clone();
        let btn = connect.clone();
        let monitor = Arc::clone(monitor);
        connect.connect_clicked(move |_| {
            let entered = code.text().to_string();
            if entered.is_empty() {
                return;
            }
            btn.set_sensitive(false);
            errors.set_visible(false);

            let fut = flow::connect_relay(
                backend.clone(),
                entered,
                relay_host(),
                relay_token(),
            );

            let nav = nav.clone();
            let state = state.clone();
            let backend = backend.clone();
            let errors = errors.clone();
            let btn2 = btn.clone();
            let monitor = monitor.clone();
            gtk_bridge::spawn(fut, move |result| {
                finish_hardware(result, &nav, &state, &backend, &errors, &btn2, &monitor);
            });
        });
    }

    nav_page("Hardware", &content)
}

// --- page 2: Apple ID login ---

fn login_page(
    nav: &adw::NavigationView,
    state: &Shared,
    backend: &Arc<dyn Backend>,
    device: &crate::protocol::DeviceInfo,
    monitor: &Arc<PowerMonitor>,
) -> adw::NavigationPage {
    let content = column();

    let subtitle = gtk::Label::builder()
        .label(format!(
            "Signing in as {} (macOS {})",
            device.name, device.os_version
        ))
        .wrap(true)
        .xalign(0.0)
        .build();
    subtitle.add_css_class("dim-label");

    let group = adw::PreferencesGroup::new();
    let apple_id = adw::EntryRow::builder().title("Apple ID").build();
    let password = adw::PasswordEntryRow::builder().title("Password").build();
    group.add(&apple_id);
    group.add(&password);

    let sign_in = pill("Sign In");
    let errors = error_label();

    content.append(&subtitle);
    content.append(&group);
    content.append(&sign_in);
    content.append(&errors);

    {
        let nav = nav.clone();
        let state = state.clone();
        let backend = backend.clone();
        let apple_id = apple_id.clone();
        let password = password.clone();
        let errors = errors.clone();
        let sign_in_btn = sign_in.clone();
        let monitor = Arc::clone(monitor);
        sign_in.connect_clicked(move |_| {
            let id = apple_id.text().to_string();
            let pw = password.text().to_string();
            if id.is_empty() || pw.is_empty() {
                return;
            }
            sign_in_btn.set_sensitive(false);
            errors.set_visible(false);

            // clone the handles the flow needs out of state, then drop the borrow
            let (config, connection, anisette, login_state) = {
                let s = state.borrow();
                (
                    s.config.clone().unwrap(),
                    s.connection.clone().unwrap(),
                    s.anisette.clone().unwrap(),
                    s.login_state.clone(),
                )
            };

            let fut = flow::advance_login(
                flow::LoginContext {
                    backend: backend.clone(),
                    config,
                    anisette,
                },
                connection,
                None,
                None,
                Some((id, pw)),
                login_state,
            );

            let nav = nav.clone();
            let state = state.clone();
            let backend = backend.clone();
            let errors = errors.clone();
            let sign_in_btn = sign_in_btn.clone();
            let monitor = monitor.clone();
            gtk_bridge::spawn(fut, move |result| {
                sign_in_btn.set_sensitive(true);
                match result {
                    Ok(adv) => {
                        {
                            let mut s = state.borrow_mut();
                            s.account = adv.account;
                            s.apple_user = adv.apple_user;
                            s.circle = adv.circle;
                            if let LoginState::NeedsSms2FaVerification(body) = &adv.state {
                                s.verify_body = Some(body.clone());
                            }
                            s.login_state = adv.state.clone();
                        }
                        route_after_login(&nav, &state, &backend, &errors, &monitor);
                    }
                    Err(e) => show_error(&errors, &e),
                }
            });
        });
    }

    nav_page("Apple ID", &content)
}

/// After login resolves: either ask for a 2FA code or go straight to register.
fn route_after_login(nav: &adw::NavigationView, state: &Shared, backend: &Arc<dyn Backend>, errors: &gtk::Label, monitor: &Arc<PowerMonitor>) {
    let login_state = state.borrow().login_state.clone();
    match login_state {
        LoginState::Needs2FaVerification | LoginState::NeedsSms2FaVerification(_) => {
            nav.push(&two_fa_page(nav, state, backend, monitor));
        }
        LoginState::LoggedIn => do_register(nav, state, backend, errors, monitor),
        other => eprintln!("unexpected post-login state: {other:?}"),
    }
}

// --- page 3: 2FA code ---

fn two_fa_page(nav: &adw::NavigationView, state: &Shared, backend: &Arc<dyn Backend>, monitor: &Arc<PowerMonitor>) -> adw::NavigationPage {
    let content = column();

    let prompt = gtk::Label::builder()
        .label("Enter the verification code Apple just sent.")
        .wrap(true)
        .xalign(0.0)
        .build();

    let group = adw::PreferencesGroup::new();
    let code = adw::EntryRow::builder().title("Code").build();
    group.add(&code);

    let verify = pill("Verify");
    let errors = error_label();

    content.append(&prompt);
    content.append(&group);
    content.append(&verify);
    content.append(&errors);

    {
        let nav = nav.clone();
        let state = state.clone();
        let backend = backend.clone();
        let code = code.clone();
        let errors = errors.clone();
        let verify_btn = verify.clone();
        let monitor = Arc::clone(monitor);
        verify.connect_clicked(move |_| {
            let entered = code.text().to_string();
            if entered.is_empty() {
                return;
            }
            verify_btn.set_sensitive(false);
            errors.set_visible(false);

            let (config, anisette, account, circle, verify_body, login_state) = {
                let s = state.borrow();
                (
                    s.config.clone().unwrap(),
                    s.anisette.clone().unwrap(),
                    s.account.clone().unwrap(),
                    s.circle.clone(),
                    s.verify_body.clone(),
                    s.login_state.clone(),
                )
            };

            let fut = flow::submit_code(
                flow::LoginContext {
                    backend: backend.clone(),
                    config,
                    anisette,
                },
                account,
                circle,
                verify_body,
                login_state,
                entered,
            );

            let nav = nav.clone();
            let state = state.clone();
            let backend = backend.clone();
            let errors = errors.clone();
            let verify_btn = verify_btn.clone();
            let monitor = monitor.clone();
            gtk_bridge::spawn(fut, move |result| {
                verify_btn.set_sensitive(true);
                match result {
                    Ok(res) => {
                        {
                            let mut s = state.borrow_mut();
                            if res.apple_user.is_some() {
                                s.apple_user = res.apple_user;
                            }
                            s.login_state = res.state.clone();
                        }
                        match res.state {
                            LoginState::LoggedIn => do_register(&nav, &state, &backend, &errors, &monitor),
                            _ => errors.set_text("Incorrect code, try again."),
                        }
                        errors.set_visible(!matches!(state.borrow().login_state, LoginState::LoggedIn));
                    }
                    Err(e) => show_error(&errors, &e),
                }
            });
        });
    }

    nav_page("Verification", &content)
}

// --- step 4: register, then finalize ---

fn do_register(nav: &adw::NavigationView, state: &Shared, backend: &Arc<dyn Backend>, errors: &gtk::Label, monitor: &Arc<PowerMonitor>) {
    let (config, connection, identity, apple_user) = {
        let s = state.borrow();
        (
            s.config.clone().unwrap(),
            s.connection.clone().unwrap(),
            s.identity.clone().unwrap(),
            s.apple_user.clone(),
        )
    };

    let fut = flow::register(backend.clone(), config, connection, identity, apple_user);

    let nav = nav.clone();
    let state = state.clone();
    let errors = errors.clone();
    let backend = backend.clone();
    let monitor = Arc::clone(monitor);
    gtk_bridge::spawn(fut, move |result| match result {
        Ok(Ok(registered)) => {
            state.borrow_mut().handles = registered.handles.clone();
            state.borrow_mut().client = Some(registered.client);
            go_messaging(&nav, &state, &backend, &monitor);
        }
        Ok(Err(alert)) => {
            errors.set_text(&format!("{}: {}", alert.title, alert.body));
            errors.set_visible(true);
        }
        Err(e) => show_error(&errors, &e),
    });
}
