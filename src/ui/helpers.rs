//! Pure / stateless utility functions extracted from [`super`](mod.rs).
//!
//! These are simple string-manipulation, date-formatting, and collection
//! helpers that perform no GTK work, no I/O, and have no side effects
//! (except `now_ms` which reads the system clock). Many are used by child
//! modules via `use super::*;`.

use std::path::PathBuf;

use gtk::prelude::*;

use crate::store::{
    ChatRef, ChatSummary, Contact, IncomingMessage, StoredMessage,
};

/// Return the custom avatar path for a chat if one is set and non-empty.
/// Returns `None` for `None`, empty string, or whitespace-only values.
/// Does NOT trim the returned path — leading whitespace is preserved for
/// non-empty paths so that callers receive the raw stored value.
pub(super) fn chat_avatar_custom_path(c: &ChatSummary) -> Option<&str> {
    c.custom_avatar_path.as_deref().filter(|p| !p.trim().is_empty())
}

/// Strip the Unicode Object Replacement Character (`\u{FFFC}`) used by GTK
/// to represent embedded widgets (e.g. emoji reactions), then trim whitespace.
pub(super) fn strip_marker(s: &str) -> String {
    s.replace('\u{FFFC}', "").trim().to_string()
}

/// Guess a MIME type from a file extension. Returns `application/octet-stream`
/// for unknown extensions.
pub(super) fn guess_mime(name: &str) -> String {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "heic" | "heif" => "image/heic",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Parse a `text/uri-list` clipboard payload into local file paths.
///
/// - One `PathBuf` per `file://` URI, in source order.
/// - Non-`file://` URIs (http, https, ftp, …) are skipped — they are not local files.
/// - Lines starting with `#` (after optional whitespace) are comments and skipped.
/// - Blank lines (including lines containing only whitespace) are skipped.
/// - Both `\n` and `\r\n` line endings are accepted.
/// - URI percent-encoded characters are decoded (e.g. `Screenshot%20from%20foo.png`
///   becomes the path `Screenshot from foo.png`).
/// - The canonical `file:///abs/path` (three slashes) form is supported.
/// - An empty input string yields an empty `Vec`.
pub(super) fn parse_uri_list(text: &str) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for line in text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(path_str) = trimmed.strip_prefix("file://") {
            let mut decoded = Vec::with_capacity(path_str.len());
            let bytes = path_str.as_bytes();
            let mut i = 0;
            let mut valid = true;
            while i < bytes.len() {
                if bytes[i] == b'%' && i + 2 < bytes.len() {
                    let hex = &path_str[i + 1..i + 3];
                    match u8::from_str_radix(hex, 16) {
                        Ok(byte) => { decoded.push(byte); i += 3; }
                        Err(_) => { valid = false; break; }
                    }
                } else {
                    decoded.push(bytes[i]);
                    i += 1;
                }
            }
            if valid {
                if let Ok(s) = String::from_utf8(decoded) {
                    result.push(PathBuf::from(s));
                }
            }
        }
    }
    result
}

pub(super) fn chat_title(c: &ChatSummary, handles: &[String], contacts: &[Contact]) -> String {
    crate::contacts::chat_display_title(c, handles, contacts)
}

pub(super) fn sender_display(m: &StoredMessage, handles: &[String], contacts: &[Contact]) -> String {
    if m.is_from_me {
        "You".to_string()
    } else {
        match m.sender.as_deref() {
            Some(addr) => crate::contacts::participant_display_name(addr, handles, contacts),
            None => "Unknown".to_string(),
        }
    }
}

/// An iMessage-style guid (uppercased UUID v4) for optimistic local inserts.
pub(super) fn new_guid() -> String {
    glib::uuid_string_random().to_string().to_uppercase()
}

/// Unix epoch milliseconds, matching the backend's message timestamps.
pub(crate) fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Normalise a raw recipient string into a `mailto:` or `tel:` URI (or `None` if invalid).
pub(super) fn normalize_recipient(recipient: &str) -> Option<String> {
    let recipient = recipient.trim();
    if recipient.is_empty() {
        return None;
    }

    if recipient.to_lowercase().starts_with("mailto:") {
        // Strip prefix, lowercase the address, re-add `mailto:`
        let addr = recipient["mailto:".len()..].to_lowercase();
        Some(format!("mailto:{}", addr))
    } else if recipient.contains('@') {
        // Bare email — lowercase and wrap in `mailto:`
        Some(format!("mailto:{}", recipient.to_lowercase()))
    } else if recipient.to_lowercase().starts_with("tel:") {
        // Strip `tel:` prefix, apply phone rules, re-add `tel:`
        let phone = &recipient["tel:".len()..];
        normalize_phone(phone)
    } else {
        // Phone path
        normalize_phone(recipient)
    }
}

/// Build a `(ChatRef, IncomingMessage)` for the "start a new chat" path.
pub(super) fn new_chat_payload(
    recipient: &str,
    text: &str,
    my_handle: &str,
) -> Option<(ChatRef, IncomingMessage)> {
    let normalized = normalize_recipient(recipient)?;

    // Build participants list (sorted for stable key)
    let mut participants = vec![my_handle.to_string(), normalized.clone()];
    participants.sort();

    let chat = ChatRef {
        participants,
        display_name: None,
        service: None,
    };

    let msg = IncomingMessage {
        guid: new_guid(),
        chat,
        sender: Some(my_handle.to_string()),
        is_from_me: true,
        text: Some(text.to_string()),
        subject: None,
        service: None,
        date: now_ms(),
        effect: None,
        reply_to_guid: None,
        reply_part: None,
        item_type: 0,
        attachments: Vec::new(),
        pending: false,
    };

    Some((msg.chat.clone(), msg))
}

/// Normalise a raw phone string into a `tel:` URI (or `None` if invalid).
pub(super) fn normalize_phone(raw: &str) -> Option<String> {
    let has_plus = raw.starts_with('+');
    // Strip everything that isn't a digit (the leading + is handled separately)
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }

    let phone = if has_plus {
        format!("tel:+{}", digits)
    } else if digits.len() == 10 {
        format!("tel:+1{}", digits)
    } else {
        format!("tel:+{}", digits)
    };

    Some(phone)
}

pub(super) fn group_key(m: &StoredMessage) -> String {
    if m.is_from_me {
        "\0me".to_string()
    } else {
        m.sender.clone().unwrap_or_default()
    }
}

pub(super) fn body_text(m: &StoredMessage) -> String {
    match (&m.text, &m.associated_guid) {
        (Some(t), _) if !strip_marker(t).is_empty() => strip_marker(t),
        // Tapback rows are rendered as reaction chips on the target message;
        // suppress the placeholder bubble body.
        (_, Some(_)) => String::new(),
        _ => "(no text)".to_string(),
    }
}

pub(super) fn fmt_time(ms: i64) -> String {
    crate::time_format::format_time(ms, crate::time_format::get())
}

pub(super) fn pretty_addr(a: &str) -> String {
    a.strip_prefix("mailto:")
        .or_else(|| a.strip_prefix("tel:"))
        .unwrap_or(a)
        .to_string()
}

/// Our own address within a conversation, used as the sender for outbound items.
pub(super) fn self_handle(participants: &[String], handles: &[String]) -> Option<String> {
    participants
        .iter()
        .find(|p| {
            handles
                .iter()
                .any(|h| h.as_str().eq_ignore_ascii_case(p.as_str()))
        })
        .cloned()
}

pub(super) fn chat_ref_of(c: &ChatSummary) -> ChatRef {
    ChatRef {
        participants: c.participants.clone(),
        display_name: c.display_name.clone(),
        service: c.service.clone(),
    }
}

pub(super) fn clear(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

pub(super) fn clear_box(b: &gtk::Box) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_uri_list_single_file_uri_returns_one_path() {
        let result = parse_uri_list("file:///tmp/foo.png");
        assert_eq!(result, vec![PathBuf::from("/tmp/foo.png")]);
    }

    #[test]
    fn parse_uri_list_multiple_file_uris_returns_all_in_order() {
        let result = parse_uri_list("file:///a\nfile:///b\nfile:///c");
        assert_eq!(result, vec![PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from("/c")]);
    }

    #[test]
    fn parse_uri_list_skips_non_file_schemes() {
        let result = parse_uri_list("file:///a\nhttps://example.com/b\nfile:///c");
        assert_eq!(result, vec![PathBuf::from("/a"), PathBuf::from("/c")]);
    }

    #[test]
    fn parse_uri_list_empty_string_returns_empty_vec() {
        let result = parse_uri_list("");
        assert!(result.is_empty());
    }

    #[test]
    fn parse_uri_list_decodes_percent_encoded_chars() {
        let result = parse_uri_list("file:///home/me/Screenshot%20from%202024.png");
        assert_eq!(result, vec![PathBuf::from("/home/me/Screenshot from 2024.png")]);
    }

    #[test]
    fn parse_uri_list_accepts_canonical_triple_slash_form() {
        let result = parse_uri_list("file:///etc/hosts");
        assert_eq!(result, vec![PathBuf::from("/etc/hosts")]);
    }

    #[test]
    fn parse_uri_list_skips_comment_lines_and_blanks() {
        let input = "# this is a comment\nfile:///a\n\n# another comment\nfile:///b\n";
        let result = parse_uri_list(input);
        assert_eq!(result, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn parse_uri_list_accepts_crlf_line_endings() {
        let result = parse_uri_list("file:///a\r\nfile:///b\r\n");
        assert_eq!(result, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    // --- chat_avatar_custom_path tests ---

    #[test]
    fn chat_avatar_custom_path_returns_some_for_set_path() {
        let c = ChatSummary {
            id: 0,
            key: String::new(),
            display_name: None,
            is_group: false,
            service: None,
            last_message_date: None,
            participants: vec![],
            unread: 0,
            custom_name: None,
            custom_avatar_path: Some("/some/path/avatar.png".into()),
        };
        let result = super::chat_avatar_custom_path(&c);
        assert_eq!(result, Some("/some/path/avatar.png"));
    }

    #[test]
    fn chat_avatar_custom_path_returns_none_when_unset() {
        let c = ChatSummary {
            id: 0,
            key: String::new(),
            display_name: None,
            is_group: false,
            service: None,
            last_message_date: None,
            participants: vec![],
            unread: 0,
            custom_name: None,
            custom_avatar_path: None,
        };
        let result = super::chat_avatar_custom_path(&c);
        assert_eq!(result, None);
    }

    #[test]
    fn chat_avatar_custom_path_filters_empty_string() {
        let c = ChatSummary {
            id: 0,
            key: String::new(),
            display_name: None,
            is_group: false,
            service: None,
            last_message_date: None,
            participants: vec![],
            unread: 0,
            custom_name: None,
            custom_avatar_path: Some("".into()),
        };
        let result = super::chat_avatar_custom_path(&c);
        assert_eq!(result, None);
    }

    #[test]
    fn chat_avatar_custom_path_filters_whitespace_only() {
        let c = ChatSummary {
            id: 0,
            key: String::new(),
            display_name: None,
            is_group: false,
            service: None,
            last_message_date: None,
            participants: vec![],
            unread: 0,
            custom_name: None,
            custom_avatar_path: Some("   ".into()),
        };
        let result = super::chat_avatar_custom_path(&c);
        assert_eq!(result, None);
    }

    #[test]
    fn chat_avatar_custom_path_preserves_leading_whitespace_when_not_empty() {
        let input = "  /x.png".to_string();
        let c = ChatSummary {
            id: 0,
            key: String::new(),
            display_name: None,
            is_group: false,
            service: None,
            last_message_date: None,
            participants: vec![],
            unread: 0,
            custom_name: None,
            custom_avatar_path: Some(input.clone()),
        };
        let result = super::chat_avatar_custom_path(&c);
        assert_eq!(result, Some(input.as_str()));
    }

    // --- chat_text_css tests ---
    //
    // These tests pin the pure string-generation behaviour: `chat_text_css(base_pt)`
    // returns a CSS rule whose `font-size` is `base_pt + text_scale::get()`.
    // The function lives in [`super::builder`](crate::ui::builder::chat_text_css).

    #[test]
    fn chat_text_css_single_base_reflects_offset() {
        let tmp = tempfile::tempdir().unwrap();
        crate::text_scale::set_data_dir_for_tests(tmp.path().to_path_buf());

        // Reset to default offset.
        crate::text_scale::set(0.0);

        let css = crate::ui::chat_text_css(13.0);
        assert!(
            css.contains("font-size: 13.00pt"),
            "at offset 0.0, expected 13.00pt, got: {css}"
        );

        // Change the offset — the function must reflect the new size.
        crate::text_scale::set(2.0);
        let css = crate::ui::chat_text_css(13.0);
        assert!(
            css.contains("font-size: 15.00pt"),
            "after offset +2.0, expected 15.00pt, got: {css}"
        );
    }

    #[test]
    fn chat_text_css_multiple_bases_all_reflect_offset() {
        let tmp = tempfile::tempdir().unwrap();
        crate::text_scale::set_data_dir_for_tests(tmp.path().to_path_buf());

        // Reset to default offset.
        crate::text_scale::set(0.0);

        let css10 = crate::ui::chat_text_css(10.0);
        let css12 = crate::ui::chat_text_css(12.0);
        let css13 = crate::ui::chat_text_css(13.0);
        assert!(css10.contains("font-size: 10.00pt"), "base 10 at offset 0");
        assert!(css12.contains("font-size: 12.00pt"), "base 12 at offset 0");
        assert!(css13.contains("font-size: 13.00pt"), "base 13 at offset 0");

        // Change offset — all bases reflect the change.
        crate::text_scale::set(1.5);
        let css10 = crate::ui::chat_text_css(10.0);
        let css12 = crate::ui::chat_text_css(12.0);
        let css13 = crate::ui::chat_text_css(13.0);
        assert!(css10.contains("font-size: 11.50pt"), "base 10 after +1.5");
        assert!(css12.contains("font-size: 13.50pt"), "base 12 after +1.5");
        assert!(css13.contains("font-size: 14.50pt"), "base 13 after +1.5");
    }
}

#[cfg(test)]
mod new_chat_payload_tests {
    use super::*;

    // ── None cases ──────────────────────────────────────────────

    #[test]
    fn empty_string_returns_none() {
        assert!(new_chat_payload("", "hi", "mailto:me@x.com").is_none());
    }

    #[test]
    fn whitespace_string_returns_none() {
        assert!(new_chat_payload("   ", "hi", "mailto:me@x.com").is_none());
    }

    #[test]
    fn no_digits_no_at_returns_none() {
        assert!(new_chat_payload("not-a-phone-or-email", "hi", "mailto:me@x.com").is_none());
    }

    // ── Phone: 10-digit US normalization ───────────────────────

    #[test]
    fn ten_digit_plain() {
        let (_chat, msg) = new_chat_payload("5551234567", "hi", "mailto:me@x.com").unwrap();
        assert!(msg.chat.participants.contains(&"tel:+15551234567".to_string()));
    }

    #[test]
    fn ten_digit_with_parens_and_dash() {
        let (_chat, msg) = new_chat_payload("(555) 123-4567", "hi", "mailto:me@x.com").unwrap();
        assert!(msg.chat.participants.contains(&"tel:+15551234567".to_string()));
    }

    #[test]
    fn ten_digit_with_spaces() {
        let (_chat, msg) = new_chat_payload("555 123 4567", "hi", "mailto:me@x.com").unwrap();
        assert!(msg.chat.participants.contains(&"tel:+15551234567".to_string()));
    }

    #[test]
    fn ten_digit_with_dashes() {
        let (_chat, msg) = new_chat_payload("555-123-4567", "hi", "mailto:me@x.com").unwrap();
        assert!(msg.chat.participants.contains(&"tel:+15551234567".to_string()));
    }

    // ── Phone: already prefixed ────────────────────────────────

    #[test]
    fn phone_with_plus() {
        let (_chat, msg) = new_chat_payload("+15551234567", "hi", "mailto:me@x.com").unwrap();
        assert!(msg.chat.participants.contains(&"tel:+15551234567".to_string()));
    }

    #[test]
    fn eleven_digit_with_dashes() {
        // "1-555-123-4567" is 11 digits -> tel:+15551234567 (no double 1)
        let (_chat, msg) = new_chat_payload("1-555-123-4567", "hi", "mailto:me@x.com").unwrap();
        assert!(msg.chat.participants.contains(&"tel:+15551234567".to_string()));
    }

    #[test]
    fn international_with_plus() {
        let (_chat, msg) = new_chat_payload("+442071234567", "hi", "mailto:me@x.com").unwrap();
        assert!(msg.chat.participants.contains(&"tel:+442071234567".to_string()));
    }

    #[test]
    fn tel_prefix_ten_digit() {
        let (_chat, msg) = new_chat_payload("tel:5551234567", "hi", "mailto:me@x.com").unwrap();
        assert!(msg.chat.participants.contains(&"tel:+15551234567".to_string()));
    }

    #[test]
    fn tel_prefix_with_plus() {
        let (_chat, msg) = new_chat_payload("tel:+15551234567", "hi", "mailto:me@x.com").unwrap();
        assert!(msg.chat.participants.contains(&"tel:+15551234567".to_string()));
    }

    // ── Email ──────────────────────────────────────────────────

    #[test]
    fn bare_email() {
        let (_chat, msg) = new_chat_payload("foo@bar.com", "hi", "mailto:me@x.com").unwrap();
        assert!(msg.chat.participants.contains(&"mailto:foo@bar.com".to_string()));
    }

    #[test]
    fn uppercase_email_is_lowercased() {
        let (_chat, msg) = new_chat_payload("FOO@BAR.COM", "hi", "mailto:me@x.com").unwrap();
        assert!(msg.chat.participants.contains(&"mailto:foo@bar.com".to_string()));
    }

    #[test]
    fn mailto_prefixed_email() {
        let (_chat, msg) = new_chat_payload("mailto:foo@bar.com", "hi", "mailto:me@x.com").unwrap();
        assert!(msg.chat.participants.contains(&"mailto:foo@bar.com".to_string()));
    }

    // ── IncomingMessage shape ──────────────────────────────────

    #[test]
    fn full_message_shape() {
        let my_handle = "mailto:me@example.com";
        let text = "hello";
        let (_chat, msg) = new_chat_payload("5551234567", text, my_handle).unwrap();

        // is_from_me
        assert!(msg.is_from_me, "is_from_me must be true");

        // sender
        assert_eq!(msg.sender.as_deref(), Some(my_handle), "sender must match my_handle");

        // text
        assert_eq!(msg.text.as_deref(), Some(text), "text must match input");

        // guid non-empty
        assert!(!msg.guid.is_empty(), "guid must be non-empty");

        // date within 60s of now
        let now = now_ms();
        let diff = (msg.date - now).abs();
        assert!(
            diff < 60_000,
            "date must be within 60s of now_ms(), diff was {}ms",
            diff,
        );

        // participants: exactly two, sorted, containing both handles
        assert_eq!(
            msg.chat.participants.len(),
            2,
            "participants must have exactly 2 entries",
        );
        assert!(
            msg.chat.participants.contains(&my_handle.to_string()),
            "participants must contain my_handle",
        );
        assert!(
            msg.chat.participants.contains(&"tel:+15551234567".to_string()),
            "participants must contain the normalized recipient",
        );
        // Verify ordering matches ChatRef::key() (sorted, lowercased, semicolon-joined)
        let expected_key = "mailto:me@example.com;tel:+15551234567";
        assert_eq!(msg.chat.key(), expected_key, "chat.key() must be stable sorted");

        // display_name == None
        assert!(msg.chat.display_name.is_none(), "display_name must be None for 1:1");

        // service == None
        assert!(msg.chat.service.is_none(), "chat.service must be None");

        // item_type == 0
        assert_eq!(msg.item_type, 0, "item_type must be 0 for normal text");
    }
}
