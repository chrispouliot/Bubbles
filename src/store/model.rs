//! Backend-agnostic data the [`super::Store`] ingests and returns. Deliberately
//! free of rustpush and GTK types, so the store layer builds and is unit-tested
//! on its own. The rustpush -> [`Ingest`] mapping lives in the backend layer
//! (`protocol::rustpush_backend::ingest_from`), keeping this module portable.

/// A chat referenced by its participant set (full `tel:`/`mailto:` addresses).
#[derive(Clone, Debug, Default)]
pub struct ChatRef {
    /// All members, *including* self, exactly as received.
    pub participants: Vec<String>,
    /// Group name (`cv_name`); `None` for 1:1s.
    pub display_name: Option<String>,
    /// "iMessage" / "SMS", when known.
    pub service: Option<String>,
}

impl ChatRef {
    /// Normalized participant list: lowercased, de-duped, sorted.
    fn normalized(&self) -> Vec<String> {
        let mut p: Vec<String> = self.participants.iter().map(|s| s.to_lowercase()).collect();
        p.sort();
        p.dedup();
        p
    }

    /// Stable identity key — what distinguishes one conversation from another.
    /// Includes self, so threads with different membership stay separate.
    pub fn key(&self) -> String {
        self.normalized().join(";")
    }

    /// `participants` includes self, so >2 distinct members is a group.
    pub fn is_group(&self) -> bool {
        self.normalized().len() > 2
    }
}

/// A content message to insert. Idempotent on `guid`.
#[derive(Clone, Debug, Default)]
pub struct IncomingMessage {
    pub guid: String,
    pub chat: ChatRef,
    pub sender: Option<String>,
    pub is_from_me: bool,
    pub text: Option<String>,
    pub subject: Option<String>,
    pub service: Option<String>,
    /// Unix epoch milliseconds.
    pub date: i64,
    pub effect: Option<String>,
    pub reply_to_guid: Option<String>,
    pub reply_part: Option<String>,
    /// 0 = normal text; non-zero reserved for rename/participant-change/etc.
    pub item_type: i64,
    /// Files attached to this message (already downloaded to `local_path`).
    pub attachments: Vec<AttachmentRecord>,
    /// Whether this message is still pending/in-flight (hasn't been confirmed
    /// sent by the server).  Default `false` via `..Default::default()`.
    pub pending: bool,
}

/// An attachment as ingested: metadata plus a local file we've already saved.
#[derive(Clone, Debug, Default)]
pub struct AttachmentRecord {
    pub guid: Option<String>,
    pub mime: Option<String>,
    pub name: Option<String>,
    pub total_bytes: Option<i64>,
    pub local_path: Option<String>,
    pub part_index: Option<i64>,
    pub is_sticker: bool,
}

/// A delivery/read receipt updating an existing message (referenced by its guid).
#[derive(Clone, Debug)]
pub enum Receipt {
    Delivered { guid: String, date: i64 },
    Read { guid: String, date: i64 },
}

/// Category of send failure stored on an outgoing message.
///
/// Persisted in `message.error` as a small integer:
///   * 0 / NULL = no error
///   * 1 = [`Timeout`]
///   * 2 = [`ConnectionLost`]
///   * 3 = [`Other`]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendErrorCategory {
    Timeout = 1,
    ConnectionLost = 2,
    Other = 3,
}

impl SendErrorCategory {
    /// Convert from the wire value stored in the DB.
    pub fn from_i64(v: Option<i64>) -> Option<Self> {
        match v? {
            1 => Some(Self::Timeout),
            2 => Some(Self::ConnectionLost),
            3 => Some(Self::Other),
            _ => None,
        }
    }
}

/// A tapback/reaction. Stored as a message row carrying `associated_*`.
#[derive(Clone, Debug, Default)]
pub struct Tapback {
    /// The reaction message's own guid (for dedupe).
    pub guid: String,
    pub chat: ChatRef,
    pub sender: Option<String>,
    pub is_from_me: bool,
    pub date: i64,
    /// Target message guid.
    pub associated_guid: String,
    pub associated_part: Option<String>,
    /// Apple tapback code: 2000-2005 add, 3000-3005 remove.
    pub associated_type: i64,
}

/// One inbound event — the single thing the store ingests.
#[derive(Clone, Debug)]
pub enum Ingest {
    Message(IncomingMessage),
    Receipt(Receipt),
    Tapback(Tapback),
    /// A sender-supplied link preview (iMessage rich link). Persisted in
    /// `message_link_preview`, upserted on `(message_guid, part_idx)`.
    LinkPreview(MessageLinkPreview),
    /// Mark an outgoing message as having failed to send, with a category
    /// describing the failure mode.
    SendFailed {
        guid: String,
        category: SendErrorCategory,
    },
    /// An edit to an existing message.
    Edited {
        guid: String,
        text: String,
    },
    /// A recognized-but-unstored control event; the &str names the variant.
    #[allow(dead_code)]
    Ignored(&'static str),
}

// --- read side ---

#[derive(Clone, Debug)]
pub struct ChatSummary {
    pub id: i64,
    pub key: String,
    pub display_name: Option<String>,
    pub is_group: bool,
    pub service: Option<String>,
    #[allow(dead_code)]
    pub last_message_date: Option<i64>,
    pub participants: Vec<String>,
    pub unread: i64,
    /// User-set name overriding the derived title; `None` falls back to the
    /// Apple group name or the participants.
    pub custom_name: Option<String>,
    /// User-set custom avatar path overriding any derived avatar; `None` falls
    /// back to the derived avatar (participant photos, group initials, etc.).
    #[allow(dead_code)]
    pub custom_avatar_path: Option<String>,
}

/// A freshly-arrived inbound message, used to drive desktop notifications.
#[derive(Clone, Debug)]
pub struct NewMessage {
    pub chat_id: i64,
    pub sender: Option<String>,
    pub text: Option<String>,
    pub has_attachment: bool,
    pub date: i64,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct StoredMessage {
    pub id: i64,
    pub guid: String,
    pub chat_id: i64,
    pub sender: Option<String>,
    pub is_from_me: bool,
    pub text: Option<String>,
    pub subject: Option<String>,
    pub service: Option<String>,
    pub date: i64,
    pub date_delivered: Option<i64>,
    pub date_read: Option<i64>,
    pub effect: Option<String>,
    pub reply_to_guid: Option<String>,
    pub reply_part: Option<String>,
    pub associated_guid: Option<String>,
    pub associated_type: Option<i64>,
    pub item_type: i64,
    pub send_error: Option<SendErrorCategory>,
    pub pending: bool,
    pub attachments: Vec<StoredAttachment>,
}

/// An attachment as read back for display.
#[derive(Clone, Debug)]
pub struct StoredAttachment {
    pub mime: Option<String>,
    pub name: Option<String>,
    pub local_path: Option<String>,
    #[allow(dead_code)]
    pub is_sticker: bool,
}

impl StoredAttachment {
    #[allow(clippy::unnecessary_map_or)]
    pub fn is_image(&self) -> bool {
        self.mime
            .as_deref()
            .map_or(false, |m| m.starts_with("image/"))
    }

    #[allow(dead_code, clippy::unnecessary_map_or)]
    pub fn is_video(&self) -> bool {
        self.mime
            .as_deref()
            .map_or(false, |m| m.starts_with("video/"))
    }
}

impl StoredAttachment {
    /// Which kind of widget to render this attachment as.
    /// Image takes precedence over Video (in the rare case a MIME matches both).
    pub fn kind(&self) -> AttachmentKind {
        if self.is_image() {
            AttachmentKind::Image
        } else if self.is_video() {
            AttachmentKind::Video
        } else {
            AttachmentKind::Other
        }
    }
}

/// Which kind of widget to render this attachment as.
/// Image takes precedence over Video (in the rare case a MIME matches both).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AttachmentKind {
    Image,
    Video,
    Other,
}

/// A sender-generated URL preview, attached to a specific message. This is the
/// iMessage rich-link / LinkPresentation data the sender's device shipped to us:
/// title, summary, URL, and the inline thumbnail bytes rustpush already pulled
/// from the balloon body. We do not fetch incoming URLs; this is the sender's
/// static snapshot, keyed by message.
///
/// `part_idx` distinguishes multiple links on the same message (rare; iMessage
/// typically carries one URL per message, but the schema allows more). The
/// `(message_guid, part_idx)` pair is the primary key so a placeholder can be
/// upserted in place when the fill-in arrives.
#[derive(Clone, Debug)]
pub struct MessageLinkPreview {
    pub message_guid: String,
    pub part_idx: i64,
    /// Canonical (post-redirect) URL the sender's device resolved to.
    pub url: Option<String>,
    /// Whatever the sender actually typed; preserved when it differs from `url`.
    pub original_url: Option<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    /// Local cache path of the thumbnail (`$XDG_CACHE_HOME/.../previews/...`).
    /// `None` when the sender didn't include one.
    pub image_path: Option<String>,
    /// Width/height of `image_path` (best-effort, used for sizing).
    pub image_width: Option<i64>,
    pub image_height: Option<i64>,
    /// `is_incomplete == true` from the LPLinkMetadata — Apple sent a
    /// placeholder balloon, the real preview is on its way. Render the
    /// compact "loading…" state and replace in place when the fill-in arrives.
    pub is_placeholder: bool,
}

impl MessageLinkPreview {
    /// True when the title *and* summary are both empty (placeholder preview or
    /// a sender-supplied empty card). The renderer collapses such cards into a
    /// compact "loading preview…" state instead of an empty shell.
    pub fn is_sparse(&self) -> bool {
        let title_blank = self
            .title
            .as_deref()
            .is_none_or(|s| s.trim().is_empty());
        let summary_blank = self
            .summary
            .as_deref()
            .is_none_or(|s| s.trim().is_empty());
        title_blank && summary_blank
    }
}

/// A live (non-cancelled) tapback/reaction on a message.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveTapback {
    pub target_guid: String,
    pub target_part: Option<String>,
    pub sender: Option<String>,
    pub is_from_me: bool,
    /// Reaction index 0..=5 (heart, thumb up, thumb down, haha, exclamation, question).
    pub reaction_index: u8,
}

/// Given a slice of raw `Tapback` rows, return the **live set** after applying
/// add/remove cancellation semantics.
///
/// The cancellation key is `(target_guid, target_part, sender, reaction_index)`:
/// for each key the last event in the slice wins.
///
/// * "add" events = `associated_type` 2000..=2005 (lower digit = reaction index).
/// * "remove" events = `associated_type` 3000..=3005 (same lower-digit index).
/// * An add followed by a later remove cancels out — no live entry.
/// * A remove with no prior add produces no live entry.
/// * Different senders, targets, or reaction indices each get their own entry.
/// * `is_from_me` on the live entry is the value from the winning event.
///
/// The output order is unspecified (the caller can sort / fingerprint as needed).
#[allow(dead_code)]
pub fn live_tapbacks(tapbacks: &[Tapback]) -> Vec<LiveTapback> {
    use std::collections::HashMap;

    /// Cancellation key: (target_guid, target_part, sender, reaction_index).
    type Key<'a> = (&'a str, Option<&'a str>, Option<&'a str>, u8);

    // Map from cancellation-key components to the most recent tapback
    // and whether it was an "add" (true) or "remove" (false).
    let mut latest: HashMap<Key<'_>, (&Tapback, bool)> = HashMap::new();

    for t in tapbacks {
        let is_add = (2000..=2005).contains(&t.associated_type);
        let is_remove = (3000..=3005).contains(&t.associated_type);
        if !is_add && !is_remove {
            continue;
        }
        let idx = (t.associated_type % 10) as u8;
        let key = (
            t.associated_guid.as_str(),
            t.associated_part.as_deref(),
            t.sender.as_deref(),
            idx,
        );
        // Last occurrence in the slice wins (position-based ordering).
        latest.insert(key, (t, is_add));
    }

    latest
        .into_values()
        .filter_map(|(t, is_add)| {
            if is_add {
                Some(LiveTapback {
                    target_guid: t.associated_guid.clone(),
                    target_part: t.associated_part.clone(),
                    sender: t.sender.clone(),
                    is_from_me: t.is_from_me,
                    reaction_index: (t.associated_type % 10) as u8,
                })
            } else {
                None
            }
        })
        .collect()
}

/// Summary of live reactions on one target message, grouped by reaction type.
/// Returned by [`group_tapbacks_by_target`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveReactionSummary {
    pub reaction_index: u8,  // 0..=5
    pub count: usize,
    pub my_reacted: bool,
}

/// Group live tapbacks by target GUID, then by reaction index.
///
/// For each (target, reaction_index) group, `count` is the number of distinct
/// senders, and `my_reacted` is true if any sender has `is_from_me: true`.
pub fn group_tapbacks_by_target(
    live: Vec<LiveTapback>,
) -> std::collections::BTreeMap<String, Vec<LiveReactionSummary>> {
    use std::collections::{BTreeMap, HashMap, HashSet};

    /// Accumulator: set of distinct senders + whether any is_from_me.
    type Group = (HashSet<Option<String>>, bool);

    // Aggregate: (target_guid, reaction_index) -> Group
    let mut groups: HashMap<(String, u8), Group> = HashMap::new();

    for tb in live {
        let entry = groups.entry((tb.target_guid, tb.reaction_index)).or_default();
        entry.0.insert(tb.sender);
        if tb.is_from_me {
            entry.1 = true;
        }
    }

    let mut result: BTreeMap<String, Vec<LiveReactionSummary>> = BTreeMap::new();
    // Sort for deterministic output order.
    let mut pairs: Vec<_> = groups.into_iter().collect();
    pairs.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));

    for ((target, idx), (senders, my_reacted)) in pairs {
        result.entry(target).or_default().push(LiveReactionSummary {
            reaction_index: idx,
            count: senders.len(),
            my_reacted,
        });
    }

    result
}

/// A URL-keyed, fetched preview. Distinct from [`MessageLinkPreview`]: that one
/// is message-scoped, sender-supplied, and immutable. This one is keyed by URL
/// (so re-opening the same link reuses the result), TTL'd, and the result of
/// *us* fetching the page. The schema lives alongside `message_link_preview`
/// in the same migration so a future fetcher can land as one atomic bump.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct LinkPreview {
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub site_name: Option<String>,
    pub image_path: Option<String>,
    /// Unix epoch ms when the row was last refreshed.
    pub fetched_at: i64,
    /// 0 = ok, 1 = failed (so a 404 isn't re-hit every render).
    pub status: i64,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------
    // is_video() — real video MIME types
    // -------------------------------------------------------------------

    #[test]
    fn is_video_returns_true_for_video_mp4() {
        let att = StoredAttachment {
            mime: Some("video/mp4".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert!(att.is_video());
    }

    #[test]
    fn is_video_returns_true_for_video_quicktime() {
        let att = StoredAttachment {
            mime: Some("video/quicktime".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert!(att.is_video());
    }

    #[test]
    fn is_video_returns_true_for_video_hevc() {
        let att = StoredAttachment {
            mime: Some("video/hevc".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert!(att.is_video());
    }

    #[test]
    fn is_video_returns_true_for_video_heic() {
        let att = StoredAttachment {
            mime: Some("video/heic".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert!(att.is_video());
    }

    // -------------------------------------------------------------------
    // is_video() — image MIME types must NOT be classified as video
    // -------------------------------------------------------------------

    #[test]
    fn is_video_returns_false_for_image_jpeg() {
        let att = StoredAttachment {
            mime: Some("image/jpeg".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert!(!att.is_video());
    }

    #[test]
    fn is_video_returns_false_for_image_png() {
        let att = StoredAttachment {
            mime: Some("image/png".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert!(!att.is_video());
    }

    #[test]
    fn is_video_returns_false_for_image_heic_still() {
        // image/heic is the still-image variant; it must NOT be classified as video.
        let att = StoredAttachment {
            mime: Some("image/heic".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert!(!att.is_video());
    }

    // -------------------------------------------------------------------
    // is_video() — arbitrary non-video MIME types
    // -------------------------------------------------------------------

    #[test]
    fn is_video_returns_false_for_application_pdf() {
        let att = StoredAttachment {
            mime: Some("application/pdf".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert!(!att.is_video());
    }

    #[test]
    fn is_video_returns_false_for_text_plain() {
        let att = StoredAttachment {
            mime: Some("text/plain".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert!(!att.is_video());
    }

    #[test]
    fn is_video_returns_false_for_audio_mpeg() {
        let att = StoredAttachment {
            mime: Some("audio/mpeg".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert!(!att.is_video());
    }

    // -------------------------------------------------------------------
    // is_video() — None MIME
    // -------------------------------------------------------------------

    #[test]
    fn is_video_returns_false_for_none_mime() {
        let att = StoredAttachment {
            mime: None,
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert!(!att.is_video());
    }

    // -------------------------------------------------------------------
    // Regression: is_image() must still work correctly
    // -------------------------------------------------------------------

    #[test]
    fn is_image_still_returns_true_for_image_jpeg() {
        let att = StoredAttachment {
            mime: Some("image/jpeg".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert!(att.is_image());
    }

    #[test]
    fn is_image_returns_false_for_video_mp4() {
        let att = StoredAttachment {
            mime: Some("video/mp4".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert!(!att.is_image());
    }

    // -------------------------------------------------------------------
    // StoredAttachment::kind() — dispatch to widget kind
    // -------------------------------------------------------------------

    #[test]
    fn attachment_kind_returns_image_for_image_jpeg() {
        let att = StoredAttachment {
            mime: Some("image/jpeg".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert_eq!(att.kind(), AttachmentKind::Image);
    }

    #[test]
    fn attachment_kind_returns_video_for_video_mp4() {
        let att = StoredAttachment {
            mime: Some("video/mp4".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert_eq!(att.kind(), AttachmentKind::Video);
    }

    #[test]
    fn attachment_kind_returns_video_for_video_quicktime() {
        let att = StoredAttachment {
            mime: Some("video/quicktime".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert_eq!(att.kind(), AttachmentKind::Video);
    }

    #[test]
    fn attachment_kind_returns_video_for_video_hevc() {
        let att = StoredAttachment {
            mime: Some("video/hevc".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert_eq!(att.kind(), AttachmentKind::Video);
    }

    #[test]
    fn attachment_kind_returns_other_for_application_pdf() {
        let att = StoredAttachment {
            mime: Some("application/pdf".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert_eq!(att.kind(), AttachmentKind::Other);
    }

    #[test]
    fn attachment_kind_returns_other_for_audio_mpeg() {
        let att = StoredAttachment {
            mime: Some("audio/mpeg".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert_eq!(att.kind(), AttachmentKind::Other);
    }

    #[test]
    fn attachment_kind_returns_other_for_none_mime() {
        let att = StoredAttachment {
            mime: None,
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert_eq!(att.kind(), AttachmentKind::Other);
    }

    #[test]
    fn attachment_kind_returns_image_for_image_heic() {
        // image/heic is the still-image variant; must be Image, not Video or Other.
        let att = StoredAttachment {
            mime: Some("image/heic".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert_eq!(att.kind(), AttachmentKind::Image);
    }

    #[test]
    fn attachment_kind_returns_image_for_image_png() {
        let att = StoredAttachment {
            mime: Some("image/png".into()),
            name: None,
            local_path: None,
            is_sticker: false,
        };
        assert_eq!(att.kind(), AttachmentKind::Image);
    }
}
