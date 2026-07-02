use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::store::NewMessage;

/// A pending notification entry for a single chat.
#[derive(Clone, Debug)]
pub struct PendingEntry {
    pub first_pending: Instant,
    pub last_message: NewMessage,
}

/// Result of [`PendingNotifications::insert_or_replace`].
#[derive(Debug)]
pub enum InsertResult {
    /// Schedule a timer for this instant.
    Schedule(Instant),
    /// Fire immediately with this message (max_age was exceeded).
    #[allow(dead_code)]
    Fire(NewMessage),
}

/// A per-chat debounce registry for desktop notifications.
///
/// Tracks the first pending time and last message per chat.
/// On `Fire` the caller is responsible for showing the notification;
/// on `Schedule` the caller sets a timer and calls `take` when it fires.
pub struct PendingNotifications {
    debounce: Duration,
    max_age: Duration,
    pending: HashMap<i64, PendingEntry>,
}

impl PendingNotifications {
    pub fn new(debounce: Duration, max_age: Duration) -> Self {
        Self {
            debounce,
            max_age,
            pending: HashMap::new(),
        }
    }

    /// Insert or replace a pending notification for `chat_id`.
    ///
    /// * First insert: stores the entry and returns `Schedule(now + debounce)`.
    /// * Subsequent insert: updates `last_message` and returns
    ///   `Schedule(min(now + debounce, first_pending + max_age))`.
    /// * If `first_pending + max_age <= now`: returns `Fire(msg)` and clears the
    ///   pending entry — the caller should show the notification immediately.
    pub fn insert_or_replace(
        &mut self,
        chat_id: i64,
        msg: NewMessage,
        now: Instant,
    ) -> InsertResult {
        if let Some(entry) = self.pending.get(&chat_id) {
            let first_pending = entry.first_pending;
            let deadline = first_pending + self.max_age;

            if deadline <= now {
                // Past the max-age deadline — fire immediately.
                self.pending.remove(&chat_id);
                return InsertResult::Fire(msg);
            }

            // Still within the debounce window: update last_message, keep first_pending.
            let schedule = std::cmp::min(now + self.debounce, deadline);
            self.pending.insert(
                chat_id,
                PendingEntry {
                    first_pending,
                    last_message: msg,
                },
            );
            InsertResult::Schedule(schedule)
        } else {
            // First notification for this chat.
            self.pending.insert(
                chat_id,
                PendingEntry {
                    first_pending: now,
                    last_message: msg,
                },
            );
            InsertResult::Schedule(now + self.debounce)
        }
    }

    /// Cancel a pending notification for `chat_id`. No-op if none exists.
    pub fn cancel(&mut self, chat_id: i64) {
        self.pending.remove(&chat_id);
    }

    /// Take (and remove) the pending entry for `chat_id`, or `None` if absent.
    pub fn take(&mut self, chat_id: i64) -> Option<PendingEntry> {
        self.pending.remove(&chat_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewMessage;
    use std::time::{Duration, Instant};

    const DEBOUNCE: Duration = Duration::from_millis(1500);
    const MAX_AGE: Duration = Duration::from_secs(5);

    fn msg(chat_id: i64, text: &str) -> NewMessage {
        NewMessage {
            chat_id,
            sender: None,
            text: Some(text.to_string()),
            has_attachment: false,
            date: 0,
        }
    }

    #[test]
    fn first_insert_schedules_at_now_plus_debounce() {
        let t0 = Instant::now();
        let mut pending = PendingNotifications::new(DEBOUNCE, MAX_AGE);
        let chat = 1001;
        let m = msg(chat, "hello");

        let result = pending.insert_or_replace(chat, m.clone(), t0);
        assert!(
            matches!(result, InsertResult::Schedule(sched) if sched == t0 + DEBOUNCE),
            "expected Schedule({:?}), got {:?}",
            t0 + DEBOUNCE,
            result,
        );

        let entry = pending.take(chat);
        assert!(entry.is_some(), "take should return Some");
        let entry = entry.unwrap();
        assert_eq!(entry.first_pending, t0);
        assert_eq!(entry.last_message.chat_id, chat);
        assert_eq!(entry.last_message.text.as_deref(), Some("hello"));
    }

    #[test]
    fn replace_keeps_first_pending_but_updates_message() {
        let t0 = Instant::now();
        let mut pending = PendingNotifications::new(DEBOUNCE, MAX_AGE);
        let chat = 1002;
        let m1 = msg(chat, "first");
        let m2 = msg(chat, "second");

        // First insert at t0
        pending.insert_or_replace(chat, m1, t0);

        // Second insert at t0 + 500ms
        let t1 = t0 + Duration::from_millis(500);
        let result = pending.insert_or_replace(chat, m2.clone(), t1);
        let expected_schedule = t1 + DEBOUNCE; // = t0 + 2.0s, < t0 + 5.0s (max_age)
        assert!(
            matches!(result, InsertResult::Schedule(sched) if sched == expected_schedule),
            "expected Schedule({:?}), got {:?}",
            expected_schedule,
            result,
        );

        let entry = pending.take(chat).expect("take should return Some");
        assert_eq!(
            entry.first_pending, t0,
            "first_pending should remain the original insert time"
        );
        assert_eq!(
            entry.last_message.text.as_deref(),
            Some("second"),
            "last_message should be the most recent message"
        );
    }

    #[test]
    fn replace_respects_max_age_deadline() {
        let t0 = Instant::now();
        let mut pending = PendingNotifications::new(DEBOUNCE, MAX_AGE);
        let chat = 1003;
        let m1 = msg(chat, "early");
        let m2 = msg(chat, "late");

        // First insert at t0
        pending.insert_or_replace(chat, m1, t0);

        // Second insert at t0 + 4s
        //   now + debounce = t0 + 4s + 1.5s = t0 + 5.5s
        //   first_pending + max_age = t0 + 5s
        //   min = t0 + 5s
        let t1 = t0 + Duration::from_secs(4);
        let result = pending.insert_or_replace(chat, m2, t1);
        let expected_schedule = t0 + MAX_AGE; // t0 + 5s
        assert!(
            matches!(result, InsertResult::Schedule(sched) if sched == expected_schedule),
            "expected Schedule({:?}), got {:?}",
            expected_schedule,
            result,
        );
    }

    #[test]
    fn insert_past_max_age_fires_immediately() {
        let t0 = Instant::now();
        let mut pending = PendingNotifications::new(DEBOUNCE, MAX_AGE);
        let chat = 1004;
        let m1 = msg(chat, "early");
        let m2 = msg(chat, "overdue");

        // First insert at t0
        pending.insert_or_replace(chat, m1, t0);

        // Second insert at t0 + 10s, past the 5s max_age.
        // first_pending + max_age = t0 + 5s <= t0 + 10s = now → Fire
        let t1 = t0 + Duration::from_secs(10);
        let result = pending.insert_or_replace(chat, m2.clone(), t1);
        assert!(
            matches!(result, InsertResult::Fire(ref msg) if msg.text.as_deref() == Some("overdue")),
            "expected Fire with 'overdue', got {:?}",
            result,
        );

        // No entry should remain after a Fire.
        assert!(
            pending.take(chat).is_none(),
            "take after Fire should return None"
        );
    }

    #[test]
    fn cancel_removes_pending() {
        let t0 = Instant::now();
        let mut pending = PendingNotifications::new(DEBOUNCE, MAX_AGE);
        let chat = 1005;
        let m = msg(chat, "to-be-cancelled");

        pending.insert_or_replace(chat, m, t0);
        pending.cancel(chat);

        assert!(
            pending.take(chat).is_none(),
            "take after cancel should return None"
        );
    }

    #[test]
    fn cancel_on_absent_chat_is_noop() {
        let mut pending = PendingNotifications::new(DEBOUNCE, MAX_AGE);
        // Should not panic.
        pending.cancel(42);
    }

    #[test]
    fn take_returns_entry_and_removes_it() {
        let t0 = Instant::now();
        let mut pending = PendingNotifications::new(DEBOUNCE, MAX_AGE);
        let chat = 1006;
        let m = msg(chat, "single-use");

        pending.insert_or_replace(chat, m.clone(), t0);

        let first_take = pending.take(chat);
        assert!(first_take.is_some(), "first take should return the entry");
        let entry = first_take.unwrap();
        assert_eq!(entry.first_pending, t0);
        assert_eq!(entry.last_message.text.as_deref(), Some("single-use"));

        let second_take = pending.take(chat);
        assert!(second_take.is_none(), "second take should return None");
    }

    #[test]
    fn pendings_for_different_chats_are_independent() {
        let t0 = Instant::now();
        let mut pending = PendingNotifications::new(DEBOUNCE, MAX_AGE);
        let chat1 = 2001;
        let chat2 = 2002;
        let m1 = msg(chat1, "chat-one");
        let m2 = msg(chat2, "chat-two");

        // Insert both chats at slightly different times.
        pending.insert_or_replace(chat1, m1, t0);
        let t1 = t0 + Duration::from_millis(300);
        pending.insert_or_replace(chat2, m2, t1);

        // Cancel chat1 — chat2 should be unaffected.
        pending.cancel(chat1);

        assert!(pending.take(chat1).is_none(), "chat1 should be cancelled");

        let entry2 = pending.take(chat2);
        assert!(entry2.is_some(), "chat2 should still have a pending entry");
        let entry2 = entry2.unwrap();
        assert_eq!(
            entry2.first_pending, t1,
            "chat2's first_pending should be its own insert time"
        );
        assert_eq!(
            entry2.last_message.text.as_deref(),
            Some("chat-two"),
            "chat2's last_message should be its own message"
        );
    }
}
