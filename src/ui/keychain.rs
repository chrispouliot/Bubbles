//! iCloud Keychain password / escrow dialog builders and the
//! `describe_escrow_metadata_for_user` display helper.
//!
//! Extracted from [`super`](mod.rs). All functions are `pub` and re-exported
//! at `crate::ui::*` to preserve the existing API surface.

use adw::prelude::*;

/// Build an `adw::AlertDialog` for prompting the user for both the old
/// trusted-device passcode (to recover the existing escrow bottle) and the
/// new local device password (to protect this device's new bottle). Both
/// are needed for [`Backend::setup_keychain_clique`]. The dialog is
/// pre-populated with the expected title, body, and two password entries.
/// The caller is responsible for presenting the dialog
/// (`dialog.present(Some(parent))`) and wiring the response handling.
///
/// The dialog has:
/// - Title: "iCloud Keychain Setup"
/// - Body: "Enter the passcode from a trusted device to unlock your iCloud
///   Keychain, then choose a new password for this device."
/// - A "Trusted Device Passcode" entry (`gtk::PasswordEntry`)
/// - A "New Device Password" entry (`gtk::PasswordEntry`)
/// - "Cancel" button (response id: "cancel")
/// - "Set Up" button (response id: "suggested", default appearance)
#[allow(dead_code)]
pub fn build_clique_password_dialog(
    _parent: Option<&adw::Window>,
    bottle_descriptions: &[String],
) -> adw::AlertDialog {
    let subtitle: &str = if bottle_descriptions.is_empty() {
        "Set up iCloud Keychain for this device. Choose a device password to protect your keychain."
    } else {
        "Select a device to recover from, then enter its passcode and a new password for this device."
    };
    let dialog = adw::AlertDialog::new(Some("iCloud Keychain Setup"), Some(subtitle));

    let box_ = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();

    // Bottle selection dropdown (shown when there are viable bottles).
    if !bottle_descriptions.is_empty() {
        let label = gtk::Label::new(Some("Recover from device:"));
        label.set_halign(gtk::Align::Start);
        label.add_css_class("heading");
        box_.append(&label);

        // Build a StringList from the descriptions. StringList::new takes &[&str].
        let desc_refs: Vec<&str> = bottle_descriptions.iter().map(|s| s.as_str()).collect();
        let model = gtk::StringList::new(&desc_refs);
        let dropdown = gtk::DropDown::builder()
            .model(&model)
            .selected(0u32)
            .hexpand(true)
            .build();
        box_.append(&dropdown);
    }

    // Passcode for recovering the selected escrow bottle.
    let escrow_entry = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .build();
    escrow_entry.set_placeholder_text(Some("Trusted Device Passcode"));

    // New password for THIS device's bottle.
    let device_entry = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .build();
    device_entry.set_placeholder_text(Some("New Device Password"));

    box_.append(&escrow_entry);
    box_.append(&device_entry);

    dialog.set_extra_child(Some(&box_));
    dialog.add_responses(&[("cancel", "Cancel"), ("suggested", "Set Up")]);
    dialog.set_response_appearance("suggested", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("suggested"));
    dialog.set_close_response("cancel");

    dialog
}

/// Build a closure suitable for the `password_prompt` parameter of
/// `orchestrate_sync_now_flow`. The closure, when called, presents the
/// iCloud Keychain setup dialog (with fields for both the old trusted-device
/// passcode and the new local device password), waits for the user's response,
/// and returns `Some((escrow_passcode, device_password))` on submit, or
/// `None` if the user cancelled.
///
/// `parent` is the parent window the dialog should be modal to.
///
/// The closure is `FnOnce` and synchronous — when called from a thread
/// where blocking is acceptable (e.g., a tokio worker thread spawned
/// via `crate::runtime::runtime().spawn`), it presents the dialog on
/// the GTK main thread and blocks the calling thread until the user
/// responds. This relies on glib's main-loop iteration during the
/// blocking wait to drive the response callback.
///
/// # Thread safety
///
/// The returned closure is `Send` (captures no non-Send types) so it
/// can be moved into a tokio task spawned via `runtime().spawn()`.
/// Inside, it uses `glib::MainContext::default().invoke` to schedule
/// the dialog presentation on the GTK main thread (this is the
/// thread-safe way to do GTK work from a non-GTK thread — `spawn_local`
/// panics with "already acquired by another thread" because the main
/// context is owned by the GTK main thread), and then blocks the
/// calling tokio worker thread on `rx.blocking_recv()` until the
/// response callback fires on the main thread.
#[allow(dead_code)]
pub fn build_password_prompt_closure(
    _parent: Option<&adw::Window>,
) -> impl FnOnce() -> Option<(String, String)> {
    // Discard the parent reference to avoid capturing a non-Send type.
    // The dialog is presented without an explicit parent window.
    let _ = _parent;
    move || {
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<(String, String)>>();
        // `glib::MainContext::default().invoke(...)` is the cross-thread way
        // to run GTK code from a tokio worker. It posts a sync closure to
        // the main context and blocks the calling thread until the main
        // thread has executed it. The closure is just "present the
        // dialog" — non-blocking from the main thread's perspective.
        // The user response comes later via the dialog's response signal,
        // which the main thread drives through its main loop while our
        // tokio thread blocks on `rx.blocking_recv()`.
        let empty_descriptions: Vec<String> = Vec::new();
        glib::MainContext::default().invoke(move || {
            let dialog = build_clique_password_dialog(None, &empty_descriptions);
            // Wrap `tx` in a `RefCell<Option<...>>` so it can be taken
            // once inside an `Fn` closure (signal handlers are `Fn`).
            let tx = std::cell::RefCell::new(Some(tx));
            dialog.connect_response(None, move |dialog, response_id| {
                let secrets = if response_id == "suggested" {
                    // The extra child is a GtkBox containing two PasswordEntry widgets.
                    dialog
                        .extra_child()
                        .and_downcast::<gtk::Box>()
                        .map(|box_| {
                            let children = box_.observe_children();
                            // When no bottle descriptions exist, the box has
                            // exactly two children, both PasswordEntry widgets
                            // (indices 0 and 1). When descriptions exist, a
                            // Label and DropDown precede them.
                            let entries: Vec<gtk::PasswordEntry> = (0..children.n_items())
                                .filter_map(|i| children.item(i).and_downcast::<gtk::PasswordEntry>())
                                .collect();
                            let escrow = entries
                                .first()
                                .map(|e| e.text().to_string())
                                .unwrap_or_default();
                            let device = entries
                                .get(1)
                                .map(|e| e.text().to_string())
                                .unwrap_or_default();
                            (escrow, device)
                        })
                } else {
                    None
                };
                if let Some(tx) = tx.borrow_mut().take() {
                    let _ = tx.send(secrets);
                }
            });
            dialog.present(None::<&gtk::Window>);
        });
        rx.blocking_recv().ok().flatten()
    }
}

/// Build a closure suitable for the `prompt` parameter of
/// `orchestrate_sync_now_flow_with_bottle_prompt`. Like
/// [`build_password_prompt_closure`] but the dialog also shows a
/// bottle-selection dropdown when `bottles` is non-empty, and the
/// returned closure yields [`CliqueSetupPromptResult`] which carries
/// the selected `EscrowData` plus both secrets.
///
/// When `bottles` is empty the dialog shows only the two password
/// fields (first-time establish path), but still returns a
/// `CliqueSetupPromptResult` with a default bottle — the calling
/// orchestrator should fall back to `setup_keychain_clique` in that
/// case.
///
/// # Thread safety
///
/// Same contract as [`build_password_prompt_closure`]: the returned
/// closure is `Send` and uses `glib::MainContext::invoke` to schedule
/// GTK work on the main thread.
pub fn build_bottle_aware_prompt_closure(
    _parent: Option<&adw::Window>,
    bottles: Vec<(crate::api::EscrowData, String)>,
) -> impl FnOnce() -> Option<crate::protocol::rustpush_backend::CliqueSetupPromptResult> {
    let _ = _parent;
    move || {
        let (tx, rx) = tokio::sync::oneshot::channel::<
            Option<crate::protocol::rustpush_backend::CliqueSetupPromptResult>,
        >();
        // The bottles are shared between the GTK invoke closure and the
        // response handler via an Arc.
        let bottles = std::sync::Arc::new(bottles);
        let descriptions: Vec<String> = bottles.iter().map(|(_, desc)| desc.clone()).collect();

        glib::MainContext::default().invoke(move || {
            let dialog = build_clique_password_dialog(None, &descriptions);
            let tx = std::cell::RefCell::new(Some(tx));
            let bottles = bottles.clone();
            dialog.connect_response(None, move |dialog, response_id| {
                let result = if response_id == "suggested" {
                    // 1. Read the selected bottle index from the DropDown (if any).
                    let selected_idx: usize = dialog
                        .extra_child()
                        .and_downcast::<gtk::Box>()
                        .and_then(|box_| {
                            let children = box_.observe_children();
                            (0..children.n_items())
                                .filter_map(|i| children.item(i).and_downcast::<gtk::DropDown>())
                                .next()
                                .map(|dd| dd.selected() as usize)
                        })
                        .unwrap_or(0);

                    // 2. Read the password fields (by type, ignore label/dropdown).
                    let secrets: Option<(String, String)> = dialog
                        .extra_child()
                        .and_downcast::<gtk::Box>()
                        .map(|box_| {
                            let children = box_.observe_children();
                            let entries: Vec<gtk::PasswordEntry> = (0..children.n_items())
                                .filter_map(|i| children.item(i).and_downcast::<gtk::PasswordEntry>())
                                .collect();
                            let escrow = entries
                                .first()
                                .map(|e| e.text().to_string())
                                .unwrap_or_default();
                            let device = entries
                                .get(1)
                                .map(|e| e.text().to_string())
                                .unwrap_or_default();
                            (escrow, device)
                        });

                    // 3. Build the prompt result when we have both secrets.
                    secrets.map(|(old, new)| {
                        let bottle = bottles[selected_idx.min(bottles.len().saturating_sub(1))]
                            .0
                            .clone();
                        crate::protocol::rustpush_backend::CliqueSetupPromptResult {
                            bottle,
                            old_passcode: old,
                            new_password: new,
                        }
                    })
                } else {
                    None
                };
                if let Some(tx) = tx.borrow_mut().take() {
                    let _ = tx.send(result);
                }
            });
            dialog.present(None::<&gtk::Window>);
        });
        rx.blocking_recv().ok().flatten()
    }
}

/// Pure display helper: turn [`rustpush::keychain::EscrowMetadata`] into
/// user-facing text containing enough information to know which device
/// credential to enter.
///
/// The output includes:
///   * device name OR model (so the user knows which device's passcode to
///     type), with the serial as a fallback if both name and model are absent;
///   * timestamp (so the user can disambiguate between multiple devices with
///     the same name);
///   * passcode type hints — specifically, a numeric passcode length when
///     `SecureBackupUsesNumericPassphrase` is set in the bottle's
///     `ClientMetadata`, so the user knows how many digits to type.
///
/// This is a pure function (no I/O, no GTK, no global state) so it can be
/// unit-tested without a display.
#[allow(dead_code)]
pub fn describe_escrow_metadata_for_user(meta: &rustpush::keychain::EscrowMetadata) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Device identity: prefer device name, fall back to model, then serial.
    let dict = meta.client_metadata.as_dictionary();
    let device_name = dict
        .and_then(|d| d.get("device_name"))
        .and_then(|v| v.as_string())
        .map(|s| s.to_string());
    let device_model = dict
        .and_then(|d| d.get("device_model"))
        .and_then(|v| v.as_string())
        .map(|s| s.to_string());

    // Device identity: show name + model (when both available), or whichever
    // is present, or the serial as last resort.
    match (&device_name, &device_model) {
        (Some(name), Some(model)) => {
            parts.push(name.clone());
            parts.push(model.clone());
        }
        (Some(name), None) => parts.push(name.clone()),
        (None, Some(model)) => parts.push(model.clone()),
        (None, None) => parts.push(meta.serial.clone()),
    }

    // Timestamp (allows disambiguation between multiple bottles).
    parts.push(meta.timestamp.clone());

    // Passcode type hint.
    let numeric_len = dict
        .and_then(|d| d.get("SecureBackupUsesNumericPassphrase"))
        .and_then(|v| v.as_boolean())
        .filter(|&b| b)
        .and_then(|_| dict.and_then(|d| d.get("SecureBackupNumericPassphraseLength")))
        .and_then(|v| v.as_signed_integer());
    let is_complex = dict
        .and_then(|d| d.get("SecureBackupUsesComplexPassphrase"))
        .and_then(|v| v.as_boolean())
        .unwrap_or(false);

    if let Some(len) = numeric_len {
        parts.push(format!("{len}-digit numeric passcode"));
    } else if is_complex {
        parts.push("complex passphrase".to_string());
    } else {
        parts.push("passcode".to_string());
    }

    parts.join(" — ")
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod clique_password_dialog_tests {
    use super::*;

    /// Compile-time signature check: `build_clique_password_dialog` must exist
    /// with the expected signature (now takes bottle descriptions). GTK widget
    /// instantiation requires a display, so this test only verifies the function
    /// is callable with the right types.
    #[test]
    fn build_clique_password_dialog_exists() {
        let _: fn(Option<&adw::Window>, &[String]) -> adw::AlertDialog = build_clique_password_dialog;
    }

    /// Compile-time signature check: `build_password_prompt_closure` must exist
    /// with the expected signature. GTK dialog interaction requires a display,
    /// so this test only verifies the function is callable with the right types
    /// and returns a closure matching the `FnOnce() -> Option<(String, String)>`
    /// contract required by `orchestrate_sync_now_flow`.
    #[test]
    fn build_password_prompt_closure_exists() {
        fn _assert_prompt_type(_: impl FnOnce() -> Option<(String, String)>) {}
        let closure = build_password_prompt_closure(None);
        _assert_prompt_type(closure);
    }
}

#[cfg(test)]
mod clique_escrow_metadata_display_tests {
    //! Pin: the UI/prompt layer must expose a pure display helper that
    //! turns `rustpush::keychain::EscrowMetadata` into user-facing text
    //! containing enough information to know which device credential to
    //! enter. Required fields:
    //!
    //!   * device name OR model (so the user knows which device's
    //!     passcode to type), with the serial as a fallback if both
    //!     name and model are absent;
    //!   * timestamp (so the user can disambiguate between multiple
    //!     devices with the same name);
    //!   * passcode type hints — specifically, a numeric passcode
    //!     length when `SecureBackupUsesNumericPassphrase` is set in
    //!     the bottle's `ClientMetadata`, so the user knows how many
    //!     digits to type.
    //!
    //! The helper is pure (no I/O, no GTK, no global state) so it can
    //! be unit-tested without a display — exactly the surface the unit
    //! spec calls out.

    /// Pin: when the bottle's `ClientMetadata` includes a device name,
    /// a device model, a timestamp, and a numeric passcode length, the
    /// display text must surface all of them so the user can match the
    /// credential entry to a specific device.
    #[test]
    fn describe_escrow_metadata_for_user_includes_device_identity_timestamp_and_passcode_hints() {
        use rustpush::keychain::EscrowMetadata;

        let mut client_meta = plist::Dictionary::new();
        client_meta.insert(
            "device_name".into(),
            plist::Value::String("Alice's iPhone".into()),
        );
        client_meta.insert(
            "device_model".into(),
            plist::Value::String("iPhone15,2".into()),
        );
        client_meta.insert(
            "SecureBackupUsesNumericPassphrase".into(),
            plist::Value::Boolean(true),
        );
        client_meta.insert(
            "SecureBackupNumericPassphraseLength".into(),
            plist::Value::Integer(6.into()),
        );

        let meta = EscrowMetadata {
            serial: "F2LXY9J7Q6L7".into(),
            build: "23A340".into(),
            passcode_generation: 2,
            timestamp: "2026-04-01T12:34:56Z".into(),
            bottle_id: "B-1234".into(),
            client_metadata: plist::Value::Dictionary(client_meta),
            escrowed_spki: plist::Data::new(Vec::new()),
            multiple_icsc: false,
        };

        // The pure helper must exist with this exact signature.
        let text = super::describe_escrow_metadata_for_user(&meta);

        // Device identity: the user must be able to identify the device.
        assert!(
            text.contains("Alice's iPhone"),
            "display text must include device_name from ClientMetadata so the user \
             knows which device to enter a passcode for; got: {text}"
        );
        assert!(
            text.contains("iPhone15,2"),
            "display text must include device_model from ClientMetadata so the user \
             can disambiguate devices with the same name; got: {text}"
        );

        // Timestamp: lets the user pick the right device when there
        // are multiple bottles from the same device name (e.g. an
        // iPhone and an iPad both named "Alice's iPhone").
        assert!(
            text.contains("2026-04-01"),
            "display text must include the bottle's timestamp so the user can \
             disambiguate between multiple bottles; got: {text}"
        );

        // Passcode type hint: numeric length tells the user how many
        // digits to type.
        assert!(
            text.contains('6'),
            "display text must include the numeric passcode length (6) from \
             ClientMetadata so the user knows how many digits to type; got: {text}"
        );
    }
}
