//! The protocol facade.
//!
//! This defines the *call surface* the onboarding flow needs, mirroring the
//! sequence the upstream Dart app runs through `rust/src/api/api.rs`
//! (which is itself thin glue over `rustpush`).
//!
//! The handles below (`Config`, `Connection`, `Account`, ...) are opaque: the
//! flow never inspects them, it only threads them between `Backend` calls. The
//! real backend (the lifted, de-FRB'd `api.rs`) wraps the corresponding
//! `rustpush` `Arc<...>` types inside them; [`stub::StubBackend`] wraps unit
//! values so the UI compiles and is click-through-able today.

use async_trait::async_trait;

pub mod stub;

#[cfg(feature = "rustpush")]
use rustpush::ReactMessageType;

#[cfg(feature = "rustpush")]
pub mod rustpush_backend;

pub use anyhow::Result;

use crate::store::{ChatRef, IncomingMessage, SendErrorCategory, Store};

/// Generates an opaque, cheaply-cloneable, `Send + Sync` handle type.
macro_rules! opaque_handle {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone)]
        pub struct $name(pub std::sync::Arc<dyn std::any::Any + Send + Sync>);

        impl $name {
            pub fn new<T: std::any::Any + Send + Sync>(v: T) -> Self {
                Self(std::sync::Arc::new(v))
            }
            /// Recover the concrete inner value (used by the real backend).
            pub fn downcast<T: std::any::Any + Send + Sync>(&self) -> Option<&T> {
                self.0.downcast_ref::<T>()
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(concat!(stringify!($name), "(..)"))
            }
        }
    };
}

opaque_handle!(
    /// `rustpush::JoinedOSConfig` — the Apple "hardware token" / validation data.
    Config
);
opaque_handle!(
    /// `rustpush::APSConnection` — the APNs/courier connection.
    Connection
);
opaque_handle!(
    /// The anisette client.
    Anisette
);
opaque_handle!(
    /// `rustpush::IDSNGMIdentity`.
    Identity
);
opaque_handle!(
    /// `Arc<Mutex<AppleAccount<..>>>`.
    Account
);
opaque_handle!(
    /// A registered `rustpush::IDSUser`.
    IdsUser
);
opaque_handle!(
    /// `CircleClientSession` — carries trusted-device 2FA state.
    CircleSession
);
opaque_handle!(
    /// SMS-2FA context (`VerifyBody`) returned when an SMS code is requested.
    VerifyBody
);
opaque_handle!(
    /// `Arc<IMClient>` — the messaging client produced after registration.
    ImClient
);

/// Device identity extracted from a [`Config`], shown to the user during setup.
#[derive(Clone, Debug, Default)]
pub struct DeviceInfo {
    pub name: String,
    #[allow(dead_code)]
    pub serial: String,
    pub os_version: String,
}

/// Mirror of `rustpush` `api::HwExtra`. Fields are TODO until the real backend
/// is wired; only the validation-data path needs them (the relay path doesn't).
#[derive(Clone, Debug, Default)]
pub struct HwExtra {}

/// An Apple-side message that blocked registration (account locked, etc.).
#[derive(Clone, Debug)]
pub struct SupportAlert {
    pub title: String,
    pub body: String,
}

/// Result of `register_ids`.
#[derive(Debug)]
pub enum RegisterOutcome {
    /// Registration succeeded; carries the registered user set.
    Registered(Vec<IdsUser>),
    /// Apple blocked registration; surface the alert to the user.
    Blocked(SupportAlert),
}

/// A session restored from disk: the end-state of a completed
/// onboarding minus the login/2FA detour — equivalent to what the flow holds
/// after `register` + `make_imclient`. Lets a relaunch skip onboarding entirely.
#[derive(Debug)]
pub struct RestoredSession {
    pub config: Config,
    pub connection: Connection,
    pub identity: Identity,
    pub client: ImClient,
    pub handles: Vec<String>,
}

/// The login/2FA state machine, mirroring `rustpush`'s `LoginState`.
#[derive(Clone, Debug)]
#[derive(Default)]
pub enum LoginState {
    /// Need an Apple ID + password.
    #[default]
    NeedsLogin,
    /// Trusted-device 2FA must be triggered (push a code to Apple devices).
    NeedsDevice2Fa,
    /// SMS 2FA must be triggered.
    NeedsSms2Fa,
    /// A trusted-device code is now expected from the user.
    Needs2FaVerification,
    /// An SMS code is now expected from the user.
    NeedsSms2FaVerification(VerifyBody),
    /// Apple requires an extra step (carries a human-readable description).
    #[allow(dead_code)]
    NeedsExtraStep(String),
    /// Fully authenticated.
    LoggedIn,
}


/// The async surface over `rustpush`. The onboarding flow in [`crate::setup::flow`]
/// is written purely against this trait, so the stub and the real backend are
/// interchangeable.
///
/// Methods take `&Handle` and return owned handles; callers clone the cheap
/// `Arc`-backed handles before moving them onto the tokio runtime.
/// Registration status for the iMessage identity resource, reported by the
/// receive loop when the underlying IDS cert/key state changes.
#[derive(Clone, Debug, PartialEq)]
pub enum RegistrationStatus {
    /// The identity resource is healthy and registered.
    Registered,
    /// A fresh registration is in progress.
    Registering,
    /// A transient failure — the resource will retry in `retry_in_s` seconds.
    TransientFailure {
        /// Seconds until the next retry attempt.
        retry_in_s: u64,
        /// Human-readable error description.
        error: String,
    },
    /// A permanent failure: the user has been logged out by Apple (token
    /// revoked, account disabled, etc.). The UI should surface this and offer
    /// to re-onboard.
    LoggedOut {
        /// Human-readable error description.
        error: String,
    },
}

/// What the receive loop pulses to the UI. Stored events collapse to `Applied`
/// (the UI re-queries); typing is ephemeral and carried inline.
#[derive(Clone, Debug)]
pub enum RecvEvent {
    /// One or more stored events were applied — refresh.
    Applied,
    /// A link preview row was upserted in the store. Carries the `(guid, part_idx)`
    /// key the UI uses to find the message and replace its preview card in
    /// place — the spec is explicit that a full `reload_messages` on this event
    /// flickers and jumps scroll, so this is its own event with its own handler.
    LinkPreviewUpdated { guid: String, part_idx: i64 },
    /// A conversation's typing state changed. `chat_key` matches
    /// [`ChatRef::key`]; `from` is the sender's handle (for membership-based
    /// matching when the conversation's participant set differs from ours).
    Typing {
        chat_key: String,
        from: Option<String>,
        typing: bool,
        /// True when a stop (`typing == false`) was triggered by an incoming
        /// message arriving rather than an explicit typing-stop. Lets the UI keep
        /// the indicator until the rebuild swaps in the message (one reflow, no
        /// remove-then-add bounce) and animate the new bubble in.
        superseded: bool,
    },
    /// The identity resource registration state changed. Carried inline so the
    /// UI can update the registration badge / banner without a full re-query.
    Registration(RegistrationStatus),
}

#[async_trait]
pub trait Backend: Send + Sync {
    // --- 1. hardware token / validation data -> Config ---

    /// `api::config_from_relay` (hosted relay, e.g. `https://hw.openbubbles.app`).
    async fn config_from_relay(
        &self,
        code: String,
        host: String,
        token: Option<String>,
    ) -> Result<Config>;

    /// `api::config_from_validation_data` (your own Mac's validation data).
    async fn config_from_validation_data(&self, data: Vec<u8>, extra: HwExtra) -> Result<Config>;

    /// `api::config_from_encoded` (a cached `bbhwinfo` blob from a prior local
    /// pairing; the OABS magic + flag byte are stripped before calling this).
    async fn config_from_encoded(&self, encoded: Vec<u8>) -> Result<Config>;

    /// `api::get_device_info`.
    async fn device_info(&self, config: &Config) -> Result<DeviceInfo>;

    // --- 2. push connection + identity + anisette ---

    /// `api::new_ngm_identity`.
    fn new_identity(&self) -> Result<Identity>;

    /// `api::setup_push`.
    async fn setup_push(&self, config: &Config, identity: &Identity) -> Result<Connection>;

    /// `api::make_anisette`.
    async fn make_anisette(&self, config: &Config, conn: &Connection) -> Result<Anisette>;

    // --- 3. Apple ID login + 2FA ---

    /// `api::try_auth` — `creds` is `Some((apple_id, password))` on first login.
    async fn try_auth(
        &self,
        config: &Config,
        conn: &Connection,
        anisette: &Anisette,
        creds: Option<(String, String)>,
    ) -> Result<(Account, LoginState)>;

    /// `api::try_icloud_login`.
    async fn try_icloud_login(&self, config: &Config, account: &Account)
        -> Result<Option<IdsUser>>;

    /// `api::send_2fa_to_devices` (trusted-device path).
    async fn send_2fa_to_devices(
        &self,
        account: &Account,
        conn: &Connection,
    ) -> Result<(CircleSession, LoginState)>;

    /// `api::verify_2fa` (trusted-device code).
    async fn verify_2fa(
        &self,
        session: &CircleSession,
        anisette: &Anisette,
        config: &Config,
        account: &Account,
        code: String,
    ) -> Result<(LoginState, Option<IdsUser>)>;

    /// Wraps `api::get_2fa_sms_opts` + `api::send_2fa_sms`. The real impl picks a
    /// number (or surfaces the option list); here it just requests the code.
    async fn send_2fa_sms(&self, account: &Account) -> Result<LoginState>;

    /// `api::verify_2fa_sms` (SMS code).
    async fn verify_2fa_sms(
        &self,
        account: &Account,
        anisette: &Anisette,
        config: &Config,
        body: &VerifyBody,
        code: String,
    ) -> Result<(LoginState, Option<IdsUser>)>;

    // --- 4. IDS registration ---

    /// `api::register_ids`.
    async fn register_ids(
        &self,
        config: &Config,
        conn: &Connection,
        identity: &Identity,
        users: Vec<IdsUser>,
    ) -> Result<RegisterOutcome>;

    /// `api::make_imclient`.
    async fn make_imclient(
        &self,
        conn: &Connection,
        identity: &Identity,
        users: Vec<IdsUser>,
    ) -> Result<ImClient>;

    /// `api::get_handles`.
    async fn get_handles(&self, client: &ImClient) -> Result<Vec<String>>;

    // --- 5. session restore ---

    /// Attempt to restore a previously-registered session from the state dir.
    /// `Ok(None)` => nothing saved (run onboarding); `Ok(Some(_))` => restored
    /// and ready to message without re-registering with Apple.
    async fn restore_session(&self) -> Result<Option<RestoredSession>>;

    // --- 6. receive ---

    /// Spawn the detached receive loop: decode inbound pushes, persist each to
    /// `store`, and pulse `notify` after every applied event so the UI can
    /// refresh. Ephemeral signals (typing) are forwarded without being stored.
    /// No-op on backends without a live connection.
    ///
    /// Returns the receive loop's kick signal (`Arc<Notify>`). Callers may call
    /// `notify_one()` on it to trigger re-subscription (e.g. after a wake-from-sleep
    /// event).
    fn start_receiving(
        &self,
        connection: &Connection,
        client: &ImClient,
        handles: Vec<String>,
        store: Store,
        notify: async_channel::Sender<RecvEvent>,
    ) -> std::sync::Arc<tokio::sync::Notify>;

    // --- 7. send ---

    /// Send a text message to `chat` as `my_handle`. Returns the locally
    /// persistable record (already flagged `is_from_me`) on success.
    async fn send_text(
        &self,
        client: &ImClient,
        chat: &ChatRef,
        my_handle: &str,
        text: String,
        guid: String,
    ) -> Result<IncomingMessage>;

    /// Send a tapback (reaction) to a target message in `chat`.
    #[cfg(feature = "rustpush")]
    #[allow(clippy::too_many_arguments)]
    async fn send_reaction(
        &self,
        client: &ImClient,
        chat: &ChatRef,
        my_handle: &str,
        target_guid: &str,
        target_part: Option<u64>,
        target_text: &str,
        reaction: &ReactMessageType,
    ) -> Result<()>;

    /// Send an edit to a previously-sent message in `chat`.
    #[cfg(feature = "rustpush")]
    #[allow(clippy::too_many_arguments)]
    async fn send_edit(
        &self,
        client: &ImClient,
        chat: &ChatRef,
        my_handle: &str,
        target_guid: &str,
        edit_part: u64,
        new_text: String,
        new_guid: String,
    ) -> Result<()>;

    /// Upload a file to MMCS and send it as an attachment. Returns the locally
    /// persistable record (with a cached `local_path`) on success.
    /// `text` is the optional caption carried with the attachment.
    #[allow(clippy::too_many_arguments)]
    async fn send_attachment(
        &self,
        client: &ImClient,
        connection: &Connection,
        chat: &ChatRef,
        my_handle: &str,
        path: String,
        mime: String,
        name: String,
        text: Option<String>,
        guid: String,
    ) -> Result<IncomingMessage>;

    /// Fire-and-forget a delivered (`read=false`) or read (`read=true`) receipt
    /// for `target_guid` to `chat`'s participants.
    fn send_receipt(
        &self,
        client: &ImClient,
        chat: &ChatRef,
        my_handle: &str,
        read: bool,
        target_guid: String,
    );

    /// Fire-and-forget a typing indicator (start with `typing = true`, stop with
    /// `false`) to `chat`'s participants. Ephemeral; never stored.
    fn send_typing(&self, client: &ImClient, chat: &ChatRef, my_handle: &str, typing: bool);

    /// Wipe the persisted login so the next launch starts at onboarding.
    fn sign_out(&self);

    // --- 8. sync ---

    /// Run one CloudKit sync session to backfill missed messages. Called on
    /// launch (with a 48-hour cutoff for the first sync) and on wake (with
    /// no cutoff, since the continuation token tracks progress). Returns a
    /// `SyncResult` with stats; logs the result. The continuation token is
    /// in-memory only for now (a follow-up will persist it across launches).
    ///
    /// When `force` is true, the `cloud_sync_enabled` config gate and the
    /// post-failure backoff are bypassed. This is used by the manual "Sync
    /// Now" button so the user can pull missed messages on demand even when
    /// automatic cloud sync is disabled.
    #[cfg(feature = "rustpush")]
    async fn sync_missed_messages(
        &self,
        store: &Store,
        cutoff_ms: i64,
        force: bool,
    ) -> crate::sync::SyncResult;

    // --- 9. keychain clique setup ---

    /// Join the iCloud Keychain encryption trust group ("clique") using the
    /// old trusted-device passcode (to recover an existing escrow bottle) and
    /// the new local device password (to create this device's bottle). Required
    /// before any CloudKit-based sync (iMessage in iCloud, keychain sync) can
    /// succeed. Idempotent: if the clique is already set up, returns `Ok(())`
    /// without making any network calls.
    ///
    /// For first-time establish (no viable bottles) the `escrow_passcode` is
    /// ignored and only `device_password` is used to create the new clique.
    ///
    /// Errors are surfaced as a string describing what went wrong. A more
    /// typed error can come in a follow-up; for now `Result<(), String>` is
    /// fine because the caller in the UI only displays the error message.
    #[allow(dead_code)]
    async fn setup_keychain_clique(
        &self,
        escrow_passcode: &str,
        device_password: &str,
    ) -> std::result::Result<(), String>;

    /// Returns `true` if the iCloud Keychain clique is set up on this device,
    /// `false` if it is not. This is a *disk-only* check — it reads the
    /// persisted `keychain.plist` and reports whether a user identity is
    /// present. It does NOT make any network calls and does NOT require the
    /// AppleAccount to be reconstructed.
    ///
    /// Used by the UI to decide whether to prompt the user for their iCloud
    /// password (clique not set up) before triggering a CloudKit sync.
    #[allow(dead_code)]
    async fn is_keychain_clique_set_up(&self) -> bool;

    /// Join the iCloud Keychain clique using a specific, user-selected escrow
    /// bottle. This is the bottle-aware variant of [`setup_keychain_clique`]:
    /// instead of silently picking the first viable bottle, the caller has
    /// already shown the user a list of viable bottles and obtained a selection.
    ///
    /// The default implementation returns an error; production backends that
    /// support bottle selection must override this.
    #[allow(dead_code)]
    async fn setup_keychain_clique_with_bottle(
        &self,
        _selected_bottle: &crate::api::EscrowData,
        _escrow_passcode: &str,
        _device_password: &str,
    ) -> std::result::Result<(), String> {
        Err("setup_keychain_clique_with_bottle: not implemented".to_string())
    }

    /// Returns viable escrow bottles for the user's iCloud Keychain clique.
    /// Each tuple carries the opaque `EscrowData` (needed by
    /// [`setup_keychain_clique_with_bottle`]) and a user-facing description
    /// string (e.g. "Alice's iPhone — iPhone15,2 — 2026-04-01 — 6-digit
    /// numeric passcode") so the UI can present a selection list.
    ///
    /// Returns a [`BottlesLookup`] that distinguishes three states: bottles
    /// present, no bottles (first-time setup), and unavailable/error. The
    /// pre-fix fallback-to-establish behaviour on error was the bug; the
    /// caller should use [`decide_bottles_lookup_action`] to determine the
    /// correct action and surface errors instead of silently treating them
    /// as no-bottles.
    #[allow(dead_code)]
    async fn get_viable_escrow_bottles(&self) -> BottlesLookup {
        BottlesLookup::NoBottles
    }
}

/// Walk the error chain and return the [`SendErrorCategory`] that best
/// describes the failure. Used both for persistence and for friendly messages.
///
///  * `TimedOut` → [`SendErrorCategory::Timeout`]
///  * `ConnectionReset`, `ConnectionAborted`, `BrokenPipe`, `UnexpectedEof`
///    → [`SendErrorCategory::ConnectionLost`]
///  * Everything else → [`SendErrorCategory::Other`]
pub fn categorize_send_error(err: &anyhow::Error) -> SendErrorCategory {
    for cause in err.chain() {
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>() {
            match io_err.kind() {
                std::io::ErrorKind::TimedOut => return SendErrorCategory::Timeout,
                std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::UnexpectedEof => {
                    return SendErrorCategory::ConnectionLost;
                }
                _ => {}
            }
        }
    }
    SendErrorCategory::Other
}

/// Map a [`SendErrorCategory`] to a short, user-facing string suitable for a
/// popover or tooltip.
pub fn friendly_category_message(cat: SendErrorCategory) -> String {
    match cat {
        SendErrorCategory::Timeout => "Connection timed out. Please try again.".into(),
        SendErrorCategory::ConnectionLost => "Lost connection. Please try again.".into(),
        SendErrorCategory::Other => "Couldn't send. Please try again.".into(),
    }
}

/// What the UI should do when the user requests a CloudKit sync.
/// The function checks whether the iCloud Keychain clique is set up and
/// returns the appropriate action.
///
/// - [`CliqueSetupAction::SyncNow`] — the clique is set up, the caller should
///   proceed with the sync (no password needed).
/// - [`CliqueSetupAction::PromptForPassword`] — the clique is not set up, the
///   caller should show the password dialog, and on user-submit call
///   `run_clique_setup_then_sync` with `password = Some(...)`.
/// - [`CliqueSetupAction::Abort`] — the backend cannot service the request
///   (e.g., account not reconstructed). The caller should display the reason
///   and abort.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliqueSetupAction {
    /// The clique is set up; proceed with the sync.
    SyncNow,
    /// The clique is not set up; prompt for a password.
    PromptForPassword,
    /// The backend cannot service the request.
    Abort(String),
}

/// Result of looking up viable escrow bottles from the iCloud Keychain.
///
/// This distinguishes three states that the pre-fix code collapsed into a
/// single empty-Vec signal, causing bottle-lookup errors to be silently
/// treated as "no bottles" (showing the old two-textbox first-time establish
/// prompt instead of surfacing the error).
#[derive(Debug, Clone, PartialEq)]
pub enum BottlesLookup {
    /// Viable escrow bottles exist. The Vec is non-empty (but callers should
    /// not rely on that invariant; an empty Vec is treated as `NoBottles` by
    /// the decision helper).
    Bottles(Vec<(crate::api::EscrowData, String)>),
    /// No viable bottles exist — legitimate first-time establish situation.
    NoBottles,
    /// Bottle lookup failed / backend cannot service the request. Carries
    /// a human-readable reason for logging / surfacing.
    Unavailable(String),
}

/// What the UI should do after a bottle lookup, driven by
/// [`decide_bottles_lookup_action`].
#[derive(Debug, Clone, PartialEq)]
pub enum BottlesLookupAction {
    /// Viable bottles exist; show a bottle-selection dialog and use the
    /// bottle-aware orchestrator path.
    ShowBottleSelection(Vec<(crate::api::EscrowData, String)>),
    /// No viable bottles; use the first-time establish path (old two-textbox
    /// prompt).
    EstablishFirstTime,
    /// Bottle lookup failed / backend cannot service the request. Surface
    /// the error reason instead of showing any prompt.
    SurfaceError(String),
}

/// Map a [`BottlesLookup`] result to the corresponding
/// [`BottlesLookupAction`] the UI should take.
///
/// This is a pure function: no I/O, no async, no display dependency.
#[allow(dead_code)]
pub fn decide_bottles_lookup_action(lookup: BottlesLookup) -> BottlesLookupAction {
    match lookup {
        BottlesLookup::Bottles(bottles) => {
            BottlesLookupAction::ShowBottleSelection(bottles)
        }
        BottlesLookup::NoBottles => BottlesLookupAction::EstablishFirstTime,
        BottlesLookup::Unavailable(reason) => BottlesLookupAction::SurfaceError(reason),
    }
}

/// Decide what the UI should do when the user requests a CloudKit sync.
///
/// This function is testable without a display because it doesn't touch GTK.
/// It only calls the backend's [`Backend::is_keychain_clique_set_up`] method.
///
/// # Stub (to be filled in)
///
/// TODO: implement the real body — check `is_keychain_clique_set_up` and
/// return the appropriate variant.  The test currently fails because this
/// function returns a placeholder value.
#[allow(dead_code)]
pub async fn decide_clique_setup_action(backend: &dyn Backend) -> CliqueSetupAction {
    if backend.is_keychain_clique_set_up().await {
        CliqueSetupAction::SyncNow
    } else {
        CliqueSetupAction::PromptForPassword
    }
}

#[cfg(test)]
mod registration_status_tests {
    use super::*;

    #[test]
    fn registration_status_enum_derives_and_variants() {
        // Pin: RegistrationStatus exists with Clone + Debug + PartialEq,
        // and all four variants can be constructed.
        //
        // registered / healthy
        let _registered = RegistrationStatus::Registered;
        // re-registering in progress
        let _registering = RegistrationStatus::Registering;
        // transient failure
        let _transient = RegistrationStatus::TransientFailure {
            retry_in_s: 42,
            error: "timeout".into(),
        };
        // permanent failure (logged out by Apple)
        let _logged_out = RegistrationStatus::LoggedOut {
            error: "token revoked".into(),
        };

        // Pin Clone
        let cloned = RegistrationStatus::Registered.clone();
        assert_eq!(cloned, RegistrationStatus::Registered);

        // Pin PartialEq: different variants are unequal
        assert_ne!(
            RegistrationStatus::Registering,
            RegistrationStatus::Registered,
        );

        // Pin Debug via format!
        let debug_str = format!(
            "{:?}",
            RegistrationStatus::TransientFailure {
                retry_in_s: 10,
                error: "err".into(),
            }
        );
        assert!(!debug_str.is_empty(), "Debug output should not be empty");
    }

    #[test]
    fn registration_status_recv_event_variant() {
        // Pin: RecvEvent::Registration(RegistrationStatus) exists.
        let status = RegistrationStatus::Registered;
        let event = RecvEvent::Registration(status.clone());
        match &event {
            RecvEvent::Registration(s) => assert_eq!(*s, status),
            other => panic!("expected Registration variant, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod clique_setup_action_tests {
    use super::*;
    use crate::protocol::stub::StubBackend;

    /// Pin: `decide_clique_setup_action` exists and, when called with a
    /// `StubBackend` whose `is_keychain_clique_set_up` returns `false`,
    /// returns `CliqueSetupAction::PromptForPassword`.
    #[tokio::test]
    async fn stub_backend_returns_prompt_for_password() {
        let backend = StubBackend::default();
        let action = decide_clique_setup_action(&backend).await;
        assert_eq!(action, CliqueSetupAction::PromptForPassword);
    }
}

#[cfg(test)]
mod clique_setup_bottles_lookup_tests {
    //! Pin: the manual Sync Now flow must not silently treat a failure
    //! to fetch viable escrow bottles as "no bottles" and show the old
    //! two-textbox first-time setup prompt. Bottle lookup must
    //! distinguish at least three states — bottles present, no bottles,
    //! and unavailable/error — so errors can be surfaced/logged and
    //! don't masquerade as first-time establish.
    //!
    //! The pre-fix `Backend::get_viable_escrow_bottles` returns
    //! `Vec<(EscrowData, String)>` defaulting to empty, and the real
    //! backend's implementation (in `rustpush_backend.rs` ≈line 1630)
    //! has multiple silent `return Vec::new()` paths: reconstruct
    //! failure, missing `keychain.plist`, and
    //! `keychain.get_viable_bottles().await` error. The UI handler at
    //! `ui/mod.rs` ≈line 2105 then branches on `bottles.is_empty()`
    //! and shows the old establish prompt — silently treating errors
    //! as "no bottles". This is the user-reported bug: Sync Now shows
    //! no device dropdown, only the two text boxes, because the
    //! backend's `Vec` return collapsed an error into the empty path.
    //!
    //! The fix introduces `super::BottlesLookup` (an enum with three
    //! variants) and a `super::decide_bottles_lookup_action` helper
    //! that maps the three states to three distinct actions. The
    //! `Unavailable` variant carries the error reason and maps to a
    //! non-establish action so errors are surfaced/logged and don't
    //! masquerade as first-time establish.
    //!
    //! This test fails to compile under the pre-fix code because
    //! `super::BottlesLookup` and `super::decide_bottles_lookup_action`
    //! do not exist. The compile error is the expected red per the
    //! unit's spec (the seams are the deliverable, not a fake stub).
    //!
    //! Test isolation: the test constructs `BottlesLookup` values
    //! directly and calls a pure function; no backend, no shared
    //! state, no env vars.

    /// Pin: the bottle lookup must distinguish three states, and the
    /// `Unavailable` state must NOT collapse to the same action as
    /// `NoBottles`. The pre-fix bug collapsed errors into the
    /// establish path; the new contract must keep them distinct so the
    /// UI can surface the error instead of showing the old two-textbox
    /// first-time setup prompt.
    #[test]
    fn clique_setup_bottles_lookup_unavailable_does_not_collapse_to_no_bottles() {
        // State 1: bottles present (empty Vec here is just to construct
        // the variant; the test only cares about variant identity).
        let with_bottles = super::BottlesLookup::Bottles(Vec::new());
        // State 2: legitimate first-time setup (no bottles exist).
        let no_bottles = super::BottlesLookup::NoBottles;
        // State 3: error / backend cannot service the request.
        let unavailable =
            super::BottlesLookup::Unavailable("reconstruct failed".to_string());

        // Map each state to its action via the decision helper. The
        // helper must exist with a sync signature taking a
        // `BottlesLookup` and returning a comparable action.
        let action_with_bottles =
            super::decide_bottles_lookup_action(with_bottles);
        let action_no_bottles = super::decide_bottles_lookup_action(no_bottles);
        let action_unavailable =
            super::decide_bottles_lookup_action(unavailable);

        // CRITICAL: `Unavailable` must not map to the same action as
        // `NoBottles`. The pre-fix bug collapsed errors into the
        // establish path; the new contract must keep them distinct so
        // the UI can surface the error instead of showing the old
        // two-textbox first-time setup prompt.
        assert_ne!(
            action_unavailable, action_no_bottles,
            "Unavailable must not collapse to the same action as NoBottles; \
             the pre-fix bug silently showed the old two-textbox first-time \
             setup prompt when bottle lookup failed. \
             Unavailable→{action_unavailable:?}, NoBottles→{action_no_bottles:?}"
        );

        // All three states must map to distinct actions (sanity check
        // that the lookup has three distinguishable outcomes, not two).
        assert_ne!(
            action_with_bottles, action_no_bottles,
            "Bottles and NoBottles must map to distinct actions"
        );
        assert_ne!(
            action_with_bottles, action_unavailable,
            "Bottles and Unavailable must map to distinct actions"
        );
    }
}
