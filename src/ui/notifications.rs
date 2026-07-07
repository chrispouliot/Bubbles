//! Desktop notification methods: coalescing, showing, withdrawing, and
//! the window-focus/activate-chat handlers.
//!
//! Extracted from `mod.rs` to keep the parent module focused on orchestration.
//! All types/functions from the parent are available via `use super::*`.

use super::*;

impl super::Ui {
    /// Raise a desktop notification for each chat that received new messages,
    /// unless that chat is the one currently open *and* focused. Coalesces per
    /// chat (id-keyed), so new messages replace the prior notification rather
    /// than stacking, and a watermark ensures each message notifies once.
    pub(super) fn process_notifications(&self) {
        let store = self.store.clone();
        let ui = self.clone();
        let since = self.notify_watermark.get();
        gtk_bridge::spawn(
            async move { store.incoming_since(since).await },
            move |res| {
                let rows = match res {
                    Ok(r) if !r.is_empty() => r,
                    _ => return,
                };
                let mut max_date = ui.notify_watermark.get();
                let mut order: Vec<i64> = Vec::new();
                let mut per_chat: std::collections::HashMap<i64, (String, String, usize)> =
                    std::collections::HashMap::new();
                for m in &rows {
                    max_date = max_date.max(m.date);
                    let preview = m
                        .text
                        .as_deref()
                        .map(strip_marker)
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| {
                            if m.has_attachment {
                                "Sent an attachment".to_string()
                            } else {
                                String::new()
                            }
                        });
                    let sender = m.sender.clone().unwrap_or_default();
                    let e = per_chat.entry(m.chat_id).or_insert_with(|| {
                        order.push(m.chat_id);
                        (String::new(), String::new(), 0)
                    });
                    e.0 = sender;
                    e.1 = preview;
                    e.2 += 1;
                }
                let mut last_msg_per_chat: std::collections::HashMap<i64, NewMessage> =
                    std::collections::HashMap::new();
                for m in &rows {
                    last_msg_per_chat.insert(m.chat_id, m.clone());
                }
                ui.notify_watermark.set(max_date);

                let open_id = ui.open_summary.borrow().as_ref().map(|c| c.id);
                let focused = ui.focused.get();
                let now = std::time::Instant::now();
                for chat_id in order {
                    let (sender, preview, count) = per_chat.remove(&chat_id).unwrap();
                    let last_msg = last_msg_per_chat.remove(&chat_id).unwrap();
                    // Don't notify for the chat the user is actively viewing.
                    if focused && open_id == Some(chat_id) {
                        ui.withdraw_chat_notification(chat_id);
                        ui.pending_notifications.borrow_mut().cancel(chat_id);
                        continue;
                    }
                    let summary = ui.chats.borrow().iter().find(|c| c.id == chat_id).cloned();
                    let (title, is_group) = match &summary {
                        Some(c) => (chat_title(c, &ui.handles, &ui.contacts.borrow()), c.is_group),
                        None => (pretty_addr(&sender), false),
                    };
                    let mut body = if is_group && !sender.is_empty() {
                        format!("{}: {}", pretty_addr(&sender), preview)
                    } else {
                        preview
                    };
                    if count > 1 {
                        body = format!("{body} (+{} earlier)", count - 1);
                    }

                    match ui
                        .pending_notifications
                        .borrow_mut()
                        .insert_or_replace(chat_id, last_msg, now)
                    {
                        pending::InsertResult::Fire(_) => {
                            ui.show_chat_notification(chat_id, &title, &body);
                        }
                        pending::InsertResult::Schedule(at) => {
                            let pending = ui.pending_notifications.clone();
                            let ui2 = ui.clone();
                            glib::timeout_add_local_once(
                                at.saturating_duration_since(std::time::Instant::now()),
                                move || {
                                    let entry = pending.borrow_mut().take(chat_id);
                                    if let Some(entry) = entry {
                                        let store = ui2.store.clone();
                                        let cid = chat_id;
                                        gtk_bridge::spawn(
                                            async move {
                                                store.latest_unread_incoming(cid).await
                                            },
                                            move |result| {
                                                if result.ok().flatten().is_some() {
                                                    // Chat is still unread — show notification.
                                                    let preview = entry
                                                        .last_message
                                                        .text
                                                        .as_deref()
                                                        .map(strip_marker)
                                                        .filter(|t| !t.is_empty())
                                                        .unwrap_or_else(|| {
                                                            if entry.last_message.has_attachment
                                                            {
                                                                "Sent an attachment".to_string()
                                                            } else {
                                                                String::new()
                                                            }
                                                        });
                                                    let sender = entry
                                                        .last_message
                                                        .sender
                                                        .unwrap_or_default();
                                                    let summary = ui2
                                                        .chats
                                                        .borrow()
                                                        .iter()
                                                        .find(|c| c.id == cid)
                                                        .cloned();
                                                    let (title, is_group) = match &summary {
                                                        Some(c) => (
                                                            chat_title(c, &ui2.handles, &ui2.contacts.borrow()),
                                                            c.is_group,
                                                        ),
                                                        None => {
                                                            (pretty_addr(&sender), false)
                                                        }
                                                    };
                                                    let body = if is_group && !sender.is_empty() {
                                                        format!(
                                                            "{}: {}",
                                                            pretty_addr(&sender),
                                                            preview
                                                        )
                                                    } else {
                                                        preview
                                                    };
                                                    ui2.show_chat_notification(
                                                        cid, &title, &body,
                                                    );
                                                }
                                            },
                                        );
                                    }
                                },
                            );
                        }
                    }
                }
            },
        );
    }

    pub(super) fn show_chat_notification(&self, chat_id: i64, title: &str, body: &str) {
        let Some(app) = gtk::gio::Application::default() else {
            return;
        };
        let n = gtk::gio::Notification::new(title);
        if !body.is_empty() {
            n.set_body(Some(body));
        }
        n.set_default_action_and_target_value("app.open-chat", Some(&chat_id.to_variant()));
        app.send_notification(Some(&format!("chat-{chat_id}")), &n);
        self.notified_chats.borrow_mut().insert(chat_id);
        crate::tray::set_unread(true);
    }

    pub(super) fn withdraw_chat_notification(&self, chat_id: i64) {
        if let Some(app) = gtk::gio::Application::default() {
            app.withdraw_notification(&format!("chat-{chat_id}"));
        }
        self.notified_chats.borrow_mut().remove(&chat_id);
        crate::tray::set_unread(!self.notified_chats.borrow().is_empty());
    }

    /// Open the chat a clicked notification targets, raising the window first.
    pub(super) fn activate_chat(&self, chat_id: i64) {
        if let Some(win) = self.window.borrow().as_ref() {
            win.present();
        }
        let summary = self.chats.borrow().iter().find(|c| c.id == chat_id).cloned();
        if let Some(c) = summary {
            self.open_chat(&c);
        }
    }

    /// On regaining focus, if the open chat picked up unread messages while we
    /// were away, re-show it with the unread divider/pill and mark it read —
    /// reusing the same flow as opening the chat fresh.
    pub(super) fn on_window_focus(&self) {
        let Some(chat) = self.open_summary.borrow().clone() else {
            return;
        };
        let store = self.store.clone();
        let ui = self.clone();
        let chat_id = chat.id;
        gtk_bridge::spawn(
            async move { store.first_unread_incoming(chat_id).await.ok().flatten() },
            move |first| {
                if first.is_some() {
                    // The divider is already on screen from the background
                    // refresh, so just mark the chat read and let it self-dismiss.
                    // No repopulate here — that's what was causing the flicker.
                    ui.maybe_send_read(&chat);
                    ui.arm_unread_dismiss();
                    ui.scroll_to(ScrollTo::Bottom);
                } else {
                    // Already read (e.g. on another device): clear any divider
                    // still lingering from that session.
                    ui.dismiss_unread_divider();
                }
            },
        );
    }
}
