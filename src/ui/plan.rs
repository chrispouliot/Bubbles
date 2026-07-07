//! Pure planning/reaction/receipt logic for chat-bubble updates.
//!
//! These types and functions decide **what** to update when the message list
//! changes, without touching GTK or widget state.  They are extracted from
//! `mod.rs` and tested in isolation.

use crate::store::{LiveReactionSummary, StoredMessage};

use super::fmt_time;

// ── chat-update plan types ───────────────────────────────────────────

/// What to do with the chat-bubble container on the next refresh.
///
/// The caller (the GTK refresh path) inspects this and applies the minimal
/// update needed instead of rebuilding the entire bubble list from scratch.
#[derive(Debug)]
pub enum ChatUpdatePlan {
    Noop,
    UpdateReceipt { new_text: String },
    UpdateChips { changes: Vec<ChipChange> },
    /// A single bubble's text changed (an edit). The UI updates the label
    /// in place without rebuilding the view.
    EditText { guid: String, new_text: String },
    Append {
        new_tail: Vec<StoredMessage>,
        receipt: ReceiptAction,
    },
    Rebuild,
}

/// A single reaction-chip update for one target message.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ChipChange {
    pub target_guid: String,
    pub new_chips: Vec<LiveReactionSummary>,
}

/// What to do with the existing receipt label underneath the last sent
/// message.
#[derive(Debug)]
pub enum ReceiptAction {
    Keep,
    Set(String),
    Remove,
}

// ── plan_chat_update and helpers ──────────────────────────────────────

/// Decide what display update is needed given the previously-rendered state
/// and the new message list from the DB.
///
/// This is the pure decision function that lets the chat view avoid a full
/// rebuild when messages are merely appended or receipts change.  The caller
/// (the GTK refresh path) acts on the returned action.
pub fn plan_chat_update(
    prev_guids: &[String],
    prev_receipt: Option<&str>,
    prev_reactions: &std::collections::BTreeMap<String, Vec<LiveReactionSummary>>,
    prev_text: &std::collections::HashMap<String, String>,
    new_msgs: &[StoredMessage],
    new_reactions: &std::collections::BTreeMap<String, Vec<LiveReactionSummary>>,
) -> ChatUpdatePlan {
    // 1. Compute the non-tapback guid set from new_msgs.
    let new_guids: Vec<String> = new_msgs
        .iter()
        .filter(|m| m.associated_guid.is_none())
        .map(|m| m.guid.clone())
        .collect();

    // 2. Compute desired new receipt state.
    let new_receipt = compute_receipt_state(new_msgs);

    // 3. Compute chip changes.
    let chip_changes = compute_chip_changes(prev_reactions, new_reactions);

    let prev_len = prev_guids.len();

    // 4. Decision tree.
    if prev_guids.is_empty() && !new_guids.is_empty() {
        return ChatUpdatePlan::Rebuild;
    }

    if new_guids == prev_guids {
        // Same set of non-tapback guids → in-place update possible.
        let receipt_changed = new_receipt.as_deref() != prev_receipt;
        let chips_changed = !chip_changes.is_empty();
        // Build new_text map for the non-tapback rows.
        let new_text: std::collections::HashMap<String, String> = new_msgs
            .iter()
            .filter(|m| m.associated_guid.is_none())
            .filter_map(|m| m.text.as_ref().map(|t| (m.guid.clone(), t.clone())))
            .collect();
        // Find the first guid whose text changed (single EditText per plan;
        // further changes are picked up on the next refresh).
        let text_change: Option<(String, String)> = prev_text
            .iter()
            .find_map(|(guid, old_text)| {
                new_text.get(guid).and_then(|new_t| {
                    if new_t != old_text {
                        Some((guid.clone(), new_t.clone()))
                    } else {
                        None
                    }
                })
            });
        let text_changed = text_change.is_some();
        match (receipt_changed, chips_changed, text_changed) {
            (false, false, false) => ChatUpdatePlan::Noop,
            (true, false, false) => match new_receipt {
                Some(text) => ChatUpdatePlan::UpdateReceipt { new_text: text },
                None => ChatUpdatePlan::Rebuild,
            },
            (false, true, false) => ChatUpdatePlan::UpdateChips {
                changes: chip_changes,
            },
            (false, false, true) => {
                let (guid, new_text) = text_change.unwrap();
                ChatUpdatePlan::EditText { guid, new_text }
            }
            // Any other combination → multiple changes, fall through to Rebuild.
            _ => ChatUpdatePlan::Rebuild,
        }
    } else if new_guids.len() > prev_len
        && new_guids[..prev_len]
            .iter()
            .zip(prev_guids.iter())
            .all(|(a, b)| a == b)
    {
        // Strict extension at the end: new_guids starts with prev_guids and
        // has more items.  Chip changes are IGNORED per spec (documented
        // limitation — the chip update will be picked up on the next refresh).
        let new_tail: Vec<StoredMessage> = new_msgs
            .iter()
            .filter(|m| m.associated_guid.is_none())
            .skip(prev_len)
            .cloned()
            .collect();

        let receipt = match (prev_receipt, new_receipt.as_deref()) {
            (Some(_), None) => ReceiptAction::Remove,
            (None, Some(text)) => ReceiptAction::Set(text.to_string()),
            (Some(old), Some(new)) if old != new => ReceiptAction::Set(new.to_string()),
            _ => ReceiptAction::Keep,
        };

        ChatUpdatePlan::Append { new_tail, receipt }
    } else {
        ChatUpdatePlan::Rebuild
    }
}

/// Compare two reaction-chip maps and produce a list of changes.
///
/// An entry in `new` that is absent from `prev` (or has different chips) is a
/// change with the new chips.  An entry present only in `prev` is a removal
/// (empty chips).  The order of the returned vector is unspecified — callers
/// sort it when asserting.
fn compute_chip_changes(
    prev: &std::collections::BTreeMap<String, Vec<LiveReactionSummary>>,
    new: &std::collections::BTreeMap<String, Vec<LiveReactionSummary>>,
) -> Vec<ChipChange> {
    let mut changes = Vec::new();
    // Check for new or changed chips.
    for (guid, new_chips) in new {
        let prev_chips = prev.get(guid);
        if prev_chips != Some(new_chips) {
            changes.push(ChipChange {
                target_guid: guid.clone(),
                new_chips: new_chips.clone(),
            });
        }
    }
    // Check for removed chips (target guid no longer in new).
    for guid in prev.keys() {
        if !new.contains_key(guid) {
            changes.push(ChipChange {
                target_guid: guid.clone(),
                new_chips: vec![],
            });
        }
    }
    changes
}

/// Compute the desired receipt state from the full message list (including
/// any trailing tapback rows). Mirrors the logic in `populate_messages`.
fn compute_receipt_state(msgs: &[StoredMessage]) -> Option<String> {
    let last_sent_idx = msgs
        .iter()
        .rposition(|m| m.is_from_me && m.associated_guid.is_none())?;
    let m = &msgs[last_sent_idx];
    if let Some(text) = receipt_status(m) {
        return Some(text);
    }
    // No real receipt yet. Placeholder only if the last sent is the very
    // last message in the list (including any trailing tapbacks).
    if last_sent_idx == msgs.len() - 1 {
        Some("\u{200b}".to_string())
    } else {
        None
    }
}

/// "Read 16:06" if read, "Delivered" if delivered, "Sending…" if pending
/// (no error), else nothing.
pub(crate) fn receipt_status(m: &StoredMessage) -> Option<String> {
    if let Some(d) = m.date_read {
        Some(format!("Read {}", fmt_time(d)))
    } else if m.date_delivered.is_some() {
        Some("Delivered".to_string())
    } else if m.pending && m.send_error.is_none() {
        Some("Sending…".to_string())
    } else {
        None
    }
}

/// Extract the wire-level `ams` text for a reaction from a stored message.
/// Prefers the message body text, falls back to the first attachment's
/// filename, and returns `""` when both are absent.
pub(crate) fn extract_target_text(m: &StoredMessage) -> String {
    m.text
        .clone()
        .filter(|t| !t.is_empty())
        .or_else(|| m.attachments.first().and_then(|a| a.name.clone()))
        .unwrap_or_default()
}

// ── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod receipt_status_tests {
    use super::*;
    use crate::store::SendErrorCategory;

    fn r(
        guid: &str,
        is_from_me: bool,
        pending: bool,
        date_delivered: Option<i64>,
        date_read: Option<i64>,
        send_error: Option<SendErrorCategory>,
    ) -> StoredMessage {
        StoredMessage {
            id: 0,
            guid: guid.to_string(),
            chat_id: 0,
            sender: None,
            is_from_me,
            text: None,
            subject: None,
            service: None,
            date: 0,
            date_delivered,
            date_read,
            effect: None,
            reply_to_guid: None,
            reply_part: None,
            associated_guid: None,
            associated_type: None,
            item_type: 0,
            send_error,
            pending,
            attachments: vec![],
        }
    }

    // ── receipt_status ───────────────────────────────────────────────

    #[test]
    fn sending_receipt_pending_without_receipt() {
        let m = r("guid", true, true, None, None, None);
        assert_eq!(receipt_status(&m), Some("Sending…".to_string()));
    }

    #[test]
    fn sending_receipt_date_read_beats_pending() {
        let m = r("guid", true, true, None, Some(1000), None);
        let got = receipt_status(&m);
        assert!(got.is_some(), "expected Some for pending+read, got None");
        let text = got.unwrap();
        assert!(
            text.starts_with("Read "),
            "expected 'Read …', got {text:?}"
        );
    }

    #[test]
    fn sending_receipt_date_delivered_beats_pending() {
        let m = r("guid", true, true, Some(2000), None, None);
        assert_eq!(receipt_status(&m), Some("Delivered".to_string()));
    }

    #[test]
    fn sending_receipt_send_error_does_not_produce_sending() {
        // pending + error → no dates → None (error indicator covers it).
        let m = r("guid", true, true, None, None, Some(SendErrorCategory::Timeout));
        assert_eq!(receipt_status(&m), None);
    }

    #[test]
    fn sending_receipt_send_error_with_delivered() {
        // pending + error + delivered → "Delivered" (dates still win).
        let m = r("guid", true, true, Some(2000), None, Some(SendErrorCategory::Timeout));
        assert_eq!(receipt_status(&m), Some("Delivered".to_string()));
    }

    // ── compute_receipt_state ────────────────────────────────────────

    #[test]
    fn sending_receipt_compute_last_pending_at_end() {
        // Last own message is pending, no error, no receipts → "Sending…".
        let msgs = vec![
            r("A", false, false, None, None, None), // incoming
            r("B", true,  true,  None, None, None), // own pending (last)
        ];
        assert_eq!(compute_receipt_state(&msgs), Some("Sending…".to_string()));
    }

    #[test]
    fn sending_receipt_compute_last_pending_not_at_end() {
        // Pending own message, not the very last message → still "Sending…".
        let msgs = vec![
            r("A", false, false, None, None, None), // incoming
            r("B", true,  true,  None, None, None), // own pending
            r("C", false, false, None, None, None), // incoming after
        ];
        assert_eq!(compute_receipt_state(&msgs), Some("Sending…".to_string()));
    }

    #[test]
    fn sending_receipt_compute_non_pending_placeholder_at_end() {
        // Non-pending, no receipts, at end → zero-width placeholder.
        let msgs = vec![
            r("A", false, false, None, None, None),
            r("B", true,  false, None, None, None),
        ];
        assert_eq!(compute_receipt_state(&msgs), Some("\u{200b}".to_string()));
    }

    #[test]
    fn sending_receipt_compute_non_pending_placeholder_absent_when_not_last() {
        // Non-pending, no receipts, not at end → None.
        let msgs = vec![
            r("A", false, false, None, None, None),
            r("B", true,  false, None, None, None),
            r("C", false, false, None, None, None),
        ];
        assert_eq!(compute_receipt_state(&msgs), None);
    }
}

#[cfg(test)]
mod extract_target_text_tests {
    //! Pins the pure helper that picks the wire-level `ams` (target text) for
    //! a reaction. The iPhone uses `ams` to render the reaction chip in the
    //! chat list, and `amk` (`p:N/{guid}`) to attach the chip to the right
    //! target message part. The helper must prefer the message's own text,
    //! fall back to the first attachment's filename, and produce `""` only
    //! when both are missing.
    use super::*;
    use crate::store::StoredAttachment;

    /// Build a `StoredMessage` with only the fields the helper inspects set.
    /// All other fields get the values a fresh, unstored message would have
    /// (zero ids, `None` for optionals, empty strings, no is_sticker).
    fn message_with(
        text: Option<&str>,
        attachments: Vec<StoredAttachment>,
    ) -> StoredMessage {
        StoredMessage {
            id: 0,
            guid: String::new(),
            chat_id: 0,
            sender: None,
            is_from_me: false,
            text: text.map(str::to_string),
            subject: None,
            service: None,
            date: 0,
            date_delivered: None,
            date_read: None,
            effect: None,
            reply_to_guid: None,
            reply_part: None,
            associated_guid: None,
            associated_type: None,
            item_type: 0,
            send_error: None,
            pending: false,
            attachments,
        }
    }

    /// Build a `StoredAttachment` with only `name` (and the `is_sticker`
    /// default `false`) set. The other fields are irrelevant to the helper.
    fn attachment(name: Option<&str>) -> StoredAttachment {
        StoredAttachment {
            mime: None,
            name: name.map(str::to_string),
            local_path: None,
            width: None,
            height: None,
            is_sticker: false,
        }
    }

    #[test]
    fn extract_target_text_returns_text_when_no_attachments() {
        let m = message_with(Some("Hello world"), vec![]);
        assert_eq!(extract_target_text(&m), "Hello world");
    }

    #[test]
    fn extract_target_text_prefers_text_over_attachment_name() {
        // The message has a caption AND a filename; the helper should pick
        // the caption (what the sender actually wrote) over the filename
        // (the system-supplied attachment name).
        let m = message_with(Some("Check this out"), vec![attachment(Some("photo.jpg"))]);
        assert_eq!(extract_target_text(&m), "Check this out");
    }

    #[test]
    fn extract_target_text_falls_back_to_attachment_name_when_text_is_none() {
        // A media-only message (no caption) — the helper must still produce
        // a non-empty `ams` for the iPhone by using the attachment's
        // filename, so the reaction chip has something to display.
        let m = message_with(None, vec![attachment(Some("photo.jpg"))]);
        assert_eq!(extract_target_text(&m), "photo.jpg");
    }

    #[test]
    fn extract_target_text_falls_back_to_attachment_name_when_text_is_empty() {
        // An empty caption is semantically the same as no caption — must
        // NOT be returned as a one-character-or-zero-length `ams`. Falls
        // through to the filename so the chip renders something.
        let m = message_with(Some(""), vec![attachment(Some("photo.jpg"))]);
        assert_eq!(extract_target_text(&m), "photo.jpg");
    }

    #[test]
    fn extract_target_text_returns_empty_when_text_none_and_attachment_has_no_name() {
        // Last resort before the empty fallback: media with no caption and
        // no filename. Returning `""` matches the pre-fix behavior, so the
        // iPhone still gets a valid (if content-less) `ams` field rather
        // than a missing one.
        let m = message_with(None, vec![attachment(None)]);
        assert_eq!(extract_target_text(&m), "");
    }

    #[test]
    fn extract_target_text_returns_empty_when_no_text_and_no_attachments() {
        let m = message_with(None, vec![]);
        assert_eq!(extract_target_text(&m), "");
    }
}

#[cfg(test)]
mod plan_chat_update_tests {
    //! Pins the behaviour of [`super::plan_chat_update`] — the pure decision
    //! function that compares previously-rendered state against the new message
    //! list and returns one of four actions so the GTK side can avoid a full
    //! rebuild.
    //!
    //! All tests construct their own fixtures and call
    //! [`super::plan_chat_update`] directly.  No GTK initialisation needed.
    use super::*;
    use std::collections::BTreeMap;
    use std::collections::HashMap;

    // ── test helpers ────────────────────────────────────────────────

    /// Minimum `StoredMessage` with the identity-relevant fields set to
    /// something useful; everything else zero / `None`.
    fn m(guid: &str, is_from_me: bool, date: i64) -> StoredMessage {
        StoredMessage {
            id: 0,
            guid: guid.to_string(),
            chat_id: 0,
            sender: None,
            is_from_me,
            text: None,
            subject: None,
            service: None,
            date,
            date_delivered: None,
            date_read: None,
            effect: None,
            reply_to_guid: None,
            reply_part: None,
            associated_guid: None,
            associated_type: None,
            item_type: 0,
            send_error: None,
            pending: false,
            attachments: vec![],
        }
    }

    fn delivered(mut m: StoredMessage, date: i64) -> StoredMessage {
        m.date_delivered = Some(date);
        m
    }

    fn read(mut m: StoredMessage, date: i64) -> StoredMessage {
        m.date_read = Some(date);
        m
    }

    fn tapback(mut m: StoredMessage, target: &str) -> StoredMessage {
        m.associated_guid = Some(target.to_string());
        m
    }

    /// Shorthand to build a `Vec<String>` from string slices.
    fn guids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// Empty reaction map shorthand.
    fn no_reactions() -> BTreeMap<String, Vec<LiveReactionSummary>> {
        BTreeMap::new()
    }

    /// Empty text map shorthand.
    fn no_text() -> HashMap<String, String> {
        HashMap::new()
    }

    /// Like `m` but with explicit text.
    fn m_text(guid: &str, is_from_me: bool, date: i64, text: &str) -> StoredMessage {
        StoredMessage {
            text: Some(text.to_string()),
            ..m(guid, is_from_me, date)
        }
    }

    // ── tests ──────────────────────────────────────────────────────

    // ── assertion helpers ──────────────────────────────────────────

    /// Assert the result is `Noop`.
    fn assert_noop(result: ChatUpdatePlan) {
        assert!(matches!(result, ChatUpdatePlan::Noop), "expected Noop, got {result:?}");
    }

    /// Assert the result is `Rebuild`.
    fn assert_rebuild(result: ChatUpdatePlan) {
        assert!(matches!(result, ChatUpdatePlan::Rebuild), "expected Rebuild, got {result:?}");
    }

    /// Assert the result is `UpdateReceipt` with exactly `expected_text`.
    fn assert_update_receipt(result: ChatUpdatePlan, expected_text: &str) {
        match result {
            ChatUpdatePlan::UpdateReceipt { new_text } => {
                assert_eq!(new_text, expected_text, "UpdateReceipt text mismatch");
            }
            other => panic!("expected UpdateReceipt, got {other:?}"),
        }
    }

    /// Assert the result is `Append` with the given tail guids and
    /// `ReceiptAction::Keep`.
    fn assert_append_keep(result: ChatUpdatePlan, expected_tail_guids: &[&str]) {
        match result {
            ChatUpdatePlan::Append { new_tail, receipt } => {
                let tail_guids: Vec<&str> =
                    new_tail.iter().map(|m| m.guid.as_str()).collect();
                assert_eq!(tail_guids, expected_tail_guids, "Append tail guids mismatch");
                assert!(
                    matches!(receipt, ReceiptAction::Keep),
                    "expected Keep receipt, got {receipt:?}",
                );
            }
            other => panic!("expected Append, got {other:?}"),
        }
    }

    /// Assert the result is `Append` with the given tail guids and
    /// `ReceiptAction::Set(expected)`.
    fn assert_append_set(result: ChatUpdatePlan, expected_tail_guids: &[&str], expected_text: &str) {
        match result {
            ChatUpdatePlan::Append { new_tail, receipt } => {
                let tail_guids: Vec<&str> =
                    new_tail.iter().map(|m| m.guid.as_str()).collect();
                assert_eq!(tail_guids, expected_tail_guids, "Append tail guids mismatch");
                match receipt {
                    ReceiptAction::Set(text) => {
                        assert_eq!(text, expected_text, "Append Set receipt text mismatch");
                    }
                    other => panic!("expected Set({expected_text:?}), got {other:?}"),
                }
            }
            other => panic!("expected Append, got {other:?}"),
        }
    }

    /// Assert the result is `Append` with the given tail guids and
    /// `ReceiptAction::Remove`.
    fn assert_append_remove(result: ChatUpdatePlan, expected_tail_guids: &[&str]) {
        match result {
            ChatUpdatePlan::Append { new_tail, receipt } => {
                let tail_guids: Vec<&str> =
                    new_tail.iter().map(|m| m.guid.as_str()).collect();
                assert_eq!(tail_guids, expected_tail_guids, "Append tail guids mismatch");
                assert!(
                    matches!(receipt, ReceiptAction::Remove),
                    "expected Remove receipt, got {receipt:?}",
                );
            }
            other => panic!("expected Append, got {other:?}"),
        }
    }

    // ── individual tests ───────────────────────────────────────────

    #[test]
    fn plan_chat_update_noop_when_guids_and_receipt_unchanged() {
        let prev = guids(&["A", "B"]);
        let new = vec![
            m("A", false, 1000),
            delivered(m("B", true, 2000), 3000),
        ];
        assert_noop(plan_chat_update(&prev, Some("Delivered"), &no_reactions(), &no_text(), &new, &no_reactions()));
    }

    #[test]
    fn plan_chat_update_update_receipt_from_none_to_delivered() {
        let prev = guids(&["A", "B"]);
        let new = vec![
            m("A", false, 1000),
            delivered(m("B", true, 2000), 3000),
        ];
        // prev_receipt was the zero-width placeholder (sent message at bottom
        // with no real receipt yet).
        assert_update_receipt(
            plan_chat_update(&prev, Some("\u{200b}"), &no_reactions(), &no_text(), &new, &no_reactions()),
            "Delivered",
        );
    }

    #[test]
    fn plan_chat_update_update_receipt_delivered_to_read() {
        let prev = guids(&["A", "B"]);
        let new = vec![
            m("A", false, 1000),
            read(delivered(m("B", true, 2000), 3000), 4000),
        ];
        match plan_chat_update(&prev, Some("Delivered"), &no_reactions(), &no_text(), &new, &no_reactions()) {
            ChatUpdatePlan::UpdateReceipt { new_text } => {
                assert!(
                    new_text.starts_with("Read "),
                    "expected Read …, got {new_text:?}",
                );
                assert!(!new_text.is_empty(), "Read text must not be empty");
            }
            other => panic!("expected UpdateReceipt, got {other:?}"),
        }
    }

    #[test]
    fn plan_chat_update_append_incoming_message_keeps_receipt_when_last_sent_unchanged() {
        let prev = guids(&["A", "B"]);
        let new = vec![
            m("A", false, 1000),
            delivered(m("B", true, 2000), 3000),
            m("C", false, 4000),
        ];
        assert_append_keep(
            plan_chat_update(&prev, Some("Delivered"), &no_reactions(), &no_text(), &new, &no_reactions()),
            &["C"],
        );
    }

    #[test]
    fn plan_chat_update_append_sent_message_adds_placeholder() {
        let prev = guids(&["A"]);
        let new = vec![m("A", false, 1000), m("B", true, 2000)];
        assert_append_set(
            plan_chat_update(&prev, None, &no_reactions(), &no_text(), &new, &no_reactions()),
            &["B"],
            "\u{200b}",
        );
    }

    #[test]
    fn plan_chat_update_append_removes_placeholder_when_new_incoming_after_last_sent() {
        let prev = guids(&["A", "B"]);
        let new = vec![
            m("A", false, 1000),
            m("B", true, 2000),  // last sent, no real receipt, was at end → placeholder
            m("C", false, 3000), // new incoming after last sent
        ];
        assert_append_remove(
            plan_chat_update(&prev, Some("\u{200b}"), &no_reactions(), &no_text(), &new, &no_reactions()),
            &["C"],
        );
    }

    #[test]
    fn plan_chat_update_append_multiple_new_messages() {
        let prev = guids(&["A"]);
        let new = vec![
            m("A", false, 1000),
            m("B", false, 2000),
            m("C", false, 3000),
            m("D", false, 4000),
        ];
        assert_append_keep(
            plan_chat_update(&prev, None, &no_reactions(), &no_text(), &new, &no_reactions()),
            &["B", "C", "D"],
        );
    }

    #[test]
    fn plan_chat_update_rebuild_on_deletion() {
        let prev = guids(&["A", "B", "C"]);
        let new = vec![m("A", false, 1000), m("C", false, 3000)];
        assert_rebuild(plan_chat_update(&prev, None, &no_reactions(), &no_text(), &new, &no_reactions()));
    }

    #[test]
    fn plan_chat_update_rebuild_on_reorder() {
        let prev = guids(&["A", "B"]);
        let new = vec![m("B", false, 2000), m("A", false, 1000)];
        assert_rebuild(plan_chat_update(&prev, None, &no_reactions(), &no_text(), &new, &no_reactions()));
    }

    #[test]
    fn plan_chat_update_edit_returns_edit_text() {
        // The plan function now compares text in addition to guids/receipt. A
        // message body change (edit) returns EditText when no other state
        // changed. The UI then updates that one bubble's label in place.
        let prev = guids(&["A", "B", "C"]);
        let prev_text: HashMap<String, String> = [
            ("A", "old A"),
            ("B", "old B"),
            ("C", "old C"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let new = vec![
            m("A", false, 1000),
            // B's text changed (edit)
            m_text("B", false, 2000, "new B"),
            m("C", false, 3000),
        ];
        match plan_chat_update(&prev, None, &no_reactions(), &prev_text, &new, &no_reactions()) {
            ChatUpdatePlan::EditText { guid, new_text } => {
                assert_eq!(guid, "B", "EditText targets the changed message");
                assert_eq!(new_text, "new B", "EditText carries the new text");
            }
            other => panic!("expected EditText, got {other:?}"),
        }
    }

    #[test]
    fn plan_chat_update_no_text_change_is_noop() {
        // Even with prev_text supplied, no actual text change returns Noop.
        let prev = guids(&["A", "B"]);
        let prev_text: HashMap<String, String> = [
            ("A", "same A"),
            ("B", "same B"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let new = vec![
            m_text("A", false, 1000, "same A"),
            m_text("B", false, 2000, "same B"),
        ];
        assert_noop(plan_chat_update(&prev, None, &no_reactions(), &prev_text, &new, &no_reactions()));
    }

    #[test]
    fn plan_chat_update_rebuild_when_prev_guids_empty_with_messages() {
        let prev: Vec<String> = vec![];
        let new = vec![m("A", false, 1000), m("B", false, 2000)];
        assert_rebuild(plan_chat_update(&prev, None, &no_reactions(), &no_text(), &new, &no_reactions()));
    }

    #[test]
    fn plan_chat_update_noop_when_new_list_has_only_extra_tapback_rows() {
        // Tapback rows (associated_guid.is_some()) are filtered out of the
        // guid set.  Adding only tapbacks leaves the non-tapback guid set
        // unchanged, so the plan is Noop.  The reaction chips themselves are
        // not detected by this function — they will be stale until the next
        // real refresh, which is acceptable for the send-flash fix.
        let prev = guids(&["A", "B"]);
        let new = vec![
            delivered(m("A", false, 1000), 0),
            delivered(m("B", true, 2000), 3000),
            tapback(m("T1", false, 1500), "A"),
        ];
        assert_noop(plan_chat_update(&prev, Some("Delivered"), &no_reactions(), &no_text(), &new, &no_reactions()));
    }

    #[test]
    fn plan_chat_update_append_tapback_does_not_show_in_tail() {
        // A tapback row in new_msgs does not count as a non-tapback message,
        // so it must not appear in Append's new_tail.
        let prev = guids(&["A"]);
        let new = vec![
            m("A", false, 1000),
            m("B", false, 2000),
            tapback(m("T1", false, 1500), "A"),
        ];
        assert_append_keep(
            plan_chat_update(&prev, None, &no_reactions(), &no_text(), &new, &no_reactions()),
            &["B"],
        );
    }

    // ── reaction helpers ──────────────────────────────────────────────

    /// Build a single `LiveReactionSummary`.
    fn chip(index: u8, count: usize, my: bool) -> LiveReactionSummary {
        LiveReactionSummary {
            reaction_index: index,
            count,
            my_reacted: my,
        }
    }

    /// Build a reaction map from a slice of (guid, chips) pairs.
    fn rmap(pairs: &[(&str, Vec<LiveReactionSummary>)]) -> BTreeMap<String, Vec<LiveReactionSummary>> {
        pairs.iter().map(|(g, c)| (g.to_string(), c.clone())).collect()
    }

    /// Assert the result is `UpdateChips` with the given changes (order-insensitive).
    fn assert_update_chips(result: ChatUpdatePlan, expected: Vec<ChipChange>) {
        match result {
            ChatUpdatePlan::UpdateChips { mut changes } => {
                changes.sort_by(|a, b| a.target_guid.cmp(&b.target_guid));
                let mut expected = expected;
                expected.sort_by(|a, b| a.target_guid.cmp(&b.target_guid));
                assert_eq!(changes, expected, "UpdateChips changes mismatch");
            }
            other => panic!("expected UpdateChips, got {other:?}"),
        }
    }

    // ── reaction chip tests ───────────────────────────────────────────

    #[test]
    fn plan_chat_update_update_chips_only_when_reactions_change() {
        // prev has reactions empty, new has a reaction on A — chips differ.
        let prev = guids(&["A", "B"]);
        let new = vec![
            m("A", false, 1000),
            m("B", false, 2000),
        ];
        let prev_r = no_reactions();
        let new_r = rmap(&[("A", vec![chip(0, 1, false)])]);
        assert_update_chips(
            plan_chat_update(&prev, None, &prev_r, &no_text(), &new, &new_r),
            vec![ChipChange {
                target_guid: "A".to_string(),
                new_chips: vec![chip(0, 1, false)],
            }],
        );
    }

    #[test]
    fn plan_chat_update_update_chips_when_existing_chip_gains_reaction() {
        let prev = guids(&["A"]);
        let new = vec![m("A", false, 1000)];
        let prev_r = rmap(&[("A", vec![chip(0, 1, false)])]);
        let new_r = rmap(&[("A", vec![chip(0, 1, false), chip(1, 1, false)])]);
        assert_update_chips(
            plan_chat_update(&prev, None, &prev_r, &no_text(), &new, &new_r),
            vec![ChipChange {
                target_guid: "A".to_string(),
                new_chips: vec![chip(0, 1, false), chip(1, 1, false)],
            }],
        );
    }

    #[test]
    fn plan_chat_update_update_chips_when_reaction_removed() {
        let prev = guids(&["A"]);
        let new = vec![m("A", false, 1000)];
        let prev_r = rmap(&[("A", vec![chip(0, 1, false)])]);
        // Key is absent in new_reactions — should treat as empty.
        let new_r = no_reactions();
        assert_update_chips(
            plan_chat_update(&prev, None, &prev_r, &no_text(), &new, &new_r),
            vec![ChipChange {
                target_guid: "A".to_string(),
                new_chips: vec![],
            }],
        );
    }

    #[test]
    fn plan_chat_update_noop_when_reactions_unchanged() {
        let prev = guids(&["A"]);
        let new = vec![m("A", false, 1000)];
        let r = rmap(&[("A", vec![chip(0, 1, false)])]);
        assert_noop(plan_chat_update(&prev, None, &r, &no_text(), &new, &r));
    }

    #[test]
    fn plan_chat_update_update_chips_multiple_targets() {
        let prev = guids(&["A", "B"]);
        let new = vec![
            m("A", false, 1000),
            m("B", false, 2000),
        ];
        let prev_r = rmap(&[("A", vec![chip(0, 1, false)])]);
        let new_r = rmap(&[("A", vec![chip(0, 1, false)]), ("B", vec![chip(1, 2, true)])]);
        // Only B changed (A is the same).
        assert_update_chips(
            plan_chat_update(&prev, None, &prev_r, &no_text(), &new, &new_r),
            vec![ChipChange {
                target_guid: "B".to_string(),
                new_chips: vec![chip(1, 2, true)],
            }],
        );
    }

    #[test]
    fn plan_chat_update_rebuild_when_both_receipt_and_chips_change() {
        // Guids unchanged, but both receipt (Delivered→Read) and chips (added
        // laugh) changed.  The safe fallback is Rebuild.
        let prev = guids(&["A", "B"]);
        let new = vec![
            m("A", false, 1000),
            read(delivered(m("B", true, 2000), 3000), 4000),
        ];
        let prev_r = rmap(&[("A", vec![chip(0, 1, false)])]);
        let new_r = rmap(&[("A", vec![chip(0, 1, false), chip(3, 1, false)])]);
        assert_rebuild(plan_chat_update(
            &prev,
            Some("Delivered"),
            &prev_r,
            &no_text(),
            &new,
            &new_r,
        ));
    }

    #[test]
    fn plan_chat_update_append_ignores_chip_changes() {
        // A new message B arrives.  Even though chips changed on A, the plan
        // should return Append (not Rebuild, not UpdateChips).  Chip changes
        // in the Append case are ignored per spec — they will be picked up on
        // the next refresh.
        let prev = guids(&["A"]);
        let new = vec![
            m("A", false, 1000),
            m("B", false, 2000),
        ];
        let prev_r = rmap(&[("A", vec![chip(0, 1, false)])]);
        let new_r = rmap(&[("A", vec![chip(0, 1, false), chip(1, 1, false)])]);
        assert_append_keep(
            plan_chat_update(&prev, None, &prev_r, &no_text(), &new, &new_r),
            &["B"],
        );
    }

    #[test]
    fn sending_receipt_plan_transition_sending_to_delivered() {
        // Same guids; previous receipt was "Sending…", new state has the last
        // own message delivered → UpdateReceipt("Delivered").
        let prev = guids(&["A", "B"]);
        let new = vec![
            m("A", false, 1000),
            delivered(m("B", true, 2000), 3000),
        ];
        assert_update_receipt(
            plan_chat_update(&prev, Some("Sending…"), &no_reactions(), &no_text(), &new, &no_reactions()),
            "Delivered",
        );
    }
} // mod plan_chat_update_tests
