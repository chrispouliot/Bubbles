#![allow(dead_code, unused_imports, clippy::type_complexity)]

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::SystemTime;
use serde::{Deserialize, Serialize};

use rustpush::cloud_messages::{
    CloudChat, CloudMessage, MessageFlags, cloudmessagesp::MessageProto,
};
use crate::store::{ChatRef, IncomingMessage, Ingest, Store, Tapback};
use async_trait::async_trait;
use rustpush::PushError;

/// Convert CloudKit's nanoseconds-since-apple-epoch to milliseconds-since-unix-epoch.
///
/// Apple epoch = 2001-01-01 00:00:00 UTC, offset from unix epoch = 978307200 seconds
/// = 978307200000 ms. Dividing by 1_000_000 converts ns → ms, truncating
/// sub-millisecond fractional parts (fine for our second-level granularity).
pub fn apple_time_to_unix_ms(apple_ns: i64) -> i64 {
    (apple_ns / 1_000_000) + 978_307_200_000
}

/// Translate a CloudKit [`CloudMessage`] record into the app's internal [`Ingest`].
///
/// Returns one of:
/// * `Ingest::Tapback` – when the protobuf carries a tapback reaction code
///   (2000..=2005 or 3000..=3005) and a target GUID.
/// * `Ingest::Message` – when the protobuf has a `text` field.
/// * `Ingest::Ignored` – for any record we can't represent (no text, not a tapback).
///
/// `my_handles` is used (case-insensitively) as a secondary `is_from_me` signal.
/// `chat_map` provides participant data for known chats; when absent a minimal
/// `ChatRef` is synthesised from the message's `chat_id`.
pub fn cloud_message_to_ingest(
    cm: CloudMessage,
    my_handles: &[String],
    chat_map: &HashMap<String, CloudChat>,
) -> Ingest {
    let proto: &MessageProto = &cm.msg_proto;
    let from_me = is_from_me(&cm.flags, &cm.sender, my_handles);

    // --- Tapback? ---
    if let Some(target_guid) = &proto.associated_message_guid {
        if proto
            .associated_message_type
            .is_some_and(|t| (2000..=2005).contains(&t) || (3000..=3005).contains(&t))
        {
            return Ingest::Tapback(Tapback {
                guid: cm.guid,
                chat: chat_ref_for(&cm.chat_id, &cm.service, chat_map, &cm.sender, from_me, &cm.destination_caller_id),
                sender: Some(cm.sender),
                is_from_me: from_me,
                date: apple_time_to_unix_ms(cm.time),
                associated_guid: target_guid.clone(),
                associated_type: proto.associated_message_type.unwrap_or(0) as i64,
                associated_part: None,
            });
        }
    }

    // --- Plain message? ---
    if let Some(ref text) = proto.text {
        return Ingest::Message(IncomingMessage {
            guid: cm.guid,
            chat: chat_ref_for(&cm.chat_id, &cm.service, chat_map, &cm.sender, from_me, &cm.destination_caller_id),
            sender: Some(cm.sender),
            is_from_me: from_me,
            text: Some(text.clone()),
            subject: proto.subject.clone(),
            service: Some(cm.service),
            date: apple_time_to_unix_ms(cm.time),
            effect: proto.effect.clone(),
            reply_to_guid: None,
            reply_part: None,
            item_type: 0,
            attachments: Vec::new(),
            pending: false,
        });
    }

    // --- Unsupported record type ---
    Ingest::Ignored("cloud_message: no text and not a tapback")
}

/// Whether a message is "from me", based on the [`MessageFlags::IS_FROM_ME`] flag
/// (primary signal) or a case-insensitive handle match against `my_handles`.
fn is_from_me(flags: &MessageFlags, sender: &str, my_handles: &[String]) -> bool {
    flags.contains(MessageFlags::IS_FROM_ME)
        || my_handles.iter().any(|h| h.eq_ignore_ascii_case(sender))
}

/// Build a [`ChatRef`] from the cloud chat map, or a minimal fallback when the
/// chat hasn't been synced yet.
///
/// When cloud metadata is available, the participants and display name come
/// from [`CloudChat`]. When metadata is absent, the raw compound `chat_id`
/// (e.g. `iMessage;-;+12345`) must **not** be used as a participant because
/// the UI splits participants on `;` and would produce fake labels. Instead
/// the message `sender` handle is used as the sole participant — it is always
/// a display-safe handle (email or phone URI).
fn chat_ref_for(
    chat_id: &str,
    service: &str,
    chat_map: &HashMap<String, CloudChat>,
    sender: &str,
    from_me: bool,
    destination_caller_id: &str,
) -> ChatRef {
    if let Some(cloud_chat) = chat_map.get(chat_id) {
        ChatRef {
            participants: cloud_chat.participants.iter().map(|p| p.uri.clone()).collect(),
            display_name: cloud_chat.display_name.clone(),
            service: Some(service.to_string()),
        }
    } else {
        // Fallback: use the sender handle rather than the raw compound
        // chat_id. The sender is always a display-safe handle (email, tel,
        // etc.) — never the opaque compound chat_id.
        //
        // For from-me messages, also include the destination_caller_id (the
        // other participant) so the resulting ChatRef keys like an existing
        // local one-to-one chat and the message merges into it rather than
        // creating a self-only chat.
        let participants: Vec<String> = if sender.is_empty() {
            // If sender is also empty, use a placeholder to keep the
            // participant list non-empty. This should be extremely rare.
            vec![format!("unknown-{}", service)]
        } else {
            let mut p = vec![sender.to_string()];
            if from_me && !destination_caller_id.is_empty() {
                p.push(destination_caller_id.to_string());
            }
            p
        };
        ChatRef {
            participants,
            display_name: None,
            service: Some(service.to_string()),
        }
    }
}

/// Summary of processing a single sync page.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProcessPageResult {
    /// Number of records processed (messages + tapbacks; deletions don't count).
    pub count: usize,
    /// True if any message in the page had a `date` strictly less than
    /// `cutoff_ms` (i.e., older than the cutoff). The sync loop uses this
    /// to stop paginating on first-launch (48-hour cap).
    pub any_older_than_cutoff: bool,
}

/// Process one page of CloudKit sync results: translate each `CloudMessage`
/// to an `Ingest` and apply it to the store. Deletions (`None` values) are
/// ignored for now — a follow-up unit will add proper deletion handling.
///
/// `cutoff_ms` is the unix-ms timestamp below which a message is considered
/// "older than the cutoff" (used to stop paginating on first launch). Set
/// to `i64::MIN` to disable the cap check (e.g., for subsequent syncs).
pub async fn process_sync_page(
    page: std::collections::HashMap<String, rustpush::cloud_messages::CloudMessage>,
    my_handles: &[String],
    chat_map: &std::collections::HashMap<String, rustpush::cloud_messages::CloudChat>,
    store: &crate::store::Store,
    cutoff_ms: i64,
) -> ProcessPageResult {
    let mut result = ProcessPageResult::default();

    for (_guid, cm) in page {
        let ingest = cloud_message_to_ingest(cm, my_handles, chat_map);

        // Extract the date for the cap check BEFORE applying (we need it
        // even if apply fails, to decide whether to stop paginating).
        let date: Option<i64> = match &ingest {
            Ingest::Message(m) => Some(m.date),
            Ingest::Tapback(t) => Some(t.date),
            Ingest::LinkPreview(_) | Ingest::Receipt(_) | Ingest::SendFailed { .. }
            | Ingest::Edited { .. } | Ingest::Ignored(_) => None,
        };

        // Ignored records don't count.
        if matches!(ingest, Ingest::Ignored(_)) {
            continue;
        }

        if let Err(e) = store.apply(ingest).await {
            log::warn!("store.apply failed during sync: {e}");
            // Still count it as processed and check the cap — we don't
            // want to retry indefinitely on a persistent error.
        }

        result.count += 1;
        if let Some(d) = date {
            if d < cutoff_ms {
                result.any_older_than_cutoff = true;
            }
        }
    }

    result
}

/// Abstract "fetch one CloudKit sync page". The real implementation wraps
/// `CloudMessagesClient::sync_messages`; tests use a mock that returns
/// predetermined pages.
#[async_trait]
pub trait CloudKitSync: Send + Sync {
    /// Fetch the next page of CloudKit sync results.
    /// Returns `(next_continuation_token, page_of_records, status)`.
    /// Status semantics (from CloudKit proto):
    ///   - 0, 1, 2: more changes available; pass `next_continuation_token` back
    ///   - 3: no more changes; sync is complete
    async fn fetch_sync_page(
        &self,
        continuation_token: Option<Vec<u8>>,
    ) -> Result<(Vec<u8>, std::collections::HashMap<String, Option<rustpush::cloud_messages::CloudMessage>>, i32), PushError>;
}

/// Summary of a single `sync_once` session.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncResult {
    /// Number of pages fetched and processed.
    pub pages_processed: usize,
    /// Total messages processed across all pages.
    pub messages_processed: usize,
    /// True if the loop stopped because a message was older than the cutoff.
    pub cap_hit: bool,
    /// True if the loop stopped because the server signaled done (status == 3).
    pub done: bool,
    /// The final continuation token (the last token returned by the server,
    /// or `None` if the loop stopped on the first page).
    pub final_token: Option<Vec<u8>>,
}

/// Run one CloudKit sync session: paginate via `syncer`, process each page
/// via `process_sync_page`, stop on cap-hit or done. `cutoff_ms` is the
/// unix-ms timestamp below which a message is considered "older than the
/// cutoff" (e.g., `now - 48h` for the first-launch cap).
pub async fn sync_once(
    syncer: &dyn CloudKitSync,
    store: &crate::store::Store,
    my_handles: &[String],
    chat_map: &std::collections::HashMap<String, rustpush::cloud_messages::CloudChat>,
    cutoff_ms: i64,
) -> SyncResult {
    let mut token: Option<Vec<u8>> = None;
    let mut result = SyncResult::default();

    loop {
        let (new_token, page, status) = match syncer.fetch_sync_page(token).await {
            Ok(t) => t,
            Err(e) => {
                log::error!("sync_once: fetch error: {e:?}");
                break;
            }
        };

        result.pages_processed += 1;

        // Filter out `None` (CloudKit deletion tombstones) for now — the
        // page-processing path doesn't handle them yet. A follow-up unit
        // will add proper deletion handling.
        let page_only_some: HashMap<String, CloudMessage> = page
            .into_iter()
            .filter_map(|(k, v)| v.map(|cm| (k, cm)))
            .collect();

        let page_result = process_sync_page(
            page_only_some,
            my_handles,
            chat_map,
            store,
            cutoff_ms,
        )
        .await;
        result.messages_processed += page_result.count;

        if page_result.any_older_than_cutoff {
            result.cap_hit = true;
            break;
        }

        if status == 3 {
            result.done = true;
            // Capture the new_token as the final continuation token.
            // We do this here rather than relying on `token` being set at the
            // end because `token` was moved into fetch_sync_page and won't
            // be re-assigned until after the break.
            result.final_token = Some(new_token);
            break;
        }

        token = Some(new_token);
    }

    result
}

#[async_trait]
impl<P: rustpush::AnisetteProvider + Send + Sync> CloudKitSync for rustpush::cloud_messages::CloudMessagesClient<P> {
    async fn fetch_sync_page(
        &self,
        continuation_token: Option<Vec<u8>>,
    ) -> Result<(Vec<u8>, std::collections::HashMap<String, Option<rustpush::cloud_messages::CloudMessage>>, i32), PushError> {
        self.sync_messages(continuation_token).await
    }
}

/// Filename used for the last-alive timestamp file.
const LAST_ALIVE_FILENAME: &str = "last_alive";

/// Write the "last alive" timestamp to `<state_dir>/last_alive`.
/// The timestamp is the unix epoch in milliseconds. Called periodically
/// while the app is running and on graceful shutdown. A simple text file
/// (the unix ms as a string) is used for debuggability — no plist.
pub fn write_last_alive(state_dir: &Path, unix_ms: i64) -> io::Result<()> {
    let path = state_dir.join(LAST_ALIVE_FILENAME);
    fs::write(&path, format!("{unix_ms}\n"))
}

/// Read the "last alive" timestamp from `<state_dir>/last_alive`.
/// Returns `None` if the file doesn't exist (first launch) or can't be
/// parsed as an i64.
pub fn read_last_alive(state_dir: &Path) -> Option<i64> {
    let path = state_dir.join(LAST_ALIVE_FILENAME);
    let contents = fs::read_to_string(&path).ok()?;
    contents.trim().parse::<i64>().ok()
}

/// Decide whether to run a CloudKit sync based on the gap since the last
/// alive timestamp. Returns `true` if `last_alive_ms` is `None` (first
/// launch) or `now_ms - last_alive_ms > threshold_ms`.
pub fn should_sync(last_alive_ms: Option<i64>, now_ms: i64, threshold_ms: i64) -> bool {
    match last_alive_ms {
        None => true,
        Some(last) => now_ms.saturating_sub(last) > threshold_ms,
    }
}

// ---------------------------------------------------------------------------
// Cloud sync config & backoff
// ---------------------------------------------------------------------------

/// File name for the bubbles config (lives in `<state_dir>/config`).
pub const CONFIG_FILENAME: &str = "config";

/// File name for the last sync error timestamp (lives in
/// `<state_dir>/last_sync_error`). Contains a single integer: unix seconds
/// of the last failed sync.
pub const LAST_SYNC_ERROR_FILENAME: &str = "last_sync_error";

/// Default backoff period: 24 hours in seconds.
pub const DEFAULT_BACKOFF_SECS: u64 = 24 * 60 * 60;

/// Simple `key=value` config file at `<state_dir>/config`.
/// Currently has one setting: `cloud_sync_enabled`.
/// Default is `true` (sync enabled) if the file is missing or the value
/// is missing/invalid.
pub struct BubblesConfig {
    pub cloud_sync_enabled: bool,
}

impl BubblesConfig {
    pub fn default() -> Self {
        Self {
            // Default to false: iCloud Keychain requires joining the user's
            // encryption trust group (the "clique") before CloudKit sync can
            // succeed, and bubbles doesn't yet expose the setup flow. Enabling
            // it by default would surface a `NotInClique` error on every
            // sync attempt. Users opt in via the settings switch once the
            // setup flow lands.
            cloud_sync_enabled: false,
        }
    }
}

/// Read the config from `path`. Returns `BubblesConfig::default()` if the
/// file doesn't exist or can't be parsed. Malformed lines are skipped;
/// the last valid assignment for a key wins.
pub fn read_config(path: &Path) -> BubblesConfig {
    let Ok(contents) = fs::read_to_string(path) else {
        return BubblesConfig::default();
    };
    let mut config = BubblesConfig::default();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            if key == "cloud_sync_enabled" {
                config.cloud_sync_enabled = value == "true" || value == "1";
            }
            // Other keys are ignored (forward-compat).
        }
        // Lines without '=' are skipped (malformed).
    }
    config
}

/// Write the config to `path`. Overwrites existing file.
pub fn write_config(path: &Path, config: &BubblesConfig) -> io::Result<()> {
    let contents = format!("cloud_sync_enabled={}\n", config.cloud_sync_enabled);
    fs::write(path, contents)
}

/// Returns `true` if the sync should run now, `false` if it's in backoff.
pub fn should_sync_now(
    last_error_secs: Option<i64>,
    now_secs: i64,
    backoff_secs: u64,
) -> bool {
    match last_error_secs {
        None => true,
        Some(t) => {
            if now_secs < t {
                // Clock skew — sync now to be safe.
                true
            } else {
                now_secs.saturating_sub(t) >= backoff_secs as i64
            }
        }
    }
}

/// Read the last sync error timestamp (unix seconds) from `path`. Returns
/// `None` if the file doesn't exist or can't be parsed.
pub fn read_last_sync_error(path: &Path) -> Option<i64> {
    let contents = fs::read_to_string(path).ok()?;
    contents.trim().parse::<i64>().ok()
}

/// Write the last sync error timestamp to `path`. Overwrites existing file.
pub fn write_last_sync_error(path: &Path, unix_secs: i64) -> io::Result<()> {
    fs::write(path, unix_secs.to_string())
}

/// Clear the last sync error file at `path` (e.g., on successful sync).
/// Idempotent: missing file is not an error.
pub fn clear_last_sync_error(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Return a finite recent-history cutoff of approximately 48 hours before
/// `now_ms` (unix epoch milliseconds).
///
/// Manual "Sync Now" should use this rather than `i64::MIN` so that Cloud
/// Messages pagination stops once it reaches messages older than ~48 h.
/// Saturates to `i64::MIN + 1` so the sentinel value is never returned
/// (the caller uses `i64::MIN` as "no cutoff").
pub fn manual_sync_cutoff_ms(now_ms: i64) -> i64 {
    const FORTY_EIGHT_HOURS_MS: i64 = 48 * 60 * 60 * 1000;
    now_ms.saturating_sub(FORTY_EIGHT_HOURS_MS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};
    use rustpush::cloud_messages::{
        CloudMessage, GZipWrapper, MessageFlags,
        cloudmessagesp::MessageProto,
    };
    use crate::store::{Ingest, IncomingMessage, Tapback, ChatRef, Store};

    // ---------------------------------------------------------------------------
    // cloud_sync_chat_metadata — pin: CloudKit-synced messages must not create
    // chat participants/list labels from opaque or compound
    // `CloudMessage.chat_id` values such as `iMessage;-;+12345`; when CloudKit
    // chat metadata is available, `chat_ref_for` (via `cloud_message_to_ingest`)
    // must use `CloudChat.participants` and `display_name` from the chat map
    // so the resulting `ChatRef` lists real `tel:`/`mailto:` URIs (not the raw
    // compound chat_id) and the UI chat list merges with live-push chat keys.
    // When no metadata is available, the raw compound chat_id must still NOT
    // become a participant (it gets split on `;` in the UI and produces fake
    // labels like "imessage, -, +12345").
    // ---------------------------------------------------------------------------

    #[test]
    fn cloud_sync_chat_metadata_uses_cloud_chat_not_raw_chat_id() {
        use rustpush::cloud_messages::CloudParticipant;

        // Compound opaque chat_id like Apple uses in CloudKit CloudMessage.
        // Real-world values look like "iMessage;-;+1234567890" — the separators
        // would render as fake participants if the raw value were used.
        let chat_id = "iMessage;-;+12345";

        // Synthetic CloudChat with realistic participants and a group name.
        let cloud_chat = CloudChat {
            participants: vec![
                CloudParticipant { uri: "tel:+12345".to_string() },
                CloudParticipant { uri: "mailto:alice@example.com".to_string() },
            ],
            display_name: Some("Weekend Crew".to_string()),
            ..Default::default()
        };

        let mut chat_map: HashMap<String, CloudChat> = HashMap::new();
        chat_map.insert(chat_id.to_string(), cloud_chat);

        // -------------------------------------------------------------------
        // GOOD PATH: chat_map has the CloudChat for this chat_id.
        // `chat_ref_for` must return a ChatRef built from CloudChat metadata,
        // not from the raw compound `chat_id`.
        // -------------------------------------------------------------------
        let cm_good = CloudMessage {
            utm: None,
            r#type: 0,
            error: 0,
            chat_id: chat_id.to_string(),
            sender: "alice@example.com".to_string(),
            time: 0,
            msg_proto_2: None,
            destination_caller_id: String::new(),
            msg_proto: GZipWrapper(MessageProto {
                text: Some("hi from cloud".to_string()),
                ..Default::default()
            }),
            flags: MessageFlags::IS_FINISHED,
            guid: "ck-msg-good".to_string(),
            msg_proto_3: None,
            service: "iMessage".to_string(),
            msg_proto_4: None,
        };

        let result = cloud_message_to_ingest(
            cm_good,
            &["me@example.com".to_string()],
            &chat_map,
        );

        match result {
            Ingest::Message(msg) => {
                // Participants must come from CloudChat URIs, NOT the raw chat_id.
                assert!(
                    !msg.chat.participants.iter().any(|p| p == chat_id),
                    "GOOD path: ChatRef.participants must not contain the raw \
                     compound chat_id '{}', got: {:?}",
                    chat_id, msg.chat.participants,
                );
                assert!(
                    msg.chat.participants.iter().any(|p| p == "tel:+12345"),
                    "GOOD path: ChatRef.participants must contain 'tel:+12345' \
                     from CloudChat metadata, got: {:?}",
                    msg.chat.participants,
                );
                assert!(
                    msg.chat.participants.iter().any(|p| p == "mailto:alice@example.com"),
                    "GOOD path: ChatRef.participants must contain \
                     'mailto:alice@example.com' from CloudChat metadata, got: {:?}",
                    msg.chat.participants,
                );
                // Display name is preserved from the CloudChat.
                assert_eq!(
                    msg.chat.display_name,
                    Some("Weekend Crew".to_string()),
                    "GOOD path: ChatRef.display_name must be preserved from \
                     CloudChat metadata",
                );

                // Deduplication invariant: the resulting ChatRef's key must
                // match a live-push ChatRef built with the same participant
                // URIs. This pins that CloudKit-synced messages merge into
                // the same chat row as live-push messages.
                let live_push_chat_ref = ChatRef {
                    participants: vec![
                        "tel:+12345".to_string(),
                        "mailto:alice@example.com".to_string(),
                    ],
                    display_name: None,
                    service: Some("iMessage".to_string()),
                };
                assert_eq!(
                    msg.chat.key(),
                    live_push_chat_ref.key(),
                    "GOOD path: CloudKit ChatRef.key() must equal live-push \
                     ChatRef.key() for the same participants (dedup invariant)",
                );
            }
            other => panic!("GOOD path: expected Ingest::Message, got {:?}", other),
        }

        // -------------------------------------------------------------------
        // FALLBACK: chat_map does NOT have the CloudChat for this chat_id
        // (this is the path currently taken by `sync_missed_messages`, which
        // passes an empty `chat_map`). The raw compound chat_id must still
        // NOT become a participant — it gets split on `;` in the UI and
        // produces fake labels like "imessage, -, +12345".
        // -------------------------------------------------------------------
        let empty_chat_map: HashMap<String, CloudChat> = HashMap::new();

        let cm_fallback = CloudMessage {
            utm: None,
            r#type: 0,
            error: 0,
            chat_id: chat_id.to_string(),
            sender: "stranger@example.com".to_string(),
            time: 0,
            msg_proto_2: None,
            destination_caller_id: String::new(),
            msg_proto: GZipWrapper(MessageProto {
                text: Some("hi stranger".to_string()),
                ..Default::default()
            }),
            flags: MessageFlags::IS_FINISHED,
            guid: "ck-msg-fallback".to_string(),
            msg_proto_3: None,
            service: "iMessage".to_string(),
            msg_proto_4: None,
        };

        let result_fb = cloud_message_to_ingest(
            cm_fallback,
            &["me@example.com".to_string()],
            &empty_chat_map,
        );

        match result_fb {
            Ingest::Message(msg) => {
                assert!(
                    !msg.chat.participants.iter().any(|p| p == chat_id),
                    "FALLBACK path: ChatRef.participants must not contain the raw \
                     compound chat_id '{}' (it would be split on ';' and produce \
                     fake labels in the UI), got: {:?}",
                    chat_id, msg.chat.participants,
                );
                assert!(
                    !msg.chat.participants.is_empty(),
                    "FALLBACK path: ChatRef.participants must not be empty \
                     (downstream code assumes a non-empty participant list)",
                );
            }
            other => panic!("FALLBACK path: expected Ingest::Message, got {:?}", other),
        }
    }

    // ---------------------------------------------------------------------------
    // cloud_sync_empty_chat_map_merges_matching_existing_chat — pin: when
    // CloudKit sync hands us a `CloudMessage` whose `chat_id` is absent from
    // `chat_map` (so the metadata-backed path can't be used), the fallback
    // `ChatRef` must still identify the *other* party so the message lands in
    // the same local chat that already exists for that participant. This is
    // especially important for from-me messages where `sender` matches one of
    // `my_handles`: a naive fallback that uses `sender` as the sole participant
    // would key the chat by the user's own handle, creating a self-only chat
    // that doesn't match the existing one-to-one thread. The compound chat_id
    // (`iMessage;-;+12345`) must never become a participant (the UI would split
    // it on `;` and produce fake labels), but the destination must be resolved
    // from the other CloudKit fields (e.g. `destination_caller_id`).
    //
    // Also pins the inverse: a CloudKit message for a *different* participant
    // must create its own chat rather than being merged into the existing one.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn cloud_sync_empty_chat_map_merges_matching_existing_chat() {
        let store = Store::open_in_memory().await.unwrap();

        let my_handle = "mailto:me@icloud.com".to_string();
        let other_handle = "tel:+12345".to_string();
        let my_handles = vec![my_handle.clone()];

        // --- 1. Seed an existing local one-to-one chat with the other party. ---
        let existing_chat = ChatRef {
            participants: vec![other_handle.clone(), my_handle.clone()],
            display_name: None,
            service: Some("iMessage".to_string()),
        };
        let seed_date_ms: i64 = 1_700_000_000_000;
        store
            .apply(Ingest::Message(IncomingMessage {
                guid: "local-seed-1".to_string(),
                chat: existing_chat.clone(),
                sender: Some(other_handle.clone()),
                is_from_me: false,
                text: Some("hello from local".to_string()),
                date: seed_date_ms,
                ..Default::default()
            }))
            .await
            .unwrap();

        let chats_before = store.chats().await.unwrap();
        assert_eq!(
            chats_before.len(),
            1,
            "seed: one local chat exists before CloudKit sync, got: {:?}",
            chats_before
                .iter()
                .map(|c| (&c.key, &c.participants))
                .collect::<Vec<_>>()
        );
        let seeded_chat_id = chats_before[0].id;
        let seeded_chat_key = chats_before[0].key.clone();

        // --- 2. Build a from-me CloudKit message for the SAME participant. ---
        //
        // The chat_id is the opaque compound value Apple actually uses
        // ("iMessage;-;+12345"); chat_map is empty so the metadata-backed
        // path can't resolve the participants and the fallback is exercised.
        // `sender == my_handle` (matches my_handles) marks this as from-me,
        // and `destination_caller_id` is the other party's normalized handle.
        let cloud_time_ns: i64 = 100_000_000_000_000_000;
        let expected_date_ms = apple_time_to_unix_ms(cloud_time_ns);

        let cm_merge = CloudMessage {
            utm: None,
            r#type: 0,
            error: 0,
            chat_id: "iMessage;-;+12345".to_string(),
            sender: my_handle.clone(),
            time: cloud_time_ns,
            msg_proto_2: None,
            destination_caller_id: other_handle.clone(),
            msg_proto: GZipWrapper(MessageProto {
                text: Some("from me to you, synced from cloud".to_string()),
                ..Default::default()
            }),
            flags: MessageFlags::IS_FINISHED | MessageFlags::IS_FROM_ME,
            guid: "ck-from-me-merge".to_string(),
            msg_proto_3: None,
            service: "iMessage".to_string(),
            msg_proto_4: None,
        };

        let empty_chat_map: HashMap<String, rustpush::cloud_messages::CloudChat> =
            HashMap::new();

        // --- 3. Convert to Ingest and assert the ChatRef identifies the other
        //        party (so the merge can happen). ---
        let ingest_merge = cloud_message_to_ingest(cm_merge, &my_handles, &empty_chat_map);
        match &ingest_merge {
            Ingest::Message(m) => {
                assert!(
                    m.chat
                        .participants
                        .iter()
                        .any(|p| p == "tel:+12345"),
                    "fallback ChatRef.participants must include the destination \
                     'tel:+12345' so the existing local chat merges; got: {:?}",
                    m.chat.participants,
                );
                assert!(
                    m.chat.participants.iter().any(|p| p == "mailto:me@icloud.com"),
                    "fallback ChatRef.participants must include 'mailto:me@icloud.com' \
                     (the from-me sender / my_handle) so the chat is a real 1:1, \
                     not a self-only chat with an unknown extra participant; \
                     got: {:?}",
                    m.chat.participants,
                );
                assert!(
                    !m.chat.participants.iter().any(|p| p.contains(';')),
                    "fallback ChatRef.participants must NOT contain the raw \
                     compound chat_id (would be split on ';' into fake labels \
                     in the UI); got: {:?}",
                    m.chat.participants,
                );
                // Dedup invariant: the resulting ChatRef's key must match the
                // existing local chat's key (same participants -> same chat).
                assert_eq!(
                    m.chat.key(),
                    seeded_chat_key,
                    "fallback ChatRef.key() must equal the existing local chat's \
                     key for the merged from-me CloudKit message; got {}, want {}",
                    m.chat.key(),
                    seeded_chat_key,
                );
            }
            other => panic!(
                "expected Ingest::Message for the from-me CloudKit message, got {:?}",
                other
            ),
        }
        store.apply(ingest_merge).await.unwrap();

        // --- 4. Verify the chat is preserved (no self-only chat was created)
        //        and the message landed in the existing chat. ---
        let chats_after = store.chats().await.unwrap();
        assert_eq!(
            chats_after.len(),
            1,
            "CloudKit message for an existing participant must merge into the \
             existing chat, not create a self-only chat; got chats: {:?}",
            chats_after
                .iter()
                .map(|c| (&c.key, &c.participants))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            chats_after[0].id, seeded_chat_id,
            "chat id must be preserved (the original chat, not a new one)"
        );
        assert_eq!(
            chats_after[0].key, seeded_chat_key,
            "chat key must be preserved (no self-only re-key)"
        );
        // `bump_chat_date` uses max semantics: the chat's `last_message_date`
        // must reflect the later of (seed, cloud) so an older synced message
        // does not move the chat backward in the list.
        assert_eq!(
            chats_after[0].last_message_date,
            Some(seed_date_ms.max(expected_date_ms)),
            "chat last_message_date must be the max of the seed and CloudKit \
             times (bump_chat_date uses max semantics so an older synced \
             message does not move the chat backward); got {:?}, want Some({})",
            chats_after[0].last_message_date,
            seed_date_ms.max(expected_date_ms),
        );

        let msgs = store.messages_page(seeded_chat_id, None, 100).await.unwrap();
        let synced = msgs
            .iter()
            .find(|m| m.guid == "ck-from-me-merge")
            .unwrap_or_else(|| {
                panic!(
                    "synced CloudKit message must land in the existing chat; \
                     got guids: {:?}",
                    msgs.iter().map(|m| &m.guid).collect::<Vec<_>>()
                )
            });
        assert_eq!(
            synced.date, expected_date_ms,
            "synced message date must reflect the CloudKit time"
        );
        assert!(
            synced.is_from_me,
            "synced from-me message must remain is_from_me"
        );
        assert_eq!(synced.sender.as_deref(), Some(my_handle.as_str()));

        // --- 5. INVERSE: a CloudKit message for a DIFFERENT participant must
        //        create its own chat rather than being merged into the existing
        //        one. ---
        let cm_diff = CloudMessage {
            utm: None,
            r#type: 0,
            error: 0,
            chat_id: "iMessage;-;+99999".to_string(),
            sender: "mailto:alice@example.com".to_string(),
            time: 200_000_000_000_000_000,
            msg_proto_2: None,
            destination_caller_id: String::new(),
            msg_proto: GZipWrapper(MessageProto {
                text: Some("hello from alice".to_string()),
                ..Default::default()
            }),
            flags: MessageFlags::IS_FINISHED,
            guid: "ck-different-participant".to_string(),
            msg_proto_3: None,
            service: "iMessage".to_string(),
            msg_proto_4: None,
        };
        let ingest_diff = cloud_message_to_ingest(cm_diff, &my_handles, &empty_chat_map);
        store.apply(ingest_diff).await.unwrap();

        let chats_final = store.chats().await.unwrap();
        assert_eq!(
            chats_final.len(),
            2,
            "CloudKit message for a different participant must create its own \
             chat (not merge into the existing one); got chats: {:?}",
            chats_final
                .iter()
                .map(|c| (&c.key, &c.participants))
                .collect::<Vec<_>>()
        );

        // The original chat is still there with its original key.
        let original_still = chats_final
            .iter()
            .find(|c| c.id == seeded_chat_id)
            .expect("original chat must still exist after a different-participant CloudKit message");
        assert_eq!(original_still.key, seeded_chat_key);

        // The new chat has alice as a participant and the alice message in it.
        let new_chats: Vec<_> = chats_final
            .iter()
            .filter(|c| c.id != seeded_chat_id)
            .collect();
        assert_eq!(new_chats.len(), 1, "exactly one new chat should exist");
        let alice_chat = new_chats[0];
        assert!(
            alice_chat
                .participants
                .iter()
                .any(|p| p.contains("alice")),
            "new chat must have alice as a participant; got: {:?}",
            alice_chat.participants
        );
        let alice_msgs = store.messages_page(alice_chat.id, None, 100).await.unwrap();
        let alice_synced = alice_msgs
            .iter()
            .find(|m| m.guid == "ck-different-participant")
            .unwrap_or_else(|| {
                panic!(
                    "second CloudKit message must be in the new chat, not the \
                     original; got guids: {:?}",
                    alice_msgs.iter().map(|m| &m.guid).collect::<Vec<_>>()
                )
            });
        assert_eq!(alice_synced.text.as_deref(), Some("hello from alice"));
    }

    // 1. Apple epoch conversion
    #[test]
    fn test_apple_epoch_conversion() {
        assert_eq!(apple_time_to_unix_ms(0), 978_307_200_000);
        assert_eq!(
            apple_time_to_unix_ms(1_000_000_000),
            978_307_201_000,
        );
    }

    // 2. Basic message translation
    #[test]
    fn test_basic_message_translation() {
        let cm = CloudMessage {
            guid: "test-guid-1".into(),
            sender: "friend@example.com".into(),
            chat_id: "chat-abc".into(),
            service: "iMessage".into(),
            time: 0,
            msg_proto: GZipWrapper(MessageProto {
                text: Some("hello world".into()),
                ..Default::default()
            }),
            flags: MessageFlags::IS_FINISHED,
            ..Default::default()
        };

        let result = cloud_message_to_ingest(
            cm,
            &["me@example.com".to_string()],
            &HashMap::new(),
        );

        match result {
            Ingest::Message(msg) => {
                assert_eq!(msg.guid, "test-guid-1");
                assert_eq!(
                    msg.sender,
                    Some("friend@example.com".to_string()),
                );
                assert!(!msg.is_from_me);
                assert_eq!(
                    msg.text,
                    Some("hello world".to_string()),
                );
                assert_eq!(msg.service, Some("iMessage".to_string()));
                assert_eq!(msg.date, 978_307_200_000);
                assert!(!msg.chat.participants.is_empty());
                assert_eq!(msg.reply_to_guid, None);
            }
            other => {
                panic!(
                    "expected Ingest::Message, got {:?}",
                    other
                )
            }
        }
    }

    // 3. is_from_me via my_handles match
    #[test]
    fn test_is_from_me_via_handles() {
        let cm = CloudMessage {
            guid: "test-guid-2".into(),
            sender: "me@example.com".into(),
            chat_id: "chat-abc".into(),
            service: "iMessage".into(),
            time: 0,
            msg_proto: GZipWrapper(MessageProto {
                text: Some("hello world".into()),
                ..Default::default()
            }),
            flags: MessageFlags::IS_FINISHED,
            ..Default::default()
        };

        let result = cloud_message_to_ingest(
            cm,
            &["me@example.com".to_string()],
            &HashMap::new(),
        );

        match result {
            Ingest::Message(msg) => {
                assert!(
                    msg.is_from_me,
                    "sender matches my_handles => is_from_me should be true",
                );
            }
            other => {
                panic!(
                    "expected Ingest::Message, got {:?}",
                    other
                )
            }
        }
    }

    // 4. is_from_me via IS_FROM_ME flag
    #[test]
    fn test_is_from_me_via_flag() {
        let cm = CloudMessage {
            guid: "test-guid-3".into(),
            sender: "anyone@example.com".into(),
            chat_id: "chat-abc".into(),
            service: "iMessage".into(),
            time: 0,
            msg_proto: GZipWrapper(MessageProto {
                text: Some("hello world".into()),
                ..Default::default()
            }),
            flags: MessageFlags::IS_FINISHED | MessageFlags::IS_FROM_ME,
            ..Default::default()
        };

        let result = cloud_message_to_ingest(
            cm,
            &["me@example.com".to_string()],
            &HashMap::new(),
        );

        match result {
            Ingest::Message(msg) => {
                assert!(
                    msg.is_from_me,
                    "IS_FROM_ME flag set => is_from_me should be true",
                );
            }
            other => {
                panic!(
                    "expected Ingest::Message, got {:?}",
                    other
                )
            }
        }
    }

    // 5. Tapback detection
    #[test]
    fn test_tapback_detection() {
        let cm = CloudMessage {
            guid: "tapback-guid".into(),
            sender: "friend@example.com".into(),
            chat_id: "chat-abc".into(),
            service: "iMessage".into(),
            time: 0,
            msg_proto: GZipWrapper(MessageProto {
                text: None,
                associated_message_type: Some(2002),
                associated_message_guid: Some("target-guid".into()),
                ..Default::default()
            }),
            flags: MessageFlags::IS_FINISHED,
            ..Default::default()
        };

        let result = cloud_message_to_ingest(
            cm,
            &["me@example.com".to_string()],
            &HashMap::new(),
        );

        match result {
            Ingest::Tapback(tb) => {
                assert_eq!(tb.associated_guid, "target-guid");
                assert_eq!(tb.associated_type, 2002);
                assert_eq!(tb.guid, "tapback-guid");
            }
            other => {
                panic!(
                    "expected Ingest::Tapback, got {:?}",
                    other
                )
            }
        }
    }

    // 6. Non-tapback, non-message record => ignored
    #[test]
    fn test_ignored_record() {
        let cm = CloudMessage {
            guid: "ignored-guid".into(),
            sender: "friend@example.com".into(),
            chat_id: "chat-abc".into(),
            service: "iMessage".into(),
            time: 0,
            msg_proto: GZipWrapper(MessageProto {
                text: None,
                associated_message_type: None,
                associated_message_guid: None,
                ..Default::default()
            }),
            flags: MessageFlags::IS_FINISHED,
            ..Default::default()
        };

        let result = cloud_message_to_ingest(
            cm,
            &["me@example.com".to_string()],
            &HashMap::new(),
        );

        match result {
            Ingest::Ignored(_) => { /* expected */ }
            other => {
                panic!(
                    "expected Ingest::Ignored, got {:?}",
                    other
                )
            }
        }
    }

    // ---------------------------------------------------------------------------
    // process_sync_page tests  (unchanged — waiting for the production function)
    // ---------------------------------------------------------------------------

    fn make_cm(guid: &str, sender: &str, text: &str, time_ns: i64) -> CloudMessage {
        CloudMessage {
            utm: None,
            r#type: 0,
            error: 0,
            chat_id: format!("chat-{guid}"),
            sender: sender.to_string(),
            time: time_ns,
            msg_proto_2: None,
            destination_caller_id: String::new(),
            msg_proto: GZipWrapper(MessageProto {
                text: Some(text.to_string()),
                ..Default::default()
            }),
            flags: MessageFlags::IS_FINISHED,
            guid: guid.to_string(),
            msg_proto_3: None,
            service: "iMessage".to_string(),
            msg_proto_4: None,
        }
    }

    fn now_apple_ns() -> i64 {
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        (unix_ms - 978_307_200_000) * 1_000_000
    }

    #[tokio::test]
    async fn process_sync_page_all_recent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("db.sqlite")).await.unwrap();

        let now = now_apple_ns();
        let one_hour_ns = 3_600_000_000_000i64;

        let mut page: HashMap<String, CloudMessage> = HashMap::new();
        page.insert(
            "guid-a".into(),
            make_cm("guid-a", "alice@example.com", "Hello!", now - one_hour_ns),
        );
        page.insert(
            "guid-b".into(),
            make_cm("guid-b", "bob@example.com", "Hi there!", now - one_hour_ns),
        );

        let now_unix_ms = apple_time_to_unix_ms(now);
        let cutoff_ms = now_unix_ms - 48 * 3600 * 1000;

        let result = process_sync_page(
            page,
            &["me@example.com".to_string()],
            &HashMap::new(),
            &store,
            cutoff_ms,
        )
        .await;

        assert_eq!(
            result,
            ProcessPageResult {
                count: 2,
                any_older_than_cutoff: false,
            }
        );
    }

    #[tokio::test]
    async fn process_sync_page_some_older_than_cutoff() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("db.sqlite")).await.unwrap();

        let now = now_apple_ns();
        let one_hour_ns = 3_600_000_000_000i64;
        let one_day_ns = 24 * one_hour_ns;

        let mut page: HashMap<String, CloudMessage> = HashMap::new();
        page.insert(
            "guid-a".into(),
            make_cm("guid-a", "alice@example.com", "Recent", now - one_hour_ns),
        );
        page.insert(
            "guid-b".into(),
            make_cm("guid-b", "bob@example.com", "A day old", now - one_day_ns),
        );
        page.insert(
            "guid-c".into(),
            make_cm(
                "guid-c",
                "charlie@example.com",
                "Three days old",
                now - 3 * one_day_ns,
            ),
        );

        let now_unix_ms = apple_time_to_unix_ms(now);
        let cutoff_ms = now_unix_ms - 48 * 3600 * 1000;

        let result = process_sync_page(
            page,
            &["me@example.com".to_string()],
            &HashMap::new(),
            &store,
            cutoff_ms,
        )
        .await;

        assert_eq!(
            result,
            ProcessPageResult {
                count: 3,
                any_older_than_cutoff: true,
            }
        );
    }

    #[tokio::test]
    async fn process_sync_page_empty_page() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("db.sqlite")).await.unwrap();

        let page: HashMap<String, CloudMessage> = HashMap::new();
        let cutoff_ms = 0;

        let result = process_sync_page(
            page,
            &["me@example.com".to_string()],
            &HashMap::new(),
            &store,
            cutoff_ms,
        )
        .await;

        assert_eq!(
            result,
            ProcessPageResult {
                count: 0,
                any_older_than_cutoff: false,
            }
        );
    }

    // ---------------------------------------------------------------------------
    // sync_once tests  (mock-based; the trait/struct/function don't exist yet)
    // ---------------------------------------------------------------------------

    use std::collections::VecDeque;
    use async_trait::async_trait;
    use tokio::sync::Mutex;

    struct MockSyncer {
        pages: Mutex<VecDeque<(
            Vec<u8>,
            HashMap<String, Option<CloudMessage>>,
            i32,
        )>>,
    }

    impl MockSyncer {
        fn new(pages: Vec<(
            Vec<u8>,
            HashMap<String, Option<CloudMessage>>,
            i32,
        )>) -> Self {
            Self { pages: Mutex::new(pages.into()) }
        }
    }

    #[async_trait]
    impl super::CloudKitSync for MockSyncer {
        async fn fetch_sync_page(
            &self,
            _continuation_token: Option<Vec<u8>>,
        ) -> Result<(
            Vec<u8>,
            HashMap<String, Option<CloudMessage>>,
            i32,
        ), rustpush::PushError> {
            self.pages
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| {
                    rustpush::PushError::ResourcePanic(
                        "mock syncer: no more pages".to_string(),
                    )
                })
        }
    }

    #[tokio::test]
    async fn sync_once_cap_hit_stops_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("db.sqlite")).await.unwrap();

        let now = now_apple_ns();
        let one_hour_ns = 3_600_000_000_000i64;
        let one_day_ns = 24 * one_hour_ns;

        let now_unix_ms = apple_time_to_unix_ms(now);
        let cutoff_ms = now_unix_ms - 48 * 3600 * 1000;

        // 72-hour-old message (older than 48h cutoff)
        let cm = make_cm("old-msg", "alice@example.com", "Old message", now - 3 * one_day_ns);
        let mut page1: HashMap<String, Option<CloudMessage>> = HashMap::new();
        page1.insert("old-msg".into(), Some(cm));

        let pages = vec![(
            b"token-a".to_vec(),
            page1,
            0_i32,
        )];

        let syncer = MockSyncer::new(pages);

        let result = super::sync_once(
            &syncer,
            &store,
            &["me@example.com".to_string()],
            &HashMap::new(),
            cutoff_ms,
        )
        .await;

        assert_eq!(result.pages_processed, 1);
        assert_eq!(result.messages_processed, 1);
        assert!(result.cap_hit, "expected cap_hit == true");
        assert!(!result.done, "expected done == false");
        assert_eq!(result.final_token, None);
    }

    #[tokio::test]
    async fn sync_once_done_stops_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("db.sqlite")).await.unwrap();

        let now = now_apple_ns();
        let one_hour_ns = 3_600_000_000_000i64;

        let now_unix_ms = apple_time_to_unix_ms(now);
        let cutoff_ms = now_unix_ms - 48 * 3600 * 1000;

        // 1-hour-old message (recent — within cutoff)
        let cm = make_cm(
            "recent-msg",
            "bob@example.com",
            "Recent",
            now - one_hour_ns,
        );
        let mut page1: HashMap<String, Option<CloudMessage>> = HashMap::new();
        page1.insert("recent-msg".into(), Some(cm));

        let pages = vec![(
            b"token-after-page-1".to_vec(),
            page1,
            3_i32,
        )];

        let syncer = MockSyncer::new(pages);

        let result = super::sync_once(
            &syncer,
            &store,
            &["me@example.com".to_string()],
            &HashMap::new(),
            cutoff_ms,
        )
        .await;

        assert_eq!(result.pages_processed, 1);
        assert_eq!(result.messages_processed, 1);
        assert!(!result.cap_hit, "expected cap_hit == false");
        assert!(result.done, "expected done == true");
        assert_eq!(
            result.final_token,
            Some(b"token-after-page-1".to_vec())
        );
    }

    #[tokio::test]
    async fn sync_once_multiple_pages_until_done() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("db.sqlite")).await.unwrap();

        let now = now_apple_ns();
        let one_hour_ns = 3_600_000_000_000i64;

        let now_unix_ms = apple_time_to_unix_ms(now);
        let cutoff_ms = now_unix_ms - 48 * 3600 * 1000;

        // Page 1: 1 recent message, status=0 (more changes)
        let cm1 = make_cm("msg-1", "alice@example.com", "First", now - one_hour_ns);
        let mut page1: HashMap<String, Option<CloudMessage>> = HashMap::new();
        page1.insert("msg-1".into(), Some(cm1));

        // Page 2: 1 recent message, status=3 (done)
        let cm2 = make_cm("msg-2", "bob@example.com", "Second", now - one_hour_ns);
        let mut page2: HashMap<String, Option<CloudMessage>> = HashMap::new();
        page2.insert("msg-2".into(), Some(cm2));

        let pages = vec![
            (b"token-1".to_vec(), page1, 0_i32),
            (b"token-2".to_vec(), page2, 3_i32),
        ];

        let syncer = MockSyncer::new(pages);

        let result = super::sync_once(
            &syncer,
            &store,
            &["me@example.com".to_string()],
            &HashMap::new(),
            cutoff_ms,
        )
        .await;

        assert_eq!(result.pages_processed, 2);
        assert_eq!(result.messages_processed, 2);
        assert!(!result.cap_hit, "expected cap_hit == false");
        assert!(result.done, "expected done == true");
        assert_eq!(result.final_token, Some(b"token-2".to_vec()));
    }

    // ---------------------------------------------------------------------------
    // write_last_alive / read_last_alive tests
    // ---------------------------------------------------------------------------

    #[test]
    fn last_alive_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        write_last_alive(tmp.path(), 1_700_000_000_000).unwrap();
        assert_eq!(read_last_alive(tmp.path()), Some(1_700_000_000_000));

        write_last_alive(tmp.path(), 0).unwrap();
        assert_eq!(read_last_alive(tmp.path()), Some(0));
    }

    #[test]
    fn read_last_alive_missing_file_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_last_alive(tmp.path()), None);
        assert_eq!(read_last_alive(&tmp.path().join("nope")), None);
    }

    // ---------------------------------------------------------------------------
    // should_sync tests
    // ---------------------------------------------------------------------------

    #[test]
    fn should_sync_threshold_logic() {
        assert!(should_sync(None, 1_000_000, 7_200_000));
        assert!(!should_sync(Some(1_000_000), 1_001_000, 7_200_000));
        assert!(should_sync(Some(1_000_000), 1_000_000 + 7_200_001, 7_200_000));
        assert!(!should_sync(Some(1_000_000), 1_000_000 + 7_200_000, 7_200_000));
    }

    #[test]
    fn should_sync_zero_threshold() {
        assert!(should_sync(Some(1_000_000), 1_000_001, 0));
    }

    // ---------------------------------------------------------------------------
    // BubblesConfig tests
    // ---------------------------------------------------------------------------

    #[test]
    fn config_default_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CONFIG_FILENAME);
        let config = BubblesConfig::default();
        write_config(&path, &config).unwrap();
        let loaded = read_config(&path);
        assert!(!loaded.cloud_sync_enabled, "default is now opt-in: cloud_sync_enabled is false until the user enables it");
    }

    #[test]
    fn config_explicit_disable() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CONFIG_FILENAME);
        std::fs::write(&path, "cloud_sync_enabled=false\n").unwrap();
        let loaded = read_config(&path);
        assert!(!loaded.cloud_sync_enabled);
    }

    #[test]
    fn config_missing_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CONFIG_FILENAME);
        let loaded = read_config(&path);
        assert_eq!(
            loaded.cloud_sync_enabled,
            BubblesConfig::default().cloud_sync_enabled,
        );
    }

    #[test]
    fn read_config_skips_malformed_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CONFIG_FILENAME);
        std::fs::write(
            &path,
            "cloud_sync_enabled=false\ngarbage line without equals\ncloud_sync_enabled=true\n",
        )
        .unwrap();
        let loaded = read_config(&path);
        assert!(loaded.cloud_sync_enabled);
    }

    // ---------------------------------------------------------------------------
    // should_sync_now tests
    // ---------------------------------------------------------------------------

    #[test]
    fn should_sync_now_no_prior_error() {
        assert!(should_sync_now(None, 100, 86400));
        assert!(should_sync_now(None, 0, 86400));
    }

    #[test]
    fn should_sync_now_within_backoff() {
        // age = 0, 0 < 86400 → false
        assert!(!should_sync_now(Some(100), 100, 86400));
        // age = 50, 50 < 86400 → false
        assert!(!should_sync_now(Some(50), 100, 86400));
        // age = 1, 1 < 86400 → false
        assert!(!should_sync_now(Some(99), 100, 86400));
        // age = 86400, 86400 >= 86400 → true
        assert!(should_sync_now(Some(0), 86400, 86400));
        // age = 86401, 86401 >= 86400 → true
        assert!(should_sync_now(Some(0), 86401, 86400));
    }

    #[test]
    fn should_sync_now_clock_skew() {
        // now_secs < last_error_secs → clock skew → return true
        assert!(should_sync_now(Some(1000), 500, 86400));
    }

    // ---------------------------------------------------------------------------
    // last_sync_error persistence tests
    // ---------------------------------------------------------------------------

    #[test]
    fn last_sync_error_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(LAST_SYNC_ERROR_FILENAME);
        write_last_sync_error(&path, 1234567890).unwrap();
        assert_eq!(read_last_sync_error(&path), Some(1234567890));
    }

    #[test]
    fn last_sync_error_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(LAST_SYNC_ERROR_FILENAME);
        assert_eq!(read_last_sync_error(&path), None);
    }

    #[test]
    fn clear_last_sync_error_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(LAST_SYNC_ERROR_FILENAME);
        std::fs::write(&path, "1234567890\n").unwrap();
        clear_last_sync_error(&path).unwrap();
        assert!(!path.exists());
        // Idempotent: missing file is not an error.
        clear_last_sync_error(&path).unwrap();
    }

    // ---------------------------------------------------------------------------
    // manual_sync_cutoff_ms — pin: manual "Sync Now" / orchestrator callers must
    // use a bounded ~48h recent-history cutoff, not `i64::MIN` (which would
    // disable the cap and trigger full-history backfill on every manual press).
    // ---------------------------------------------------------------------------

    /// 48 hours expressed in milliseconds.
    const FORTY_EIGHT_HOURS_MS: i64 = 48 * 60 * 60 * 1000;

    #[test]
    fn manual_sync_cutoff_ms_is_finite_and_recent() {
        // Arbitrary fixed point in 2023 — supplied explicitly so the test
        // is deterministic and doesn't depend on wall clock / GTK.
        let now_ms: i64 = 1_700_000_000_000;

        let cutoff = manual_sync_cutoff_ms(now_ms);

        // 1. The cutoff must NOT be `i64::MIN`. `i64::MIN` is the sentinel
        //    the UI was previously passing, and it disables the per-page
        //    "any_older_than_cutoff" cap (`if d < cutoff_ms` in
        //    process_sync_page) so CloudKit sync would walk the entire
        //    history on every manual press. Pin that the helper returns a
        //    finite value.
        assert_ne!(
            cutoff, i64::MIN,
            "manual_sync_cutoff_ms must not return i64::MIN (would disable cutoff)",
        );
        assert!(
            cutoff > i64::MIN,
            "manual_sync_cutoff_ms must return a finite i64, got {cutoff}",
        );

        // 2. The cutoff must be approximately 48 hours behind `now_ms`.
        //    Allow ±1 minute of slack so a future 47h59m or 48h01m tweak
        //    doesn't break this pin, but reject the "no cutoff / wildly off"
        //    cases.
        let expected = now_ms - FORTY_EIGHT_HOURS_MS;
        let delta = cutoff - expected; // signed: positive ⇒ cutoff newer than expected
        assert!(
            delta.abs() <= 60_000,
            "manual_sync_cutoff_ms({now_ms}) = {cutoff}; expected ≈ {expected} \
             (now - 48h); delta = {delta} ms, exceeds ±1 min tolerance",
        );

        // 3. The cutoff must be a function of `now_ms` — shifting `now_ms`
        //    forward by 1 hour must shift the cutoff forward by exactly 1
        //    hour. This rules out a hard-coded constant that happens to
        //    fall inside the ±1 min band by coincidence.
        let now_ms_plus_1h = now_ms + 3_600_000;
        let cutoff_plus_1h = manual_sync_cutoff_ms(now_ms_plus_1h);
        assert_eq!(
            cutoff_plus_1h, cutoff + 3_600_000,
            "cutoff must shift 1:1 with now_ms",
        );
    }
}
