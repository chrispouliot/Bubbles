//! A no-op backend that returns canned values, so the UI compiles and the whole
//! onboarding flow can be clicked through before `rustpush` is linked.
//!
//! Replace with `RustpushBackend` (the lifted `api.rs`) when ready — the flow
//! and UI don't change, only the `Arc<dyn Backend>` handed to them in `main`.

use async_trait::async_trait;

use super::*;

/// Drives the flow down the trusted-device 2FA path and then to a logged-in
/// state with one phone handle.
///
/// The `clique_set_up` field controls `is_keychain_clique_set_up`: when `true`
/// the stub reports the clique is set up (`SyncNow` path); when `false` (the
/// default) it reports not set up (`PromptForPassword` path). Tests that need
/// the sync-now path set this to `true`; all existing tests use the default.
#[derive(Default)]
pub struct StubBackend {
    /// If `true`, `is_keychain_clique_set_up` returns `true`.
    pub clique_set_up: bool,
}

#[async_trait]
impl Backend for StubBackend {
    async fn config_from_relay(
        &self,
        _code: String,
        _host: String,
        _token: Option<String>,
    ) -> Result<Config> {
        Ok(Config::new(()))
    }

    async fn config_from_validation_data(
        &self,
        _data: Vec<u8>,
        _extra: HwExtra,
    ) -> Result<Config> {
        Ok(Config::new(()))
    }

    async fn config_from_encoded(&self, _encoded: Vec<u8>) -> Result<Config> {
        Ok(Config::new(()))
    }

    async fn device_info(&self, _config: &Config) -> Result<DeviceInfo> {
        Ok(DeviceInfo {
            name: "MacBookPro18,3".into(),
            serial: "C02XXXXXXXXX".into(),
            os_version: "14.5".into(),
        })
    }

    fn new_identity(&self) -> Result<Identity> {
        Ok(Identity::new(()))
    }

    async fn setup_push(&self, _config: &Config, _identity: &Identity) -> Result<Connection> {
        Ok(Connection::new(()))
    }

    async fn make_anisette(&self, _config: &Config, _conn: &Connection) -> Result<Anisette> {
        Ok(Anisette::new(()))
    }

    async fn try_auth(
        &self,
        _config: &Config,
        _conn: &Connection,
        _anisette: &Anisette,
        creds: Option<(String, String)>,
    ) -> Result<(Account, LoginState)> {
        let next = if creds.is_some() {
            // pretend the account has trusted devices -> ask for a code
            LoginState::NeedsDevice2Fa
        } else {
            LoginState::NeedsLogin
        };
        Ok((Account::new(()), next))
    }

    async fn try_icloud_login(
        &self,
        _config: &Config,
        _account: &Account,
    ) -> Result<Option<IdsUser>> {
        Ok(Some(IdsUser::new(())))
    }

    async fn send_2fa_to_devices(
        &self,
        _account: &Account,
        _conn: &Connection,
    ) -> Result<(CircleSession, LoginState)> {
        Ok((CircleSession::new(()), LoginState::Needs2FaVerification))
    }

    async fn verify_2fa(
        &self,
        _session: &CircleSession,
        _anisette: &Anisette,
        _config: &Config,
        _account: &Account,
        _code: String,
    ) -> Result<(LoginState, Option<IdsUser>)> {
        Ok((LoginState::LoggedIn, Some(IdsUser::new(()))))
    }

    async fn send_2fa_sms(&self, _account: &Account) -> Result<LoginState> {
        Ok(LoginState::NeedsSms2FaVerification(VerifyBody::new(())))
    }

    async fn verify_2fa_sms(
        &self,
        _account: &Account,
        _anisette: &Anisette,
        _config: &Config,
        _body: &VerifyBody,
        _code: String,
    ) -> Result<(LoginState, Option<IdsUser>)> {
        Ok((LoginState::LoggedIn, Some(IdsUser::new(()))))
    }

    async fn register_ids(
        &self,
        _config: &Config,
        _conn: &Connection,
        _identity: &Identity,
        _users: Vec<IdsUser>,
    ) -> Result<RegisterOutcome> {
        Ok(RegisterOutcome::Registered(vec![IdsUser::new(())]))
    }

    async fn make_imclient(
        &self,
        _conn: &Connection,
        _identity: &Identity,
        _users: Vec<IdsUser>,
    ) -> Result<ImClient> {
        Ok(ImClient::new(()))
    }

    async fn get_handles(&self, _client: &ImClient) -> Result<Vec<String>> {
        Ok(vec![
            "tel:+15555550123".into(),
            "mailto:you@example.com".into(),
        ])
    }

    async fn restore_session(&self) -> Result<Option<RestoredSession>> {
        // The stub never has a saved session; always run onboarding.
        Ok(None)
    }

    fn start_receiving(
        &self,
        _connection: &Connection,
        _client: &ImClient,
        _handles: Vec<String>,
        _store: crate::store::Store,
        _notify: async_channel::Sender<crate::protocol::RecvEvent>,
    ) -> std::sync::Arc<tokio::sync::Notify> {
        // No live connection in the stub; nothing to receive.
        std::sync::Arc::new(tokio::sync::Notify::new())
    }

    async fn send_text(
        &self,
        _client: &ImClient,
        chat: &crate::store::ChatRef,
        my_handle: &str,
        text: String,
        guid: String,
    ) -> Result<crate::store::IncomingMessage> {
        Ok(crate::store::IncomingMessage {
            guid,
            chat: chat.clone(),
            sender: Some(my_handle.to_string()),
            is_from_me: true,
            text: Some(text),
            date: 0,
            ..Default::default()
        })
    }

    #[cfg(feature = "rustpush")]
    #[allow(clippy::too_many_arguments)]
    async fn send_reaction(
        &self,
        _client: &ImClient,
        _chat: &ChatRef,
        _my_handle: &str,
        _target_guid: &str,
        _target_part: Option<u64>,
        _target_text: &str,
        _reaction: &rustpush::ReactMessageType,
    ) -> Result<()> {
        Ok(())
    }

    #[cfg(feature = "rustpush")]
    #[allow(clippy::too_many_arguments)]
    async fn send_edit(
        &self,
        _client: &ImClient,
        _chat: &ChatRef,
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
        _client: &ImClient,
        _connection: &Connection,
        chat: &crate::store::ChatRef,
        my_handle: &str,
        path: String,
        mime: String,
        name: String,
        text: Option<String>,
        guid: String,
    ) -> Result<crate::store::IncomingMessage> {
        Ok(crate::store::IncomingMessage {
            guid,
            chat: chat.clone(),
            sender: Some(my_handle.to_string()),
            is_from_me: true,
            text,
            date: 0,
            attachments: vec![crate::store::AttachmentRecord {
                mime: Some(mime),
                name: Some(name),
                local_path: Some(path),
                part_index: Some(0),
                ..Default::default()
            }],
            ..Default::default()
        })
    }

    fn send_receipt(
        &self,
        _client: &ImClient,
        _chat: &crate::store::ChatRef,
        _my_handle: &str,
        _read: bool,
        _target_guid: String,
    ) {
    }

    fn send_typing(
        &self,
        _client: &ImClient,
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

    async fn setup_keychain_clique(
        &self,
        _password: &str,
    ) -> std::result::Result<(), String> {
        Ok(())
    }

    async fn is_keychain_clique_set_up(&self) -> bool {
        self.clique_set_up
    }
}

#[allow(dead_code)]
fn stub_nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_chat_ref() -> crate::store::ChatRef {
        crate::store::ChatRef {
            participants: vec!["mailto:a@icloud.com".into(), "mailto:b@icloud.com".into()],
            display_name: None,
            service: Some("iMessage".into()),
        }
    }

    #[tokio::test]
    async fn send_attachment_without_caption() {
        let backend = StubBackend::default();
        let client = ImClient::new(());
        let connection = Connection::new(());
        let chat = sample_chat_ref();
        let my_handle = "tel:+15555550123";
        let path = "/tmp/test.jpg".to_string();
        let mime = "image/jpeg".to_string();
        let name = "photo.jpg".to_string();
        let guid = "guid-no-caption".to_string();

        let result = backend
            .send_attachment(
                &client,
                &connection,
                &chat,
                my_handle,
                path,
                mime,
                name,
                None,
                guid.clone(),
            )
            .await
            .expect("send_attachment should succeed");

        assert_eq!(result.text, None, "text should be None when no caption is provided");
        assert_eq!(result.attachments.len(), 1, "should have exactly one attachment");
    }

    #[cfg(feature = "rustpush")]
    #[tokio::test]
    async fn send_reaction_returns_ok() {
        // Pin: the `Backend::send_reaction` method exists on `StubBackend` with
        // this exact signature, and the stub returns `Ok(())` for any reaction
        // payload (the whole point of the stub is offline iteration, so the
        // reaction send is a no-op success).
        use rustpush::{ReactMessageType, Reaction};

        let backend = StubBackend::default();
        let client = ImClient::new(());
        let chat = sample_chat_ref();
        let my_handle = "tel:+15555550123";
        let target_guid = "target-guid-abc";
        let target_part: Option<u64> = Some(0);
        let reaction = ReactMessageType::React {
            reaction: Reaction::Heart,
            enable: true,
        };

        let target_text = "Hello world";

        let result = backend
            .send_reaction(
                &client,
                &chat,
                my_handle,
                target_guid,
                target_part,
                target_text,
                &reaction,
            )
            .await;

        assert!(
            result.is_ok(),
            "StubBackend::send_reaction should return Ok(()) for any inputs, got {result:?}"
        );
    }

    #[cfg(feature = "rustpush")]
    #[tokio::test]
    async fn send_edit_returns_ok() {
        // Pin: the `Backend::send_edit` method exists on `StubBackend` with
        // this exact signature, and the stub returns `Ok(())` for any edit
        // payload (the whole point of the stub is offline iteration, so the
        // edit send is a no-op success).

        let backend = StubBackend::default();
        let client = ImClient::new(());
        let chat = sample_chat_ref();
        let my_handle = "tel:+15555550123";
        let target_guid = "target-guid-abc";
        let edit_part: u64 = 0;
        let new_text = "Edited message text".to_string();
        let new_guid = "new-guid-xyz".to_string();

        let result = backend
            .send_edit(
                &client,
                &chat,
                my_handle,
                target_guid,
                edit_part,
                new_text,
                new_guid,
            )
            .await;

        assert!(
            result.is_ok(),
            "StubBackend::send_edit should return Ok(()) for any inputs, got {result:?}"
        );
    }

    #[tokio::test]
    async fn send_attachment_with_caption() {
        let backend = StubBackend::default();
        let client = ImClient::new(());
        let connection = Connection::new(());
        let chat = sample_chat_ref();
        let my_handle = "tel:+15555550123";
        let path = "/tmp/document.pdf".to_string();
        let mime = "application/pdf".to_string();
        let name = "report.pdf".to_string();
        let caption = "caption text".to_string();
        let guid = "guid-with-caption".to_string();

        let result = backend
            .send_attachment(
                &client,
                &connection,
                &chat,
                my_handle,
                path.clone(),
                mime.clone(),
                name.clone(),
                Some(caption.clone()),
                guid.clone(),
            )
            .await
            .expect("send_attachment should succeed");

        assert_eq!(
            result.text, Some(caption),
            "text should be Some(caption) when a caption is provided"
        );
        assert_eq!(result.attachments.len(), 1, "should have exactly one attachment");

        let attachment = &result.attachments[0];
        assert_eq!(
            attachment.mime.as_ref(),
            Some(&mime),
            "attachment mime should match the passed mime"
        );
        assert_eq!(
            attachment.name.as_ref(),
            Some(&name),
            "attachment name should match the passed name"
        );
        assert_eq!(
            attachment.local_path.as_ref(),
            Some(&path),
            "attachment local_path should match the passed path"
        );
    }

    #[tokio::test]
    async fn setup_keychain_clique_returns_ok() {
        // Pin: `Backend::setup_keychain_clique` exists on the trait with
        // signature
        //   `async fn setup_keychain_clique(&self, password: &str) -> Result<()>`
        // and the `StubBackend` implementation returns `Ok(())` without making
        // any network calls. This is the contract for the iCloud Keychain
        // clique-setup unit. The stub is deliberately offline — it proves the
        // trait method compiles and defaults to success for click-through
        // iteration.
        let backend = StubBackend::default();
        let result = backend.setup_keychain_clique("test-password").await;
        assert!(
            result.is_ok(),
            "StubBackend::setup_keychain_clique should return Ok(()) for any \
             password, got {result:?}"
        );
    }

    #[tokio::test]
    async fn is_keychain_clique_set_up_stub_returns_false() {
        // Pin: `Backend::is_keychain_clique_set_up` exists on the trait
        // with signature `async fn is_keychain_clique_set_up(&self) -> bool`,
        // and the `StubBackend` implementation returns `false` — the stub
        // is offline and never has a real clique.  This is the offline
        // counterpart to the production disk-check tests.
        let backend = StubBackend::default();
        assert!(!backend.is_keychain_clique_set_up().await);
    }

    #[cfg(feature = "rustpush")]
    #[tokio::test]
    async fn run_clique_setup_then_sync_with_password() {
        // Pin: `run_clique_setup_then_sync` accepts a `Some(password)`, calls
        // `setup_keychain_clique` (which always succeeds on the stub), then
        // `sync_missed_messages`, and returns the sync result.
        let backend = StubBackend::default();
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(tmp.path().join("db.sqlite")).await.unwrap();
        let result = crate::protocol::rustpush_backend::run_clique_setup_then_sync(
            &backend,
            &store,
            i64::MIN,
            false,
            Some("test-password".to_string()),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), crate::sync::SyncResult::default());
    }
}
