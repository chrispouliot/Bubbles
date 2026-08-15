//! The real backend: implements [`Backend`] over `rustpush` + the vendored
//! `api.rs` subset (exposed as `crate::api`). Where [`stub`](super::stub) is
//! a no-op for offline iteration, this is the full Apple-stack path: Apple
//! ID login (NAC validation goes through the locally-vendored
//! `open-absinthe` crate), 2FA via SMS or trusted device, message
//! send/receive, tapbacks, attachments, link previews.
//!
//! Gated by the `rustpush` Cargo feature, which is in the workspace's
//! `default` set, so this module compiles in normal `cargo build` runs.
//! `main` constructs the corresponding `Arc<dyn Backend>` at startup;
//! `--no-default-features` drops the rustpush dep entirely and falls back
//! to [`stub`](super::stub).
//!
//! Handle mapping (our opaque handle <- concrete type):
//!   Config        <- api::JoinedOSConfig
//!   Connection    <- ConnHandle { conn: APSConnection, idms: Arc<IdmsAuthListener> }
//!   Anisette      <- ArcAnisetteClient<DefaultAnisetteProvider>
//!   Identity      <- IDSNGMIdentity
//!   Account       <- Arc<Mutex<AppleAccount<DefaultAnisetteProvider>>>
//!   IdsUser       <- IDSUser
//!   CircleSession <- CircleHandle { session+watcher behind a Mutex, + idms }
//!   VerifyBody    <- rustpush::VerifyBody
//!   ImClient      <- Arc<IMClient>

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use tokio::sync::{broadcast, Mutex};

use rustpush::{
    APSConnection, APSMessage, AppleAccount, ArcAnisetteClient, Attachment,
    CircleClientSession, ConversationData, DebugMutex, DebugRwLock, DefaultAnisetteProvider,
    EditMessage, IDSError, IDSNGMIdentity, IDSUser, IMClient, IdmsAuthListener,
    IndexedMessagePart, LPLinkMetadata, LinkMeta, LoginState as RpLoginState, MMCSFile,
    Message, MessageInst, MessagePart, MessageParts, MessageType, NormalMessage, OSConfig,
    PushError, ReactMessage, Reaction, ReactMessageType, SendResult, TokenProvider,
    VerifyBody as RpVerifyBody,
};

use crate::store::{
    AttachmentRecord, ChatRef, IncomingMessage, Ingest, MessageLinkPreview, Receipt, Store,
    Tapback,
};

use crate::api;
use crate::api::buffered_conn::BufferedApsConn;
use crate::protocol::*;

type Anis = ArcAnisetteClient<DefaultAnisetteProvider>;
// api.rs uses `rustpush::DebugMutex as Mutex`, so the account is wrapped in DebugMutex.
type AppleAcct = Arc<DebugMutex<AppleAccount<DefaultAnisetteProvider>>>;

/// Connection + the idms listener created alongside it (needed for 2FA verify).
struct ConnHandle {
    conn: Arc<BufferedApsConn>,
    idms: Arc<IdmsAuthListener>,
}

/// Trusted-device 2FA session: the circle session and the APS watcher subscribed
/// *before* the push was sent, plus the idms listener. All behind one Mutex so
/// `verify_2fa` can take `&mut` to the session and the receiver at once.
struct CircleHandle {
    inner: Mutex<(CircleClientSession<DefaultAnisetteProvider>, broadcast::Receiver<APSMessage>)>,
    idms: Arc<IdmsAuthListener>,
}

pub struct RustpushBackend {
    state_path: String,
    /// In-memory AppleAccount handle; used by `sync_missed_messages` to build
    /// the TokenProvider / CloudKitClient / KeychainClient for batch sync.
    /// Populated after a successful login (via `try_auth`).
    /// Not persisted across launches.
    account: StdMutex<Option<AppleAcct>>,
    /// In-memory anisette client handle; used by `sync_missed_messages`.
    /// Populated after `make_anisette` is called.
    anisette: StdMutex<Option<Anis>>,
    /// In-memory OS config; used by `sync_missed_messages`.
    /// Populated by `setup_push` (or `restore_session`).
    config: StdMutex<Option<api::JoinedOSConfig>>,
    /// Stored APS connection; used by `reconstruct_account` to call
    /// `try_auth` with `creds: None`.
    /// Populated by `setup_push` (or `restore_session`).
    conn: StdMutex<Option<Arc<BufferedApsConn>>>,
}

impl RustpushBackend {
    pub fn new(state_path: impl Into<String>) -> Self {
        // Caller must have run api::do_first_time_init(&state_path) once at boot.
        Self {
            state_path: state_path.into(),
            account: StdMutex::new(None),
            anisette: StdMutex::new(None),
            config: StdMutex::new(None),
            conn: StdMutex::new(None),
        }
    }

    /// Reconstruct the AppleAccount from the encrypted credentials in
    /// `gsa.plist` via `login_email_pass`. On success, stores the
    /// `AppleAccount` in `self.account` and returns `Ok(())`. On failure
    /// (missing gsa.plist, decryption error, network auth failure), logs
    /// a warning and returns `Err(())` without updating `self.account`.
    ///
    /// This is the auth path that lets the sync run on every launch. It
    /// also regenerates the IDS cert (via the `do_login` step) so that
    /// Apple's server-side state is fresh on every launch — this is what
    /// keeps the iMessage cert from going stale every few hours. The
    /// previous "MME cache" optimization that skipped `do_login` when the
    /// MME was fresh was removed because it caused the cert to stay bad
    /// when the rereg failed (Apple's auth state rotates faster than the
    /// 7-day MME window).
    ///
    /// When `force` is true, the `cloud_sync_enabled` config gate and the
    /// post-failure backoff are bypassed. This is used by the manual "Sync
    /// Now" button so the user can pull missed messages on demand even when
    /// automatic cloud sync is disabled or the last sync hit the backoff.
    async fn reconstruct_account(&self, force: bool) -> Result<Option<IDSUser>, ()> {
        let conn_arc = self.conn.lock().unwrap().clone();
        let conn = match conn_arc {
            Some(c) => c,
            None => {
                log::debug!("reconstruct_account: no stored connection, skipping");
                return Err(());
            }
        };
        let config = match self.config.lock().unwrap().clone() {
            Some(c) => c,
            None => {
                log::debug!("reconstruct_account: no stored config, skipping");
                return Err(());
            }
        };

        // Check that gsa.plist exists before attempting reconstruction.
        let state_dir = std::path::PathBuf::from(&self.state_path);
        let gsa_path = state_dir.join("gsa.plist");
        if !gsa_path.exists() {
            log::debug!("reconstruct_account: no gsa.plist at {}, skipping", gsa_path.display());
            return Err(());
        }

        // Check cloud sync enabled config (skipped when `force` is true so the
        // manual "Sync Now" button works regardless of the user's preference).
        if !force {
            let bubbles_config = crate::sync::read_config(
                &state_dir.join(crate::sync::CONFIG_FILENAME),
            );
            if !bubbles_config.cloud_sync_enabled {
                log::info!("reconstruct_account: cloud sync disabled in config, skipping");
                return Err(());
            }

            // Check sync backoff (also skipped when forced).
            let last_error_secs = crate::sync::read_last_sync_error(
                &state_dir.join(crate::sync::LAST_SYNC_ERROR_FILENAME),
            );
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if !crate::sync::should_sync_now(
                last_error_secs,
                now_secs,
                crate::sync::DEFAULT_BACKOFF_SECS,
            ) {
                log::info!(
                    "reconstruct_account: in sync backoff (last error at {:?}), skipping",
                    last_error_secs,
                );
                return Err(());
            }
        } else {
            log::info!("reconstruct_account: forced (manual sync) — skipping cloud_sync_enabled and backoff checks");
        }

        // Recreate the anisette from the stored connection.
        let inner = conn.inner();
        let anisette = api::make_anisette(self.state_path.clone(), &config, inner).await;
        // Store the anisette even if try_auth later fails — it's expensive
        // to recreate and may be reusable.
        *self.anisette.lock().unwrap() = Some(anisette.clone());

        // Call try_auth with creds: None — reads gsa.plist and decrypts the
        // password via GSAConfig::get_password() (uses the AES keystore
        // initialized at boot in do_first_time_init, no idms needed).
        match api::try_auth(
            self.state_path.clone(),
            &config,
            inner,
            &anisette,
            None,  // <- key: None means "reconstruct from gsa.plist"
        ).await {
            Ok((account, _login_state)) => {
                // Always run do_login. The previous "skip if MME is fresh"
                // optimization caused the IDS cert to stay bad whenever the
                // rereg failed (Apple's auth state rotates faster than the
                // 7-day MME window). Running do_login on every launch keeps
                // the Apple account state fresh so rustpush's auto-rereg
                // succeeds in the background.
                let new_user = match api::do_login(
                    self.state_path.clone(),
                    &account,
                    None,
                    &config,
                ).await {
                    Ok(user) => {
                        log::info!("reconstruct_account: do_login succeeded");
                        Some(user)
                    }
                    Err(e) => {
                        log::warn!("reconstruct_account: do_login failed: {e:?}");
                        // Continue anyway — the account is still partially usable, and
                        // sync_missed_messages will report the failure clearly. Storing
                        // the account lets a future attempt (e.g., on the next launch or
                        // wake) try do_login again without re-doing try_auth.
                        None
                    }
                };

                log::info!("reconstruct_account: successfully reconstructed AppleAccount");
                // Clear any previous sync error — a successful reconstruction
                // means the backoff should be reset.
                if let Err(e) = crate::sync::clear_last_sync_error(
                    &state_dir.join(crate::sync::LAST_SYNC_ERROR_FILENAME),
                ) {
                    log::warn!("clear_last_sync_error failed: {e}");
                }
                *self.account.lock().unwrap() = Some(account);
                Ok(new_user)
            }
            Err(e) => {
                log::warn!("reconstruct_account: try_auth failed: {e:?}");
                // Anisette is still set; account is not. Subsequent attempts
                // can reuse the anisette.
                let unix_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                if let Err(write_err) = crate::sync::write_last_sync_error(
                    &state_dir.join(crate::sync::LAST_SYNC_ERROR_FILENAME),
                    unix_secs,
                ) {
                    log::warn!("write_last_sync_error failed: {write_err}");
                }
                Err(())
            }
        }
    }
}

// --- handle <-> concrete-type accessors ---

fn cfg(c: &Config) -> &api::JoinedOSConfig {
    c.downcast().expect("Config holds JoinedOSConfig")
}
fn conn(c: &Connection) -> &ConnHandle {
    c.downcast().expect("Connection holds ConnHandle")
}

/// Force-refresh the APS connection (e.g., after wake from sleep).
///
/// Re-establishes the underlying TCP/TLS connection to APNs so the receive
/// loop can re-subscribe without waiting for the 60-second keepalive ping.
pub async fn refresh_aps(connection: &Connection) -> anyhow::Result<()> {
    let handle = conn(connection);
    handle.conn.inner().refresh_now().await?;
    log::info!("APS connection refreshed");
    Ok(())
}
fn anis(a: &Anisette) -> &Anis {
    a.downcast().expect("Anisette holds ArcAnisetteClient")
}
fn ident(i: &Identity) -> &IDSNGMIdentity {
    i.downcast().expect("Identity holds IDSNGMIdentity")
}
fn acct(a: &Account) -> &AppleAcct {
    a.downcast().expect("Account holds Arc<Mutex<AppleAccount>>")
}
fn circle(c: &CircleSession) -> &CircleHandle {
    c.downcast().expect("CircleSession holds CircleHandle")
}
fn vbody(v: &VerifyBody) -> &RpVerifyBody {
    v.downcast().expect("VerifyBody holds rustpush::VerifyBody")
}
fn client(c: &ImClient) -> &Arc<IMClient> {
    c.downcast().expect("ImClient holds Arc<IMClient>")
}

/// rustpush `LoginState` -> our facade `LoginState`.
fn map_state(s: RpLoginState) -> LoginState {
    match s {
        RpLoginState::LoggedIn => LoginState::LoggedIn,
        RpLoginState::NeedsLogin => LoginState::NeedsLogin,
        RpLoginState::NeedsDevice2FA => LoginState::NeedsDevice2Fa,
        RpLoginState::Needs2FAVerification => LoginState::Needs2FaVerification,
        RpLoginState::NeedsSMS2FA => LoginState::NeedsSms2Fa,
        RpLoginState::NeedsSMS2FAVerification(body) => {
            LoginState::NeedsSms2FaVerification(VerifyBody::new(body))
        }
        RpLoginState::NeedsExtraStep(msg) => LoginState::NeedsExtraStep(msg),
    }
}

/// Map a `rustpush::ResourceState` to our facade `RegistrationStatus`.
///
/// This is a pure function — no I/O, no side effects. Wired into the receive
/// loop's resource-watcher in a follow-up unit.
pub fn registration_status_from(state: &rustpush::ResourceState) -> RegistrationStatus {
    match state {
        rustpush::ResourceState::Generated => RegistrationStatus::Registered,
        rustpush::ResourceState::Generating => RegistrationStatus::Registering,
        rustpush::ResourceState::Failed(failure) => {
            let error = failure.error.to_string();
            match failure.retry_wait {
                Some(retry_in_s) => {
                    // IDS 6005 "bad authentication" failures are permanent
                    // auth errors — a passive retry loop will never recover.
                    // Surface them as LoggedOut (re-auth required) even when
                    // the resource manager suggests a retry_wait backoff.
                    if is_6005_error(&failure.error) {
                        RegistrationStatus::LoggedOut { error }
                    } else {
                        RegistrationStatus::TransientFailure {
                            retry_in_s,
                            error,
                        }
                    }
                }
                None => RegistrationStatus::LoggedOut { error },
            }
        }
        rustpush::ResourceState::Closed => {
            RegistrationStatus::LoggedOut {
                error: "connection closed".into(),
            }
        }
    }
}

/// Returned by `subscribe` when the underlying connection is dead and
/// cannot be subscribed to. The receive loop calls `reconnect` to
/// re-establish the connection, then retries `subscribe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionDead;

/// Returned by `reconnect` when the re-establishment attempt fails.
#[derive(Debug)]
#[allow(dead_code)]
pub struct ReconnectError(pub String);

/// Subscribe with infinite retries and exponential backoff.
///
/// Retries forever — never returns until subscribe succeeds. Exponential
/// backoff between attempts starts at 1s, doubles per consecutive failure,
/// and is capped at 60s. A `kick.notify_one()` short-circuits the backoff
/// to trigger an immediate retry. A failing `reconnect` is logged and the
/// loop continues (it never aborts).
async fn subscribe_with_reconnect<S, R, RFut>(
    subscribe: &S,
    reconnect: &R,
    kick: std::sync::Arc<tokio::sync::Notify>,
) -> tokio::sync::broadcast::Receiver<APSMessage>
where
    S: Fn() -> Result<tokio::sync::broadcast::Receiver<APSMessage>, ConnectionDead>,
    R: Fn() -> RFut,
    RFut: std::future::Future<Output = Result<(), ReconnectError>>,
{
    let one_sec = std::time::Duration::from_secs(1);
    let max_backoff = std::time::Duration::from_secs(60);
    let mut backoff = one_sec;
    loop {
        match subscribe() {
            Ok(rx) => return rx,
            Err(ConnectionDead) => {
                log::warn!("connection dead, attempting reconnect");
                if let Err(e) = reconnect().await {
                    log::error!("reconnect failed: {e:?}");
                }
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {
                        backoff = std::cmp::min(backoff * 2, max_backoff);
                    }
                    _ = kick.notified() => {
                        // Kick short-circuits — no backoff change.
                    }
                }
            }
        }
    }
}

/// Long-running receive loop. Calls `subscribe` to obtain a broadcast
/// receiver of inbound APS messages and processes each one via `process`.
/// On `Lagged` the dropped count is logged and the loop continues; on
/// `Closed` the loop re-subscribes (with infinite retry via
/// [`subscribe_with_reconnect`]); on `kick.notified()` the loop
/// re-subscribes. The `reconnect` closure is called when `subscribe`
/// returns `Err(ConnectionDead)`. The loop never exits — it retries
/// subscribe indefinitely until it succeeds.
/// Extracted from `start_receiving` so the loop structure (subscribe +
/// recv + error arms + reconnect) can be tested without a live APNs connection.
async fn run_receive_loop<S, P, R, SFut, RFut>(
    subscribe: S,
    process: P,
    kick: std::sync::Arc<tokio::sync::Notify>,
    reconnect: R,
)
where
    S: Fn() -> Result<tokio::sync::broadcast::Receiver<APSMessage>, ConnectionDead> + Send,
    P: Fn(APSMessage) -> SFut + Send,
    SFut: std::future::Future<Output = ()> + Send,
    R: Fn() -> RFut + Send,
    RFut: std::future::Future<Output = Result<(), ReconnectError>> + Send,
{
    let mut rx = subscribe_with_reconnect(&subscribe, &reconnect, std::sync::Arc::clone(&kick)).await;
    log::info!("receive loop started");
    loop {
        tokio::select! {
            result = rx.recv() => match result {
                Ok(msg) => process(msg).await,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("receive lagged, dropped {n} messages");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    log::warn!("receive channel closed, re-subscribing");
                    rx = subscribe_with_reconnect(&subscribe, &reconnect, std::sync::Arc::clone(&kick)).await;
                }
            },
            _ = kick.notified() => {
                log::info!("kick received, re-subscribing");
                rx = subscribe_with_reconnect(&subscribe, &reconnect, std::sync::Arc::clone(&kick)).await;
            }
        }
    }
}

#[async_trait]
impl Backend for RustpushBackend {
    // --- 1. hardware token / validation data ---

    async fn config_from_relay(
        &self,
        code: String,
        host: String,
        token: Option<String>,
    ) -> Result<Config> {
        // api signature takes `token: &Option<String>`
        let cfg = api::config_from_relay(code, host, &token).await?;
        Ok(Config::new(cfg))
    }

    async fn config_from_validation_data(&self, data: Vec<u8>, _extra: HwExtra) -> Result<Config> {
        // Standard device-identity values for the raw-validation-data path,
        // matching upstream hw_inp.dart. (Relay path is primary and skips this.)
        let extra = api::HwExtra {
            version: "13.6.4".into(),
            protocol_version: 1660,
            device_id: uuid::Uuid::new_v4().to_string(),
            icloud_ua: "com.apple.iCloudHelper/282 CFNetwork/1408.0.4 Darwin/22.5.0".into(),
            aoskit_version: "com.apple.AOSKit/282 (com.apple.accountsd/113)".into(),
        };
        // config_from_validation_data is synchronous.
        let cfg = api::config_from_validation_data(data, extra)?;
        Ok(Config::new(cfg))
    }

    async fn config_from_encoded(&self, encoded: Vec<u8>) -> Result<Config> {
        // Rehydrate a MacOSConfig from a cached bbhwinfo blob (device_id and
        // version are embedded in the blob, so no HwExtra is needed).
        let cfg = api::config_from_encoded(encoded)?;
        Ok(Config::new(cfg))
    }

    async fn device_info(&self, config: &Config) -> Result<DeviceInfo> {
        let d = api::get_device_info(cfg(config))?;
        Ok(DeviceInfo {
            name: d.name,
            serial: d.serial,
            os_version: d.os_version,
        })
    }

    // --- 2. push + identity + anisette ---

    fn new_identity(&self) -> Result<Identity> {
        Ok(Identity::new(api::new_ngm_identity()?))
    }

    async fn setup_push(&self, config: &Config, identity: &Identity) -> Result<Connection> {
        // Store the OS config for later use by sync_missed_messages.
        *self.config.lock().unwrap() = Some(cfg(config).clone());
        // `state: None` = fresh connection. For session restore,
        // pass the saved Option<APSState> here instead.
        let (conn, err) =
            api::setup_push(cfg(config), ident(identity), None, self.state_path.clone()).await;
        if let Some(e) = err {
            log::warn!("setup_push returned a non-fatal error: {e:?}");
        }
        *self.conn.lock().unwrap() = Some(conn.clone());
        let idms = api::make_idms(&conn).await;
        Ok(Connection::new(ConnHandle { conn, idms }))
    }

    async fn make_anisette(&self, config: &Config, connection: &Connection) -> Result<Anisette> {
        let a = api::make_anisette(self.state_path.clone(), cfg(config), conn(connection).conn.inner()).await;
        // Store for later use by sync_missed_messages.
        *self.anisette.lock().unwrap() = Some(a.clone());
        Ok(Anisette::new(a))
    }

    // --- 3. login + 2FA ---

    async fn try_auth(
        &self,
        config: &Config,
        connection: &Connection,
        anisette: &Anisette,
        creds: Option<(String, String)>,
    ) -> Result<(Account, LoginState)> {
        let (account, state) = api::try_auth(
            self.state_path.clone(),
            cfg(config),
            conn(connection).conn.inner(),
            anis(anisette),
            creds,
        )
        .await?;
        // Store for later use by sync_missed_messages.
        *self.account.lock().unwrap() = Some(account.clone());
        Ok((Account::new(account), map_state(state)))
    }

    async fn try_icloud_login(
        &self,
        config: &Config,
        account: &Account,
    ) -> Result<Option<IdsUser>> {
        let user = api::try_icloud_login(self.state_path.clone(), cfg(config), acct(account)).await?;
        Ok(user.map(IdsUser::new))
    }

    async fn send_2fa_to_devices(
        &self,
        account: &Account,
        connection: &Connection,
    ) -> Result<(CircleSession, LoginState)> {
        let ch = conn(connection);
        // Subscribe BEFORE sending so the verification push isn't missed.
        let watcher = api::subscribe_conn(&ch.conn);
        let (session, state, _sid) = api::send_2fa_to_devices(acct(account), ch.conn.inner()).await?;
        let handle = CircleHandle {
            inner: Mutex::new((session, watcher)),
            idms: ch.idms.clone(),
        };
        Ok((CircleSession::new(handle), map_state(state)))
    }

    async fn verify_2fa(
        &self,
        session: &CircleSession,
        anisette: &Anisette,
        config: &Config,
        account: &Account,
        code: String,
    ) -> Result<(LoginState, Option<IdsUser>)> {
        let ch = circle(session);
        let mut guard = ch.inner.lock().await;
        let (sess, watcher) = &mut *guard;
        let (state, user) = api::verify_2fa(
            self.state_path.clone(),
            sess,
            anis(anisette),
            cfg(config),
            acct(account),
            watcher,
            &ch.idms,
            code,
        )
        .await?;
        Ok((map_state(state), user.map(IdsUser::new)))
    }

    async fn send_2fa_sms(&self, account: &Account) -> Result<LoginState> {
        let (phones, maybe_state) = api::get_2fa_sms_opts(acct(account)).await?;
        if let Some(s) = maybe_state {
            return Ok(map_state(s));
        }
        // VERIFY: this just picks the first trusted number. To match upstream's
        // picker, surface `phones` (TrustedPhoneNumber { id, number_with_dial_code,
        // .. }) to the UI and pass the chosen id. `locked` is the circle session
        // from a prior device-2FA attempt; None is fine for a pure SMS flow.
        let phone_id = phones
            .first()
            .map(|p| p.id)
            .ok_or_else(|| anyhow::anyhow!("no trusted phone numbers"))?;
        let state = api::send_2fa_sms(None, acct(account), phone_id).await?;
        Ok(map_state(state))
    }

    async fn verify_2fa_sms(
        &self,
        account: &Account,
        anisette: &Anisette,
        config: &Config,
        body: &VerifyBody,
        code: String,
    ) -> Result<(LoginState, Option<IdsUser>)> {
        let (state, user) = api::verify_2fa_sms(
            self.state_path.clone(),
            acct(account),
            anis(anisette),
            cfg(config),
            vbody(body),
            code,
        )
        .await?;
        Ok((map_state(state), user.map(IdsUser::new)))
    }

    // --- 4. registration ---

    async fn register_ids(
        &self,
        config: &Config,
        connection: &Connection,
        identity: &Identity,
        users: Vec<IdsUser>,
    ) -> Result<RegisterOutcome> {
        // FRB took ownership via duplicate_user upstream; we mirror that so the
        // handles stay reusable.
        let users_vec: Vec<IDSUser> = users
            .iter()
            .map(|u| api::duplicate_user(u.downcast::<IDSUser>().expect("IdsUser")))
            .collect();

        let (new_users, alert) = api::register_ids(
            self.state_path.clone(),
            cfg(config),
            conn(connection).conn.inner(),
            ident(identity),
            users_vec,
        )
        .await?;

        match alert {
            Some(a) => Ok(RegisterOutcome::Blocked(SupportAlert {
                title: a.title,
                body: a.body,
            })),
            None => Ok(RegisterOutcome::Registered(
                new_users
                    .unwrap_or_default()
                    .into_iter()
                    .map(IdsUser::new)
                    .collect(),
            )),
        }
    }

    async fn make_imclient(
        &self,
        connection: &Connection,
        identity: &Identity,
        users: Vec<IdsUser>,
    ) -> Result<ImClient> {
        let users_vec: Vec<IDSUser> = users
            .iter()
            .map(|u| api::duplicate_user(u.downcast::<IDSUser>().expect("IdsUser")))
            .collect();
        let c = api::make_imclient(
            self.state_path.clone(),
            conn(connection).conn.inner(),
            &users_vec,
            ident(identity),
        )
        .await;
        Ok(ImClient::new(c))
    }

    async fn get_handles(&self, c: &ImClient) -> Result<Vec<String>> {
        Ok(api::get_handles(client(c)).await?)
    }

    async fn restore_session(&self) -> Result<Option<RestoredSession>> {
        let path = self.state_path.clone();

        // hw_info.plist (push + identity + os_config) and id.plist (registered
        // users) are both written during a successful onboarding. Either being
        // absent means we have nothing to restore -> onboard.
        let Some(saved) = api::read_hardware(path.clone()) else {
            return Ok(None);
        };
        let Some(users) = api::restore_users(path.clone()) else {
            return Ok(None);
        };
        if users.is_empty() {
            return Ok(None);
        }

        let identity = api::decode_identity(&saved.identity)?;
        let config = saved.os_config.clone();
        // Store for later use by sync_missed_messages.
        *self.config.lock().unwrap() = Some(config.clone());

        // Reconnect APNs reusing the saved push state (no fresh activation).
        let (conn, err) =
            api::setup_push(&config, &identity, Some(saved.push), path.clone()).await;
        if let Some(e) = err {
            log::warn!("restore setup_push returned a non-fatal error: {e:?}");
        }
        *self.conn.lock().unwrap() = Some(conn.clone());
        let idms = api::make_idms(&conn).await;

        // Rehydrate the messaging client straight from the persisted
        // registration in id.plist — no re-register, no validation data needed.
        let imclient = api::make_imclient(path.clone(), conn.inner(), &users, &identity).await;
        let handles = api::get_handles(&imclient).await?;

        Ok(Some(RestoredSession {
            config: Config::new(config),
            connection: Connection::new(ConnHandle { conn, idms }),
            identity: Identity::new(identity),
            client: ImClient::new(imclient),
            handles,
        }))
    }

    fn start_receiving(
        &self,
        connection: &Connection,
        c: &ImClient,
        handles: Vec<String>,
        store: Store,
        notify: async_channel::Sender<RecvEvent>,
    ) -> std::sync::Arc<tokio::sync::Notify> {
        let conn = conn(connection).conn.clone();
        let imclient = client(c).clone();
        let imclient_for_watch = imclient.clone(); // retained for the resource-state watcher
        let notify_for_watch = notify.clone(); // retained for the resource-state watcher
        let kick = std::sync::Arc::new(tokio::sync::Notify::new());
        let kick_spawn = std::sync::Arc::clone(&kick);
        crate::runtime::runtime().spawn(async move {
            // Shared connection state — replaceable by `reconnect`.
            let state: std::sync::Arc<std::sync::Mutex<Arc<BufferedApsConn>>> =
                std::sync::Arc::new(std::sync::Mutex::new(conn));

            let subscribe = {
                let state = std::sync::Arc::clone(&state);
                move || -> Result<tokio::sync::broadcast::Receiver<APSMessage>, ConnectionDead> {
                    let conn = state.lock().unwrap_or_else(|p| p.into_inner()).clone();
                    Ok(api::subscribe_conn(&conn))
                }
            };

            let reconnect = {
                let state = std::sync::Arc::clone(&state);
                move || {
                    let state = std::sync::Arc::clone(&state);
                    async move {
                        let conn = state.lock().unwrap_or_else(|p| p.into_inner()).clone();
                        conn.inner().refresh_now().await
                            .map_err(|e| ReconnectError(format!("refresh failed: {e}")))?;
                        log::info!("connection re-established via refresh");
                        Ok(())
                    }
                }
            };

            run_receive_loop(
                subscribe,
                move |msg| {
                    let imclient = imclient.clone();
                    let handles = handles.clone();
                    let store = store.clone();
                    let notify = notify.clone();
                    let state = std::sync::Arc::clone(&state);
                    async move {
                        let conn = state.lock().unwrap_or_else(|p| p.into_inner()).clone();
                        match imclient.handle(msg).await {
                            Ok(Some(inst)) => {
                                log_inst(&inst);
                                // Typing is ephemeral: forward it straight to the UI
                                // (keyed like the store's chat key) and don't persist
                                // or acknowledge it.
                                if let Message::Typing(typing, _) = &inst.message {
                                    let from_me = is_from_me(&inst, &handles);
                                    log::debug!(
                                        "typing recv from={:?} from_me={from_me} typing={typing}",
                                        inst.sender
                                    );
                                    if !from_me {
                                        if let Some(conv) = &inst.conversation {
                                            let chat_key = ChatRef {
                                                participants: conv.participants.clone(),
                                                display_name: conv.cv_name.clone(),
                                                service: None,
                                            }
                                            .key();
                                            let _ = notify
                                                .send(RecvEvent::Typing {
                                                    chat_key,
                                                    from: inst.sender.clone(),
                                                    typing: *typing,
                                                    superseded: false,
                                                })
                                                .await;
                                        }
                                    }
                                } else {
                                    let mut ingest = ingest_from(&inst, &handles);
                                    // Download any attachments and attach them to the record.
                                    if let Ingest::Message(im) = &mut ingest {
                                        im.attachments = download_inbound(&inst, conn.inner(), &im.guid).await;
                                    }
                                    if let Err(e) = store.apply(ingest).await {
                                        log::warn!("store apply error: {e:#}");
                                    }
                                    // Sender-generated link preview (iMessage rich link):
                                    // rustpush already pulled the balloon body and gave us
                                    // the inline thumbnail bytes; we cache them to disk and
                                    // upsert the row. Same guid as the message so a
                                    // placeholder is replaced in place by its fill-in.
                                    // MUST NOT fetch the URL — that would leak the
                                    // recipient's IP to the sender's hosting (a tracking
                                    // beacon). The sender already shipped us the snapshot.
                                    //
                                    // We do NOT pulse RecvEvent::Applied here: a full
                                    // `reload_messages` on a preview-only update flickers
                                    // and can jump scroll (per the plan). Instead we send
                                    // RecvEvent::LinkPreviewUpdated, which the UI handles
                                    // as an in-place card replacement.
                                    if let Message::Message(nm) = &inst.message {
                                        if let Some(lm) = &nm.link_meta {
                                            match extract_link_preview(&inst.id, lm) {
                                                Some(p) => {
                                                    if let Err(e) =
                                                        store.apply(Ingest::LinkPreview(p)).await
                                                    {
                                                        log::warn!("store apply link preview: {e:#}");
                                                    } else {
                                                        let _ = notify
                                                            .send(RecvEvent::LinkPreviewUpdated {
                                                                guid: inst.id.clone(),
                                                                part_idx: 0,
                                                            })
                                                            .await;
                                                    }
                                                }
                                                None => log::debug!("link_meta present but no preview extracted for {}", inst.id),
                                            }
                                        }
                                    }
                                    // Acknowledge inbound content with a Delivered receipt.
                                    if SEND_DELIVERED_RECEIPTS && is_incoming_content(&inst, &handles) {
                                        send_receipt_for(&imclient, &inst, &handles, false).await;
                                    }
                                    // Pulse the UI; drop if no receiver is listening.
                                    let _ = notify.send(RecvEvent::Applied).await;
                                    // A real inbound message means they've stopped typing:
                                    // clear the indicator now rather than waiting for a
                                    // typing-stop that iMessage doesn't always send.
                                    if is_incoming_content(&inst, &handles) {
                                        if let Some(conv) = &inst.conversation {
                                            let chat_key = ChatRef {
                                                participants: conv.participants.clone(),
                                                display_name: conv.cv_name.clone(),
                                                service: None,
                                            }
                                            .key();
                                            let _ = notify
                                                .send(RecvEvent::Typing {
                                                    chat_key,
                                                    from: inst.sender.clone(),
                                                    typing: false,
                                                    superseded: true,
                                                })
                                                .await;
                                        }
                                    }
                                }
                            }
                            Ok(None) => {}
                            Err(e) => log::warn!("handle error: {e:?}"),
                        }
                    }
                },
                kick_spawn,
                reconnect,
            )
            .await
        });

        // Spawn the identity resource state watcher. Subscribes to the resource
        // manager's watch channel, sends one initial event, then forwards every
        // change to the UI via `notify`. Also guards against spammy 6005 retry-now
        // calls at launch: the first 6005 fires `identity.refresh_now()` immediately,
        // subsequent 6005s within the cooldown are suppressed, and the cooldown
        // resets on a successful `Generated` state. Exits when the watch channel
        // closes (resource manager dropped).
        crate::runtime::runtime().spawn(async move {
            let mut rx = imclient_for_watch.identity.resource_state.subscribe();
            let mut guard = Launch6005RetryGuard::new(std::time::Duration::from_secs(60));
            // Send initial state immediately so the UI reflects reality at startup.
            // Clone the value to drop the borrow before the await (the Ref is
            // not Send and cannot cross the await boundary).
            let initial = rx.borrow_and_update().clone();
            // Evaluate initial state through the guard (fires refresh_now on first
            // 6005, or resets on Generated, but doesn't block the UI notification).
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if guard.evaluate(&initial, now_secs) {
                // Guard allows a 6005 retry-now — fire `refresh_now()`
                // asynchronously. This is a best-effort self-heal; we don't
                // block the UI notification on it.
                let imc = imclient_for_watch.clone();
                crate::runtime::runtime().spawn(async move {
                    match imc.identity.refresh_now().await {
                        Ok(()) => log::info!(
                            "6005 retry-now: identity.refresh_now() succeeded"
                        ),
                        Err(e) => log::warn!(
                            "6005 retry-now: identity.refresh_now() failed: {e:?}"
                        ),
                    }
                });
            }
            let _ = notify_for_watch
                .send(RecvEvent::Registration(registration_status_from(&initial)))
                .await;
            loop {
                if rx.changed().await.is_err() {
                    // The resource watch channel was closed — resource is gone.
                    break;
                }
                let s = rx.borrow_and_update().clone();
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if guard.evaluate(&s, now_secs) {
                    // Guard allows a 6005 retry-now — fire `refresh_now()`
                    // asynchronously. This is a best-effort self-heal; we don't
                    // block the UI notification on it.
                    let imc = imclient_for_watch.clone();
                    crate::runtime::runtime().spawn(async move {
                        match imc.identity.refresh_now().await {
                            Ok(()) => log::info!(
                                "6005 retry-now: identity.refresh_now() succeeded"
                            ),
                            Err(e) => log::warn!(
                                "6005 retry-now: identity.refresh_now() failed: {e:?}"
                            ),
                        }
                    });
                }
                let _ = notify_for_watch
                    .send(RecvEvent::Registration(registration_status_from(&s)))
                    .await;
            }
        });
        kick
    }

    async fn send_text(
        &self,
        c: &ImClient,
        chat: &ChatRef,
        my_handle: &str,
        text: String,
        guid: String,
    ) -> Result<IncomingMessage> {
        let imclient = client(c).clone();
        let conversation = conversation_from(chat);
        let normal = NormalMessage::new(text.clone(), MessageType::IMessage);
        let mut inst = MessageInst::new(conversation, my_handle, Message::Message(normal));
        inst.id = guid.clone();
        let date = now_ms();
        let inst = Mutex::new(inst);

        // Try the send. If the underlying error is a 6005 cert/identity
        // rejection from Apple (the typical "stale IDS cert" symptom — the
        // auto-rereg already fired once and failed), attempt a self-heal
        // before giving up. The self-heal forces a fresh re-register on the
        // IMClient's IdentityManager; if Apple's auth state has cleared
        // since the last rereg, this succeeds and the cert is updated in
        // place so the retry uses the new cert. The next launch's
        // `reconstruct_account` (which always runs `do_login`) provides
        // a second chance to refresh the cert.
        let result = crate::retry::retry(3, std::time::Duration::from_millis(500), || async {
            let mut guard = inst.lock().await;
            imclient.send(&mut guard).await
        })
        .await;

        let send_result = match result {
            Ok(job) => Ok(job),
            Err(e) if is_6005_error(&e) => {
                log::warn!(
                    "send failed with IDS 6005; attempting cert self-heal (this can take a few seconds)"
                );
                // First, force a re-register on the IMClient's
                // IdentityManager. If Apple's state has cleared since the
                // last rereg, this succeeds and the cert is updated in
                // place. If it times out or fails for any other reason
                // (e.g. rereg endpoint hung — the typical case after a
                // suspend), fall back to a full `do_login` via
                // `reconstruct_account(force: true)`. That's the same path
                // a fresh launch takes, so it always succeeds when the
                // Apple account state on disk is intact — and the user
                // reported that "quit and relaunch it works again," which
                // is exactly this path.
                let retry_send = || async {
                    let mut guard = inst.lock().await;
                    imclient.send(&mut guard).await
                };
                // Use `refresh_now` (not `refresh`) — `refresh` signals on
                // `retry_signal`, which the ResourceManager's backoff sleep
                // does not wait on, so a refresh-while-backoff just times
                // out at MAX_RESOURCE_WAIT (30s). `refresh_now` signals on
                // `retry_now_signal`, which the backoff sleep *does* wait
                // on, so it actually wakes the sleep and triggers a fresh
                // generation. Without this, the cert self-heal was a no-op
                // every time the resource was in a backoff window.
                match imclient.identity.refresh_now().await {
                    Ok(()) => {
                        log::info!("cert self-heal: re-register succeeded, retrying send");
                        retry_send().await
                    }
                    Err(rereg_err) => {
                        log::warn!(
                            "cert self-heal: re-register failed ({rereg_err:?}), \
                             trying do_login fallback (this may take a few seconds)"
                        );
                        match self.reconstruct_account(true).await {
                            Ok(Some(new_user)) => {
                                log::info!(
                                    "cert self-heal: do_login fallback succeeded, \
                                     updating IdentityResource and retrying send"
                                );
                                // Update the IMClient's IdentityResource with the
                                // fresh user data from do_login. update_users
                                // replaces the users list and triggers a
                                // re-register (refresh_now), which uses the fresh
                                // cert/key material from the new IDSUser.
                                imclient.identity.resource.update_users(vec![new_user]).await.ok();
                                retry_send().await
                            }
                            Ok(None) => {
                                // do_login failed but try_auth succeeded — the
                                // account is reconstructed but no fresh user data
                                // is available. Retry the send with whatever state
                                // exists.
                                log::warn!(
                                    "cert self-heal: do_login returned no user, \
                                     retrying send anyway"
                                );
                                retry_send().await
                            }
                            Err(()) => {
                                log::error!(
                                    "cert self-heal: do_login fallback also failed; \
                                     original rereg error: {rereg_err:?}"
                                );
                                Err(e)
                            }
                        }
                    }
                }
            }
            Err(e) => Err(e),
        };

        let mut job = send_result?;

        // Drain the delivery-result broadcast channel to collect per-target
        // outcomes.  The channel closes when the InnerSendJob task finishes
        // (drops its sender, which also means the JoinHandle is ready).
        let mut delivery_results: Vec<SendResult> = Vec::new();
        loop {
            match job.process.recv().await {
                Ok((_handle, result)) => delivery_results.push(result),
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("send delivery channel lagged by {n} messages");
                    continue;
                }
            }
        }

        // Await the background handle to catch JoinError / PushError.
        if let Some(handle) = job.handle {
            handle
                .await
                .map_err(|e| anyhow::anyhow!("send delivery task panicked: {e}"))?
                .map_err(|e| anyhow::anyhow!("send delivery failed: {e}"))?;
        }

        // Map delivery outcomes.
        // - At least one Sent → success (partial per-device failures OK).
        // - No results at all (relay/no_response, empty targets) → success.
        // - All TimedOut → timeout error (catches categorize_send_error's
        //   io::ErrorKind::TimedOut sniff).
        // - All APSError or mixed failures → generic send error.
        if !delivery_results.iter().any(|r| matches!(r, SendResult::Sent))
            && !delivery_results.is_empty()
        {
            let all_timed_out = delivery_results
                .iter()
                .all(|r| matches!(r, SendResult::TimedOut));
            if all_timed_out {
                return Err(anyhow::anyhow!(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "message delivery timed out for all targets",
                )));
            } else {
                return Err(anyhow::anyhow!(
                    "message delivery failed (APNS errors for all targets)"
                ));
            }
        }

        Ok(IncomingMessage {
            guid,
            chat: chat.clone(),
            sender: Some(my_handle.to_string()),
            is_from_me: true,
            text: Some(text),
            service: Some("iMessage".into()),
            date,
            ..Default::default()
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_reaction(
        &self,
        c: &ImClient,
        chat: &ChatRef,
        my_handle: &str,
        target_guid: &str,
        target_part: Option<u64>,
        target_text: &str,
        reaction: &ReactMessageType,
    ) -> Result<()> {
        let imclient = client(c).clone();
        let mut inst = build_react_message_inst(
            chat,
            my_handle,
            target_guid,
            target_part,
            target_text,
            reaction,
        );
        imclient
            .send(&mut inst)
            .await
            .map_err(|e| anyhow::anyhow!("send reaction failed: {e:?}"))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_edit(
        &self,
        c: &ImClient,
        chat: &ChatRef,
        my_handle: &str,
        target_guid: &str,
        edit_part: u64,
        new_text: String,
        new_guid: String,
    ) -> Result<()> {
        let _ = new_guid; // we use new_guid() inside build_edit_message_inst
        let imclient = client(c).clone();
        let inst = build_edit_message_inst(chat, my_handle, target_guid, edit_part, new_text);
        let inst = Mutex::new(inst);
        crate::retry::retry(3, std::time::Duration::from_millis(500), || async {
            let mut guard = inst.lock().await;
            imclient
                .send(&mut guard)
                .await
                .map_err(|e| anyhow::anyhow!("send edit failed: {e:?}"))
        })
        .await?;
        Ok(())
    }

    async fn send_attachment(
        &self,
        c: &ImClient,
        connection: &Connection,
        chat: &ChatRef,
        my_handle: &str,
        path: String,
        mime: String,
        name: String,
        text: Option<String>,
        guid: String,
    ) -> Result<IncomingMessage> {
        use std::io::Seek;

        let imclient = client(c).clone();
        let aps = conn(connection).conn.clone();
        let conversation = conversation_from(chat);
        let uti = mime_to_uti(&mime);

        // Upload to MMCS (the file is read twice: once to prepare, once to send).
        let mut file = std::fs::File::open(&path)
            .map_err(|e| anyhow::anyhow!("open {path}: {e}"))?;
        let total = file.metadata().map(|m| m.len() as i64).ok();
        let prepared = MMCSFile::prepare_put(&mut file)
            .await
            .map_err(|e| anyhow::anyhow!("prepare attachment: {e:?}"))?;
        file.rewind().map_err(|e| anyhow::anyhow!("rewind: {e}"))?;
        let attachment =
            Attachment::new_mmcs(aps.inner(), &prepared, file, &mime, &uti, &name, |_, _| {})
                .await
                .map_err(|e| anyhow::anyhow!("upload attachment: {e:?}"))?;

        let mut normal = NormalMessage::new(String::new(), MessageType::IMessage);
        normal.parts = build_attachment_message_parts(text.as_deref(), attachment);
        let mut inst = MessageInst::new(conversation, my_handle, Message::Message(normal));
        inst.id = guid.clone();
        let date = now_ms();
        let mut job = imclient
            .send(&mut inst)
            .await
            .map_err(|e| anyhow::anyhow!("send failed: {e:?}"))?;

        // Drain delivery results (same policy as send_text).
        let mut delivery_results: Vec<SendResult> = Vec::new();
        loop {
            match job.process.recv().await {
                Ok((_handle, result)) => delivery_results.push(result),
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("send attachment delivery channel lagged by {n} messages");
                    continue;
                }
            }
        }
        if let Some(handle) = job.handle {
            handle
                .await
                .map_err(|e| anyhow::anyhow!("send delivery task panicked: {e}"))?
                .map_err(|e| anyhow::anyhow!("send delivery failed: {e}"))?;
        }
        if !delivery_results.iter().any(|r| matches!(r, SendResult::Sent))
            && !delivery_results.is_empty()
        {
            let all_timed_out = delivery_results
                .iter()
                .all(|r| matches!(r, SendResult::TimedOut));
            if all_timed_out {
                return Err(anyhow::anyhow!(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "attachment delivery timed out for all targets",
                )));
            } else {
                return Err(anyhow::anyhow!(
                    "attachment delivery failed (APNS errors for all targets)"
                ));
            }
        }

        // Cache a local copy so the UI can render the sent image right away.
        let local_path = cache_copy(&path, &guid, 0, &name).map(|p| p.to_string_lossy().into_owned());

        Ok(IncomingMessage {
            guid,
            chat: chat.clone(),
            sender: Some(my_handle.to_string()),
            is_from_me: true,
            text,
            service: Some("iMessage".into()),
            date,
            attachments: vec![AttachmentRecord {
                mime: Some(mime),
                name: Some(name),
                total_bytes: total,
                local_path,
                part_index: Some(0),
                ..Default::default()
            }],
            ..Default::default()
        })
    }

    fn send_receipt(
        &self,
        c: &ImClient,
        chat: &ChatRef,
        my_handle: &str,
        read: bool,
        target_guid: String,
    ) {
        let imclient = client(c).clone();
        let conversation = conversation_from(chat);
        let my_handle = my_handle.to_string();
        crate::runtime::runtime().spawn(async move {
            let msg = if read { Message::Read } else { Message::Delivered };
            let mut inst = MessageInst::new(conversation, &my_handle, msg);
            inst.id = target_guid;
            let kind = if read { "read" } else { "delivered" };
            match imclient.send(&mut inst).await {
                Ok(_) => log::info!("→ sent {kind} receipt for {}", inst.id),
                Err(e) => log::warn!("{kind} receipt error: {e:?}"),
            }
        });
    }

    fn send_typing(&self, c: &ImClient, chat: &ChatRef, my_handle: &str, typing: bool) {
        let imclient = client(c).clone();
        let conversation = conversation_from(chat);
        let my_handle = my_handle.to_string();
        crate::runtime::runtime().spawn(async move {
            let mut inst =
                MessageInst::new(conversation, &my_handle, Message::Typing(typing, None));
            match imclient.send(&mut inst).await {
                Ok(_) => log::debug!("→ sent typing={typing}"),
                Err(e) => log::warn!("typing send error: {e:?}"),
            }
        });
    }

    fn sign_out(&self) {
        api::clear_session(&self.state_path);
    }

    #[cfg(feature = "rustpush")]
    async fn sync_missed_messages(
        &self,
        store: &crate::store::Store,
        cutoff_ms: i64,
        force: bool,
    ) -> crate::sync::SyncResult {
        // Best-effort: if the session state is incomplete (e.g., after a
        // restart), try to reconstruct the AppleAccount from gsa.plist.
        // On failure (the common case until the follow-up stores the APS
        // connection), the method falls through to the default-return path.
        if self.account.lock().unwrap().is_none() {
            let _ = self.reconstruct_account(force).await;
        }

        // Need all three session handles; if any is missing, the session isn't
        // fully set up yet, so we can't sync. Return a default result.
        let account = self.account.lock().unwrap().clone();
        let anisette = self.anisette.lock().unwrap().clone();
        let config = self.config.lock().unwrap().clone();
        let (account, anisette, config) = match (account, anisette, config) {
            (Some(a), Some(an), Some(c)) => (a, an, c),
            _ => {
                log::debug!(
                    "sync_missed_messages: session state not fully populated, skipping"
                );
                return crate::sync::SyncResult::default();
            }
        };

        // Build Arc<dyn OSConfig> from the JoinedOSConfig by matching on the
        // concrete variant — each inner type already implements OSConfig.
        let config_arc: Arc<dyn OSConfig> = match &config {
            api::JoinedOSConfig::MacOS(conf) => conf.clone(),
            api::JoinedOSConfig::Relay(conf) => conf.clone(),
        };

        // Read persisted CloudKit and Keychain state from disk (written by
        // `do_login` inside `api::verify_2fa`/`api::verify_2fa_sms`).
        let dir = std::path::PathBuf::from(&self.state_path);
        let cloudkit_state: rustpush::cloudkit::CloudKitState =
            match plist::from_file(dir.join("cloudkit.plist")) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("sync: failed to read cloudkit.plist: {e}");
                    return crate::sync::SyncResult::default();
                }
            };
        let keychain_state: rustpush::keychain::KeychainClientState =
            match plist::from_file(dir.join("keychain.plist")) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("sync: failed to read keychain.plist: {e}");
                    return crate::sync::SyncResult::default();
                }
            };

        // Build the TokenProvider from the in-memory AppleAccount.
        // This is the same `TokenProvider` used by the CloudKit and Keychain
        // clients to authenticate with Apple's token service.
        // (Previously this block injected a cached MME delegate here to
        // skip the Apple auth round-trip on launch. That optimization was
        // removed along with the rest of the MME cache because it caused
        // the IDS cert to stay bad whenever the rereg failed.)
        let token_provider = TokenProvider::new(account.clone(), config_arc.clone());

        // Build the CloudKitClient.
        let ck_client: Arc<rustpush::cloudkit::CloudKitClient<_>> = Arc::new(
            rustpush::cloudkit::CloudKitClient {
                state: DebugRwLock::new(cloudkit_state),
                anisette: anisette.clone(),
                config: config_arc.clone(),
                token_provider: token_provider.clone(),
            },
        );

        // Build the KeychainClient, wired to persist state changes back to
        // disk via the `update_state` callback.
        let kc_path = dir.join("keychain.plist");
        let kc_client: Arc<rustpush::keychain::KeychainClient<_>> = Arc::new(
            rustpush::keychain::KeychainClient {
                anisette: anisette.clone(),
                token_provider: token_provider.clone(),
                state: DebugRwLock::new(keychain_state),
                config: config_arc.clone(),
                update_state: Box::new(move |update| {
                    if let Err(e) = plist::to_file_xml(&kc_path, update) {
                        log::warn!("sync: failed to write keychain.plist: {e}");
                    }
                }),
                container: Mutex::new(None),
                security_container: Mutex::new(None),
                client: ck_client.clone(),
            },
        );

        // Build the CloudMessagesClient — the HTTP client that talks to Apple's
        // CloudKit sync endpoint.
        let msg_client =
            rustpush::cloud_messages::CloudMessagesClient::new(ck_client, kc_client);

        // Empty handles for now — the IS_FROM_ME flag in CloudMessage.flags is
        // the primary signal. A follow-up can read the registered handles from
        // disk (id.plist) to add the handle-based fallback.
        let my_handles: Vec<String> = Vec::new();
        let chat_map = std::collections::HashMap::new();

        crate::sync::sync_once(&msg_client, store, &my_handles, &chat_map, cutoff_ms).await
    }

    async fn setup_keychain_clique(
        &self,
        escrow_passcode: &str,
        device_password: &str,
    ) -> std::result::Result<(), String> {
        let dir = std::path::PathBuf::from(&self.state_path);

        // If the in-memory session isn't populated yet, reconstruct it
        // from gsa.plist first. This is the same path `sync_missed_messages`
        // uses, and it's needed because the new `orchestrate_sync_now_flow`
        // may call `setup_keychain_clique` *before* any sync has happened
        // (e.g., the first time the user toggles cloud sync on). We pass
        // `force: true` so the `cloud_sync_enabled` and backoff gates
        // don't apply: the setup is always triggered by an explicit user
        // action (the user entered a password and clicked "Set Up"), so
        // the gates are not relevant here — the user has already opted
        // in by entering their password.
        if self.account.lock().unwrap().is_none()
            && self.reconstruct_account(true).await.is_err()
        {
            return Err("account not reconstructed: sign in first".to_string());
        }

        // Need the in-memory session handles to build a TokenProvider.
        let account = self.account.lock().unwrap().clone();
        let anisette = self.anisette.lock().unwrap().clone();
        let config = self.config.lock().unwrap().clone();
        let (account, anisette, config) = match (account, anisette, config) {
            (Some(a), Some(an), Some(c)) => (a, an, c),
            _ => {
                return Err(
                    "account not reconstructed: sign in first".to_string(),
                );
            }
        };

        // Build Arc<dyn OSConfig> from the JoinedOSConfig.
        let config_arc: Arc<dyn OSConfig> = match &config {
            api::JoinedOSConfig::MacOS(conf) => conf.clone(),
            api::JoinedOSConfig::Relay(conf) => conf.clone(),
        };

        // Build a TokenProvider from the AppleAccount.
        let token_provider = TokenProvider::new(account.clone(), config_arc.clone());

        // Read persisted Keychain state from disk (written by do_login).
        let keychain_state: rustpush::keychain::KeychainClientState =
            match plist::from_file(dir.join("keychain.plist")) {
                Ok(s) => s,
                Err(e) => {
                    return Err(format!("read keychain state: {e}"));
                }
            };

        // Read persisted CloudKit state from disk for the CloudKitClient.
        let cloudkit_state: rustpush::cloudkit::CloudKitState =
            match plist::from_file(dir.join("cloudkit.plist")) {
                Ok(s) => s,
                Err(e) => {
                    return Err(format!("read cloudkit state: {e}"));
                }
            };

        // Build a CloudKitClient (required by KeychainClient).
        let ck_client: Arc<rustpush::cloudkit::CloudKitClient<_>> = Arc::new(
            rustpush::cloudkit::CloudKitClient {
                state: DebugRwLock::new(cloudkit_state),
                anisette: anisette.clone(),
                config: config_arc.clone(),
                token_provider: token_provider.clone(),
            },
        );

        // Build the KeychainClient, wired to persist state changes back to
        // disk via the `update_state` callback.
        let kc_path = dir.join("keychain.plist");
        let keychain = rustpush::keychain::KeychainClient {
            anisette: anisette.clone(),
            token_provider: token_provider.clone(),
            state: DebugRwLock::new(keychain_state),
            config: config_arc.clone(),
            update_state: Box::new(move |update| {
                if let Err(e) = plist::to_file_xml(&kc_path, update) {
                    log::warn!(
                        "setup_keychain_clique: failed to write keychain.plist: {e}"
                    );
                }
            }),
            container: Mutex::new(None),
            security_container: Mutex::new(None),
            client: ck_client.clone(),
        };

        // Idempotent: if already in the clique, nothing to do.
        if keychain.is_in_clique().await {
            return Ok(());
        }

        // Discover viable escrow bottles to decide the setup route.
        let bottles = keychain
            .get_viable_bottles()
            .await
            .map_err(|e| format!("get viable bottles: {e:?}"))?;

        match select_clique_setup_route(&bottles, 0) {
            CliqueSetupRoute::JoinFromEscrow { bottle_index } => {
                // Existing clique detected — recover the peer from escrow
                // and join via voucher (calls Cuttlefish joinWithVoucher).
                // `escrow_passcode` decrypts the existing bottle; `device_password`
                // becomes this device's new bottle password.
                let (bottle, _) = &bottles[bottle_index];
                keychain
                    .join_clique_from_escrow(
                        bottle,
                        escrow_passcode.as_bytes(),
                        device_password.as_bytes(),
                    )
                    .await
                    .map_err(|e| format!("join clique from escrow: {e:?}"))?;
            }
            CliqueSetupRoute::Establish => {
                // First-time setup — create a new identity and establish a
                // brand-new clique (calls Cuttlefish establish).
                // Only `device_password` is relevant here; there is no existing
                // escrow bottle to decrypt.
                let identity = keychain
                    .new_user_identity(false)
                    .await
                    .map_err(|e| format!("create identity: {e:?}"))?;

                keychain
                    .join_clique(device_password.as_bytes(), &identity, None, &[], vec![])
                    .await
                    .map_err(|e| format!("join clique: {e:?}"))?;
            }
        }

        Ok(())
    }

    async fn is_keychain_clique_set_up(&self) -> bool {
        let path = std::path::PathBuf::from(&self.state_path).join("keychain.plist");
        let dict: plist::Dictionary = match plist::from_file(&path) {
            Ok(d) => d,
            Err(_) => return false,
        };
        dict.contains_key("user_identity")
    }

    async fn setup_keychain_clique_with_bottle(
        &self,
        selected_bottle: &crate::api::EscrowData,
        escrow_passcode: &str,
        device_password: &str,
    ) -> std::result::Result<(), String> {
        let dir = std::path::PathBuf::from(&self.state_path);

        // If the in-memory session isn't populated yet, reconstruct it
        // from gsa.plist first.
        if self.account.lock().unwrap().is_none()
            && self.reconstruct_account(true).await.is_err()
        {
            return Err("account not reconstructed: sign in first".to_string());
        }

        let account = self.account.lock().unwrap().clone();
        let anisette = self.anisette.lock().unwrap().clone();
        let config = self.config.lock().unwrap().clone();
        let (account, anisette, config) = match (account, anisette, config) {
            (Some(a), Some(an), Some(c)) => (a, an, c),
            _ => {
                return Err(
                    "account not reconstructed: sign in first".to_string(),
                );
            }
        };

        let config_arc: Arc<dyn OSConfig> = match &config {
            api::JoinedOSConfig::MacOS(conf) => conf.clone(),
            api::JoinedOSConfig::Relay(conf) => conf.clone(),
        };

        let token_provider = TokenProvider::new(account.clone(), config_arc.clone());

        let keychain_state: rustpush::keychain::KeychainClientState =
            match plist::from_file(dir.join("keychain.plist")) {
                Ok(s) => s,
                Err(e) => {
                    return Err(format!("read keychain state: {e}"));
                }
            };

        let cloudkit_state: rustpush::cloudkit::CloudKitState =
            match plist::from_file(dir.join("cloudkit.plist")) {
                Ok(s) => s,
                Err(e) => {
                    return Err(format!("read cloudkit state: {e}"));
                }
            };

        let ck_client: Arc<rustpush::cloudkit::CloudKitClient<_>> = Arc::new(
            rustpush::cloudkit::CloudKitClient {
                state: DebugRwLock::new(cloudkit_state),
                anisette: anisette.clone(),
                config: config_arc.clone(),
                token_provider: token_provider.clone(),
            },
        );

        let kc_path = dir.join("keychain.plist");
        let keychain = rustpush::keychain::KeychainClient {
            anisette: anisette.clone(),
            token_provider: token_provider.clone(),
            state: DebugRwLock::new(keychain_state),
            config: config_arc.clone(),
            update_state: Box::new(move |update| {
                if let Err(e) = plist::to_file_xml(&kc_path, update) {
                    log::warn!(
                        "setup_keychain_clique_with_bottle: failed to write keychain.plist: {e}"
                    );
                }
            }),
            container: Mutex::new(None),
            security_container: Mutex::new(None),
            client: ck_client.clone(),
        };

        // Idempotent: if already in the clique, nothing to do.
        if keychain.is_in_clique().await {
            return Ok(());
        }

        // Use the user-selected bottle directly to join the clique.
        keychain
            .join_clique_from_escrow(
                selected_bottle,
                escrow_passcode.as_bytes(),
                device_password.as_bytes(),
            )
            .await
            .map_err(|e| format!("join clique from escrow: {e:?}"))?;

        Ok(())
    }

    async fn get_viable_escrow_bottles(
        &self,
    ) -> crate::protocol::BottlesLookup {
        let dir = std::path::PathBuf::from(&self.state_path);

        // If the in-memory session isn't populated yet, reconstruct it
        // from gsa.plist first.
        if self.account.lock().unwrap().is_none()
            && self.reconstruct_account(true).await.is_err()
        {
            return crate::protocol::BottlesLookup::Unavailable(
                "get_viable_escrow_bottles: failed to reconstruct account".to_string(),
            );
        }

        let account = self.account.lock().unwrap().clone();
        let anisette = self.anisette.lock().unwrap().clone();
        let config = self.config.lock().unwrap().clone();
        let (account, anisette, config) = match (account, anisette, config) {
            (Some(a), Some(an), Some(c)) => (a, an, c),
            _ => {
                return crate::protocol::BottlesLookup::Unavailable(
                    "get_viable_escrow_bottles: missing account, anisette, or config"
                        .to_string(),
                );
            }
        };

        let config_arc: Arc<dyn OSConfig> = match &config {
            api::JoinedOSConfig::MacOS(conf) => conf.clone(),
            api::JoinedOSConfig::Relay(conf) => conf.clone(),
        };

        let token_provider = TokenProvider::new(account.clone(), config_arc.clone());

        let keychain_state: rustpush::keychain::KeychainClientState =
            match plist::from_file(dir.join("keychain.plist")) {
                Ok(s) => s,
                Err(e) => {
                    return crate::protocol::BottlesLookup::Unavailable(format!(
                        "get_viable_escrow_bottles: failed to load keychain.plist: {e}",
                    ));
                }
            };

        let cloudkit_state: rustpush::cloudkit::CloudKitState =
            match plist::from_file(dir.join("cloudkit.plist")) {
                Ok(s) => s,
                Err(e) => {
                    return crate::protocol::BottlesLookup::Unavailable(format!(
                        "get_viable_escrow_bottles: failed to load cloudkit.plist: {e}",
                    ));
                }
            };

        let ck_client: Arc<rustpush::cloudkit::CloudKitClient<_>> = Arc::new(
            rustpush::cloudkit::CloudKitClient {
                state: DebugRwLock::new(cloudkit_state),
                anisette: anisette.clone(),
                config: config_arc.clone(),
                token_provider: token_provider.clone(),
            },
        );

        let keychain = rustpush::keychain::KeychainClient {
            anisette: anisette.clone(),
            token_provider: token_provider.clone(),
            state: DebugRwLock::new(keychain_state),
            config: config_arc.clone(),
            update_state: Box::new(|_| {}),
            container: Mutex::new(None),
            security_container: Mutex::new(None),
            client: ck_client.clone(),
        };

        // Fetch viable escrow bottles.
        let bottles: Vec<(crate::api::EscrowData, rustpush::keychain::EscrowMetadata)> =
            match keychain.get_viable_bottles().await {
                Ok(b) => b,
                Err(e) => {
                    return crate::protocol::BottlesLookup::Unavailable(format!(
                        "get_viable_escrow_bottles: get_viable_bottles failed: {e:?}",
                    ));
                }
            };

        if bottles.is_empty() {
            return crate::protocol::BottlesLookup::NoBottles;
        }

        // Build one user-facing description per bottle (inline logic
        // mirroring `describe_escrow_metadata_for_user` in the UI module
        // — kept here to avoid a circular dependency from protocol → ui).
        let described: Vec<(crate::api::EscrowData, String)> = bottles
            .into_iter()
            .map(|(data, meta)| {
                let desc = describe_bottle(&meta);
                (data, desc)
            })
            .collect();

        crate::protocol::BottlesLookup::Bottles(described)
    }
}

/// Format an `EscrowMetadata` into a user-facing display string.
/// Mirrors the logic in `ui::describe_escrow_metadata_for_user`.
fn describe_bottle(meta: &rustpush::keychain::EscrowMetadata) -> String {
    let mut parts: Vec<String> = Vec::new();

    let dict = meta.client_metadata.as_dictionary();
    let device_name = dict
        .and_then(|d| d.get("device_name"))
        .and_then(|v| v.as_string())
        .map(|s| s.to_string());
    let device_model = dict
        .and_then(|d| d.get("device_model"))
        .and_then(|v| v.as_string())
        .map(|s| s.to_string());

    match (&device_name, &device_model) {
        (Some(name), Some(model)) => {
            parts.push(name.clone());
            parts.push(model.clone());
        }
        (Some(name), None) => parts.push(name.clone()),
        (None, Some(model)) => parts.push(model.clone()),
        (None, None) => parts.push(meta.serial.clone()),
    }

    parts.push(meta.timestamp.clone());

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

/// Run an iCloud sync, optionally setting up the iCloud Keychain clique
/// first if a password is provided. The behavior is:
///
/// - If `password` is `Some(p)`, first call `setup_keychain_clique(p)`.
///   If that fails, return the error (the sync is not attempted).
/// - If `password` is `None`, skip the setup and proceed directly to the
///   sync.
/// - Then call `sync_missed_messages(store, cutoff_ms, force)` and
///   return its result wrapped in `Ok(...)`.
///
/// This is the orchestrator the UI calls after the user submits the
/// iCloud password dialog (or skips the dialog if the clique is already
/// set up).
#[allow(dead_code)]
pub async fn run_clique_setup_then_sync(
    backend: &dyn Backend,
    store: &crate::store::Store,
    cutoff_ms: i64,
    force: bool,
    password: Option<String>,
) -> std::result::Result<crate::sync::SyncResult, String> {
    // If a password was provided, set up the iCloud Keychain clique first.
    // On error, return the error immediately without attempting the sync.
    if let Some(p) = password {
        // Backward-compat bridge: callers that only supply one password
        // use it for both the escrow passcode and the new device password.
        backend.setup_keychain_clique(&p, &p).await?;
    }

    // Run the missed-messages sync and wrap the result in Ok.
    Ok(backend.sync_missed_messages(store, cutoff_ms, force).await)
}

/// Like [`run_clique_setup_then_sync`] but accepts two distinct setup secrets:
/// the old trusted-device passcode used to recover an existing escrow bottle,
/// and the new local device password used to create this device's bottle.
///
/// The behavior mirrors the single-password version:
/// - If **both** secrets are `Some(...)`, call `setup_keychain_clique(old, new)`
///   first. If that fails, return the error without attempting the sync.
/// - If **either** secret is `None`, skip the setup and proceed directly to
///   the sync.
/// - Then call `sync_missed_messages(store, cutoff_ms, force)` and return its
///   result wrapped in `Ok(...)`.
///
/// This is the entry point the UI should call instead of the single-password
/// function once it can collect both inputs from the user.
#[allow(dead_code)]
pub async fn run_clique_setup_then_sync_with_secrets(
    backend: &dyn Backend,
    store: &crate::store::Store,
    cutoff_ms: i64,
    force: bool,
    escrow_passcode: Option<String>,
    device_password: Option<String>,
) -> std::result::Result<crate::sync::SyncResult, String> {
    // Both secrets must be present to attempt setup.
    if let (Some(old), Some(new)) = (&escrow_passcode, &device_password) {
        backend.setup_keychain_clique(old, new).await?;
    }

    // Run the missed-messages sync and wrap the result in Ok.
    Ok(backend.sync_missed_messages(store, cutoff_ms, force).await)
}

/// Orchestrate the "Sync Now" or "toggle cloud sync on" user flow.
///
/// Called from the click handler (for "Sync Now") or the switch handler
/// (for toggling `cloud_sync_enabled` to true). The function:
///
/// 1. Calls `decide_clique_setup_action(backend)` to check whether the
///    iCloud Keychain clique is set up.
/// 2. Based on the action:
///    - `SyncNow` → proceeds directly to the sync (no password needed).
///    - `PromptForPassword` → calls `password_prompt` to obtain the
///      two setup secrets: the old trusted-device passcode (to recover
///      the existing escrow bottle) and the new local device password
///      (to protect this device's new bottle). If the user submits
///      both, proceeds with the sync via
///      [`run_clique_setup_then_sync_with_secrets`]. If the user
///      cancels, returns `Ok(SyncResult::default())` to indicate "no
///      sync was attempted".
///    - `Abort(reason)` → returns `Err(reason)`.
///
/// `password_prompt` is a closure (or function pointer) that the UI
/// layer provides to show the password dialog and wait for the user's
/// response. It returns `Some((escrow_passcode, device_password))` on
/// submit, `None` on cancel.
///
/// The closure is run via `tokio::task::spawn_blocking` so it can
/// safely block the calling thread (it blocks on the GTK main thread
/// to present the dialog, then on a oneshot channel for the response).
/// Running it directly on a tokio worker thread would deadlock the
/// runtime ("Cannot block the current thread from within a runtime").
#[allow(dead_code)]
pub async fn orchestrate_sync_now_flow<F>(
    backend: &dyn Backend,
    store: &crate::store::Store,
    cutoff_ms: i64,
    force: bool,
    password_prompt: F,
) -> std::result::Result<crate::sync::SyncResult, String>
where
    F: FnOnce() -> Option<(String, String)> + Send + 'static,
{
    let action = crate::protocol::decide_clique_setup_action(backend).await;
    let (old_passcode, new_password) = match action {
        CliqueSetupAction::SyncNow => (None, None),
        CliqueSetupAction::PromptForPassword => {
            match tokio::task::spawn_blocking(password_prompt).await {
                Ok(Some((old, new))) => (Some(old), Some(new)),
                Ok(None) => return Ok(crate::sync::SyncResult::default()),
                Err(join_err) => {
                    return Err(format!("password prompt task failed: {join_err}"))
                }
            }
        }
        CliqueSetupAction::Abort(reason) => return Err(reason),
    };
    run_clique_setup_then_sync_with_secrets(
        backend,
        store,
        cutoff_ms,
        force,
        old_passcode,
        new_password,
    )
    .await
}

/// The result of a bottle-selection prompt: the user's chosen escrow bottle
/// and the two distinct setup secrets (old trusted-device passcode and new
/// local device password).
#[derive(Debug, Clone)]
pub struct CliqueSetupPromptResult {
    /// The escrow bottle the user selected from the list of viable bottles.
    pub bottle: crate::api::EscrowData,
    /// The passcode the user entered for the selected device (decrypts the
    /// escrow bottle).
    pub old_passcode: String,
    /// The new local device password (creates this device's bottle).
    pub new_password: String,
}

/// Like [`orchestrate_sync_now_flow`] but the password-prompt closure returns
/// an [`CliqueSetupPromptResult`] that includes the user-selected escrow
/// bottle together with the two setup secrets, and the orchestrator forwards
/// the selected bottle to the backend via
/// [`Backend::setup_keychain_clique_with_bottle`].
///
/// This is the entry point the UI should call once it can show a bottle
/// selection dialog alongside the password entry fields. The old
/// `orchestrate_sync_now_flow` (which takes a closure returning a bare
/// `Option<(String, String)>`) is preserved for backward compatibility.
///
/// # Behavior
///
/// 1. Calls `decide_clique_setup_action(backend)` to check whether the
///    iCloud Keychain clique is set up.
/// 2. Based on the action:
///    - `SyncNow` → proceeds directly to the sync (no setup needed).
///    - `PromptForPassword` → calls `prompt` (via `spawn_blocking`) to
///      obtain a [`CliqueSetupPromptResult`]. If the user submits (Some),
///      calls `backend.setup_keychain_clique_with_bottle(bottle, old, new)`,
///      then syncs. If the user cancels (None), returns the default
///      no-op sync result.
///    - `Abort(reason)` → returns `Err(reason)`.
#[allow(dead_code)]
pub async fn orchestrate_sync_now_flow_with_bottle_prompt<F>(
    backend: &dyn Backend,
    store: &crate::store::Store,
    cutoff_ms: i64,
    force: bool,
    prompt: F,
) -> std::result::Result<crate::sync::SyncResult, String>
where
    F: FnOnce() -> Option<CliqueSetupPromptResult> + Send + 'static,
{
    let action = crate::protocol::decide_clique_setup_action(backend).await;
    match action {
        CliqueSetupAction::SyncNow => {
            // Clique already set up; just sync.
            Ok(backend.sync_missed_messages(store, cutoff_ms, force).await)
        }
        CliqueSetupAction::PromptForPassword => {
            let result = match tokio::task::spawn_blocking(prompt).await {
                Ok(Some(r)) => r,
                Ok(None) => return Ok(crate::sync::SyncResult::default()),
                Err(join_err) => {
                    return Err(format!("password prompt task failed: {join_err}"))
                }
            };
            // Forward the user-selected bottle + both secrets to the backend.
            backend
                .setup_keychain_clique_with_bottle(
                    &result.bottle,
                    &result.old_passcode,
                    &result.new_password,
                )
                .await?;
            Ok(backend.sync_missed_messages(store, cutoff_ms, force).await)
        }
        CliqueSetupAction::Abort(reason) => Err(reason),
    }
}

/// Spike-only: dump an inbound message's salient fields. `Message` doesn't
/// derive `Debug`, so we hand-format the variants we care about.
fn log_inst(inst: &MessageInst) {
    let (participants, name) = match &inst.conversation {
        Some(c) => (
            c.participants.join(", "),
            c.cv_name.clone().unwrap_or_default(),
        ),
        None => (String::new(), String::new()),
    };
    let body = match &inst.message {
        Message::Message(n) => {
            let mut s = format!("text={:?}", n.parts.raw_text());
            if let Some(r) = &n.reply_guid {
                s += &format!(" reply_to={r}");
            }
            if let Some(e) = &n.effect {
                s += &format!(" effect={e}");
            }
            if let Some(sub) = &n.subject {
                s += &format!(" subject={sub:?}");
            }
            s
        }
        other => format!("[{}]", variant_name(other)),
    };
    let name = if name.is_empty() {
        String::new()
    } else {
        format!(" name={name:?}")
    };
    log::info!(
        "RECV id={} ts={} sender={:?} chat=[{participants}]{name} {body}",
        inst.id,
        inst.sent_timestamp,
        inst.sender,
    );
}

fn variant_name(m: &Message) -> &'static str {
    match m {
        Message::Message(_) => "Message",
        Message::RenameMessage(_) => "Rename",
        Message::ChangeParticipants(_) => "ChangeParticipants",
        Message::React(_) => "React",
        Message::Delivered => "Delivered",
        Message::Read => "Read",
        Message::Typing(..) => "Typing",
        Message::Unsend(_) => "Unsend",
        Message::Edit(_) => "Edit",
        Message::IconChange(_) => "IconChange",
        Message::Error(_) => "Error",
        _ => "Other",
    }
}

/// Map a decrypted `MessageInst` into a store [`Ingest`]. `my_handles` is our
/// own address set (from `get_handles`), used to compute `is_from_me`. This is
/// the bridge receive loop will run before `Store::apply`.
pub fn ingest_from(inst: &MessageInst, my_handles: &[String]) -> Ingest {
    let guid = inst.id.clone();
    let date = inst.sent_timestamp as i64;
    let sender = inst.sender.clone();
    let is_from_me = inst
        .sender
        .as_deref()
        .map(|s| my_handles.iter().any(|h| h.eq_ignore_ascii_case(s)))
        .unwrap_or(false);

    // Receipts and tapbacks carry no conversation; content messages do.
    let chat = |service: Option<String>| -> ChatRef {
        match &inst.conversation {
            Some(c) => ChatRef {
                participants: c.participants.clone(),
                display_name: c.cv_name.clone(),
                service,
            },
            None => ChatRef::default(),
        }
    };

    match &inst.message {
        Message::Message(n) => {
            let service = Some(service_str(&n.service));
            Ingest::Message(IncomingMessage {
                guid,
                chat: chat(service.clone()),
                sender,
                is_from_me,
                text: Some(n.parts.raw_text()),
                subject: n.subject.clone(),
                service,
                date,
                effect: n.effect.clone(),
                reply_to_guid: n.reply_guid.clone(),
                reply_part: n.reply_part.clone(),
                item_type: 0,
                attachments: Vec::new(),
                pending: false,
            })
        }
        Message::React(r) => match tapback_type(&r.reaction) {
            Some(associated_type) => Ingest::Tapback(Tapback {
                guid,
                chat: chat(None),
                sender,
                is_from_me,
                date,
                associated_guid: r.to_uuid.clone(),
                associated_part: r.to_part.map(|p| p.to_string()),
                associated_type,
            }),
            None => Ingest::Ignored("react-nonstandard"),
        },
        Message::Edit(e) => Ingest::Edited {
            guid: e.tuuid.clone(),
            text: e.new_parts.raw_text(),
        },
        Message::Read => Ingest::Receipt(Receipt::Read { guid, date }),
        Message::Delivered => Ingest::Receipt(Receipt::Delivered { guid, date }),
        other => Ingest::Ignored(variant_name(other)),
    }
}

/// Where downloaded/sent attachment files live (mirrors `glib::user_data_dir`).
fn attachments_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
                .join(".local/share")
        });
    base.join("bubbles").join("attachments")
}

fn ext_for(mime: &str, name: &str) -> String {
    if let Some(dot) = name.rfind('.') {
        if dot + 1 < name.len() {
            return name[dot..].to_string();
        }
    }
    match mime {
        "image/jpeg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/heic" | "image/heif" => ".heic",
        "image/webp" => ".webp",
        "video/mp4" | "video/quicktime" => ".mp4",
        "application/pdf" => ".pdf",
        _ => ".bin",
    }
    .to_string()
}

/// Apple names the still and motion members of a Live Photo with the same
/// basename (for example, `IMG_1234.HEIC` and `IMG_1234.MOV`). Keep that
/// basename as the store-level pairing key; the decoded `iris` flag guards
/// against pairing ordinary files that happen to share a name.
fn live_photo_pairing_id(name: &str) -> Option<String> {
    std::path::Path::new(name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .map(str::to_lowercase)
}

fn mime_to_uti(mime: &str) -> String {
    match mime {
        "image/jpeg" => "public.jpeg",
        "image/png" => "public.png",
        "image/gif" => "com.compuserve.gif",
        "image/heic" | "image/heif" => "public.heic",
        "image/webp" => "org.webmproject.webp",
        "video/mp4" => "public.mpeg-4",
        "video/quicktime" => "com.apple.quicktime-movie",
        "application/pdf" => "com.adobe.pdf",
        _ => "public.data",
    }
    .to_string()
}

/// Copy an outbound file into the attachment cache so the UI can render it from
/// a stable path immediately after sending.
fn cache_copy(src: &str, guid: &str, part: i64, name: &str) -> Option<std::path::PathBuf> {
    let dir = attachments_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let dest = dir.join(format!("{guid}_{part}{}", ext_for("", name)));
    std::fs::copy(src, &dest).ok()?;
    Some(dest)
}

/// Download every attachment on an inbound message into the cache, returning the
/// records to persist. Failures are logged and skipped, not fatal.
async fn download_inbound(
    inst: &MessageInst,
    conn: &APSConnection,
    guid: &str,
) -> Vec<AttachmentRecord> {
    let Message::Message(n) = &inst.message else {
        return Vec::new();
    };
    if !n.parts.has_attachments() {
        return Vec::new();
    }
    let dir = attachments_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("attachment dir: {e}");
        return Vec::new();
    }

    let mut out = Vec::new();
    for (i, p) in n.parts.0.iter().enumerate() {
        let MessagePart::Attachment(att) = &p.part else {
            continue;
        };
        let part_index = p.idx.unwrap_or(i) as i64;
        let path = dir.join(format!("{guid}_{part_index}{}", ext_for(&att.mime, &att.name)));
        let att_owned = att.clone();
        let conn_owned = conn.clone();
        match crate::attachment_cache::download_to_path(&path, move |file| {
            Box::pin(async move {
                att_owned.get_attachment(&conn_owned, file, |_, _| {}).await
            })
        }).await {
            Ok(()) => {
                log::info!("↓ saved attachment {} ({})", att.name, att.mime);
                out.push(AttachmentRecord {
                    guid: None,
                    mime: Some(att.mime.clone()),
                    name: Some(att.name.clone()),
                    total_bytes: Some(att.get_size() as i64),
                    local_path: Some(path.to_string_lossy().into_owned()),
                    part_index: Some(part_index),
                    is_sticker: false,
                    is_live_photo: att.iris,
                    pairing_id: live_photo_pairing_id(&att.name),
                });
            }
            Err(crate::attachment_cache::DownloadError::Download(e)) => {
                log::warn!("download attachment {}: {e:?}", att.name);
            }
            Err(crate::attachment_cache::DownloadError::Io(e)) => {
                log::warn!("io writing attachment {}: {e}", att.name);
            }
        }
    }
    out
}

fn service_str(t: &MessageType) -> String {
    match t {
        MessageType::IMessage => "iMessage".into(),
        MessageType::SMS { .. } => "SMS".into(),
    }
}

/// Apple tapback code: 2000-2005 add, 3000-3005 remove. `None` for emoji /
/// sticker / extension reactions we don't model yet (logged as Ignored).
fn tapback_type(t: &ReactMessageType) -> Option<i64> {
    let ReactMessageType::React { reaction, enable } = t else {
        return None;
    };
    let idx: i64 = match reaction {
        Reaction::Heart => 0,
        Reaction::Like => 1,
        Reaction::Dislike => 2,
        Reaction::Laugh => 3,
        Reaction::Emphasize => 4,
        Reaction::Question => 5,
        _ => return None,
    };
    Some(if *enable { 2000 + idx } else { 3000 + idx })
}

// --- link preview extraction ---

/// Pick the primary thumbnail blob out of a `LinkMeta`. The image is *not* a
/// normal MMCS attachment on the message — rustpush decodes the balloon body
/// and keeps its inline attachments here, indexed by the
/// `RichLinkImageAttachmentSubstitute` that the `LPLinkMetadata` carries.
/// `None` if the sender didn't include one.
fn preview_image_bytes(lm: &LinkMeta) -> Option<&[u8]> {
    let idx = lm.data.image.as_ref()?.rich_link_image_attachment_substitute_index as usize;
    lm.attachments.get(idx).map(|v| v.as_slice())
}

/// Pick a file extension for the thumbnail blob. Prefer the substitute's
/// `mime_type` (set by most modern iOS/macOS senders) and fall back to sniffing
/// the magic bytes. The renderer only cares that the file is the kind of image
/// the loader can decode; an unrecognised blob falls back to the neutral icon.
fn preview_image_ext(bytes: &[u8], mime: Option<&str>) -> &'static str {
    if let Some(m) = mime {
        let m = m.split(';').next().unwrap_or("").trim();
        match m {
            "image/png" => return "png",
            "image/jpeg" | "image/jpg" => return "jpg",
            "image/gif" => return "gif",
            "image/webp" => return "webp",
            "image/heic" | "image/heif" => return "heic",
            _ => {}
        }
    }
    if bytes.len() >= 8 && bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        "png"
    } else if bytes.len() >= 3 && bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "jpg"
    } else if bytes.len() >= 6 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        "gif"
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        "webp"
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        // ISO base media: HEIC/HEIF/MP4 all share this. Default to .heic for
        // the common Apple case; the renderer can ignore what it can't decode.
        "heic"
    } else {
        "bin"
    }
}

/// Persist the thumbnail bytes to the cache and return the path. Errors are
/// logged and treated as "no image": a card with no thumbnail still renders
/// (using the neutral-icon fallback), so a flaky disk must not drop the link.
fn write_preview_image(guid: &str, part_idx: i64, bytes: &[u8], mime: Option<&str>) -> Option<String> {
    let dir = crate::store::preview_image_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("preview dir {dir:?}: {e}");
        return None;
    }
    let ext = preview_image_ext(bytes, mime);
    let path = dir.join(format!("{guid}_{part_idx}.{ext}"));
    match std::fs::write(&path, bytes) {
        Ok(()) => Some(path.to_string_lossy().into_owned()),
        Err(e) => {
            log::warn!("write preview {path:?}: {e}");
            None
        }
    }
}

/// URL the receiver should display / open. The `original_url` (what the sender
/// actually typed) wins when present and not blank — clicking the card opens
/// the *intended* link, not whatever redirect chain the sender's previewer
/// followed. We fall back to `url` (the post-redirect canonical) when the
/// original is missing, and to `None` when both are absent (degenerate).
fn preview_url(data: &LPLinkMetadata) -> Option<String> {
    if let Some(s) = original_url(data) {
        if !s.is_empty() {
            return Some(s);
        }
    }
    canonical_url(data).filter(|s| !s.is_empty())
}

fn canonical_url(data: &LPLinkMetadata) -> Option<String> {
    data.url.as_ref().map(|u| u.clone().into())
}

fn original_url(data: &LPLinkMetadata) -> Option<String> {
    data.original_url.as_ref().map(|u| u.clone().into())
}

/// Pull the (rare) `image_metadata` size string into our i64 width/height
/// fields. The size is "{w}x{h}" or "{w}×{h}" — best-effort, no fallback to
/// decoding the image (we don't want a synchronous decode on the receive path).
fn preview_dimensions(data: &LPLinkMetadata) -> (Option<i64>, Option<i64>) {
    let Some(s) = data.image_metadata.as_ref().map(|m| m.size.clone()) else {
        return (None, None);
    };
    parse_size_string(&s)
}

fn parse_size_string(s: &str) -> (Option<i64>, Option<i64>) {
    // Common separators: "1200x630", "1200×630", "1200 X 630".
    let cleaned: String = s
        .chars()
        .map(|c| if c == '\u{00D7}' || c == 'x' || c == 'X' { 'x' } else { c })
        .collect();
    let mut parts = cleaned.split('x');
    let w = parts.next().and_then(|p| p.trim().parse::<i64>().ok());
    let h = parts.next().and_then(|p| p.trim().parse::<i64>().ok());
    (w, h)
}

/// Build the `MessageLinkPreview` to persist for an inbound message, or `None`
/// if the message carries no `link_meta`. The thumbnail blob is written to the
/// cache here so the UI can read straight from disk later.
fn extract_link_preview(guid: &str, lm: &LinkMeta) -> Option<MessageLinkPreview> {
    let data = &lm.data;
    let url = preview_url(data);
    let original_url = original_url(data);
    let title = data.title.clone();
    let summary = data.summary.clone();
    let is_placeholder = data.is_incomplete.unwrap_or(false);
    let (image_width, image_height) = preview_dimensions(data);
    // Use the substitute's mime when the sender gave us one; fall through to
    // magic-byte sniffing for older senders / stripped payloads.
    let mime = data
        .image
        .as_ref()
        .map(|s| s.mime_type.as_str())
        .filter(|m| !m.is_empty());
    let image_path = preview_image_bytes(lm)
        .and_then(|bytes| write_preview_image(guid, 0, bytes, mime));
    Some(MessageLinkPreview {
        message_guid: guid.to_string(),
        part_idx: 0,
        url,
        original_url,
        title,
        summary,
        image_path,
        image_width,
        image_height,
        is_placeholder,
    })
}

/// Default: acknowledge inbound messages with Delivered receipts.
const SEND_DELIVERED_RECEIPTS: bool = true;

fn conversation_from(chat: &ChatRef) -> ConversationData {
    ConversationData {
        participants: chat.participants.clone(),
        cv_name: chat.display_name.clone(),
        sender_guid: None,
        after_guid: None,
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn is_from_me(inst: &MessageInst, handles: &[String]) -> bool {
    inst.sender
        .as_deref()
        .map(|s| handles.iter().any(|h| h.eq_ignore_ascii_case(s)))
        .unwrap_or(false)
}

fn is_incoming_content(inst: &MessageInst, handles: &[String]) -> bool {
    matches!(inst.message, Message::Message(_)) && !is_from_me(inst, handles)
}

/// Guard for launch-time 6005 retry-now suppression.
///
/// Allows the first 6005 resource failure to trigger `identity.refresh_now()`
/// immediately, then suppresses repeats within a configurable cooldown, and
/// re-arms after the cooldown elapses or on a `Generated` (successful
/// registration) state. Non-6005 failures are always ignored.
///
/// Pure — no I/O, no async, no shared state.
#[derive(Debug)]
pub struct Launch6005RetryGuard {
    cooldown_secs: u64,
    last_retry_secs: Option<u64>,
}

impl Launch6005RetryGuard {
    pub fn new(cooldown: std::time::Duration) -> Self {
        Self {
            cooldown_secs: cooldown.as_secs(),
            last_retry_secs: None,
        }
    }

    /// Evaluate a resource state change and decide whether to trigger
    /// `identity.refresh_now()`.
    ///
    /// Returns `true` when the guard allows a 6005 retry-now (the caller
    /// should call `identity.refresh_now()`). Returns `false` to suppress
    /// (or for non-6005 states).
    ///
    /// Side effect: on `Generated` the internal cooldown is reset so a
    /// *future* 6005 can fire immediately; on an allowed 6005 the last-retry
    /// timestamp is updated to the current `now_secs`.
    pub fn evaluate(&mut self, state: &rustpush::ResourceState, now_secs: u64) -> bool {
        match state {
            rustpush::ResourceState::Generated => {
                self.last_retry_secs = None;
                false
            }
            rustpush::ResourceState::Generating | rustpush::ResourceState::Closed => {
                false
            }
            rustpush::ResourceState::Failed(failure) => {
                if !is_6005_error(&failure.error) {
                    return false;
                }
                match self.last_retry_secs {
                    None => {
                        self.last_retry_secs = Some(now_secs);
                        true
                    }
                    Some(last) => {
                        if now_secs.saturating_sub(last) >= self.cooldown_secs {
                            self.last_retry_secs = Some(now_secs);
                            true
                        } else {
                            false
                        }
                    }
                }
            }
        }
    }
}

/// Walk a `PushError` chain and return `true` if any layer contains an
/// `IDSError(6005)` — Apple's "Bad authentication, re-enter device details
/// if persistent" rejection.
///
/// This is used by `send_text` to detect a stale IDS cert (the typical
/// symptom: every send returns 6005 and the auto-rereg has already fired
/// and failed once) and trigger an in-process self-heal via a fresh rereg.
fn is_6005_error(err: &PushError) -> bool {
    match err {
        PushError::LookupFailed(IDSError(6005))
        | PushError::AuthInvalid(IDSError(6005))
        | PushError::RegisterFailed(IDSError(6005)) => true,
        // Errors are sometimes wrapped one or more layers deep; recurse.
        PushError::DoNotRetry(inner) => is_6005_error(inner),
        // `ResourceFailure(ResourceFailure { retry_wait, error })` is the
        // form produced when a `ResourceManager::refresh()` call observes
        // a failed rereg. The inner `error` is the original failure
        // (typically `AuthInvalid(IDSError(6005))` for the case we care
        // about), so we recurse into it.
        PushError::ResourceFailure(rf) => is_6005_error(&rf.error),
        _ => false,
    }
}

#[cfg(test)]
mod is_6005_error_tests {
    use super::*;
    use std::sync::Arc;

    /// Regression: a `ResourceFailure` wrapping `AuthInvalid(IDSError(6005))`
    /// must be detected as a 6005 error. This is the form produced by
    /// `ResourceManager::refresh()` when an auto-rereg fails with 6005.
    /// Without unwrapping `ResourceFailure`, the self-heal in `send_text`
    /// never fires (the user sees a silent "send failed" with no recovery
    /// attempt, exactly the bug from the 2026-06-29 13:27 incident).
    #[test]
    fn resource_failure_wrapping_6005_is_detected() {
        let inner = Arc::new(PushError::AuthInvalid(IDSError(6005)));
        let rf = rustpush::ResourceFailure {
            retry_wait: Some(300),
            error: inner,
        };
        let err = PushError::ResourceFailure(rf);
        assert!(
            is_6005_error(&err),
            "ResourceFailure wrapping AuthInvalid(6005) must be detected as 6005"
        );
    }

    /// `ResourceFailure` wrapping a non-6005 error must NOT be detected.
    /// (E.g., a permanent network error wrapped in ResourceFailure should
    /// not trigger the cert self-heal.)
    #[test]
    fn resource_failure_wrapping_non_6005_is_not_detected() {
        let inner = Arc::new(PushError::AuthInvalid(IDSError(9999)));
        let rf = rustpush::ResourceFailure {
            retry_wait: Some(300),
            error: inner,
        };
        let err = PushError::ResourceFailure(rf);
        assert!(
            !is_6005_error(&err),
            "ResourceFailure wrapping AuthInvalid(9999) must NOT be detected as 6005"
        );
    }

    /// `DoNotRetry(ResourceFailure(AuthInvalid(6005)))` — the doubly-wrapped
    /// form seen in the delivered-receipt / typing / read-receipt warnings
    /// in the production logs — must also be detected (recurses through
    /// both `DoNotRetry` and `ResourceFailure`).
    #[test]
    fn do_not_retry_wrapping_resource_failure_with_6005_is_detected() {
        let inner = Arc::new(PushError::AuthInvalid(IDSError(6005)));
        let rf = rustpush::ResourceFailure {
            retry_wait: Some(300),
            error: inner,
        };
        let err = PushError::DoNotRetry(Box::new(PushError::ResourceFailure(rf)));
        assert!(
            is_6005_error(&err),
            "DoNotRetry(ResourceFailure(AuthInvalid(6005))) must be detected as 6005"
        );
    }
}

/// Build a wire-level `ReactMessage` from the caller's parameters.
fn build_react_message(
    target_guid: &str,
    target_part: Option<u64>,
    to_text: &str,
    reaction: &ReactMessageType,
) -> ReactMessage {
    ReactMessage {
        to_uuid: target_guid.to_string(),
        to_part: target_part.or(Some(0)),
        reaction: reaction.clone(),
        to_text: to_text.to_string(),
        embedded_profile: None,
    }
}

/// Build a `MessageParts` wire payload for an iMessage that carries an
/// attachment plus an optional text caption.
///
/// When `text` is `Some(s)` the payload contains two parts in this order:
/// `MessagePart::Attachment` then `MessagePart::Text` (caption).
/// The iPhone uses the first part's wire `message-part` index as the
/// primary render target, so the attachment must come first to be
/// rendered inline alongside the caption (rather than being demoted to
/// previews-only).
/// When `text` is `None` the payload contains a single `MessagePart::Attachment`.
fn build_attachment_message_parts(text: Option<&str>, attachment: Attachment) -> MessageParts {
    match text {
        Some(s) => MessageParts(vec![
            IndexedMessagePart {
                part: MessagePart::Attachment(attachment),
                idx: None,
                ext: None,
            },
            IndexedMessagePart {
                part: MessagePart::Text(s.to_string(), Default::default()),
                idx: None,
                ext: None,
            },
        ]),
        None => MessageParts(vec![IndexedMessagePart {
            part: MessagePart::Attachment(attachment),
            idx: None,
            ext: None,
        }]),
    }
}

/// Generate a fresh GUID (uppercased UUID v4) for message identification.
fn new_guid() -> String {
    glib::uuid_string_random().to_string().to_uppercase()
}

/// Build a [`MessageInst`] ready to send as a reaction (tapback) to a target
/// message. Sets `inst.id` to a fresh GUID so the receiver can correlate it.
fn build_react_message_inst(
    chat: &ChatRef,
    my_handle: &str,
    target_guid: &str,
    target_part: Option<u64>,
    target_text: &str,
    reaction: &ReactMessageType,
) -> MessageInst {
    let conversation = conversation_from(chat);
    let react = build_react_message(target_guid, target_part, target_text, reaction);
    let mut inst = MessageInst::new(conversation, my_handle, Message::React(react));
    inst.id = new_guid();
    inst
}

/// Build a [`MessageInst`] ready to send as an edit to a previously-sent
/// message. Sets `inst.id` to a fresh GUID so the receiver can correlate it.
fn build_edit_message_inst(
    chat: &ChatRef,
    my_handle: &str,
    target_guid: &str,
    edit_part: u64,
    new_text: String,
) -> MessageInst {
    let conversation = conversation_from(chat);
    let edit = EditMessage {
        tuuid: target_guid.to_string(),
        edit_part,
        new_parts: MessageParts(vec![IndexedMessagePart {
            part: MessagePart::Text(new_text, Default::default()),
            idx: None,
            ext: None,
        }]),
    };
    let mut inst = MessageInst::new(conversation, my_handle, Message::Edit(edit));
    inst.id = new_guid();
    inst
}

/// Send a Delivered (`read=false`) or Read (`read=true`) receipt for `inst`,
/// addressed from whichever of our `handles` is in the conversation.
async fn send_receipt_for(
    imclient: &Arc<IMClient>,
    inst: &MessageInst,
    handles: &[String],
    read: bool,
) {
    let Some(conversation) = inst.conversation.clone() else {
        return;
    };
    let my_handle = conversation
        .participants
        .iter()
        .find(|p| handles.iter().any(|h| h.eq_ignore_ascii_case(p)))
        .cloned()
        .or_else(|| handles.first().cloned());
    let Some(my_handle) = my_handle else {
        return;
    };
    let msg = if read { Message::Read } else { Message::Delivered };
    let mut receipt = MessageInst::new(conversation, &my_handle, msg);
    receipt.id = inst.id.clone();
    let kind = if read { "read" } else { "delivered" };
    match imclient.send(&mut receipt).await {
        Ok(_) => log::info!("→ sent {kind} receipt for {}", receipt.id),
        Err(e) => log::warn!("{kind} receipt error: {e:?}"),
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the `Backend` impl on `RustpushBackend` and its helper
    //! functions. The whole module is gated by `--features rustpush`, so this
    //! `mod tests` only compiles with the feature — matches the project's
    //! existing per-module cfg convention.
    use super::*;
    use rustpush::AttachmentType;

    /// Pin: the pure helper that builds the wire-level `ReactMessage` for
    /// `send_reaction`:
    ///
    /// * carries the caller's `(target_guid, reaction)` straight through to
    ///   `(to_uuid, reaction)` and leaves `embedded_profile` unset;
    /// * **defaults `to_part` to `Some(0)`** when the caller passes `None`
    ///   (matches Android OpenBubbles' `toPart: repPart ?? 0` so the iPhone
    ///   can resolve the `p:N/` part prefix on the target message);
    /// * passes through non-`None` `to_part` values unchanged (the default
    ///   must not override an explicit part index);
    /// * **threads `to_text` through** to the wire `ams` field (the iPhone
    ///   uses `ams` to render the reaction chip in the chat list).
    #[test]
    fn build_react_message_field_mapping() {
        // Case 1: `target_part = None` -> defaults to `Some(0)`; `to_text = "Hello world"` flows through.
        // Heart reaction, `enable = true`. This is the bug-repro case: the
        // pre-fix code passed `target_part = None` straight through, leaving
        // the `amk` field as a bare GUID with no `p:0/` prefix, so the iPhone
        // couldn't attach the reaction chip to the target message.
        let r1 = build_react_message(
            "target-guid-1",
            None,
            "Hello world",
            &ReactMessageType::React {
                reaction: Reaction::Heart,
                enable: true,
            },
        );
        assert_eq!(r1.to_uuid, "target-guid-1", "to_uuid should be the target_guid");
        assert_eq!(
            r1.to_part,
            Some(0),
            "to_part: None must default to Some(0) so the iPhone sees the p:0/ prefix"
        );
        assert_eq!(
            r1.to_text, "Hello world",
            "to_text should flow through to the wire field (ams)"
        );
        let (reaction, enable) = match r1.reaction.clone() {
            ReactMessageType::React { reaction, enable } => (reaction, enable),
            _ => panic!("expected ReactMessageType::React variant"),
        };
        assert!(
            matches!(reaction, Reaction::Heart),
            "reaction should be Heart"
        );
        assert!(enable, "enable should be true");
        assert!(
            r1.embedded_profile.is_none(),
            "embedded_profile should be None"
        );

        // Case 2: explicit `target_part = Some(0)` stays `Some(0)`; `to_text = ""` stays empty.
        // Like reaction, `enable = false`. Pins that the defaulting doesn't
        // re-default a part the caller already set, and that the empty
        // "last-resort" `to_text` value is preserved (the iPhone still has a
        // valid `ams` field, just an empty one).
        let r2 = build_react_message(
            "target-guid-2",
            Some(0),
            "",
            &ReactMessageType::React {
                reaction: Reaction::Like,
                enable: false,
            },
        );
        assert_eq!(r2.to_uuid, "target-guid-2", "to_uuid should be the target_guid");
        assert_eq!(
            r2.to_part,
            Some(0),
            "to_part: Some(0) should stay Some(0); the default must not override an explicit value"
        );
        assert!(
            r2.to_text.is_empty(),
            "to_text should stay empty when the caller passes \"\""
        );
        let (reaction, enable) = match r2.reaction.clone() {
            ReactMessageType::React { reaction, enable } => (reaction, enable),
            _ => panic!("expected ReactMessageType::React variant"),
        };
        assert!(
            matches!(reaction, Reaction::Like),
            "reaction should be Like"
        );
        assert!(!enable, "enable should be false");
        assert!(
            r2.embedded_profile.is_none(),
            "embedded_profile should be None"
        );

        // Case 3: explicit non-zero `target_part = Some(3)` is preserved.
        // Confirms the defaulting logic doesn't clobber a caller-supplied
        // part index (matters for multi-part messages where the part index
        // disambiguates which balloon the reaction targets).
        let r3 = build_react_message(
            "target-guid-3",
            Some(3),
            "Caption text",
            &ReactMessageType::React {
                reaction: Reaction::Heart,
                enable: true,
            },
        );
        assert_eq!(
            r3.to_part,
            Some(3),
            "to_part: Some(3) must be preserved, not overwritten with the default Some(0)"
        );
        assert_eq!(
            r3.to_text, "Caption text",
            "to_text should flow through to the wire field (ams)"
        );
    }

    /// Pin: the pure helper that builds the `MessageInst` for
    /// `send_reaction`:
    ///
    /// * sets `inst.id` to a non-empty, unique value (regression guard: the
    ///   pre-fix code path built the `MessageInst` inline and never assigned
    ///   `inst.id`, so reactions arrived at the receiver without a way to
    ///   attach to the target message);
    /// * wraps the caller's `(target_guid, target_part, reaction)` into a
    ///   `Message::React(...)` payload, **defaulting `target_part = None` to
    ///   `Some(0)`** in the wire payload;
    /// * **threads `target_text` through** to `inst.message`'s
    ///   `ReactMessage.to_text` field (the `ams` field the iPhone uses to
    ///   render the reaction chip);
    /// * carries `my_handle` through to `inst.sender`.
    ///
    /// Two calls must produce distinct ids — a correct implementation uses
    /// `new_guid()` (or an equivalent per-call GUID generator), not a
    /// hardcoded constant.
    #[test]
    fn build_react_message_inst_sets_id_and_payload() {
        let chat = ChatRef {
            participants: vec!["mailto:a@icloud.com".into()],
            display_name: None,
            service: Some("iMessage".into()),
        };
        let my_handle = "mailto:me@icloud.com";
        let reaction = ReactMessageType::React {
            reaction: Reaction::Heart,
            enable: true,
        };

        // Call with `target_part = None` (the UI's actual call site) — the
        // helper must default it to `Some(0)`, and `target_text` must reach
        // the wire `ams` field. Both are part of the fix.
        let inst = build_react_message_inst(
            &chat,
            my_handle,
            "target-guid",
            None,
            "Hello world",
            &reaction,
        );

        // 1. inst.id is non-empty — regression guard for the bug.
        assert!(
            !inst.id.is_empty(),
            "inst.id should be a freshly-generated GUID, not empty"
        );

        // 4. inst.sender carries the my_handle argument through.
        assert_eq!(
            inst.sender.as_deref(),
            Some(my_handle),
            "inst.sender should equal the my_handle argument"
        );

        // 3. inst.message is a Message::React carrying the caller's payload.
        // NOTE: `rustpush::Message` has only `Display`, no `Debug` — the
        // panic on a non-React variant must use `{other}`, not `{other:?}`.
        match &inst.message {
            Message::React(react) => {
                assert_eq!(react.to_uuid, "target-guid", "to_uuid should be target_guid");
                assert_eq!(
                    react.to_part,
                    Some(0),
                    "to_part: None must default to Some(0) on the wire payload"
                );
                assert_eq!(
                    react.to_text, "Hello world",
                    "to_text should flow through to inst.message's React payload (ams field)"
                );
                let (r, enable) = match react.reaction.clone() {
                    ReactMessageType::React { reaction, enable } => (reaction, enable),
                    _ => panic!("expected ReactMessageType::React variant"),
                };
                assert!(matches!(r, Reaction::Heart), "reaction payload should be Heart");
                assert!(enable, "enable should be true");
            }
            other => panic!("expected Message::React, got {other}"),
        }

        // 2. Two calls produce distinct ids — guards against hardcoding
        //    the fix to a constant GUID.
        let inst2 = build_react_message_inst(
            &chat,
            my_handle,
            "target-guid",
            None,
            "Hello world",
            &reaction,
        );
        assert_ne!(
            inst.id, inst2.id,
            "two calls to build_react_message_inst must produce distinct ids"
        );
    }

    /// Pin: the pure helper that builds the wire-level `MessageParts` for
    /// `send_attachment`:
    ///
    /// * **with `text = Some(...)`**: produces a two-part `MessageParts`
    ///   ordered **`MessagePart::Attachment(...)` first, then
    ///   `MessagePart::Text(...)` (the caption)**, both with `idx: None` and
    ///   `ext: None`. The text content is preserved as-is. The Attachment-
    ///   first ordering is required so that the iPhone renders the attachment
    ///   inline alongside the caption — in `rustpush::MessageParts::to_xml`
    ///   the auto-increment `my_part_idx` is only bumped on
    ///   `MessagePart::Attachment` arms, so with `[Attachment, Text]` the
    ///   attachment claims `message-part="0"` and the text gets `"1"`; with
    ///   `[Text, Attachment]` both share `"0"` and the iPhone demotes the
    ///   attachment to previews-only.
    /// * **with `text = None`**: produces a single-part `MessageParts`
    ///   containing only the `MessagePart::Attachment(...)` (photo-only case —
    ///   regression guard).
    ///
    /// This is the extracted seam that replaces the buggy inline construction
    /// in `send_attachment`, where `NormalMessage::new(text, ...)` correctly
    /// seeded the text part but the very next line overwrote `normal.parts`
    /// with a one-element `MessageParts` containing only the `Attachment`,
    /// dropping the caption from the wire payload.
    #[test]
    fn build_attachment_message_parts_cases() {
        fn fixture_attachment() -> Attachment {
            Attachment {
                a_type: AttachmentType::Inline(vec![]),
                part: 0,
                uti_type: "public.jpeg".into(),
                mime: "image/jpeg".into(),
                name: "photo.jpg".into(),
                iris: false,
            }
        }

        // -- Case 1: text = Some("hello") — two parts, attachment first. --
        let attach = fixture_attachment();
        let parts = build_attachment_message_parts(Some("hello"), attach.clone());

        assert_eq!(
            parts.0.len(),
            2,
            "with text: must produce exactly 2 IndexedMessagePart entries"
        );

        // First entry: MessagePart::Attachment with the same attachment,
        // idx=None, ext=None.  Pin the order: attachment first, then text
        // caption.  The iPhone uses the first part's wire `message-part`
        // index as the primary render target, so the Attachment must come
        // first to be rendered inline alongside the caption (rather than
        // being demoted to previews-only).
        match &parts.0[0].part {
            MessagePart::Attachment(a) => {
                assert_eq!(a.part, attach.part, "attachment.part should match");
                assert_eq!(
                    a.uti_type, attach.uti_type,
                    "attachment.uti_type should match"
                );
                assert_eq!(a.mime, attach.mime, "attachment.mime should match");
                assert_eq!(a.name, attach.name, "attachment.name should match");
                assert_eq!(a.iris, attach.iris, "attachment.iris should match");
                match (&a.a_type, &attach.a_type) {
                    (AttachmentType::Inline(l), AttachmentType::Inline(r)) => {
                        assert_eq!(l, r, "attachment a_type (Inline) data should match")
                    }
                    _ => panic!("expected a_type to be Inline in both places"),
                }
            }
            _ => panic!(
                "parts[0] should be MessagePart::Attachment (first part must be the \
                 attachment so the iPhone renders it inline with the caption); got a \
                 non-Attachment variant"
            ),
        }
        assert_eq!(parts.0[0].idx, None, "parts[0].idx should be None");
        assert!(parts.0[0].ext.is_none(), "parts[0].ext should be None");

        // Second entry: MessagePart::Text with the caption, idx=None,
        // ext=None.  The text content is preserved as-is.
        match &parts.0[1].part {
            MessagePart::Text(t, _) => assert_eq!(t, "hello", "text content must be preserved"),
            _ => panic!("parts[1] should be MessagePart::Text (the caption)"),
        }
        assert_eq!(parts.0[1].idx, None, "parts[1].idx should be None");
        assert!(parts.0[1].ext.is_none(), "parts[1].ext should be None");

        // -- Case 2: text = None — single attachment part only. --
        let attach2 = fixture_attachment();
        let parts2 = build_attachment_message_parts(None, attach2.clone());

        assert_eq!(
            parts2.0.len(),
            1,
            "without text: must produce exactly 1 IndexedMessagePart entry"
        );

        match &parts2.0[0].part {
            MessagePart::Attachment(a) => {
                assert_eq!(a.part, attach2.part, "attachment.part should match");
                assert_eq!(
                    a.uti_type, attach2.uti_type,
                    "attachment.uti_type should match"
                );
                assert_eq!(a.mime, attach2.mime, "attachment.mime should match");
                assert_eq!(a.name, attach2.name, "attachment.name should match");
                assert_eq!(a.iris, attach2.iris, "attachment.iris should match");
                match (&a.a_type, &attach2.a_type) {
                    (AttachmentType::Inline(l), AttachmentType::Inline(r)) => {
                        assert_eq!(l, r, "attachment a_type (Inline) data should match")
                    }
                    _ => panic!("expected a_type to be Inline in both places"),
                }
            }
            _ => panic!("single part should be MessagePart::Attachment"),
        }
        assert_eq!(parts2.0[0].idx, None, "single part idx should be None");
        assert!(parts2.0[0].ext.is_none(), "single part ext should be None");
    }

    /// Pin: the pure helper that builds the `MessageInst` for `send_edit`:
    ///
    /// * sets `inst.id` to a non-empty, unique value (regression guard: a
    ///   missing or hardcoded id would cause the edited message to not
    ///   correlate on the receiver side);
    /// * wraps the caller's `(target_guid, edit_part, new_text)` into a
    ///   `Message::Edit(EditMessage { tuuid, edit_part, new_parts })` where
    ///   `new_parts` is a single-element `MessageParts` containing
    ///   `MessagePart::Text(new_text, Default::default())`;
    /// * carries `my_handle` through to `inst.sender`.
    ///
    /// Two calls must produce distinct ids — a correct implementation uses
    /// `new_guid()` (or an equivalent per-call GUID generator), not a
    /// hardcoded constant.
    #[test]
    fn build_edit_message_inst_wire_payload() {
        let chat = ChatRef {
            participants: vec!["mailto:a@icloud.com".into()],
            display_name: None,
            service: Some("iMessage".into()),
        };
        let my_handle = "tel:+15555550123";

        let inst = build_edit_message_inst(
            &chat,
            my_handle,
            "target-guid-xyz",
            0,
            "Edited body".to_string(),
        );

        // 1. inst.id is non-empty — regression guard.
        assert!(
            !inst.id.is_empty(),
            "inst.id should be a freshly-generated GUID, not empty"
        );

        // 2. inst.sender carries the my_handle argument through.
        assert_eq!(
            inst.sender.as_deref(),
            Some(my_handle),
            "inst.sender should equal the my_handle argument"
        );

        // 3. inst.message is a Message::Edit carrying the caller's payload.
        // NOTE: `rustpush::Message` has only `Display`, no `Debug` — the
        // panic on a non-Edit variant must use `{other}`, not `{other:?}`.
        match &inst.message {
            Message::Edit(em) => {
                assert_eq!(
                    em.tuuid, "target-guid-xyz",
                    "tuuid should be the target GUID"
                );
                assert_eq!(em.edit_part, 0, "edit_part should be 0");

                assert_eq!(
                    em.new_parts.0.len(),
                    1,
                    "new_parts should contain exactly one part"
                );

                match &em.new_parts.0[0].part {
                    MessagePart::Text(t, _) => {
                        assert_eq!(t, "Edited body", "text content should be preserved");
                    }
                    _ => panic!("expected MessagePart::Text in new_parts[0]"),
                }
            }
            other => panic!("expected Message::Edit, got {other}"),
        }
    }

    /// Pin: two calls to `build_edit_message_inst` must produce distinct
    /// `inst.id` values — guards against a regression that hardcodes the
    /// GUID to a constant.
    #[test]
    fn build_edit_message_inst_unique_ids() {
        let chat = ChatRef {
            participants: vec!["mailto:a@icloud.com".into()],
            display_name: None,
            service: Some("iMessage".into()),
        };
        let my_handle = "tel:+15555550123";

        let inst_a = build_edit_message_inst(
            &chat,
            my_handle,
            "target-guid-xyz",
            0,
            "Edited body".to_string(),
        );
        let inst_b = build_edit_message_inst(
            &chat,
            my_handle,
            "target-guid-xyz",
            0,
            "Edited body".to_string(),
        );

        assert_ne!(
            inst_a.id, inst_b.id,
            "two calls to build_edit_message_inst must produce distinct ids"
        );
    }

    #[tokio::test]
    async fn receive_loop_recovers_from_closed() {
        use std::cell::{Cell, RefCell};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::broadcast;

        const EXPECTED_MSG_COUNT: usize = 3;

        // --- Channel 1: sender dropped so the receiver returns Closed immediately ---
        let (tx1, rx1) = broadcast::channel::<APSMessage>(16);
        drop(tx1);

        // --- Channel 2: sender stays alive with messages queued ---
        let (tx2, rx2) = broadcast::channel::<APSMessage>(16);
        for _ in 0..EXPECTED_MSG_COUNT {
            tx2.send(APSMessage::Ping).unwrap();
        }
        // Keep tx2 alive so rx2 (and any clone created via RefCell::take) stays open.
        let _tx2_keepalive = tx2;

        // Wrap both receivers in RefCell<Option<...>> so the Fn() closure can
        // return them via borrow_mut().take() without consuming itself.
        let rx1_cell = RefCell::new(Some(rx1));
        let rx2_cell = RefCell::new(Some(rx2));

        let call_count = Cell::new(0u8);
        let subscribe = move || -> Result<_, ConnectionDead> {
            let call = call_count.get();
            call_count.set(call + 1);
            match call {
                0 => Ok(rx1_cell
                    .borrow_mut()
                    .take()
                    .expect("subscribe call 0: rx1 should be present")),
                _ => Ok(rx2_cell
                    .borrow_mut()
                    .take()
                    .expect("subscribe call 1+: rx2 should be present")),
            }
        };

        let processed = Arc::new(AtomicUsize::new(0));
        let process = {
            let processed_clone = Arc::clone(&processed);
            move |_msg: APSMessage| {
                let p = Arc::clone(&processed_clone);
                async move {
                    p.fetch_add(1, Ordering::SeqCst);
                }
            }
        };

        // Bounded run: the loop either breaks (current bug) or keeps waiting
        // after processing messages (fixed). The timeout ensures the test
        // terminates either way.
        let reconnect = || async { Ok::<(), ReconnectError>(()) };
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            run_receive_loop(subscribe, process, std::sync::Arc::new(tokio::sync::Notify::new()), reconnect),
        )
        .await;

        assert_eq!(
            processed.load(Ordering::SeqCst),
            EXPECTED_MSG_COUNT,
            "subscribe must be called again after Closed, and messages from the new receiver must be processed"
        );
    }

    #[tokio::test]
    async fn receive_loop_resubscribes_on_kick() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::time::Duration;
        use tokio::sync::broadcast;
        use tokio::sync::Notify;

        let subscribe_call_count = Arc::new(AtomicUsize::new(0));
        let processed_count = Arc::new(AtomicUsize::new(0));

        // -- Pre-made receivers (not fresh subscribe() calls) so that the
        //    receivers the subscribe closure returns already contain queued
        //    messages.  A broadcast::Receiver created via Sender::subscribe()
        //    starts at the current write tail and cannot see messages sent
        //    before its creation, so we must make the receivers ahead of time.
        let (tx, _rx) = broadcast::channel::<APSMessage>(16);

        // rx1: created before the first send — sees position 0.
        let rx1 = tx.subscribe();
        tx.send(APSMessage::Ping).unwrap();

        // rx2: created before the second send — sees position 1.
        let rx2 = tx.subscribe();
        tx.send(APSMessage::Ping).unwrap();
        // Keep tx alive (otherwise receivers would return Closed).
        let _tx_keepalive = tx;

        let rx1_cell = Mutex::new(Some(rx1));
        let rx2_cell = Mutex::new(Some(rx2));

        let subscribe = {
            let call_count = Arc::clone(&subscribe_call_count);
            move || -> Result<_, ConnectionDead> {
                call_count.fetch_add(1, Ordering::SeqCst);
                // Dispatch based on the *previous* count value so the first
                // call (value 0) returns rx1 and all subsequent calls return rx2.
                let prev = call_count.load(Ordering::SeqCst) - 1;
                match prev {
                    0 => Ok(rx1_cell.lock().unwrap().take().expect(
                        "subscribe call 0: rx1 should be present",
                    )),
                    _ => Ok(rx2_cell.lock().unwrap().take().expect(
                        "subscribe call 1+: rx2 should be present",
                    )),
                }
            }
        };

        let process = {
            let processed = Arc::clone(&processed_count);
            move |_msg: APSMessage| {
                let p = Arc::clone(&processed);
                async move {
                    p.fetch_add(1, Ordering::SeqCst);
                }
            }
        };

        let kick = Arc::new(Notify::new());

        let reconnect = || async { Ok::<(), ReconnectError>(()) };

        // Spawn the loop in the background.
        let handle = {
            let kick = Arc::clone(&kick);
            tokio::spawn(async move {
                run_receive_loop(subscribe, process, kick, reconnect).await;
            })
        };

        // Give the loop time to process the first message and then await the kick.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The first subscribe should have been called, and the first message processed.
        assert!(
            subscribe_call_count.load(Ordering::SeqCst) >= 1,
            "subscribe should have been called at least once before kick"
        );
        assert!(
            processed_count.load(Ordering::SeqCst) >= 1,
            "at least one message should have been processed before kick"
        );

        // Signal the kick — the loop should re-subscribe.
        kick.notify_one();

        // Wait for the re-subscription and message processing.
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(
            subscribe_call_count.load(Ordering::SeqCst) >= 2,
            "subscribe must be called a second time after kick.notify_one()"
        );
        assert!(
            processed_count.load(Ordering::SeqCst) >= 2,
            "a second message must be processed after the kick triggers re-subscription"
        );

        // Clean up: abort the background task so the test doesn't hang.
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn receive_loop_calls_reconnect_on_dead_connection() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::time::Duration;
        use tokio::sync::broadcast;

        let subscribe_call_count = Arc::new(AtomicUsize::new(0));
        let reconnect_call_count = Arc::new(AtomicUsize::new(0));
        let processed_count = Arc::new(AtomicUsize::new(0));

        // Shared connection state: None = dead, Some(tx) = alive.
        let conn: Arc<Mutex<Option<tokio::sync::broadcast::Sender<APSMessage>>>> =
            Arc::new(Mutex::new(None));

        let subscribe = {
            let conn = Arc::clone(&conn);
            let call_count = Arc::clone(&subscribe_call_count);
            move || {
                call_count.fetch_add(1, Ordering::SeqCst);
                let guard = conn.lock().unwrap();
                match &*guard {
                    None => Err(ConnectionDead),
                    Some(tx) => {
                        // Create the receiver BEFORE sending the message, so
                        // the receiver starts at the current tail and the
                        // message sent after is visible.
                        let rx = tx.subscribe();
                        tx.send(APSMessage::Ping).unwrap();
                        Ok(rx)
                    }
                }
            }
        };

        let process = {
            let processed = Arc::clone(&processed_count);
            move |_msg: APSMessage| {
                let p = Arc::clone(&processed);
                async move {
                    p.fetch_add(1, Ordering::SeqCst);
                }
            }
        };

        let reconnect = {
            let conn = Arc::clone(&conn);
            let counter = Arc::clone(&reconnect_call_count);
            move || {
                let conn = Arc::clone(&conn);
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let (tx, _rx) = broadcast::channel::<APSMessage>(16);
                    // Don't send the message here — subscribe() will send it
                    // after creating the receiver, ensuring the receiver can
                    // see it.
                    *conn.lock().unwrap() = Some(tx);
                    Ok::<(), ReconnectError>(())
                }
            }
        };

        let kick = Arc::new(tokio::sync::Notify::new());

        let handle = {
            let kick = Arc::clone(&kick);
            tokio::spawn(async move {
                run_receive_loop(subscribe, process, kick, reconnect).await;
            })
        };

        // Let the spawned task run: first subscribe call (gets Err), reconnect,
        // then the 1s backoff sleep starts.
        tokio::task::yield_now().await;

        // Reconnect should have been called once before the backoff.
        assert_eq!(
            reconnect_call_count.load(Ordering::SeqCst),
            1,
            "reconnect must be called exactly once, not in a spin loop"
        );
        assert_eq!(
            subscribe_call_count.load(Ordering::SeqCst),
            1,
            "only the first subscribe call should have run so far"
        );

        // Advance virtual time past the 1s base backoff to trigger the retry.
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // Now the retry subscribe should have succeeded and the message processed.
        assert_eq!(
            processed_count.load(Ordering::SeqCst),
            1,
            "one message from the new connection must be processed"
        );
        assert!(
            subscribe_call_count.load(Ordering::SeqCst) >= 2,
            "subscribe must be called at least twice (once returning Err, once returning Ok after reconnect)"
        );

        handle.abort();
    }

    /// Pin: `ingest_from` maps `Message::Edit` to `Ingest::Edited` carrying the
    /// target message GUID and the replacement text from `EditMessage.new_parts.raw_text()`.
    ///
    /// This is the core mapping behavior for the "edit message" feature — the
    /// receive loop uses `Ingest::Edited` to locate and update the stored message
    /// in `Store::apply`.
    #[test]
    fn ingest_from_edit_returns_edited_ingest() {
        let chat = ChatRef {
            participants: vec!["tel:+15555550123".into()],
            display_name: None,
            service: Some("iMessage".into()),
        };

        let inst = build_edit_message_inst(
            &chat,
            "tel:+15555550123",
            "TARGET-GUID-ABC",
            0,
            "Edited body text".to_string(),
        );

        let result = ingest_from(&inst, &["tel:+15555550123".to_string()]);

        match result {
            Ingest::Edited { guid, text } => {
                assert_eq!(
                    guid, "TARGET-GUID-ABC",
                    "guid should be the EditMessage.tuuid, i.e. the target message guid"
                );
                assert_eq!(
                    text, "Edited body text",
                    "text should be the concatenated raw_text from EditMessage.new_parts"
                );
            }
            other => panic!("expected Ingest::Edited, got {other:?}"),
        }
    }

    /// Pin: the guid in `Ingest::Edited` is the **target** message GUID
    /// (`EditMessage.tuuid`), NOT `MessageInst.id` (the edit's own identity).
    /// This is a regression guard — using `inst.id` would make the store
    /// update the wrong message.
    #[test]
    fn ingest_from_edit_guid_is_target_not_inst_id() {
        let chat = ChatRef {
            participants: vec!["tel:+15555550123".into()],
            display_name: None,
            service: Some("iMessage".into()),
        };

        let inst = build_edit_message_inst(
            &chat,
            "tel:+15555550123",
            "TARGET-GUID-ABC",
            0,
            "Edited body text".to_string(),
        );

        let inst_id = inst.id.clone();
        let result = ingest_from(&inst, &[] as &[String]);

        match result {
            Ingest::Edited { guid, .. } => {
                assert_ne!(
                    guid, inst_id,
                    "guid must NOT be inst.id — it should be the EditMessage.tuuid target"
                );
            }
            other => panic!("expected Ingest::Edited, got {other:?}"),
        }
    }

    /// Pin: `Backend::is_keychain_clique_set_up` — disk-only check on
    /// `keychain.plist`.  Returns `false` when the file does not exist
    /// (clique never set up, no persisted state at all).
    #[tokio::test]
    async fn is_keychain_clique_set_up_returns_false_when_no_plist() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = RustpushBackend::new(tmp.path().to_string_lossy().to_string());
        assert!(!backend.is_keychain_clique_set_up().await);
    }

    /// Pin: `Backend::is_keychain_clique_set_up` — returns `false` when
    /// `keychain.plist` exists but has no `user_identity` field (the clique
    /// state was persisted but no user identity was ever created).
    #[tokio::test]
    async fn is_keychain_clique_set_up_returns_false_when_no_user_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("keychain.plist");
        plist::to_file_xml(&path, &plist::Dictionary::new()).unwrap();
        let backend = RustpushBackend::new(tmp.path().to_string_lossy().to_string());
        assert!(!backend.is_keychain_clique_set_up().await);
    }

    /// Pin: `Backend::is_keychain_clique_set_up` — returns `true` when
    /// `keychain.plist` exists and has a `user_identity` field (the clique
    /// was set up and a user identity was persisted to disk).
    #[tokio::test]
    async fn is_keychain_clique_set_up_returns_true_when_user_identity_present() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("keychain.plist");
        let mut dict = plist::Dictionary::new();
        dict.insert(
            "user_identity".into(),
            plist::Value::Dictionary(plist::Dictionary::new()),
        );
        plist::to_file_xml(&path, &dict).unwrap();
        let backend = RustpushBackend::new(tmp.path().to_string_lossy().to_string());
        assert!(backend.is_keychain_clique_set_up().await);
    }

    /// Pin: the free orchestrator `run_clique_setup_then_sync` exists with the
    /// expected signature and, when `password` is `None`, skips the clique setup
    /// and returns `Ok(SyncResult::default())` via the stub backend's no-op sync.
    #[tokio::test]
    async fn run_clique_setup_then_sync_no_password() {
        let backend = crate::protocol::stub::StubBackend::default();
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(tmp.path().join("db.sqlite")).await.unwrap();
        let result = super::run_clique_setup_then_sync(
            &backend,
            &store,
            i64::MIN,
            false,
            None,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), crate::sync::SyncResult::default());
    }

    /// Pin: `decide_clique_setup_action` — when `keychain.plist` exists with a
    /// `user_identity` field, `is_keychain_clique_set_up` returns `true`, so
    /// the action should be `SyncNow`.
    #[tokio::test]
    async fn decide_clique_setup_action_returns_sync_now_when_clique_set_up() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("keychain.plist");
        let mut dict = plist::Dictionary::new();
        dict.insert(
            "user_identity".into(),
            plist::Value::Dictionary(plist::Dictionary::new()),
        );
        plist::to_file_xml(&path, &dict).unwrap();
        let backend = RustpushBackend::new(tmp.path().to_string_lossy().to_string());
        let action = crate::protocol::decide_clique_setup_action(&backend).await;
        assert_eq!(action, crate::protocol::CliqueSetupAction::SyncNow);
    }

    /// Pin: `decide_clique_setup_action` — when `keychain.plist` does not
    /// exist, `is_keychain_clique_set_up` returns `false`, so the action
    /// should be `PromptForPassword`.
    #[tokio::test]
    async fn decide_clique_setup_action_returns_prompt_when_no_keychain_plist() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = RustpushBackend::new(tmp.path().to_string_lossy().to_string());
        let action = crate::protocol::decide_clique_setup_action(&backend).await;
        assert_eq!(action, crate::protocol::CliqueSetupAction::PromptForPassword);
    }

    /// Pin: `orchestrate_sync_now_flow` — when the clique is not set up and
    /// the user submits both secrets via the prompt closure, the function calls
    /// `run_clique_setup_then_sync_with_secrets` with the two distinct secrets
    /// and returns `Ok(...)`. Uses the stub backend's no-op implementations for
    /// both clique setup and sync, so the result is `SyncResult::default()`.
    #[tokio::test]
    async fn orchestrate_sync_now_flow_prompt_with_password() {
        let backend = crate::protocol::stub::StubBackend::default();
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(tmp.path().join("db.sqlite")).await.unwrap();
        let result = super::orchestrate_sync_now_flow(
            &backend,
            &store,
            i64::MIN,
            false,
            || Some(("test-escrow-passcode".to_string(), "test-device-password".to_string())),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), crate::sync::SyncResult::default());
    }

    /// Pin: `orchestrate_sync_now_flow` — when the clique is not set up and
    /// the user cancels the password dialog (`password_prompt` returns `None`),
    /// the function returns `Ok(SyncResult::default())` without attempting
    /// any sync (the "cancelled" sentinel).
    #[tokio::test]
    async fn orchestrate_sync_now_flow_user_cancels() {
        let backend = crate::protocol::stub::StubBackend::default();
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(tmp.path().join("db.sqlite")).await.unwrap();
        let result = super::orchestrate_sync_now_flow(
            &backend,
            &store,
            i64::MIN,
            false,
            || None,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), crate::sync::SyncResult::default());
    }

    /// Pin: `orchestrate_sync_now_flow` — when the action is
    /// `PromptForPassword`, the `password_prompt` closure is invoked exactly
    /// once.
    #[tokio::test]
    async fn orchestrate_sync_now_flow_prompt_calls_closure_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let backend = crate::protocol::stub::StubBackend::default();
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(tmp.path().join("db.sqlite")).await.unwrap();
        let call_count = Arc::new(AtomicUsize::new(0));
        let counter = call_count.clone();
        let result = super::orchestrate_sync_now_flow(
            &backend,
            &store,
            i64::MIN,
            false,
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                Some(("old-passcode".to_string(), "new-password".to_string()))
            },
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "password_prompt closure should be called exactly once when action is PromptForPassword"
        );
    }

    /// Pin: `orchestrate_sync_now_flow` — when the clique IS set up
    /// (`StubBackend::clique_set_up = true`), the action is `SyncNow`, the
    /// sync proceeds directly, and the `password_prompt` closure is NOT
    /// called.
    #[tokio::test]
    async fn orchestrate_sync_now_flow_sync_now_skips_prompt() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let backend = crate::protocol::stub::StubBackend { clique_set_up: true };
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(tmp.path().join("db.sqlite")).await.unwrap();
        let call_count = Arc::new(AtomicUsize::new(0));
        let counter = call_count.clone();
        let result = super::orchestrate_sync_now_flow(
            &backend,
            &store,
            i64::MIN,
            false,
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                Some(("should-not-be-called".to_string(), "should-not-be-called".to_string()))
            },
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            0,
            "password_prompt closure should NOT be called when clique is set up (SyncNow path)"
        );
    }

    /// NEW CONTRACT: `subscribe_with_reconnect` never gives up — retries
    /// indefinitely with exponential backoff.  This test pins that a subscribe
    /// that fails 10 times (ConnectionDead) before succeeding on the 11th call
    /// eventually returns a receiver (not None), and that subscribe is called
    /// exactly 11 times.
    ///
    /// The NEW signature adds a `kick: Arc<Notify>` parameter and returns a
    /// bare `broadcast::Receiver` (non-optional).
    #[tokio::test(start_paused = true)]
    async fn subscribe_with_reconnect_never_gives_up() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::broadcast;

        let subscribe_count = Arc::new(AtomicUsize::new(0));
        // Pre-create a channel whose receiver we return on the 11th call.
        let (tx, _rx) = broadcast::channel::<APSMessage>(16);

        let subscribe = {
            let count = Arc::clone(&subscribe_count);
            move || -> Result<_, ConnectionDead> {
                let c = count.fetch_add(1, Ordering::SeqCst);
                if c < 10 {
                    Err(ConnectionDead)
                } else {
                    Ok(tx.subscribe())
                }
            }
        };
        let reconnect = || async { Ok::<(), ReconnectError>(()) };
        let kick = Arc::new(tokio::sync::Notify::new());

        let handle = tokio::spawn(async move {
            // NEW 3-parameter signature — will not compile against the old
            // 2-parameter `subscribe_with_reconnect`.
            subscribe_with_reconnect(&subscribe, &reconnect, kick).await
        });

        // Let the first subscribe call happen.
        tokio::task::yield_now().await;

        // Drive one backoff period at a time through all 10 retries.
        // Sequence: 1, 2, 4, 8, 16, 32, 60, 60, 60, 60 (seconds).
        // Each advance fires exactly one pending sleep; after each one the
        // woken task runs subscribe (fails), reconnect (succeeds), and
        // registers the next backoff sleep.
        for &backoff in &[1, 2, 4, 8, 16, 32, 60, 60, 60, 60] {
            tokio::time::advance(Duration::from_secs(backoff)).await;
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }

        // The 11th subscribe call should have succeeded — the JoinHandle
        // resolves immediately.
        let rx = handle.await.expect("task should complete after 10 failures");
        drop(rx);

        assert_eq!(
            subscribe_count.load(Ordering::SeqCst),
            11,
            "subscribe must be called 11 times (10 failures + 1 success)"
        );
    }

    /// NEW CONTRACT: exponential backoff base ~1s, doubling per consecutive
    /// failure, capped at 60s.
    ///
    /// Pins:
    ///   (a) the second attempt does NOT happen before ~1s of virtual time
    ///   (b) after many failures the inter-attempt gap never exceeds 60s
    #[tokio::test(start_paused = true)]
    async fn subscribe_with_reconnect_backoff_timing() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        let call_count = Arc::new(AtomicUsize::new(0));

        let subscribe = {
            let cc = call_count.clone();
            move || -> Result<_, ConnectionDead> {
                cc.fetch_add(1, Ordering::SeqCst);
                Err(ConnectionDead)
            }
        };
        let reconnect = || async { Ok::<(), ReconnectError>(()) };
        let kick = Arc::new(tokio::sync::Notify::new());

        let handle = {
            let kick = Arc::clone(&kick);
            tokio::spawn(async move {
                // This always fails so the future never completes — we abort.
                let _rx =
                    subscribe_with_reconnect(&subscribe, &reconnect, kick).await;
                unreachable!("subscribe always fails")
            })
        };

        // ---- (a) second attempt does not happen before ~1s ----

        // First call happens immediately.
        tokio::task::yield_now().await;
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "first subscribe call must happen immediately"
        );

        // After 999ms the second attempt must NOT have happened.
        tokio::time::advance(Duration::from_millis(999)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "second attempt must not happen before ~1s"
        );

        // After 1ms more (1s total) the second attempt fires.
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            2,
            "second attempt must happen after ~1s"
        );

        // ---- (b) advance past the doubling phase to reach the 60s cap ----
        // Backoff sequence from here: 2, 4, 8, 16, 32, 60s.
        for &backoff in &[2, 4, 8, 16, 32, 60] {
            tokio::time::advance(Duration::from_secs(backoff)).await;
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }
        let prev_count = call_count.load(Ordering::SeqCst);
        assert!(
            prev_count >= 7,
            "must have had at least 7 failures to reach capped state, got {prev_count}"
        );

        // Backoff is now capped at 60s: advance 59s, no new call.
        tokio::time::advance(Duration::from_secs(59)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            prev_count,
            "no new attempt with 59s advance (cap = 60s)"
        );

        // Advance 2s more (61s from last call): cap exceeded, new call.
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(
            call_count.load(Ordering::SeqCst) > prev_count,
            "new attempt must fire after 61s (exceeds the 60s cap)"
        );

        handle.abort();
    }

    /// NEW CONTRACT: a `kick.notify_one()` while the backoff sleep is pending
    /// causes an immediate retry without waiting out the full backoff.
    #[tokio::test(start_paused = true)]
    async fn subscribe_with_reconnect_kick_short_circuit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        let call_count = Arc::new(AtomicUsize::new(0));

        let subscribe = {
            let cc = call_count.clone();
            move || -> Result<_, ConnectionDead> {
                cc.fetch_add(1, Ordering::SeqCst);
                Err(ConnectionDead)
            }
        };
        let reconnect = || async { Ok::<(), ReconnectError>(()) };
        let kick = Arc::new(tokio::sync::Notify::new());

        let handle = {
            let kick = Arc::clone(&kick);
            tokio::spawn(async move {
                let _rx =
                    subscribe_with_reconnect(&subscribe, &reconnect, kick).await;
                unreachable!("subscribe always fails")
            })
        };

        // Let the first subscribe call happen.
        tokio::task::yield_now().await;
        assert_eq!(call_count.load(Ordering::SeqCst), 1);

        // Advance only 100ms — the 1s backoff has NOT expired yet.
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "no retry yet: still inside the 1s backoff"
        );

        // Fire the kick — should cause an immediate retry without advancing
        // to the full 1s.
        kick.notify_one();
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert_eq!(
            call_count.load(Ordering::SeqCst),
            2,
            "kick must cause an immediate subscribe retry"
        );
        // Verify only 100ms of virtual time has passed (not the full 1s backoff).
        assert!(
            tokio::time::Instant::now().elapsed() < Duration::from_secs(1),
            "kick should short-circuit before the backoff expires"
        );

        handle.abort();
    }

    /// NEW CONTRACT: a failing `reconnect` (returns `Err(ReconnectError)`)
    /// must NOT abort — the loop keeps retrying.
    #[tokio::test(start_paused = true)]
    async fn subscribe_with_reconnect_reconnect_error_does_not_abort() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::time::Duration;
        use tokio::sync::broadcast;

        let subscribe_count = Arc::new(AtomicUsize::new(0));
        let reconnect_count = Arc::new(AtomicUsize::new(0));

        let (tx, _rx) = broadcast::channel::<APSMessage>(16);
        let conn: Arc<Mutex<Option<broadcast::Sender<APSMessage>>>> =
            Arc::new(Mutex::new(None));

        let subscribe = {
            let sc = subscribe_count.clone();
            let conn = conn.clone();
            move || -> Result<_, ConnectionDead> {
                sc.fetch_add(1, Ordering::SeqCst);
                let guard = conn.lock().unwrap();
                match &*guard {
                    None => Err(ConnectionDead),
                    Some(sender) => Ok(sender.subscribe()),
                }
            }
        };

        let reconnect = {
            let rc = reconnect_count.clone();
            let conn = conn.clone();
            let tx = tx.clone();
            move || {
                let conn = conn.clone();
                let rc = rc.clone();
                let tx = tx.clone();
                async move {
                    let c = rc.fetch_add(1, Ordering::SeqCst);
                    // First 2 reconnect attempts fail, 3rd succeeds.
                    if c < 2 {
                        Err(ReconnectError("simulated failure".into()))
                    } else {
                        *conn.lock().unwrap() = Some(tx);
                        Ok(())
                    }
                }
            }
        };

        let kick = Arc::new(tokio::sync::Notify::new());

        let handle = tokio::spawn(async move {
            subscribe_with_reconnect(&subscribe, &reconnect, kick).await
        });

        // Let the first subscribe call happen (fails), reconnect(1) fails,
        // sleep(1s) starts.
        tokio::task::yield_now().await;

        // Stepwise through each backoff period.
        // Pipeline:
        //   subscribe(1) fails, reconnect(1) fails, sleep(1s)
        // → subscribe(2) fails, reconnect(2) fails, sleep(2s)
        // → subscribe(3) fails, reconnect(3) succeeds, sleep(4s)
        // → subscribe(4) succeeds
        for &backoff in &[1, 2, 4] {
            tokio::time::advance(Duration::from_secs(backoff)).await;
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }

        let rx = handle.await
            .expect("task should complete after reconnect eventually succeeds");
        drop(rx);

        assert_eq!(
            reconnect_count.load(Ordering::SeqCst),
            3,
            "reconnect must be called 3 times (2 failures + 1 success)"
        );
        assert_eq!(
            subscribe_count.load(Ordering::SeqCst),
            4,
            "subscribe must be called 4 times (3 failures + 1 success)"
        );
    }

    /// NEW CONTRACT: `run_receive_loop` survives arbitrarily many consecutive
    /// subscribe failures (>3, the old limit) and processes a message after
    /// eventual reconnect.
    #[tokio::test(start_paused = true)]
    async fn run_receive_loop_survives_consecutive_failures() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::broadcast;

        let subscribe_call_count = Arc::new(AtomicUsize::new(0));
        let processed_count = Arc::new(AtomicUsize::new(0));

        // Pre-create a sender so subscribe can return Ok after 4 failures.
        let (tx, _rx) = broadcast::channel::<APSMessage>(16);

        let subscribe = {
            let sc = subscribe_call_count.clone();
            let tx_clone = tx.clone();
            move || -> Result<_, ConnectionDead> {
                let c = sc.fetch_add(1, Ordering::SeqCst);
                // Fail the first 4 times (>3 old limit), then succeed.
                if c < 4 {
                    Err(ConnectionDead)
                } else {
                    let rx = tx_clone.subscribe();
                    tx_clone.send(APSMessage::Ping).unwrap();
                    Ok(rx)
                }
            }
        };

        let process = {
            let pc = processed_count.clone();
            move |_msg: APSMessage| {
                let p = pc.clone();
                async move { p.fetch_add(1, Ordering::SeqCst); }
            }
        };

        let reconnect = || async { Ok::<(), ReconnectError>(()) };
        let kick = Arc::new(tokio::sync::Notify::new());

        let handle = {
            let kick = Arc::clone(&kick);
            tokio::spawn(async move {
                run_receive_loop(subscribe, process, kick, reconnect).await;
            })
        };

        // Let the first subscribe call happen.
        tokio::task::yield_now().await;
        assert_eq!(subscribe_call_count.load(Ordering::SeqCst), 1,
            "first subscribe call must happen immediately");

        // Stepwise through 4 backoff periods: 1, 2, 4, 8s.
        for &backoff in &[1, 2, 4, 8] {
            tokio::time::advance(Duration::from_secs(backoff)).await;
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }

        assert!(
            subscribe_call_count.load(Ordering::SeqCst) >= 5,
            "subscribe must have been called at least 5 times (4 failures + 1 success)"
        );
        assert_eq!(
            processed_count.load(Ordering::SeqCst),
            1,
            "one message must be processed after eventual reconnect"
        );

        handle.abort();
    }

    // --- registration_status_from mapping tests ---

    #[test]
    fn registration_status_maps_generated_to_registered() {
        let state = rustpush::ResourceState::Generated;
        assert_eq!(
            super::registration_status_from(&state),
            RegistrationStatus::Registered,
        );
    }

    #[test]
    fn registration_status_maps_generating_to_registering() {
        let state = rustpush::ResourceState::Generating;
        assert_eq!(
            super::registration_status_from(&state),
            RegistrationStatus::Registering,
        );
    }

    #[test]
    fn registration_status_maps_transient_failure() {
        let failure = rustpush::ResourceFailure {
            retry_wait: Some(30),
            error: Arc::new(rustpush::PushError::BadMsg),
        };
        let state = rustpush::ResourceState::Failed(failure);
        let status = super::registration_status_from(&state);
        match &status {
            RegistrationStatus::TransientFailure { retry_in_s, error } => {
                assert_eq!(*retry_in_s, 30);
                // error string should be PushError::BadMsg rendered via Display
                assert_eq!(error, &rustpush::PushError::BadMsg.to_string());
            }
            other => panic!("expected TransientFailure, got {other:?}"),
        }
    }

    #[test]
    fn registration_status_maps_permanent_failure() {
        let failure = rustpush::ResourceFailure {
            retry_wait: None,
            error: Arc::new(rustpush::PushError::BadMsg),
        };
        let state = rustpush::ResourceState::Failed(failure);
        let status = super::registration_status_from(&state);
        match &status {
            RegistrationStatus::LoggedOut { error } => {
                assert_eq!(error, &rustpush::PushError::BadMsg.to_string());
            }
            other => panic!("expected LoggedOut, got {other:?}"),
        }
    }

    #[test]
    fn registration_status_maps_closed_to_logged_out() {
        let state = rustpush::ResourceState::Closed;
        let status = super::registration_status_from(&state);
        match &status {
            RegistrationStatus::LoggedOut { .. } => {} // any error string is fine
            other => panic!("expected LoggedOut, got {other:?}"),
        }
    }

    /// Pin: an IDS 6005 "bad authentication" failure that arrives with a
    /// non-None `retry_wait` must still be surfaced to the app as
    /// `RegistrationStatus::LoggedOut` — not `TransientFailure`.
    ///
    /// Background: at launch, the IdentityManager's auto-rereg can fail
    /// with `PushError::AuthInvalid(IDSError(6005))` (Apple's
    /// "Bad authentication, re-enter device details if persistent" — the
    /// "stale IDS cert" symptom). The ResourceManager wraps that into a
    /// `ResourceState::Failed(ResourceFailure { retry_wait: Some(N), error })`
    /// and the receive loop forwards it to the UI via
    /// `registration_status_from`. The current implementation keys
    /// purely on `retry_wait`: any `Some(_)` → `TransientFailure`. That
    /// is wrong for auth-invalid 6005 — Apple's 6005 means the existing
    /// credentials are no longer accepted; a passive backoff retry won't
    /// recover, and the user must re-onboard. Surfacing 6005 as a
    /// transient retry causes the UI to show a "retrying..." banner that
    /// never resolves instead of the re-auth prompt the user needs to
    /// see.
    ///
    /// This test pins the contract: 6005 (any wrapping form the resource
    /// manager might produce) must map to `LoggedOut`, with the
    /// underlying 6005 / "authentication" cause preserved in the error
    /// string so the UI can show a useful message.
    #[test]
    fn registration_status_maps_6005_retry_failure_to_logged_out() {
        // The exact input the IdentityManager's resource state
        // observer can produce at launch: a ResourceFailure carrying
        // an AuthInvalid(6005) inner error, with the resource manager
        // suggesting a 300s backoff retry. The pre-fix implementation
        // keys on `retry_wait` alone and maps this to
        // `TransientFailure { retry_in_s: 300, ... }`, which is the
        // bug: the UI then loops a transient retry banner forever
        // instead of prompting the user to re-auth.
        let failure = rustpush::ResourceFailure {
            retry_wait: Some(300),
            error: Arc::new(rustpush::PushError::AuthInvalid(IDSError(6005))),
        };
        let state = rustpush::ResourceState::Failed(failure);

        let status = super::registration_status_from(&state);

        // PRIMARY assertion: must be LoggedOut (re-auth), NOT
        // TransientFailure. The pre-fix code returns the latter.
        match &status {
            RegistrationStatus::LoggedOut { error } => {
                // The error string must still surface the underlying
                // 6005 / bad-authentication cause so the UI can
                // render a useful message ("re-enter your Apple ID
                // password", etc.) rather than a generic
                // "logged out" string. The Display impl of
                // `PushError::AuthInvalid(IDSError(6005))` includes
                // both "6005" and "authentication" (the latter
                // because IDSError's Display for 6005 is "Bad
                // authentication, try again and re-enter device
                // details if persistent. (6005)" and the
                // thiserror wrapper prepends "Bad auth cert "). We
                // accept either — both prove the 6005 cause is
                // preserved.
                assert!(
                    error.contains("6005") || error.contains("authentication"),
                    "LoggedOut error string should mention the underlying \
                     6005 / bad-authentication cause, got: {error:?}"
                );
            }
            RegistrationStatus::TransientFailure { retry_in_s, error } => {
                panic!(
                    "6005 AuthInvalid with retry_wait:Some(300) must map to \
                     LoggedOut (re-auth required), NOT TransientFailure \
                     (silent retry loop). The UI is currently showing a \
                     transient retry banner for a permanent auth failure. \
                     retry_in_s={retry_in_s} error={error:?}"
                );
            }
            other => panic!(
                "expected LoggedOut for IDS 6005 AuthInvalid, got {other:?}"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// iCloud Keychain clique setup routing
// ---------------------------------------------------------------------------

/// Route to use when setting up the iCloud Keychain clique.
///
/// - [`CliqueSetupRoute::JoinFromEscrow`]: at least one viable escrow bottle
///   was found — join the existing clique via escrow recovery (voucher-based).
/// - [`CliqueSetupRoute::Establish`]: no viable bottles — establish a brand-new
///   clique (the first-time setup path).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CliqueSetupRoute {
    /// Join via escrow recovery using the first viable bottle.
    JoinFromEscrow {
        /// Index into the viable-bottles list for the bottle to use.
        bottle_index: usize,
    },
    /// Establish a brand-new clique (first-time setup).
    Establish,
}

/// Select the clique setup route based on available viable escrow bottles.
///
/// Returns [`CliqueSetupRoute::JoinFromEscrow`] with `bottle_index: 0` when
/// the list is non-empty. Returns [`CliqueSetupRoute::Establish`] when
/// the list is empty (no previous clique to join).
pub(crate) fn select_clique_setup_route(
    bottles: &[(crate::api::EscrowData, rustpush::keychain::EscrowMetadata)],
    selected_index: usize,
) -> CliqueSetupRoute {
    if bottles.is_empty() {
        CliqueSetupRoute::Establish
    } else {
        let bottle_index = selected_index.min(bottles.len().saturating_sub(1));
        CliqueSetupRoute::JoinFromEscrow { bottle_index }
    }
}

#[cfg(test)]
mod clique_setup_route_tests {
    //! Pin: manual iCloud Keychain setup must prefer joining an existing
    //! iCloud Keychain clique from escrow when viable bottles are available,
    //! rather than attempting to establish a new clique. If no viable escrow
    //! bottles are available, the existing first-time setup/establish
    //! behavior remains allowed.
    //!
    //! The current `RustpushBackend::setup_keychain_clique` unconditionally
    //! creates a new identity and calls `KeychainClient::join_clique(.., None, ..)`,
    //! which triggers Cuttlefish `establish` — overwriting any existing
    //! clique membership the account already has on another device.
    //!
    //! The fix routes through a `select_clique_setup_route` helper that
    //! inspects the list of viable escrow bottles returned by
    //! `KeychainClient::get_viable_bottles()` and picks:
    //!   * `JoinFromEscrow` when at least one viable bottle is available
    //!     (the account already belongs to a clique on another device —
    //!     recover that peer and join via voucher), or
    //!   * `Establish` when no viable bottles are available (first-time
    //!     setup on a brand-new account / device).
    //!
    //! The two tests below exercise both branches. They will fail to compile
    //! until the `select_clique_setup_route` function and `CliqueSetupRoute`
    //! type are introduced in `rustpush_backend.rs` — acceptable red per
    //! the unit's spec (the seam is the deliverable, not a fake stub).
    use rustpush::keychain::EscrowMetadata;

    /// Build a sample `(EscrowData, EscrowMetadata)` pair for testing.
    /// The contents are irrelevant — the route-selection helper only
    /// inspects the list *length* (viable vs not viable), not the bottle
    /// contents, so a default-constructed `EscrowData` and a minimally-
    /// populated `EscrowMetadata` are sufficient.
    fn sample_bottle() -> (crate::api::EscrowData, EscrowMetadata) {
        (
            crate::api::EscrowData::default(),
            EscrowMetadata {
                serial: "ABC123".into(),
                build: "23A".into(),
                passcode_generation: 0,
                timestamp: "2026-01-01".into(),
                bottle_id: "bottle-1".into(),
                client_metadata: plist::Value::Dictionary(plist::Dictionary::new()),
                escrowed_spki: plist::Data::new(Vec::new()),
                multiple_icsc: false,
            },
        )
    }

    /// Pin: when at least one viable escrow bottle is available, the
    /// route must be `JoinFromEscrow` (not `Establish`). This is the core
    /// behavior change — existing peers in the iCloud Keychain clique
    /// should be joined via escrow, not replaced by a brand-new clique
    /// (which would orphan the other devices in the existing clique).
    #[test]
    fn clique_setup_route_prefers_escrow_when_bottles_available() {
        let bottles = vec![sample_bottle()];
        let route = super::select_clique_setup_route(&bottles, 0);
        assert!(
            matches!(route, super::CliqueSetupRoute::JoinFromEscrow { .. }),
            "expected JoinFromEscrow when viable bottles are available, got {route:?}"
        );
    }

    /// Pin: when no viable escrow bottles are available, the route must
    /// be `Establish` — the existing first-time setup behavior. This
    /// preserves the "new device, no existing clique" path so the
    /// first-ever setup on an account without prior escrow records
    /// still works.
    #[test]
    fn clique_setup_route_establishes_when_no_bottles() {
        let bottles: Vec<(crate::api::EscrowData, EscrowMetadata)> = Vec::new();
        let route = super::select_clique_setup_route(&bottles, 0);
        assert!(
            matches!(route, super::CliqueSetupRoute::Establish),
            "expected Establish when no viable bottles are available, got {route:?}"
        );
    }

    /// Pin: `select_clique_setup_route` must take an explicit
    /// user-selected bottle index as a second argument and preserve it
    /// in the returned route. When the caller passes `2` and there are
    /// 3 viable bottles, the function must NOT silently coerce the
    /// index to 0 — it must surface `JoinFromEscrow { bottle_index: 2 }`
    /// so the downstream code can recover and join the user-chosen
    /// peer's bottle.
    ///
    /// This is the regression pin: the pre-fix code unconditionally
    /// returned `JoinFromEscrow { bottle_index: 0 }` for any non-empty
    /// input, which silently picked whichever bottle happened to be
    /// first in the viable list — potentially a stale or wrong device —
    /// and ignored the user's choice from the bottle-selection dialog.
    #[test]
    fn clique_setup_route_preserves_user_selected_bottle_index() {
        let bottles = vec![sample_bottle(), sample_bottle(), sample_bottle()];

        // The function must take a second argument (the user-selected
        // index) and return a route that preserves it. Pre-fix code:
        // single-arg, always returns index 0. This call therefore fails
        // to compile under the pre-fix signature, which is the
        // expected red per the unit's spec — the seam (a second
        // `selected_index: usize` argument) is the deliverable.
        let route = super::select_clique_setup_route(&bottles, 2);

        match route {
            super::CliqueSetupRoute::JoinFromEscrow { bottle_index } => {
                assert_eq!(
                    bottle_index, 2,
                    "user-selected bottle index must be preserved in the route; \
                     got {bottle_index}, expected 2. A single-arg function that \
                     silently picks index 0 is the exact bug this test pins."
                );
            }
            other => panic!(
                "expected JoinFromEscrow {{ bottle_index: 2 }}, got {other:?} — \
                 the route must surface the user-selected bottle, not fall back \
                 to a default."
            ),
        }
    }
}

#[cfg(test)]
mod clique_setup_secrets_tests {
    //! Pin: manual iCloud Keychain setup must distinguish the old trusted
    //! device passcode used to recover an existing escrow bottle from the
    //! new local device password used to create this device's bottle. A
    //! single iCloud-account-password `&str` must not be the only setup
    //! input for the escrow route.
    //!
    //! The vendored `join_clique_from_escrow(bottle, password, device_password)`
    //! takes two DISTINCT byte slices: the first decrypts the escrow bottle
    //! being recovered; the second becomes this device's password for the
    //! new bottle it will produce. The current `Backend::setup_keychain_clique`
    //! trait method is `(password: &str)`, and `RustpushBackend::setup_keychain_clique`
    //! forwards the same string for both arguments:
    //!
    //!     join_clique_from_escrow(bottle, password.as_bytes(), password.as_bytes())
    //!
    //! This test will fail to compile until the orchestrator exposes a new
    //! entry point that carries both secrets through, and the
    //! `Backend::setup_keychain_clique` trait method is widened to take two
    //! distinct arguments. The compile failure is the acceptable red per
    //! the unit's spec (the seam — a value type / entry point for two
    //! distinct clique-setup secrets — is the deliverable, not a fake stub).
    //!
    //! Test isolation: each `#[tokio::test]` builds its own recording fake
    //! backend and its own tempdir-backed store; no shared state, no env vars.
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Recording fake backend. `setup_keychain_clique` captures the two
    /// distinct secrets it was called with; every other trait method is a
    /// no-op that returns the default for its return type. The 2-argument
    /// `setup_keychain_clique` signature is the seam under test: it pins
    /// that the trait method must accept the old escrow passcode and the
    /// new local device password as separate arguments, not collapse them
    /// into one.
    struct RecordingCliqueBackend {
        recorded: Mutex<Option<(String, String)>>,
    }

    impl RecordingCliqueBackend {
        fn new() -> Self {
            Self {
                recorded: Mutex::new(None),
            }
        }
        fn recorded(&self) -> Option<(String, String)> {
            self.recorded.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl crate::protocol::Backend for RecordingCliqueBackend {
        // The seam: 2-arg `setup_keychain_clique` — escrow_passcode and
        // device_password are forwarded as separate strings, not collapsed
        // into a single shared password.
        async fn setup_keychain_clique(
            &self,
            escrow_passcode: &str,
            device_password: &str,
        ) -> Result<(), String> {
            *self.recorded.lock().unwrap() = Some((
                escrow_passcode.to_string(),
                device_password.to_string(),
            ));
            Ok(())
        }

        async fn is_keychain_clique_set_up(&self) -> bool {
            false
        }

        // --- No-op impls for the rest of the `Backend` trait. ---
        // The trait has no default methods, so the fake must provide every
        // one to satisfy `&dyn Backend`. The orchestrator under test only
        // calls `setup_keychain_clique` and `sync_missed_messages`; the rest
        // exist solely to make this a valid `Backend` impl.

        async fn config_from_relay(
            &self,
            _code: String,
            _host: String,
            _token: Option<String>,
        ) -> Result<crate::protocol::Config> {
            Ok(crate::protocol::Config::new(()))
        }

        async fn config_from_validation_data(
            &self,
            _data: Vec<u8>,
            _extra: crate::protocol::HwExtra,
        ) -> Result<crate::protocol::Config> {
            Ok(crate::protocol::Config::new(()))
        }

        async fn config_from_encoded(
            &self,
            _encoded: Vec<u8>,
        ) -> Result<crate::protocol::Config> {
            Ok(crate::protocol::Config::new(()))
        }

        async fn device_info(
            &self,
            _config: &crate::protocol::Config,
        ) -> Result<crate::protocol::DeviceInfo> {
            Ok(crate::protocol::DeviceInfo::default())
        }

        fn new_identity(&self) -> Result<crate::protocol::Identity> {
            Ok(crate::protocol::Identity::new(()))
        }

        async fn setup_push(
            &self,
            _config: &crate::protocol::Config,
            _identity: &crate::protocol::Identity,
        ) -> Result<crate::protocol::Connection> {
            Ok(crate::protocol::Connection::new(()))
        }

        async fn make_anisette(
            &self,
            _config: &crate::protocol::Config,
            _conn: &crate::protocol::Connection,
        ) -> Result<crate::protocol::Anisette> {
            Ok(crate::protocol::Anisette::new(()))
        }

        async fn try_auth(
            &self,
            _config: &crate::protocol::Config,
            _conn: &crate::protocol::Connection,
            _anisette: &crate::protocol::Anisette,
            _creds: Option<(String, String)>,
        ) -> Result<(crate::protocol::Account, crate::protocol::LoginState)> {
            Ok((
                crate::protocol::Account::new(()),
                crate::protocol::LoginState::default(),
            ))
        }

        async fn try_icloud_login(
            &self,
            _config: &crate::protocol::Config,
            _account: &crate::protocol::Account,
        ) -> Result<Option<crate::protocol::IdsUser>> {
            Ok(Some(crate::protocol::IdsUser::new(())))
        }

        async fn send_2fa_to_devices(
            &self,
            _account: &crate::protocol::Account,
            _conn: &crate::protocol::Connection,
        ) -> Result<(crate::protocol::CircleSession, crate::protocol::LoginState)> {
            Ok((
                crate::protocol::CircleSession::new(()),
                crate::protocol::LoginState::default(),
            ))
        }

        async fn verify_2fa(
            &self,
            _session: &crate::protocol::CircleSession,
            _anisette: &crate::protocol::Anisette,
            _config: &crate::protocol::Config,
            _account: &crate::protocol::Account,
            _code: String,
        ) -> Result<(crate::protocol::LoginState, Option<crate::protocol::IdsUser>)> {
            Ok((crate::protocol::LoginState::default(), None))
        }

        async fn send_2fa_sms(
            &self,
            _account: &crate::protocol::Account,
        ) -> Result<crate::protocol::LoginState> {
            Ok(crate::protocol::LoginState::default())
        }

        async fn verify_2fa_sms(
            &self,
            _account: &crate::protocol::Account,
            _anisette: &crate::protocol::Anisette,
            _config: &crate::protocol::Config,
            _body: &crate::protocol::VerifyBody,
            _code: String,
        ) -> Result<(crate::protocol::LoginState, Option<crate::protocol::IdsUser>)> {
            Ok((crate::protocol::LoginState::default(), None))
        }

        async fn register_ids(
            &self,
            _config: &crate::protocol::Config,
            _conn: &crate::protocol::Connection,
            _identity: &crate::protocol::Identity,
            _users: Vec<crate::protocol::IdsUser>,
        ) -> Result<crate::protocol::RegisterOutcome> {
            Ok(crate::protocol::RegisterOutcome::Registered(Vec::new()))
        }

        async fn make_imclient(
            &self,
            _conn: &crate::protocol::Connection,
            _identity: &crate::protocol::Identity,
            _users: Vec<crate::protocol::IdsUser>,
        ) -> Result<crate::protocol::ImClient> {
            Ok(crate::protocol::ImClient::new(()))
        }

        async fn get_handles(
            &self,
            _client: &crate::protocol::ImClient,
        ) -> Result<Vec<String>> {
            Ok(Vec::new())
        }

        async fn restore_session(&self) -> Result<Option<crate::protocol::RestoredSession>> {
            Ok(None)
        }

        fn start_receiving(
            &self,
            _connection: &crate::protocol::Connection,
            _client: &crate::protocol::ImClient,
            _handles: Vec<String>,
            _store: crate::store::Store,
            _notify: async_channel::Sender<crate::protocol::RecvEvent>,
        ) -> std::sync::Arc<tokio::sync::Notify> {
            std::sync::Arc::new(tokio::sync::Notify::new())
        }

        async fn send_text(
            &self,
            _client: &crate::protocol::ImClient,
            _chat: &crate::store::ChatRef,
            _my_handle: &str,
            text: String,
            guid: String,
        ) -> Result<crate::store::IncomingMessage> {
            Ok(crate::store::IncomingMessage {
                guid,
                text: Some(text),
                ..Default::default()
            })
        }

        #[cfg(feature = "rustpush")]
        async fn send_reaction(
            &self,
            _client: &crate::protocol::ImClient,
            _chat: &crate::store::ChatRef,
            _my_handle: &str,
            _target_guid: &str,
            _target_part: Option<u64>,
            _target_text: &str,
            _reaction: &rustpush::ReactMessageType,
        ) -> Result<()> {
            Ok(())
        }

        #[cfg(feature = "rustpush")]
        async fn send_edit(
            &self,
            _client: &crate::protocol::ImClient,
            _chat: &crate::store::ChatRef,
            _my_handle: &str,
            _target_guid: &str,
            _edit_part: u64,
            _new_text: String,
            _new_guid: String,
        ) -> Result<()> {
            Ok(())
        }

        async fn send_attachment(
            &self,
            _client: &crate::protocol::ImClient,
            _connection: &crate::protocol::Connection,
            _chat: &crate::store::ChatRef,
            _my_handle: &str,
            path: String,
            mime: String,
            name: String,
            text: Option<String>,
            guid: String,
        ) -> Result<crate::store::IncomingMessage> {
            Ok(crate::store::IncomingMessage {
                guid,
                text,
                attachments: vec![crate::store::AttachmentRecord {
                    mime: Some(mime),
                    name: Some(name),
                    local_path: Some(path),
                    ..Default::default()
                }],
                ..Default::default()
            })
        }

        fn send_receipt(
            &self,
            _client: &crate::protocol::ImClient,
            _chat: &crate::store::ChatRef,
            _my_handle: &str,
            _read: bool,
            _target_guid: String,
        ) {
        }

        fn send_typing(
            &self,
            _client: &crate::protocol::ImClient,
            _chat: &crate::store::ChatRef,
            _my_handle: &str,
            _typing: bool,
        ) {
        }

        fn sign_out(&self) {}

        #[cfg(feature = "rustpush")]
        async fn sync_missed_messages(
            &self,
            _store: &crate::store::Store,
            _cutoff_ms: i64,
            _force: bool,
        ) -> crate::sync::SyncResult {
            crate::sync::SyncResult::default()
        }
    }

    /// Pin: the new escrow-aware orchestrator entry point must forward the
    /// old trusted device passcode and the new local device password to
    /// `Backend::setup_keychain_clique` as two DISTINCT arguments. The
    /// single-password `Option<String>` interface is not sufficient for the
    /// escrow route — recovering an existing bottle and creating a new one
    /// need separate secrets.
    #[tokio::test]
    async fn run_clique_setup_then_sync_with_secrets_forwards_old_and_new_separately() {
        let backend = RecordingCliqueBackend::new();
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(tmp.path().join("db.sqlite"))
            .await
            .unwrap();

        // Two semantically distinct values: the old escrow passcode and the
        // new device password. If the orchestrator collapsed them into a
        // single argument, the test would see the same string in both
        // recording slots.
        let old_passcode = "OLD-ESCROW-PASSCODE-1234";
        let new_device_password = "NEW-DEVICE-PASSWORD-5678";
        assert_ne!(old_passcode, new_device_password);

        let result = super::run_clique_setup_then_sync_with_secrets(
            &backend,
            &store,
            i64::MIN,
            false,
            Some(old_passcode.to_string()),
            Some(new_device_password.to_string()),
        )
        .await;

        assert!(
            result.is_ok(),
            "orchestrator should succeed when both setup secrets are provided, \
             got {result:?}"
        );

        let (got_old, got_new) = backend
            .recorded()
            .expect("setup_keychain_clique should have been called exactly once");

        assert_eq!(
            got_old, old_passcode,
            "first argument to setup_keychain_clique must be the old trusted \
             device passcode (the escrow bottle secret), not a single shared \
             password"
        );
        assert_eq!(
            got_new, new_device_password,
            "second argument to setup_keychain_clique must be the new local \
             device password (this device's bottle secret), distinct from the \
             escrow passcode"
        );
        assert_ne!(
            got_old, got_new,
            "the two setup secrets must be distinct — a single password is not \
             sufficient to both recover an existing escrow bottle and create \
             a new local bottle"
        );
    }
}

#[cfg(test)]
mod clique_setup_bottle_prompt_tests {
    //! Pin: the manual sync orchestrator's prompt contract must carry
    //! the selected bottle together with the old recovery credential
    //! and the new local device password, so the selected device can
    //! be matched to the credential the user types. A bare
    //! `(String, String)` tuple (the current contract) is not enough —
    //! the orchestrator has no way to know which bottle the passcode
    //! corresponds to.
    //!
    //! The fix introduces three seams:
    //!
    //!   1. A `CliqueSetupPromptResult` value type with three fields:
    //!      `bottle: EscrowData`, `old_passcode: String`,
    //!      `new_password: String`.
    //!   2. A new orchestrator entry point
    //!      `orchestrate_sync_now_flow_with_bottle_prompt` that takes a
    //!      closure returning `Option<CliqueSetupPromptResult>` and
    //!      forwards the bottle to the backend.
    //!   3. A new `Backend::setup_keychain_clique_with_bottle` trait
    //!      method (default impl returns an error; the production
    //!      backend overrides it).
    //!
    //! The test below exercises the full path:
    //!   1. The fake backend records the bottle identity and the two
    //!      distinct secrets it received on
    //!      `setup_keychain_clique_with_bottle`.
    //!   2. The prompt closure returns
    //!      `Some(CliqueSetupPromptResult { ... })` with a recognizable
    //!      bottle id.
    //!   3. The orchestrator is called with the fake backend and the
    //!      prompt.
    //!   4. The test asserts the recorded bottle's id matches the
    //!      user-selected bottle, and the two secrets match what the
    //!      closure returned.
    //!
    //! All state is per-test: the fake is freshly constructed, the
    //! tempdir is per-test, and the closure captures only the
    //! test-local values. No env vars, no shared state.
    //!
    //! This test fails to compile under the pre-fix code because:
    //!   - `super::CliqueSetupPromptResult` does not exist,
    //!   - `super::orchestrate_sync_now_flow_with_bottle_prompt` does
    //!     not exist, and
    //!   - the `Backend` trait has no
    //!     `setup_keychain_clique_with_bottle` method, so the fake's
    //!     impl block references a non-existent trait method.
    //!
    //! The compile error is the expected red per the unit's spec
    //! (the seams are the deliverable, not a fake stub).
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Recording fake backend. `setup_keychain_clique_with_bottle`
    /// captures the bottle identity (just the `id` field, which is
    /// what the test asserts on) and the two distinct secrets. Every
    /// other trait method is a no-op that returns the default for
    /// its return type, so the fake is a valid `Backend` impl.
    struct BottlePromptRecordingBackend {
        recorded: Mutex<Option<(String, String, String)>>,
    }

    impl BottlePromptRecordingBackend {
        fn new() -> Self {
            Self {
                recorded: Mutex::new(None),
            }
        }
        fn recorded(&self) -> Option<(String, String, String)> {
            self.recorded.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl crate::protocol::Backend for BottlePromptRecordingBackend {
        // The seam: the backend must accept a user-selected bottle
        // together with the two distinct secrets. The default impl
        // (in the trait) returns an error; this fake overrides it to
        // record what the orchestrator forwarded.
        async fn setup_keychain_clique_with_bottle(
            &self,
            selected_bottle: &crate::api::EscrowData,
            escrow_passcode: &str,
            device_password: &str,
        ) -> std::result::Result<(), String> {
            *self.recorded.lock().unwrap() = Some((
                selected_bottle.id.clone().unwrap_or_default(),
                escrow_passcode.to_string(),
                device_password.to_string(),
            ));
            Ok(())
        }

        async fn setup_keychain_clique(
            &self,
            _escrow_passcode: &str,
            _device_password: &str,
        ) -> std::result::Result<(), String> {
            // Not called by the new orchestrator path; included for
            // trait completeness so the fake is a valid `Backend`
            // impl. The existing test for the 2-arg method (in
            // `clique_setup_secrets_tests`) covers the call path for
            // the legacy method.
            Ok(())
        }

        async fn is_keychain_clique_set_up(&self) -> bool {
            // Clique NOT set up — forces the orchestrator into the
            // `PromptForPassword` path, which is the path the test
            // pins. The fake's `is_keychain_clique_set_up` is
            // per-instance (each test builds a fresh fake), so
            // different tests can independently set the
            // "already-set-up" flag without cross-contamination.
            false
        }

        // --- No-op impls for the rest of the `Backend` trait. ---
        // The trait has no default methods, so the fake must provide
        // every one to satisfy `&dyn Backend`. The orchestrator under
        // test only calls `is_keychain_clique_set_up`,
        // `setup_keychain_clique_with_bottle`, and
        // `sync_missed_messages`; the rest exist solely to make this
        // a valid `Backend` impl.

        async fn config_from_relay(
            &self,
            _code: String,
            _host: String,
            _token: Option<String>,
        ) -> Result<crate::protocol::Config> {
            Ok(crate::protocol::Config::new(()))
        }

        async fn config_from_validation_data(
            &self,
            _data: Vec<u8>,
            _extra: crate::protocol::HwExtra,
        ) -> Result<crate::protocol::Config> {
            Ok(crate::protocol::Config::new(()))
        }

        async fn config_from_encoded(
            &self,
            _encoded: Vec<u8>,
        ) -> Result<crate::protocol::Config> {
            Ok(crate::protocol::Config::new(()))
        }

        async fn device_info(
            &self,
            _config: &crate::protocol::Config,
        ) -> Result<crate::protocol::DeviceInfo> {
            Ok(crate::protocol::DeviceInfo::default())
        }

        fn new_identity(&self) -> Result<crate::protocol::Identity> {
            Ok(crate::protocol::Identity::new(()))
        }

        async fn setup_push(
            &self,
            _config: &crate::protocol::Config,
            _identity: &crate::protocol::Identity,
        ) -> Result<crate::protocol::Connection> {
            Ok(crate::protocol::Connection::new(()))
        }

        async fn make_anisette(
            &self,
            _config: &crate::protocol::Config,
            _conn: &crate::protocol::Connection,
        ) -> Result<crate::protocol::Anisette> {
            Ok(crate::protocol::Anisette::new(()))
        }

        async fn try_auth(
            &self,
            _config: &crate::protocol::Config,
            _conn: &crate::protocol::Connection,
            _anisette: &crate::protocol::Anisette,
            _creds: Option<(String, String)>,
        ) -> Result<(crate::protocol::Account, crate::protocol::LoginState)> {
            Ok((
                crate::protocol::Account::new(()),
                crate::protocol::LoginState::default(),
            ))
        }

        async fn try_icloud_login(
            &self,
            _config: &crate::protocol::Config,
            _account: &crate::protocol::Account,
        ) -> Result<Option<crate::protocol::IdsUser>> {
            Ok(Some(crate::protocol::IdsUser::new(())))
        }

        async fn send_2fa_to_devices(
            &self,
            _account: &crate::protocol::Account,
            _conn: &crate::protocol::Connection,
        ) -> Result<(crate::protocol::CircleSession, crate::protocol::LoginState)> {
            Ok((
                crate::protocol::CircleSession::new(()),
                crate::protocol::LoginState::default(),
            ))
        }

        async fn verify_2fa(
            &self,
            _session: &crate::protocol::CircleSession,
            _anisette: &crate::protocol::Anisette,
            _config: &crate::protocol::Config,
            _account: &crate::protocol::Account,
            _code: String,
        ) -> Result<(crate::protocol::LoginState, Option<crate::protocol::IdsUser>)> {
            Ok((crate::protocol::LoginState::default(), None))
        }

        async fn send_2fa_sms(
            &self,
            _account: &crate::protocol::Account,
        ) -> Result<crate::protocol::LoginState> {
            Ok(crate::protocol::LoginState::default())
        }

        async fn verify_2fa_sms(
            &self,
            _account: &crate::protocol::Account,
            _anisette: &crate::protocol::Anisette,
            _config: &crate::protocol::Config,
            _body: &crate::protocol::VerifyBody,
            _code: String,
        ) -> Result<(crate::protocol::LoginState, Option<crate::protocol::IdsUser>)> {
            Ok((crate::protocol::LoginState::default(), None))
        }

        async fn register_ids(
            &self,
            _config: &crate::protocol::Config,
            _conn: &crate::protocol::Connection,
            _identity: &crate::protocol::Identity,
            _users: Vec<crate::protocol::IdsUser>,
        ) -> Result<crate::protocol::RegisterOutcome> {
            Ok(crate::protocol::RegisterOutcome::Registered(Vec::new()))
        }

        async fn make_imclient(
            &self,
            _conn: &crate::protocol::Connection,
            _identity: &crate::protocol::Identity,
            _users: Vec<crate::protocol::IdsUser>,
        ) -> Result<crate::protocol::ImClient> {
            Ok(crate::protocol::ImClient::new(()))
        }

        async fn get_handles(
            &self,
            _client: &crate::protocol::ImClient,
        ) -> Result<Vec<String>> {
            Ok(Vec::new())
        }

        async fn restore_session(&self) -> Result<Option<crate::protocol::RestoredSession>> {
            Ok(None)
        }

        fn start_receiving(
            &self,
            _connection: &crate::protocol::Connection,
            _client: &crate::protocol::ImClient,
            _handles: Vec<String>,
            _store: crate::store::Store,
            _notify: async_channel::Sender<crate::protocol::RecvEvent>,
        ) -> std::sync::Arc<tokio::sync::Notify> {
            std::sync::Arc::new(tokio::sync::Notify::new())
        }

        async fn send_text(
            &self,
            _client: &crate::protocol::ImClient,
            _chat: &crate::store::ChatRef,
            _my_handle: &str,
            text: String,
            guid: String,
        ) -> Result<crate::store::IncomingMessage> {
            Ok(crate::store::IncomingMessage {
                guid,
                text: Some(text),
                ..Default::default()
            })
        }

        #[cfg(feature = "rustpush")]
        async fn send_reaction(
            &self,
            _client: &crate::protocol::ImClient,
            _chat: &crate::store::ChatRef,
            _my_handle: &str,
            _target_guid: &str,
            _target_part: Option<u64>,
            _target_text: &str,
            _reaction: &rustpush::ReactMessageType,
        ) -> Result<()> {
            Ok(())
        }

        #[cfg(feature = "rustpush")]
        async fn send_edit(
            &self,
            _client: &crate::protocol::ImClient,
            _chat: &crate::store::ChatRef,
            _my_handle: &str,
            _target_guid: &str,
            _edit_part: u64,
            _new_text: String,
            _new_guid: String,
        ) -> Result<()> {
            Ok(())
        }

        async fn send_attachment(
            &self,
            _client: &crate::protocol::ImClient,
            _connection: &crate::protocol::Connection,
            _chat: &crate::store::ChatRef,
            _my_handle: &str,
            path: String,
            mime: String,
            name: String,
            text: Option<String>,
            guid: String,
        ) -> Result<crate::store::IncomingMessage> {
            Ok(crate::store::IncomingMessage {
                guid,
                text,
                attachments: vec![crate::store::AttachmentRecord {
                    mime: Some(mime),
                    name: Some(name),
                    local_path: Some(path),
                    ..Default::default()
                }],
                ..Default::default()
            })
        }

        fn send_receipt(
            &self,
            _client: &crate::protocol::ImClient,
            _chat: &crate::store::ChatRef,
            _my_handle: &str,
            _read: bool,
            _target_guid: String,
        ) {
        }

        fn send_typing(
            &self,
            _client: &crate::protocol::ImClient,
            _chat: &crate::store::ChatRef,
            _my_handle: &str,
            _typing: bool,
        ) {
        }

        fn sign_out(&self) {}

        async fn sync_missed_messages(
            &self,
            _store: &crate::store::Store,
            _cutoff_ms: i64,
            _force: bool,
        ) -> crate::sync::SyncResult {
            crate::sync::SyncResult::default()
        }
    }

    /// Pin: the new orchestrator entry point
    /// `orchestrate_sync_now_flow_with_bottle_prompt` must accept a
    /// prompt closure that returns `Option<CliqueSetupPromptResult>`
    /// (a struct carrying the user-selected bottle + the two distinct
    /// setup secrets), and must forward the selected bottle to the
    /// backend via `Backend::setup_keychain_clique_with_bottle` — not
    /// silently pick a default bottle and drop the user's selection.
    ///
    /// Pre-fix behavior: the orchestrator took a closure returning
    /// `Option<(String, String)>` and called the 2-arg
    /// `setup_keychain_clique(old, new)` — the bottle was never visible
    /// to the orchestrator and was silently picked by the backend (via
    /// `select_clique_setup_route`, which defaulted to bottle index 0).
    /// Post-fix behavior: the orchestrator carries the user-selected
    /// bottle through the prompt, and the backend receives it via
    /// `setup_keychain_clique_with_bottle`. This test pins the full
    /// path: prompt → orchestrator → backend.
    #[tokio::test]
    #[allow(clippy::field_reassign_with_default)]
    async fn orchestrate_sync_now_flow_with_bottle_prompt_forwards_selected_bottle_to_backend(
    ) {
        let backend = BottlePromptRecordingBackend::new();
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(tmp.path().join("db.sqlite"))
            .await
            .unwrap();

        // Construct the user-selected bottle with a recognizable id.
        // Post-fix: the orchestrator must forward THIS bottle
        // (id = "B-SELECTED-42") to the backend so the backend can
        // match the credential to the right device. Pre-fix: the
        // orchestrator had no notion of "selected bottle" — the
        // backend would silently pick bottle index 0 and the user
        // would get a wrong-device passcode prompt.
        let mut selected_bottle = crate::api::EscrowData::default();
        selected_bottle.id = Some("B-SELECTED-42".to_string());

        let old_passcode = "OLD-ESCROW-PASSCODE-1234";
        let new_password = "NEW-DEVICE-PASSWORD-5678";
        assert_ne!(
            old_passcode, new_password,
            "test setup: the two secrets must be distinct so the backend \
             receives them as separate arguments, not collapsed into one"
        );

        // The new orchestrator + the new prompt result type. Both
        // seams are required for this test to compile. Pre-fix code
        // has neither, so the test fails to compile — the expected
        // red per the unit's spec.
        let bottle_for_closure = selected_bottle.clone();
        let result = super::orchestrate_sync_now_flow_with_bottle_prompt(
            &backend,
            &store,
            i64::MIN,
            false,
            move || {
                Some(super::CliqueSetupPromptResult {
                    bottle: bottle_for_closure,
                    old_passcode: old_passcode.to_string(),
                    new_password: new_password.to_string(),
                })
            },
        )
        .await;

        assert!(
            result.is_ok(),
            "orchestrator should succeed when the prompt returns a valid \
             CliqueSetupPromptResult, got {result:?}"
        );

        let (got_bottle_id, got_old, got_new) = backend
            .recorded()
            .expect("setup_keychain_clique_with_bottle should have been called exactly once");

        // The user-selected bottle must reach the backend. If the
        // orchestrator dropped the bottle (e.g. unwrapped the
        // struct and passed only the secrets), the backend would
        // see an empty id and the assertion would fail.
        assert_eq!(
            got_bottle_id, "B-SELECTED-42",
            "the user-selected bottle must be forwarded to the backend; the \
             orchestrator must not silently pick a default bottle (index 0) \
             and drop the user's selection. The prompt contract must carry the \
             bottle so the orchestrator can route the credential to the right \
             device."
        );
        assert_eq!(
            got_old, old_passcode,
            "first secret forwarded to the backend must be the old trusted-device \
             passcode (the escrow bottle secret)"
        );
        assert_eq!(
            got_new, new_password,
            "second secret forwarded to the backend must be the new local device \
             password (this device's bottle secret)"
        );
    }
}

#[cfg(test)]
mod launch_6005_retry_guard_tests {
    //! Pin: the launch-time 6005 retry-now guard for the registration
    //! watcher. The receive loop currently pulses every resource-state change
    //! straight to the UI; in production, the auto-rereg can fail with
    //! `PushError::AuthInvalid(IDSError(6005))` *very* rapidly during the
    //! initial resource warmup (the IdentityManager tries, fails, the
    //! ResourceManager wraps it in a `ResourceState::Failed` with a short
    //! `retry_wait`, the watcher re-fires, etc.). A naive "trigger
    //! `identity.refresh_now()` on every 6005" would spam the rereg endpoint
    //! a few times per second at launch and worsen the failure mode.
    //!
    //! The guard adds a cooldown: the first 6005 retry-now fires immediately
    //! (so the self-heal path is not silently dropped — the typical
    //! bug-repro is "no retry ever happens"), subsequent 6005 retry-nows
    //! within the cooldown are suppressed, and any 6005 after the cooldown
    //! elapses is allowed again. A successful (Generated) state resets the
    //! guard so a *future* 6005 (e.g. after a sleep/wake cycle invalidates
    //! the cert) is again allowed to fire immediately — without the reset,
    //! the user would have to wait out the full cooldown after every
    //! successful registration.
    //!
    //! This test pins the contract of the seam — a `Launch6005RetryGuard`
    //! struct with a `cooldown: Duration` and an internal `last_retry:
    //! Option<u64>` (seconds), constructed via `new(cooldown)` and queried
    //! via `evaluate(&ResourceState, now_secs) -> bool`. The struct lives
    //! next to `is_6005_error` in `rustpush_backend.rs` and is the unit
    //! the resource watcher would call to decide whether to call
    //! `identity.refresh_now()`. Pure, no I/O, no async.
    //!
    //! Test isolation: each test builds a fresh guard and uses synthetic
    //! `u64` second timestamps — no shared state, no env vars, no clock.
    //!
    //! This test fails to compile under the pre-fix code because
    //! `super::Launch6005RetryGuard` does not exist. The compile error is
    //! the expected red per the unit's spec (the seam — a stateful guard
    //! struct + a 5-pin behavior contract — is the deliverable, not a fake
    //! stub).
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    /// Build a `ResourceState::Failed` carrying an `AuthInvalid(6005)` error
    /// — the exact form the IdentityManager's resource manager produces when
    /// a launch-time rereg fails with Apple's "bad authentication" code.
    /// `retry_wait: Some(300)` matches the `MAX_RESOURCE_WAIT` style
    /// suggestion the resource manager typically attaches — the test does
    /// NOT key on it; it keys on the inner error being 6005.
    fn failed_6005() -> rustpush::ResourceState {
        rustpush::ResourceState::Failed(rustpush::ResourceFailure {
            retry_wait: Some(300),
            error: Arc::new(rustpush::PushError::AuthInvalid(rustpush::IDSError(6005))),
        })
    }

    /// Build a `ResourceState::Failed` carrying a non-6005 error (a
    /// `BadMsg` PushError, the canonical generic-error sentinel in
    /// `rustpush::PushError`). The guard must NOT trigger the 6005
    /// self-heal retry-now on this — the self-heal is 6005-specific and
    /// must not be widened to any failure.
    fn failed_non_6005() -> rustpush::ResourceState {
        rustpush::ResourceState::Failed(rustpush::ResourceFailure {
            retry_wait: Some(300),
            error: Arc::new(rustpush::PushError::BadMsg),
        })
    }

    /// Pin 1: the **first** 6005 `Failed` state observed at launch must
    /// return `true` from `evaluate` — the self-heal retry-now fires
    /// immediately. This is the bug-repro pin: a guard that defaults to
    /// "suppress all 6005s" (a tempting "safe" implementation) would
    /// silently drop the launch-time retry and the user would never see
    /// the self-heal fire. The pre-fix code (no guard at all) fires
    /// repeatedly; the post-fix guard must fire exactly once on the first
    /// observation, and no more until the cooldown elapses.
    #[test]
    fn first_6005_is_allowed_immediately() {
        let mut guard = Launch6005RetryGuard::new(Duration::from_secs(60));
        let state = failed_6005();
        // The first 6005 — at any `now` value — must be allowed. The guard
        // starts with no prior retry, so the cooldown is irrelevant for
        // this call: it's the "fire immediately" baseline.
        assert!(
            guard.evaluate(&state, 0),
            "first 6005 at t=0 must be allowed to fire refresh_now() \
             immediately — a guard that suppresses the first 6005 would \
             silently drop the launch-time self-heal and reintroduce the \
             'no retry ever happens' bug"
        );
        // Sanity: also allow at a non-zero t (e.g. mid-test), as long as
        // the guard has not yet observed a prior retry.
        let mut guard2 = Launch6005RetryGuard::new(Duration::from_secs(60));
        assert!(
            guard2.evaluate(&state, 1_000_000),
            "first 6005 at t=1_000_000 (no prior retry) must also be allowed"
        );
    }

    /// Pin 2: a second 6005 `Failed` observed *within* the cooldown window
    /// must be suppressed — the guard returns `false` and the watcher
    /// must NOT call `identity.refresh_now()`. The cooldown prevents
    /// the rereg endpoint from being spammed by the resource manager's
    /// tight retry loop at launch.
    #[test]
    fn second_6005_within_cooldown_is_suppressed() {
        let mut guard = Launch6005RetryGuard::new(Duration::from_secs(60));
        let state = failed_6005();

        // First 6005 at t=0: allowed.
        assert!(
            guard.evaluate(&state, 0),
            "first 6005 must be allowed (sets the cooldown baseline)"
        );
        // Second 6005 at t=10 (well within the 60s cooldown): suppressed.
        assert!(
            !guard.evaluate(&state, 10),
            "second 6005 at t=10 must be SUPPRESSED (cooldown=60s, only \
             10s elapsed since the first retry) — without this, the rereg \
             endpoint would be spammed by the resource manager's tight \
             retry loop at launch"
        );
        // And at t=59 (1s before the cooldown boundary): still suppressed.
        assert!(
            !guard.evaluate(&state, 59),
            "second 6005 at t=59 (last second of the cooldown) must also be \
             suppressed"
        );
    }

    /// Pin 3: a 6005 `Failed` observed *after* the cooldown has elapsed
    /// must be allowed again — the guard re-arms. This is the "if the
    /// self-heal didn't fix the cert the first time, try again later"
    /// behavior. After this call the cooldown re-starts.
    #[test]
    fn next_6005_allowed_again_after_cooldown_elapses() {
        let mut guard = Launch6005RetryGuard::new(Duration::from_secs(60));
        let state = failed_6005();

        // First 6005 at t=0: allowed.
        assert!(guard.evaluate(&state, 0));
        // Second 6005 at t=30: suppressed (within 60s cooldown).
        assert!(!guard.evaluate(&state, 30));
        // Third 6005 at t=61 (1s past the cooldown boundary): allowed.
        assert!(
            guard.evaluate(&state, 61),
            "6005 at t=61 (cooldown=60s, 61s elapsed since the first retry) \
             must be ALLOWED — after the cooldown the guard must re-arm so \
             the self-heal gets a second chance if the first attempt did \
             not clear Apple's auth state"
        );
        // And after a second allowed retry, a subsequent 6005 within the
        // new cooldown must again be suppressed (cooldown re-starts).
        assert!(
            !guard.evaluate(&state, 90),
            "6005 at t=90 (29s after the re-armed retry at t=61) must be \
             suppressed — the cooldown restarts after every allowed retry"
        );
    }

    /// Pin 4: a *non-6005* `Failed` state must NOT trigger the 6005
    /// self-heal retry-now. The self-heal is keyed on the inner error
    /// being a 6005 (`is_6005_error`); widening it to "any failure" would
    /// fire `identity.refresh_now()` for transient network blips, server
    /// restarts, etc. — exactly the spam the guard is supposed to prevent.
    #[test]
    fn non_6005_failure_does_not_trigger_retry() {
        let mut guard = Launch6005RetryGuard::new(Duration::from_secs(60));
        let state = failed_non_6005();

        // A non-6005 failure at t=0 must be suppressed.
        assert!(
            !guard.evaluate(&state, 0),
            "non-6005 failure (BadMsg) at t=0 must NOT trigger the 6005 \
             self-heal retry-now — the self-heal is 6005-specific; a guard \
             that widens to 'any failure' would spam identity.refresh_now() \
             on every transient error"
        );
        // And at any later time: still suppressed.
        assert!(
            !guard.evaluate(&state, 1_000_000),
            "non-6005 failure at t=1_000_000 must also NOT trigger the \
             6005 self-heal retry-now"
        );
    }

    /// Pin 5: a `ResourceState::Generated` observation resets the guard
    /// — a *future* 6005 (e.g. after a sleep/wake cycle invalidates the
    /// cert, the typical launch-time 6005 re-fire scenario) must be
    /// allowed to fire `refresh_now()` immediately, *without* having to
    /// wait out the cooldown. The cooldown is a backoff for repeated
    /// failures; a successful registration in between means the failure
    /// is no longer "in flight" and the next 6005 is a fresh incident.
    ///
    /// Sequence: first 6005 (allow) → second 6005 within cooldown
    /// (suppress) → Generated (reset) → fresh 6005 (allow immediately).
    #[test]
    fn generated_state_resets_guard() {
        let mut guard = Launch6005RetryGuard::new(Duration::from_secs(60));
        let state_6005 = failed_6005();
        let state_generated = rustpush::ResourceState::Generated;

        // First 6005 at t=0: allowed.
        assert!(guard.evaluate(&state_6005, 0));
        // Second 6005 at t=10: suppressed (within cooldown).
        assert!(
            !guard.evaluate(&state_6005, 10),
            "second 6005 within cooldown must be suppressed (sanity)"
        );
        // A Generated state arrives at t=20 — the rereg has succeeded.
        // This must reset the guard.
        let _ = guard.evaluate(&state_generated, 20);
        // A *fresh* 6005 at t=21 (1s after the Generated reset): must be
        // allowed immediately — the cooldown is no longer in effect.
        assert!(
            guard.evaluate(&state_6005, 21),
            "a fresh 6005 at t=21 (1s after a Generated reset at t=20) must \
             be allowed to fire refresh_now() immediately — without the \
             reset, the user would have to wait out the full cooldown after \
             every successful registration, defeating the purpose of the \
             self-heal"
        );
    }

    /// Pin 6 (sanity): the `Generating` and `Closed` states (the other
    /// `ResourceState` variants) must NOT trigger the 6005 self-heal
    /// retry-now. `Generating` means a rereg is already in flight
    /// (calling `refresh_now()` again would race it); `Closed` means
    /// the resource is gone (refresh would be a no-op or error). This
    /// pins that the guard is keyed on the 6005 inner error, not on
    /// "any non-Generated state".
    #[test]
    fn generating_and_closed_states_do_not_trigger_retry() {
        let mut guard = Launch6005RetryGuard::new(Duration::from_secs(60));

        // Generating: a rereg is already in flight. The self-heal must
        // not stack another refresh_now on top.
        assert!(
            !guard.evaluate(&rustpush::ResourceState::Generating, 0),
            "Generating state must NOT trigger 6005 self-heal retry-now — \
             a rereg is already in flight and refresh_now() would race it"
        );
        // Closed: the resource is gone. refresh_now() is a no-op at
        // best, an error at worst.
        assert!(
            !guard.evaluate(&rustpush::ResourceState::Closed, 0),
            "Closed state must NOT trigger 6005 self-heal retry-now — \
             the resource is gone and refresh_now() would be a no-op or error"
        );
        // Registered (Generated) on its own: must NOT trigger the 6005
        // self-heal (it is the success state). But it should still be
        // accepted without panicking and should reset any prior cooldown
        // (covered by pin 5).
        assert!(
            !guard.evaluate(&rustpush::ResourceState::Generated, 0),
            "Generated state must NOT by itself trigger a 6005 self-heal \
             retry-now (it is the success state; the watcher should pass it \
             straight to the UI as RegistrationStatus::Registered)"
        );
    }
}
