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
