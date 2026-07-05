//! Pure in-memory contact matching and search.
//!
//! These functions operate over a `&[Contact]` slice with no I/O, no traits, and
//! no dependencies outside `crate::store`. They are the pure-logic core that the
//! UI uses to resolve a sender address to a display name and to power the
//! To-field autocomplete.

#![allow(dead_code)]

use crate::store::{ChatSummary, Contact, Store};
use async_trait::async_trait;

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Extract all ASCII digit characters from a string.
fn digits(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Strip a `mailto:` scheme prefix (if present) and lowercase the result.
fn strip_mailto(s: &str) -> String {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("mailto:") {
        rest.to_lowercase()
    } else {
        s.to_lowercase()
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Does `query` (a normalized `tel:+…` or `mailto:…` URI) refer to the same
/// destination as any of the contact's addresses?
///
/// Dispatch on the query's URI scheme:
///
/// * `mailto:…` – case-insensitive exact match. Strip the `mailto:` prefix from
///   the query (lowercase the rest). For each contact address of `kind == "email"`,
///   strip a `mailto:` prefix if present, lowercase, and compare.
///
/// * `tel:…` – tolerant digit-suffix match. Reduce the query and each contact
///   phone address to digits only. The query matches if one digit string is a
///   suffix of the other (tolerates missing/extra country codes and arbitrary
///   formatting). Empty digit strings never match.
///
/// A phone query never matches an email address and vice versa.
pub fn contact_matches_address(contact: &Contact, query: &str) -> bool {
    let query = query.trim();

    if let Some(rest) = query.strip_prefix("mailto:") {
        // Email query: case-insensitive exact match
        let query_email = rest.to_lowercase();
        contact.addresses.iter().any(|addr| {
            if addr.kind != "email" {
                return false;
            }
            let addr_email = strip_mailto(&addr.value);
            query_email == addr_email
        })
    } else if let Some(rest) = query.strip_prefix("tel:") {
        // Phone query: digits-suffix tolerant match
        let query_digits = digits(rest);
        if query_digits.is_empty() {
            return false;
        }
        contact.addresses.iter().any(|addr| {
            if addr.kind != "phone" {
                return false;
            }
            let addr_digits = digits(&addr.value);
            if addr_digits.is_empty() {
                return false;
            }
            addr_digits.ends_with(&query_digits) || query_digits.ends_with(&addr_digits)
        })
    } else {
        false
    }
}

/// Return a reference to the **first** contact in slice order for which
/// `contact_matches_address` is true, or `None`.
pub fn find_contact<'a>(contacts: &'a [Contact], query: &str) -> Option<&'a Contact> {
    contacts.iter().find(|c| contact_matches_address(c, query))
}

/// Free-text autocomplete search over contacts.
///
/// A contact matches if **any** of these hold (all case-insensitive):
///
/// * its `display_name` contains the query (substring);
/// * any of its **phone** addresses, reduced to digits, contains the query's
///   digits (a query with no digits never matches by phone);
/// * any of its **email** addresses, with `mailto:` prefix stripped, contains
///   the query as a substring (only checked when the query has no digits, so
///   that a digit-only query like `"555"` does not accidentally match an email
///   address that happens to contain those digits).
///
/// An empty or whitespace-only query returns an empty `Vec`.
pub fn search_contacts<'a>(contacts: &'a [Contact], query: &str) -> Vec<&'a Contact> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }

    let query_lower = query.to_lowercase();
    let query_digits = digits(query);
    let has_digits = !query_digits.is_empty();

    contacts
        .iter()
        .filter(|c| {
            // 1. Display-name substring match (case-insensitive)
            if c.display_name.to_lowercase().contains(&query_lower) {
                return true;
            }

            // 2. Phone-digit containment (only meaningful when query has digits)
            if has_digits {
                for addr in &c.addresses {
                    if addr.kind == "phone" {
                        let addr_digits = digits(&addr.value);
                        if addr_digits.contains(&query_digits) {
                            return true;
                        }
                    }
                }
            }

            // 3. Email substring match (only for non-digit queries, to prevent
            //    a phone-digit query from crossing into email addresses)
            if !has_digits {
                for addr in &c.addresses {
                    if addr.kind == "email" {
                        let addr_email = strip_mailto(&addr.value);
                        if addr_email.contains(&query_lower) {
                            return true;
                        }
                    }
                }
            }

            false
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Contact-aware title / display name helpers
// ---------------------------------------------------------------------------

/// Strip a leading `mailto:` or `tel:` scheme prefix from an address,
/// returning the remainder unchanged. Neither prefix → return the input
/// as-is.
fn pretty_addr(a: &str) -> String {
    if let Some(rest) = a.strip_prefix("mailto:") {
        rest.to_string()
    } else if let Some(rest) = a.strip_prefix("tel:") {
        rest.to_string()
    } else {
        a.to_string()
    }
}

/// Build a display title for a chat, applying the following precedence
/// (first non-empty wins):
///
/// 1. `chat.custom_name` (if non-empty after trimming).
/// 2. `chat.display_name` (if non-empty — no trim).
/// 3. If the chat has **exactly one** non-self participant, look up that
///    participant's address via `find_contact` and return the contact's
///    `display_name` if found. Group chats (2+ non-self) skip this step.
/// 4. Non-self participants → `pretty_addr` each, joined by `", "`.
/// 5. All participants → `pretty_addr` each, joined by `", "`.
/// 6. `chat.key` (only when `participants` is empty).
pub fn chat_display_title(
    chat: &ChatSummary,
    my_handles: &[String],
    contacts: &[Contact],
) -> String {
    // 1. custom_name (trimmed non-empty check)
    if let Some(name) = &chat.custom_name {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    // 2. display_name (plain is_empty check)
    if let Some(name) = &chat.display_name {
        if !name.is_empty() {
            return name.clone();
        }
    }

    // Predicate: is a participant address one of "my" handles?
    let is_me = |addr: &str| -> bool { my_handles.iter().any(|h| h.eq_ignore_ascii_case(addr)) };

    let non_self: Vec<&String> = chat
        .participants
        .iter()
        .filter(|p| !is_me(p.as_str()))
        .collect();

    // 3. Exactly one non-self → contact display name
    if non_self.len() == 1 {
        #[allow(clippy::collapsible_if)]
        if let Some(contact) = find_contact(contacts, non_self[0]) {
            if !contact.display_name.is_empty() {
                return contact.display_name.clone();
            }
        }
    }

    // 4. Non-self participants → pretty_addr, join
    if !non_self.is_empty() {
        return non_self
            .iter()
            .map(|a| pretty_addr(a))
            .collect::<Vec<_>>()
            .join(", ");
    }

    // 5. All participants → pretty_addr, join
    if !chat.participants.is_empty() {
        return chat
            .participants
            .iter()
            .map(|a| pretty_addr(a))
            .collect::<Vec<_>>()
            .join(", ");
    }

    // 6. Empty participants → chat.key
    chat.key.clone()
}

/// Resolve a participant address to a human-readable display name.
///
/// 1. If `address` matches any of `my_handles` (case-insensitive) → `"You"`.
/// 2. If `find_contact` yields a contact → its `display_name`.
/// 3. Otherwise → `pretty_addr(address)`.
pub fn participant_display_name(
    address: &str,
    my_handles: &[String],
    contacts: &[Contact],
) -> String {
    // 1. Self check (case-insensitive)
    if my_handles.iter().any(|h| h.eq_ignore_ascii_case(address)) {
        return "You".to_string();
    }

    // 2. Contact lookup
    if let Some(contact) = find_contact(contacts, address) {
        return contact.display_name.clone();
    }

    // 3. Pretty-print the address
    pretty_addr(address)
}

/// Return the contact photo bytes to render for a chat, or `None` when no
/// contact photo is available (caller falls back to custom-avatar-path-then-initials).
///
/// Precedence:
///
/// 1. `chat.custom_avatar_path` — if it's `Some(s)` where `s.trim()` is
///    non-empty, return `None` (the caller handles the custom photo from disk).
/// 2. If `custom_avatar_path` is `None` or whitespace-only (treated as unset),
///    proceed.
/// 3. For **1:1 chats only** (exactly one non-self participant), look up that
///    participant's address via `find_contact`; if a contact is found AND its
///    `avatar` is `Some(bytes)` where `bytes` is non-empty, return
///    `Some(&bytes[..])`.
/// 4. For **group chats** (2+ non-self participants), return `None`.
/// 5. If no contact matches, or contact has no avatar, or empty avatar bytes,
///    return `None`.
/// 6. A self-only chat (no non-self participant) returns `None`.
pub fn chat_avatar_bytes<'a>(
    chat: &ChatSummary,
    my_handles: &[String],
    contacts: &'a [Contact],
) -> Option<&'a [u8]> {
    // 1. custom_avatar_path — non-empty after trim → None (caller handles custom)
    if let Some(path) = &chat.custom_avatar_path {
        if !path.trim().is_empty() {
            return None;
        }
    }

    // Predicate: is a participant address one of "my" handles?
    let is_me = |addr: &str| -> bool { my_handles.iter().any(|h| h.eq_ignore_ascii_case(addr)) };

    let non_self: Vec<&String> = chat
        .participants
        .iter()
        .filter(|p| !is_me(p.as_str()))
        .collect();

    // 3. Exactly one non-self participant → 1:1, try contact photo
    if non_self.len() == 1 {
        if let Some(contact) = find_contact(contacts, non_self[0]) {
            if let Some(bytes) = &contact.avatar {
                if !bytes.is_empty() {
                    return Some(&bytes[..]);
                }
            }
        }
    }

    // 2+ non-self participants, 0 non-self, or no matching contact/photo → None
    None
}

// ---------------------------------------------------------------------------
// Contact cache refresh orchestration
// ---------------------------------------------------------------------------

/// An external source of contacts that can be fetched to populate the local
/// contact cache.
#[allow(dead_code)]
#[async_trait]
pub trait ContactSource: Send + Sync {
    /// Fetch all contacts from the source. Returning an error is a graceful
    /// degradation — the cache is left untouched.
    async fn fetch_all(&self) -> anyhow::Result<Vec<Contact>>;
}

/// Refresh the in-app contact cache from `source`.
///
/// 1. `fetch_all` from the source — on error the cache is **not** touched.
/// 2. Clear the existing cache.
/// 3. Upsert each fetched contact.
#[allow(dead_code)]
pub async fn refresh_cache(source: &dyn ContactSource, store: &Store) -> anyhow::Result<()> {
    let contacts = source.fetch_all().await?;
    store.clear_contacts().await?;
    for contact in contacts {
        store.upsert_contact(contact).await?;
    }
    Ok(())
}

/// Refresh the in-app contact cache from `source` and return the fetched
/// contacts so the caller can populate an in-memory cache without a second
/// DB round-trip.
#[allow(dead_code)]
pub async fn refresh_and_collect(
    source: &dyn ContactSource,
    store: &Store,
) -> anyhow::Result<Vec<Contact>> {
    let contacts = source.fetch_all().await?;
    store.clear_contacts().await?;
    for c in &contacts {
        store.upsert_contact(c.clone()).await?;
    }
    Ok(contacts)
}

#[cfg(test)]
mod tests {
    use super::{
        chat_avatar_bytes, chat_display_title, contact_matches_address, find_contact,
        participant_display_name, refresh_cache, search_contacts,
    };
    use crate::store::{ChatSummary, Contact, ContactAddress, Store};
    use async_trait::async_trait;

    // ------------------------------------------------------------------
    // contact_matches_address
    // ------------------------------------------------------------------

    #[test]
    fn email_match_exact() {
        let contact = Contact {
            uid: "u1".into(),
            display_name: "Alice".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "mailto:alice@example.com".into(),
                kind: "email".into(),
            }],
        };
        assert!(
            contact_matches_address(&contact, "mailto:alice@example.com"),
            "exact email should match",
        );
    }

    #[test]
    fn email_match_case_insensitive() {
        let contact = Contact {
            uid: "u2".into(),
            display_name: "Bob".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "mailto:alice@example.com".into(),
                kind: "email".into(),
            }],
        };
        assert!(
            contact_matches_address(&contact, "mailto:ALICE@Example.COM"),
            "email matching should be case-insensitive",
        );
    }

    #[test]
    fn email_no_match_different_address() {
        let contact = Contact {
            uid: "u3".into(),
            display_name: "Bob".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "mailto:bob@example.com".into(),
                kind: "email".into(),
            }],
        };
        assert!(
            !contact_matches_address(&contact, "mailto:alice@example.com"),
            "different email should not match",
        );
    }

    #[test]
    fn email_match_without_prefix_on_contact() {
        let contact = Contact {
            uid: "u4".into(),
            display_name: "Alice".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "alice@example.com".into(), // no mailto: prefix
                kind: "email".into(),
            }],
        };
        assert!(
            contact_matches_address(&contact, "mailto:alice@example.com"),
            "should match contact address even when it lacks the mailto: prefix",
        );
    }

    #[test]
    fn phone_match_tolerant_to_formatting() {
        let contact = Contact {
            uid: "u5".into(),
            display_name: "Charlie".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "+1 (555) 555-0100".into(),
                kind: "phone".into(),
            }],
        };
        assert!(
            contact_matches_address(&contact, "tel:+15555550100"),
            "should match formatted phone against normalized tel: URI (digits-suffix)",
        );
    }

    #[test]
    fn phone_match_short_suffix() {
        let contact = Contact {
            uid: "u6".into(),
            display_name: "Diana".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "555-0100".into(),
                kind: "phone".into(),
            }],
        };
        assert!(
            contact_matches_address(&contact, "tel:+15555550100"),
            "short local number with 7 digits should match as suffix of full number",
        );
    }

    #[test]
    fn phone_no_match_different_number() {
        let contact = Contact {
            uid: "u7".into(),
            display_name: "Eve".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "tel:+15555550999".into(),
                kind: "phone".into(),
            }],
        };
        assert!(
            !contact_matches_address(&contact, "tel:+15555550100"),
            "different phone number should not match",
        );
    }

    #[test]
    fn phone_query_does_not_match_email_address() {
        let contact = Contact {
            uid: "u8".into(),
            display_name: "Grace".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "mailto:grace@example.com".into(),
                kind: "email".into(),
            }],
        };
        assert!(
            !contact_matches_address(&contact, "tel:+15555550100"),
            "phone query should never match an email address",
        );
    }

    #[test]
    fn email_query_does_not_match_phone_address() {
        let contact = Contact {
            uid: "u9".into(),
            display_name: "Heidi".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "tel:+15555550100".into(),
                kind: "phone".into(),
            }],
        };
        assert!(
            !contact_matches_address(&contact, "mailto:heidi@example.com"),
            "email query should never match a phone address",
        );
    }

    // ------------------------------------------------------------------
    // find_contact
    // ------------------------------------------------------------------

    #[test]
    fn find_by_address_returns_first_matching_contact() {
        let contacts = vec![
            Contact {
                uid: "a".into(),
                display_name: "Alice".into(),
                avatar: None,
                addresses: vec![ContactAddress {
                    value: "mailto:alice@example.com".into(),
                    kind: "email".into(),
                }],
            },
            Contact {
                uid: "b".into(),
                display_name: "Bob".into(),
                avatar: None,
                addresses: vec![ContactAddress {
                    value: "mailto:bob@example.com".into(),
                    kind: "email".into(),
                }],
            },
        ];
        let found = find_contact(&contacts, "mailto:bob@example.com");
        assert_eq!(
            found.map(|c| c.uid.as_str()),
            Some("b"),
            "should return the first (and only) matching contact by uid",
        );
    }

    #[test]
    fn find_by_address_returns_none_when_no_match() {
        let contacts = vec![Contact {
            uid: "c".into(),
            display_name: "Carol".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "mailto:carol@example.com".into(),
                kind: "email".into(),
            }],
        }];
        let found = find_contact(&contacts, "mailto:zzz@example.com");
        assert!(found.is_none(), "no contact has this address");
    }

    // ------------------------------------------------------------------
    // search_contacts
    // ------------------------------------------------------------------

    #[test]
    fn search_by_name_substring_case_insensitive() {
        let contacts = vec![Contact {
            uid: "d".into(),
            display_name: "Alice".into(),
            avatar: None,
            addresses: vec![],
        }];
        let results = search_contacts(&contacts, "ali");
        assert_eq!(
            results.len(),
            1,
            "\"ali\" should match Alice by name substring",
        );
        assert_eq!(results[0].uid, "d");
    }

    #[test]
    fn search_by_phone_digits() {
        let contacts = vec![Contact {
            uid: "e".into(),
            display_name: "Eve".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "+1 (555) 555-0100".into(),
                kind: "phone".into(),
            }],
        }];
        let results = search_contacts(&contacts, "555");
        assert_eq!(
            results.len(),
            1,
            "\"555\" should match a phone with digits containing 555",
        );
        assert_eq!(results[0].uid, "e");
    }

    #[test]
    fn search_by_email_substring() {
        let contacts = vec![Contact {
            uid: "f".into(),
            display_name: "Frank".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "mailto:frank@example.com".into(),
                kind: "email".into(),
            }],
        }];
        let results = search_contacts(&contacts, "frank@");
        assert_eq!(
            results.len(),
            1,
            "\"frank@\" should match the email address substring",
        );
        assert_eq!(results[0].uid, "f");
    }

    #[test]
    fn search_empty_query_returns_nothing() {
        let contacts = vec![Contact {
            uid: "g".into(),
            display_name: "Grace".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "mailto:grace@example.com".into(),
                kind: "email".into(),
            }],
        }];
        assert!(
            search_contacts(&contacts, "").is_empty(),
            "empty query should return nothing",
        );
        assert!(
            search_contacts(&contacts, "   ").is_empty(),
            "whitespace-only query should return nothing",
        );
    }

    #[test]
    fn search_no_match_returns_empty() {
        let contacts = vec![Contact {
            uid: "h".into(),
            display_name: "Heidi".into(),
            avatar: None,
            addresses: vec![],
        }];
        let results = search_contacts(&contacts, "zzz");
        assert!(results.is_empty(), "\"zzz\" matches no contact");
    }

    #[test]
    fn search_returns_multiple_in_order() {
        let contacts = vec![
            Contact {
                uid: "i".into(),
                display_name: "Alice".into(),
                avatar: None,
                addresses: vec![],
            },
            Contact {
                uid: "j".into(),
                display_name: "Aaron".into(),
                avatar: None,
                addresses: vec![],
            },
        ];
        let results = search_contacts(&contacts, "a");
        assert_eq!(results.len(), 2, "both names contain 'a'");
        assert_eq!(results[0].uid, "i", "Alice is first in input order");
        assert_eq!(results[1].uid, "j", "Aaron is second in input order");
    }

    #[test]
    fn search_does_not_match_kind_crossed() {
        let contacts = vec![Contact {
            uid: "k".into(),
            display_name: "Kate".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "mailto:555@example.com".into(),
                kind: "email".into(),
            }],
        }];
        // Query "555" should NOT match a contact whose only "555" lives in an
        // email address; phone-digit search only looks at phone addresses and
        // email-substring search only looks at email addresses.
        let results = search_contacts(&contacts, "555");
        assert!(
            results.is_empty(),
            "\"555\" in an email address should not match a phone-digit search",
        );
    }

    // ------------------------------------------------------------------
    // chat_display_title
    // ------------------------------------------------------------------

    #[test]
    fn custom_name_wins_over_contact() {
        let contacts = vec![Contact {
            uid: "c1".into(),
            display_name: "Carol".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "tel:+15555550100".into(),
                kind: "phone".into(),
            }],
        }];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        let chat = ChatSummary {
            id: 0,
            key: "chat-key".into(),
            display_name: None,
            is_group: false,
            service: None,
            last_message_date: None,
            participants: vec!["mailto:me@x.com".into(), "tel:+15555550100".into()],
            unread: 0,
            custom_name: Some("Mom".into()),
            custom_avatar_path: None,
        };
        assert_eq!(
            chat_display_title(&chat, &my_handles, &contacts),
            "Mom",
        );
    }

    #[test]
    fn custom_name_wins_over_display_name() {
        let contacts = vec![];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        let chat = ChatSummary {
            id: 0,
            key: "chat-key".into(),
            display_name: Some("Apple Group".into()),
            is_group: false,
            service: None,
            last_message_date: None,
            participants: vec!["mailto:me@x.com".into(), "tel:+15555550100".into()],
            unread: 0,
            custom_name: Some("Mom".into()),
            custom_avatar_path: None,
        };
        assert_eq!(
            chat_display_title(&chat, &my_handles, &contacts),
            "Mom",
        );
    }

    #[test]
    fn display_name_wins_for_group_with_contacts() {
        let contacts = vec![
            Contact {
                uid: "c1".into(),
                display_name: "Alice".into(),
                avatar: None,
                addresses: vec![ContactAddress {
                    value: "tel:+15555550100".into(),
                    kind: "phone".into(),
                }],
            },
            Contact {
                uid: "c2".into(),
                display_name: "Bob".into(),
                avatar: None,
                addresses: vec![ContactAddress {
                    value: "mailto:bob@example.com".into(),
                    kind: "email".into(),
                }],
            },
        ];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        let chat = ChatSummary {
            id: 0,
            key: "chat-key".into(),
            display_name: Some("Family".into()),
            is_group: true,
            service: None,
            last_message_date: None,
            participants: vec![
                "mailto:me@x.com".into(),
                "tel:+15555550100".into(),
                "mailto:bob@example.com".into(),
            ],
            unread: 0,
            custom_name: None,
            custom_avatar_path: None,
        };
        assert_eq!(
            chat_display_title(&chat, &my_handles, &contacts),
            "Family",
        );
    }

    #[test]
    fn contact_name_used_for_1to1_without_custom_or_display() {
        let contacts = vec![Contact {
            uid: "c1".into(),
            display_name: "Carol".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "tel:+15555550100".into(),
                kind: "phone".into(),
            }],
        }];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        let chat = ChatSummary {
            id: 0,
            key: "chat-key".into(),
            display_name: None,
            is_group: false,
            service: None,
            last_message_date: None,
            participants: vec!["mailto:me@x.com".into(), "tel:+15555550100".into()],
            unread: 0,
            custom_name: None,
            custom_avatar_path: None,
        };
        assert_eq!(
            chat_display_title(&chat, &my_handles, &contacts),
            "Carol",
        );
    }

    #[test]
    fn pretty_addr_fallback_when_no_contact_for_1to1() {
        let contacts = vec![];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        let chat = ChatSummary {
            id: 0,
            key: "chat-key".into(),
            display_name: None,
            is_group: false,
            service: None,
            last_message_date: None,
            participants: vec!["mailto:me@x.com".into(), "tel:+15555550100".into()],
            unread: 0,
            custom_name: None,
            custom_avatar_path: None,
        };
        assert_eq!(
            chat_display_title(&chat, &my_handles, &contacts),
            "+15555550100",
        );
    }

    #[test]
    fn group_without_display_name_uses_pretty_addr_list_not_contacts() {
        let contacts = vec![
            Contact {
                uid: "c1".into(),
                display_name: "Alice".into(),
                avatar: None,
                addresses: vec![ContactAddress {
                    value: "tel:+15555550100".into(),
                    kind: "phone".into(),
                }],
            },
            Contact {
                uid: "c2".into(),
                display_name: "Bob".into(),
                avatar: None,
                addresses: vec![ContactAddress {
                    value: "mailto:bob@example.com".into(),
                    kind: "email".into(),
                }],
            },
        ];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        let chat = ChatSummary {
            id: 0,
            key: "chat-key".into(),
            display_name: None,
            is_group: true,
            service: None,
            last_message_date: None,
            participants: vec![
                "mailto:me@x.com".into(),
                "tel:+15555550100".into(),
                "mailto:bob@example.com".into(),
            ],
            unread: 0,
            custom_name: None,
            custom_avatar_path: None,
        };
        // Non-self participants (in order): tel:+15555550100 → "+15555550100",
        //                                   mailto:bob@example.com → "bob@example.com"
        assert_eq!(
            chat_display_title(&chat, &my_handles, &contacts),
            "+15555550100, bob@example.com",
        );
    }

    #[test]
    fn custom_name_empty_after_trim_falls_through() {
        let contacts = vec![Contact {
            uid: "c1".into(),
            display_name: "Carol".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "tel:+15555550100".into(),
                kind: "phone".into(),
            }],
        }];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        let chat = ChatSummary {
            id: 0,
            key: "chat-key".into(),
            display_name: None,
            is_group: false,
            service: None,
            last_message_date: None,
            participants: vec!["mailto:me@x.com".into(), "tel:+15555550100".into()],
            unread: 0,
            custom_name: Some("   ".into()),
            custom_avatar_path: None,
        };
        assert_eq!(
            chat_display_title(&chat, &my_handles, &contacts),
            "Carol",
        );
    }

    #[test]
    fn self_only_chat_falls_to_pretty_addr() {
        let contacts = vec![];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        let chat = ChatSummary {
            id: 0,
            key: "chat-key".into(),
            display_name: None,
            is_group: false,
            service: None,
            last_message_date: None,
            participants: vec!["mailto:me@x.com".into()],
            unread: 0,
            custom_name: None,
            custom_avatar_path: None,
        };
        assert_eq!(
            chat_display_title(&chat, &my_handles, &contacts),
            "me@x.com",
        );
    }

    // ------------------------------------------------------------------
    // participant_display_name
    // ------------------------------------------------------------------

    #[test]
    fn my_handle_returns_you() {
        let contacts = vec![];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        assert_eq!(
            participant_display_name("mailto:me@x.com", &my_handles, &contacts),
            "You",
        );
    }

    #[test]
    fn my_handle_case_insensitive() {
        let contacts = vec![];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        assert_eq!(
            participant_display_name("MAILTO:Me@X.COM", &my_handles, &contacts),
            "You",
        );
    }

    #[test]
    fn contact_match_returns_contact_name() {
        let contacts = vec![Contact {
            uid: "c1".into(),
            display_name: "Carol".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "tel:+15555550100".into(),
                kind: "phone".into(),
            }],
        }];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        assert_eq!(
            participant_display_name("tel:+15555550100", &my_handles, &contacts),
            "Carol",
        );
    }

    #[test]
    fn no_contact_returns_pretty_addr() {
        let contacts = vec![];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        assert_eq!(
            participant_display_name("mailto:alice@example.com", &my_handles, &contacts),
            "alice@example.com",
        );
    }

    // ------------------------------------------------------------------
    // chat_avatar_bytes — contact photo precedence
    // ------------------------------------------------------------------

    #[test]
    fn chat_avatar_custom_path_set_returns_none() {
        let contacts = vec![Contact {
            uid: "alice".into(),
            display_name: "Alice".into(),
            avatar: Some(b"alice-photo-bytes".to_vec()),
            addresses: vec![ContactAddress {
                value: "mailto:alice@example.com".into(),
                kind: "email".into(),
            }],
        }];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        let chat = ChatSummary {
            id: 0,
            key: "chat-key".into(),
            display_name: None,
            is_group: false,
            service: None,
            last_message_date: None,
            participants: vec![
                "mailto:me@x.com".into(),
                "mailto:alice@example.com".into(),
            ],
            unread: 0,
            custom_name: None,
            custom_avatar_path: Some("/path/to/face.png".into()),
        };
        assert_eq!(
            chat_avatar_bytes(&chat, &my_handles, &contacts),
            None,
            "custom path set should return None (caller handles custom)",
        );
    }

    #[test]
    fn chat_avatar_custom_path_empty_returns_contact_photo() {
        let contacts = vec![Contact {
            uid: "alice".into(),
            display_name: "Alice".into(),
            avatar: Some(b"alice-photo-bytes".to_vec()),
            addresses: vec![ContactAddress {
                value: "mailto:alice@example.com".into(),
                kind: "email".into(),
            }],
        }];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        let chat = ChatSummary {
            id: 0,
            key: "chat-key".into(),
            display_name: None,
            is_group: false,
            service: None,
            last_message_date: None,
            participants: vec![
                "mailto:me@x.com".into(),
                "mailto:alice@example.com".into(),
            ],
            unread: 0,
            custom_name: None,
            custom_avatar_path: None,
        };
        assert_eq!(
            chat_avatar_bytes(&chat, &my_handles, &contacts),
            contacts[0].avatar.as_deref(),
            "no custom path should return the contact photo",
        );
    }

    #[test]
    fn chat_avatar_group_returns_none_even_with_photos() {
        let contacts = vec![
            Contact {
                uid: "alice".into(),
                display_name: "Alice".into(),
                avatar: Some(b"alice-photo".to_vec()),
                addresses: vec![ContactAddress {
                    value: "mailto:alice@example.com".into(),
                    kind: "email".into(),
                }],
            },
            Contact {
                uid: "bob".into(),
                display_name: "Bob".into(),
                avatar: Some(b"bob-photo".to_vec()),
                addresses: vec![ContactAddress {
                    value: "mailto:bob@example.com".into(),
                    kind: "email".into(),
                }],
            },
        ];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        let chat = ChatSummary {
            id: 0,
            key: "chat-key".into(),
            display_name: None,
            is_group: true,
            service: None,
            last_message_date: None,
            participants: vec![
                "mailto:me@x.com".into(),
                "mailto:alice@example.com".into(),
                "mailto:bob@example.com".into(),
            ],
            unread: 0,
            custom_name: None,
            custom_avatar_path: None,
        };
        assert_eq!(
            chat_avatar_bytes(&chat, &my_handles, &contacts),
            None,
            "group chat should never return a single contact's photo",
        );
    }

    #[test]
    fn chat_avatar_1to1_no_matching_contact_returns_none() {
        let contacts = vec![];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        let chat = ChatSummary {
            id: 0,
            key: "chat-key".into(),
            display_name: None,
            is_group: false,
            service: None,
            last_message_date: None,
            participants: vec![
                "mailto:me@x.com".into(),
                "mailto:unknown@example.com".into(),
            ],
            unread: 0,
            custom_name: None,
            custom_avatar_path: None,
        };
        assert_eq!(
            chat_avatar_bytes(&chat, &my_handles, &contacts),
            None,
            "no matching contact should return None",
        );
    }

    #[test]
    fn chat_avatar_1to1_matching_contact_no_photo_returns_none() {
        let contacts = vec![Contact {
            uid: "alice".into(),
            display_name: "Alice".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "mailto:alice@example.com".into(),
                kind: "email".into(),
            }],
        }];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        let chat = ChatSummary {
            id: 0,
            key: "chat-key".into(),
            display_name: None,
            is_group: false,
            service: None,
            last_message_date: None,
            participants: vec![
                "mailto:me@x.com".into(),
                "mailto:alice@example.com".into(),
            ],
            unread: 0,
            custom_name: None,
            custom_avatar_path: None,
        };
        assert_eq!(
            chat_avatar_bytes(&chat, &my_handles, &contacts),
            None,
            "contact without avatar should return None",
        );
    }

    #[test]
    fn chat_avatar_custom_path_whitespace_returns_contact_photo() {
        let contacts = vec![Contact {
            uid: "alice".into(),
            display_name: "Alice".into(),
            avatar: Some(b"alice-photo".to_vec()),
            addresses: vec![ContactAddress {
                value: "mailto:alice@example.com".into(),
                kind: "email".into(),
            }],
        }];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        let chat = ChatSummary {
            id: 0,
            key: "chat-key".into(),
            display_name: None,
            is_group: false,
            service: None,
            last_message_date: None,
            participants: vec![
                "mailto:me@x.com".into(),
                "mailto:alice@example.com".into(),
            ],
            unread: 0,
            custom_name: None,
            custom_avatar_path: Some("   ".into()),
        };
        assert_eq!(
            chat_avatar_bytes(&chat, &my_handles, &contacts),
            contacts[0].avatar.as_deref(),
            "whitespace-only custom path should be treated as unset and return contact photo",
        );
    }

    #[test]
    fn chat_avatar_self_only_chat_returns_none() {
        let contacts = vec![];
        let my_handles = vec!["mailto:me@x.com".to_string()];
        let chat = ChatSummary {
            id: 0,
            key: "chat-key".into(),
            display_name: None,
            is_group: false,
            service: None,
            last_message_date: None,
            participants: vec!["mailto:me@x.com".into()],
            unread: 0,
            custom_name: None,
            custom_avatar_path: None,
        };
        assert_eq!(
            chat_avatar_bytes(&chat, &my_handles, &contacts),
            None,
            "self-only chat should return None since there is no non-self participant",
        );
    }

    // ------------------------------------------------------------------
    // ContactSource trait + refresh_cache orchestration
    // ------------------------------------------------------------------

    struct FakeContactSource {
        contacts: Vec<Contact>,
        error: Option<String>,
    }

    #[async_trait]
    impl super::ContactSource for FakeContactSource {
        async fn fetch_all(&self) -> anyhow::Result<Vec<Contact>> {
            if let Some(msg) = &self.error {
                Err(anyhow::anyhow!("{}", msg))
            } else {
                Ok(self.contacts.clone())
            }
        }
    }

    #[tokio::test]
    async fn refresh_cache_populates_from_source() {
        let store = Store::open_in_memory().await.unwrap();
        let alice = Contact {
            uid: "u1".into(),
            display_name: "Alice".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "tel:+1".into(),
                kind: "phone".into(),
            }],
        };
        let bob = Contact {
            uid: "u2".into(),
            display_name: "Bob".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "tel:+2".into(),
                kind: "phone".into(),
            }],
        };
        let source = FakeContactSource {
            contacts: vec![alice, bob],
            error: None,
        };
        refresh_cache(&source, &store).await.unwrap();

        let found = store.lookup_contact_by_addr("tel:+1".into()).await.unwrap();
        assert!(found.is_some(), "Alice should be found after refresh");
        assert_eq!(found.unwrap().display_name, "Alice");

        let found = store.lookup_contact_by_addr("tel:+2".into()).await.unwrap();
        assert!(found.is_some(), "Bob should be found after refresh");
        assert_eq!(found.unwrap().display_name, "Bob");
    }

    #[tokio::test]
    async fn refresh_cache_replaces_existing_cache() {
        let store = Store::open_in_memory().await.unwrap();
        let old = Contact {
            uid: "u1".into(),
            display_name: "Old Alice".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "tel:+1".into(),
                kind: "phone".into(),
            }],
        };
        store.upsert_contact(old).await.unwrap();

        let new = Contact {
            uid: "u1".into(),
            display_name: "New Alice".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "tel:+1".into(),
                kind: "phone".into(),
            }],
        };
        let source = FakeContactSource {
            contacts: vec![new],
            error: None,
        };
        refresh_cache(&source, &store).await.unwrap();

        let found = store.lookup_contact_by_addr("tel:+1".into()).await.unwrap();
        assert!(found.is_some(), "Alice should be found after refresh");
        assert_eq!(found.unwrap().display_name, "New Alice");
    }

    #[tokio::test]
    async fn refresh_cache_empty_source_clears_cache() {
        let store = Store::open_in_memory().await.unwrap();
        let alice = Contact {
            uid: "u1".into(),
            display_name: "Alice".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "tel:+1".into(),
                kind: "phone".into(),
            }],
        };
        store.upsert_contact(alice).await.unwrap();

        let source = FakeContactSource {
            contacts: vec![],
            error: None,
        };
        refresh_cache(&source, &store).await.unwrap();

        let found = store.lookup_contact_by_addr("tel:+1".into()).await.unwrap();
        assert!(
            found.is_none(),
            "cache should be empty after refresh with empty source",
        );
    }

    #[tokio::test]
    async fn refresh_cache_source_error_leaves_cache_unchanged() {
        let store = Store::open_in_memory().await.unwrap();
        let alice = Contact {
            uid: "u1".into(),
            display_name: "Alice".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "tel:+1".into(),
                kind: "phone".into(),
            }],
        };
        store.upsert_contact(alice).await.unwrap();

        let source = FakeContactSource {
            contacts: vec![],
            error: Some("transient failure".into()),
        };
        assert!(
            refresh_cache(&source, &store).await.is_err(),
            "refresh should fail when source errors",
        );

        let found = store.lookup_contact_by_addr("tel:+1".into()).await.unwrap();
        assert!(found.is_some(), "Alice should survive a source error");
        assert_eq!(found.unwrap().display_name, "Alice");
    }

    #[tokio::test]
    async fn refresh_cache_source_error_propagates() {
        let store = Store::open_in_memory().await.unwrap();
        let source = FakeContactSource {
            contacts: vec![],
            error: Some("eds down".into()),
        };
        let err = refresh_cache(&source, &store).await.unwrap_err();
        assert!(
            err.to_string().contains("eds down"),
            "error should contain source error message: {}",
            err,
        );
    }
}
