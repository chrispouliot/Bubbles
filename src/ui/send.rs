//! Compose/send pipeline: typing indicators, attachment handling, reactions,
//! edits, retries, and the send-text/send-attachment helpers.
//!
//! Extracted from `mod.rs` to keep the parent module focused on orchestration.
//! All types/functions from the parent are available via `use super::*`.

use super::*;

impl super::Ui {
    /// Set the pending attachment and show the chip. The chip's label is
    /// updated to the file name.
    pub(super) fn set_pending_attachment(&self, att: PendingAttachment) {
        self.pending_chip_label.set_text(&att.name);
        if att.mime.starts_with("image/") {
            match gtk::gdk::Texture::from_filename(&att.path) {
                Ok(texture) => self.pending_chip_icon.set_paintable(Some(&texture)),
                Err(e) => {
                    eprintln!(
                        "pending attachment thumbnail: failed to decode {}: {e}",
                        att.path.display()
                    );
                    self.pending_chip_icon.set_icon_name(Some("text-x-generic-symbolic"));
                }
            }
        } else {
            self.pending_chip_icon.set_icon_name(Some("text-x-generic-symbolic"));
        }
        self.pending_chip.set_visible(true);
        *self.pending_attachment.borrow_mut() = Some(att);
    }

    /// Clear the pending attachment and hide the chip. Safe to call when
    /// nothing is pending.
    pub(super) fn clear_pending_attachment(&self) {
        self.pending_chip.set_visible(false);
        self.pending_chip_label.set_text("");
        self.pending_chip_icon.set_paintable(None::<&gtk::gdk::Paintable>);
        *self.pending_attachment.borrow_mut() = None;
    }

    /// Inspect the default clipboard and, if it carries a file URI or a
    /// supported image mime, attach the first item via `set_pending_attachment`.
    /// Returns `Propagation::Stop` when we initiate an attach (so the entry's
    /// default text paste is suppressed) and `Propagation::Proceed` otherwise.
    pub(super) fn try_attach_from_clipboard(&self) -> glib::Propagation {
        let Some(display) = gtk::gdk::Display::default() else {
            return glib::Propagation::Proceed;
        };
        let clipboard = display.clipboard();
        let formats = clipboard.formats();

        // Priority: text/uri-list wins over images.
        if formats.contain_mime_type("text/uri-list") {
            let ui = self.clone();
            clipboard.read_async(
                &["text/uri-list"],
                glib::Priority::DEFAULT,
                gtk::gio::Cancellable::NONE,
                move |res| match res {
                    Ok((stream, _mime)) => {
                        stream.read_bytes_async(
                            64 * 1024,
                            glib::Priority::DEFAULT,
                            gtk::gio::Cancellable::NONE,
                            move |result| match result {
                                Ok(bytes) => {
                                    let text = String::from_utf8_lossy(bytes.as_ref()).into_owned();
                                    let paths = parse_uri_list(&text);
                                    if let Some(first) = paths.first() {
                                        let name = first
                                            .file_name()
                                            .map(|n| n.to_string_lossy().into_owned())
                                            .unwrap_or_else(|| "file".to_string());
                                        let mime = guess_mime(&name);
                                        ui.set_pending_attachment(PendingAttachment {
                                            path: first.clone(),
                                            name,
                                            mime,
                                        });
                                    }
                                }
                                Err(e) => {
                                    eprintln!("clipboard uri-list read failed: {e:#}");
                                }
                            },
                        );
                    }
                    Err(e) => {
                        eprintln!("clipboard uri-list read failed: {e:#}");
                    }
                },
            );
            return glib::Propagation::Stop;
        }

        // Image path: ask the clipboard for a Texture directly. This bypasses the
        // mime-based Texture→PNG serializer that produces stub PNGs (valid envelope,
        // zero pixels) when the source provides only a gdk::Texture GType — which is
        // the case for gnome-screenshot and most modern GTK apps.
        let has_image = formats.contains_type(gtk::gdk::Texture::static_type())
            || ["image/png", "image/jpeg", "image/webp", "image/gif"]
                .iter()
                .any(|m| formats.contain_mime_type(m));
        if has_image {
            let ui = self.clone();
            clipboard.read_texture_async(
                gtk::gio::Cancellable::NONE,
                move |res| match res {
                    Ok(Some(texture)) => {
                        // Unique temp path so concurrent pastes don't collide.
                        static COUNTER: AtomicU64 = AtomicU64::new(0);
                        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                        let pid = std::process::id();
                        let filename = format!("pasted-{}-{}.png", pid, n);
                        let path = std::env::temp_dir().join(&filename);

                        if let Err(e) = texture.save_to_png(&path) {
                            eprintln!("clipboard image save_to_png failed: {e:#}");
                            return;
                        }

                        // Defensive: if the source gave us a stub PNG, fail loud here
                        // so the user sees a clear warning instead of a silent black
                        // image on the recipient's device.
                        if let Ok(meta) = std::fs::metadata(&path) {
                            if meta.len() < 1024 {
                                eprintln!(
                                    "clipboard image paste wrote a suspiciously small PNG ({} bytes); \
                                     the source image may not have been real pixels",
                                    meta.len()
                                );
                            }
                        }

                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "image.png".to_string());
                        ui.set_pending_attachment(PendingAttachment {
                            path,
                            name,
                            mime: "image/png".to_string(),
                        });
                    }
                    Ok(None) => {
                        eprintln!("clipboard image: no texture available");
                    }
                    Err(e) => {
                        eprintln!("clipboard image read_texture_async failed: {e:#}");
                    }
                },
            );
            return glib::Propagation::Stop;
        }

        glib::Propagation::Proceed
    }

    /// React to compose-entry edits: send a typing start when text first appears,
    /// a stop when it's cleared, and re-arm an idle timer that stops after a pause
    /// (so we don't leave the other side showing dots if the user walks away).
    pub(super) fn note_typing_activity(&self, typing_now: bool) {
        let Some(chat) = self.open_summary.borrow().clone() else {
            return;
        };
        if typing_now && !self.typing_sent.replace(true) {
            self.send_typing(&chat, true);
        } else if !typing_now && self.typing_sent.replace(false) {
            self.send_typing(&chat, false);
        }
        if !typing_now {
            return;
        }
        let gen = self.typing_idle_gen.get().wrapping_add(1);
        self.typing_idle_gen.set(gen);
        let ui = self.clone();
        glib::timeout_add_seconds_local_once(6, move || {
            if ui.typing_idle_gen.get() == gen && ui.typing_sent.replace(false) {
                if let Some(chat) = ui.open_summary.borrow().clone() {
                    ui.send_typing(&chat, false);
                }
            }
        });
    }

    pub(super) fn send_typing(&self, chat: &ChatSummary, typing: bool) {
        let Some(my_handle) = self_handle(&chat.participants, &self.handles) else {
            return;
        };
        self.backend
            .send_typing(&self.client, &chat_ref_of(chat), &my_handle, typing);
    }

    /// An inbound typing event. Shown only for the open chat, matched by chat key
    /// or — when the typing conversation's participant set differs from ours — by
    /// the sender being one of the open chat's participants. The bubble lives at
    /// the end of the timeline (so it scrolls with the messages); auto-hides after
    /// a grace period since iMessage doesn't reliably send a matching stop.
    pub(super) fn handle_typing(&self, chat_key: &str, from: Option<&str>, typing: bool, superseded: bool) {
        let Some(open) = self.open_summary.borrow().clone() else {
            return;
        };
        let matched = open.key == chat_key
            || from.is_some_and(|f| {
                open.participants
                    .iter()
                    .any(|p| pretty_addr(p).eq_ignore_ascii_case(&pretty_addr(f)))
            });
        if !matched {
            return;
        }
        if !typing {
            self.typing_active.set(false);
            let gen = self.typing_gen.get().wrapping_add(1);
            self.typing_gen.set(gen);
            if superseded {
                // A message is arriving. Leave the dots in place and let the
                // imminent rebuild swap them for the message in a single reflow
                // (no remove-then-add bounce); tag the new bubble to fade in.
                self.morph_pending.set(true);
                let ui = self.clone();
                glib::timeout_add_seconds_local_once(2, move || {
                    // Backstop: if the rebuild somehow didn't clear the row, do it.
                    if ui.typing_gen.get() == gen {
                        ui.morph_pending.set(false);
                        ui.remove_typing_row();
                    }
                });
            } else {
                self.remove_typing_row();
            }
            return;
        }
        self.typing_active.set(true);
        let adj = self.scroller.vadjustment();
        let at_bottom = adj.value() + adj.page_size() >= adj.upper() - 80.0;
        let was_present = self.typing_row.borrow().is_some();
        self.append_typing_row(open.is_group);
        // Keep the dots visible when they first appear at the bottom.
        if at_bottom && !was_present {
            self.scroll_to(ScrollTo::Bottom);
        }
        let gen = self.typing_gen.get().wrapping_add(1);
        self.typing_gen.set(gen);
        let ui = self.clone();
        glib::timeout_add_seconds_local_once(12, move || {
            if ui.typing_gen.get() == gen {
                ui.typing_active.set(false);
                ui.remove_typing_row();
            }
        });
    }

    pub(super) fn hide_typing_indicator(&self) {
        self.typing_active.set(false);
        self.morph_pending.set(false);
        self.typing_gen.set(self.typing_gen.get().wrapping_add(1));
        self.remove_typing_row();
    }

    /// Append the typing bubble as the trailing item in the timeline, if not
    /// already present.
    fn append_typing_row(&self, is_group: bool) {
        if self.typing_row.borrow().is_some() {
            return;
        }
        let row = typing_row(is_group);
        self.msg_container.append(&row);
        *self.typing_row.borrow_mut() = Some(row);
    }

    pub(super) fn remove_typing_row(&self) {
        if let Some(row) = self.typing_row.borrow_mut().take() {
            self.msg_container.remove(&row);
        }
    }

    /// A timeline rebuild clears the container (and our row with it); drop the
    /// stale handle and re-append a fresh bubble if typing is still active, so a
    /// refresh mid-typing doesn't drop the indicator.
    pub(super) fn refresh_typing_row(&self, is_group: bool) {
        *self.typing_row.borrow_mut() = None;
        if self.typing_active.get() {
            self.append_typing_row(is_group);
        }
    }

    pub(super) fn compose_send(&self, entry: &gtk::Entry) {
        let text = entry.text().to_string();
        // Take the pending attachment (if any) and clear it eagerly so the
        // chip disappears immediately on send.
        let pending = self.pending_attachment.borrow_mut().take();
        self.pending_chip.set_visible(false);
        self.pending_chip_label.set_text("");

        if let Some(att) = pending {
            // --- attachment path ---
            let Some(chat) = self.open_summary.borrow().clone() else {
                return;
            };
            let Some(my_handle) = self_handle(&chat.participants, &self.handles) else {
                eprintln!("no self handle in chat; cannot send");
                return;
            };
            let path_str = att.path.to_string_lossy().into_owned();
            let chat_ref = chat_ref_of(&chat);
            let guid = new_guid();
            let chat_id = chat.id;
            let is_group = chat.is_group;
            let text_for_msg = if text.is_empty() {
                None
            } else {
                Some(text.clone())
            };

            // Optimistic record points at the chosen file so the image renders now.
            let optimistic = IncomingMessage {
                guid: guid.clone(),
                chat: chat_ref.clone(),
                sender: Some(my_handle.clone()),
                is_from_me: true,
                text: text_for_msg,
                service: Some("iMessage".into()),
                date: now_ms(),
                pending: true,
                attachments: vec![AttachmentRecord {
                    guid: Some(format!("{guid}-0")),
                    mime: Some(att.mime.clone()),
                    name: Some(att.name.clone()),
                    local_path: Some(path_str.clone()),
                    part_index: Some(0),
                    ..Default::default()
                }],
                ..Default::default()
            };

            let store = self.store.clone();
            let store_for_send = store.clone(); // retained for inner callback
            let backend = self.backend.clone();
            let client = self.client.clone();
            let connection = self.connection.clone();
            let ui = self.clone();
            let guid_inner = guid.clone();
            if !text.is_empty() {
                entry.set_text("");
            }
            gtk_bridge::spawn(
                async move { store.apply(Ingest::Message(optimistic)).await },
                move |res| {
                    if let Err(e) = res {
                        eprintln!("optimistic insert failed: {e:#}");
                    }
                    ui.reload_messages(chat_id, is_group);
                    ui.reload_chats(|_| {});
                    let store_inner = store_for_send.clone();
                    let ui_inner = ui.clone();
                    let guid_send = guid_inner.clone();
                    gtk_bridge::spawn(
                        async move {
                            backend
                                .send_attachment(
                                    &client, &connection, &chat_ref, &my_handle, path_str,
                                    att.mime, att.name, if text.is_empty() { None } else { Some(text) },
                                    guid,
                                )
                                .await?;
                            Ok::<(), anyhow::Error>(())
                        },
                        move |res| {
                            match res {
                                Ok(()) => {
                                    let store = store_inner.clone();
                                    let ui = ui_inner.clone();
                                    let guid = guid_send.clone();
                                    gtk_bridge::spawn(
                                        async move { store.mark_sent(&guid).await },
                                        move |_| {
                                            ui.reload_messages(chat_id, is_group);
                                            ui.reload_chats(|_| {});
                                        },
                                    );
                                }
                                Err(e) => {
                                    let category = crate::protocol::categorize_send_error(&e);
                                    let store = store_inner.clone();
                                    let ui = ui_inner.clone();
                                    let guid = guid_send.clone();
                                    gtk_bridge::spawn(
                                        async move {
                                            store
                                                .apply(Ingest::SendFailed { guid, category })
                                                .await
                                        },
                                        move |_| {
                                            ui.reload_messages(chat_id, is_group);
                                            ui.reload_chats(|_| {});
                                        },
                                    );
                                }
                            }
                        },
                    );
                },
            );
            return;
        }

        // --- text-only path ---
        if text.trim().is_empty() {
            return;
        }
        let Some(chat) = self.open_summary.borrow().clone() else {
            return;
        };
        entry.set_text("");
        self.send_text(&chat, text);
    }

    fn send_text(&self, chat: &ChatSummary, text: String) {
        let Some(my_handle) = self_handle(&chat.participants, &self.handles) else {
            eprintln!("no self handle in chat; cannot send");
            return;
        };
        let chat_ref = chat_ref_of(chat);
        let guid = new_guid();
        let chat_id = chat.id;
        let is_group = chat.is_group;

        // Optimistic record: persist + show the bubble now, before the network
        // round-trip. The real send reuses this guid, so its echo dedupes.
        let optimistic = IncomingMessage {
            guid: guid.clone(),
            chat: chat_ref.clone(),
            sender: Some(my_handle.clone()),
            is_from_me: true,
            text: Some(text.clone()),
            service: Some("iMessage".into()),
            date: now_ms(),
            pending: true,
            ..Default::default()
        };

        let store = self.store.clone();
        let backend = self.backend.clone();
        let client = self.client.clone();
        let ui = self.clone();
        let store_for_network = store.clone();
        gtk_bridge::spawn(
            async move { store.apply(Ingest::Message(optimistic)).await },
            move |res| {
                if let Err(e) = res {
                    eprintln!("optimistic insert failed: {e:#}");
                }
                ui.reload_messages(chat_id, is_group);
                ui.reload_chats(|_| {});
                // Fire the network send in the background. The optimistic row
                // already carries the final guid, so the echo dedupes and the
                // plan-based refresh (Noop) skips the rebuild, avoiding any
                // scroll stutter or thumbnail flash a beat after send.
                let guid_fail = guid.clone();
                let store_fail = store_for_network.clone();
                let ui_fail = ui.clone();
                gtk_bridge::spawn(
                    async move {
                        backend
                            .send_text(&client, &chat_ref, &my_handle, text, guid)
                            .await?;
                        Ok::<(), anyhow::Error>(())
                    },
                    move |res| {
                        match res {
                            Ok(()) => {
                                let store = store_fail.clone();
                                let ui = ui_fail.clone();
                                let guid = guid_fail.clone();
                                gtk_bridge::spawn(
                                    async move { store.mark_sent(&guid).await },
                                    move |_| {
                                        ui.reload_messages(chat_id, is_group);
                                        ui.reload_chats(|_| {});
                                    },
                                );
                            }
                            Err(e) => {
                                let category = crate::protocol::categorize_send_error(&e);
                                let store = store_fail.clone();
                                let ui = ui_fail.clone();
                                let guid = guid_fail;
                                gtk_bridge::spawn(
                                    async move {
                                        store
                                            .apply(Ingest::SendFailed { guid, category })
                                            .await
                                    },
                                    move |res2| {
                                        if let Err(e2) = res2 {
                                            eprintln!("persist send-failed error: {e2:#}");
                                        }
                                        ui.reload_messages(chat_id, is_group);
                                        ui.reload_chats(|_| {});
                                    },
                                );
                            }
                        }
                    },
                );
            },
        );
    }

    /// Send a tapback reaction to a target message. The optimistic insert +
    /// network send pattern mirrors `send_text`.
    #[cfg(feature = "rustpush")]
    fn send_reaction(&self, target_guid: &str, index: usize, target_text: &str) {
        let reaction = match index {
            0 => Reaction::Heart,
            1 => Reaction::Like,
            2 => Reaction::Dislike,
            3 => Reaction::Laugh,
            4 => Reaction::Emphasize,
            5 => Reaction::Question,
            _ => return,
        };
        let reaction_msg = ReactMessageType::React {
            reaction,
            enable: true,
        };

        let Some(chat) = self.open_summary.borrow().clone() else {
            return;
        };
        let Some(my_handle) = self_handle(&chat.participants, &self.handles) else {
            eprintln!("no self handle in chat; cannot send reaction");
            return;
        };
        let chat_ref = chat_ref_of(&chat);

        let tapback = Tapback {
            guid: new_guid(),
            chat: chat_ref.clone(),
            sender: Some(my_handle.clone()),
            is_from_me: true,
            date: now_ms(),
            associated_guid: target_guid.to_string(),
            associated_part: None,
            associated_type: 2000 + index as i64,
        };

        let store = self.store.clone();
        let backend = self.backend.clone();
        let client = self.client.clone();
        let ui = self.clone();
        let guid_owned = target_guid.to_string();
        let text_owned = target_text.to_string();
        let chat_id = chat.id;
        let is_group = chat.is_group;

        gtk_bridge::spawn(
            async move { store.apply(Ingest::Tapback(tapback)).await },
            move |res| {
                if let Err(e) = res {
                    eprintln!("optimistic tapback insert failed: {e:#}");
                }
                ui.reload_messages(chat_id, is_group);
                ui.reload_chats(|_| {});
                gtk_bridge::spawn(
                    async move {
                        backend
                            .send_reaction(
                                &client,
                                &chat_ref,
                                &my_handle,
                                &guid_owned,
                                None,
                                &text_owned,
                                &reaction_msg,
                            )
                            .await?;
                        Ok::<(), anyhow::Error>(())
                    },
                    move |res| {
                        if let Err(e) = res {
                            eprintln!("reaction send failed: {e:#}");
                        }
                    },
                );
            },
        );
    }

    /// Edit the text of a previously-sent message in the open chat. Mirrors
    /// `send_text` for the apply-then-send pattern, with one key difference:
    /// the target message's GUID is preserved (we update an existing row, not
    /// insert a new one), so the planner uses the `EditText` in-place path
    /// (the old forced rebuild is no longer needed).
    #[cfg(feature = "rustpush")]
    fn send_edit(&self, target_guid: String, edit_part: u64, new_text: String) {
        let Some(chat) = self.open_summary.borrow().clone() else {
            eprintln!("send_edit: no open chat; cannot edit");
            return;
        };
        let Some(my_handle) = self_handle(&chat.participants, &self.handles) else {
            eprintln!("send_edit: no self handle in chat; cannot edit");
            return;
        };
        let chat_ref = chat_ref_of(&chat);
        let chat_id = chat.id;
        let is_group = chat.is_group;

        let store = self.store.clone();
        let backend = self.backend.clone();
        let client = self.client.clone();
        let ui = self.clone();

        let guid_for_apply = target_guid.clone();
        let text_for_apply = new_text.clone();
        let guid_for_send = target_guid.clone();
        let text_for_send = new_text.clone();
        let new_guid = new_guid();

        gtk_bridge::spawn(
            async move {
                store
                    .apply(Ingest::Edited {
                        guid: guid_for_apply,
                        text: text_for_apply,
                    })
                    .await?;
                Ok::<(), anyhow::Error>(())
            },
            move |res| {
                if let Err(e) = res {
                    eprintln!("edit store apply failed: {e:#}");
                }

                // The planner's EditText path handles the in-place text
                // update. No forced rebuild needed.
                ui.reload_messages(chat_id, is_group);
                ui.reload_chats(|_| {});

                // Fire the network send in the background. On failure, v1 just
                // logs — the local edit stays. A follow-up can revert by
                // applying Ingest::Edited with the previous text.
                let chat_ref_for_send = chat_ref.clone();
                let my_handle_for_send = my_handle.clone();
                gtk_bridge::spawn(
                    async move {
                        backend
                            .send_edit(
                                &client,
                                &chat_ref_for_send,
                                &my_handle_for_send,
                                &guid_for_send,
                                edit_part,
                                text_for_send,
                                new_guid,
                            )
                            .await?;
                        Ok::<(), anyhow::Error>(())
                    },
                    move |res| {
                        if let Err(e) = res {
                            eprintln!("edit send failed: {e:#}");
                        }
                    },
                );
            },
        );
    }

    /// Build a closure suitable as the `on_reaction` callback for
    /// `populate_messages` / `build_message_widgets`. With the `rustpush` feature
    /// it dispatches to `send_reaction`; without it, it logs a stub message.
    pub(super) fn make_reaction_handler(&self) -> Option<Rc<ReactionHandler>> {
        #[cfg(feature = "rustpush")]
        {
            let ui = self.clone();
            Some(Rc::new(move |guid, index, target_text| {
                ui.send_reaction(&guid, index, &target_text)
            }))
        }
        #[cfg(not(feature = "rustpush"))]
        {
            Some(Rc::new(move |_guid, index, _target_text| {
                eprintln!("reaction {} send skipped (rustpush feature disabled)", index);
            }))
        }
    }

    /// Build a closure suitable as the `on_edit` callback for
    /// `populate_messages` / `build_message_widgets`.
    ///
    /// Unit 5: no-op — the button appears and the popover closes but nothing
    /// is sent. Unit 6 will replace this with code that opens the editor;
    /// Unit 7 will replace it again with code that does the full send.
    pub(super) fn make_edit_handler(&self) -> Option<Rc<EditHandler>> {
        // Unit 5 returned a no-op. Unit 6: opens an editor popover with a
        // multi-line TextView pre-filled with the current text, plus Save
        // and Cancel buttons. Save extracts the new text and fires the
        // save callback (no-op for this unit), then closes the popover.
        // Cancel just closes the popover.
        let ui = self.clone();
        Some(Rc::new(move |target_guid: String, current_text: String| {
            // Build the popover
            let popover = gtk::Popover::builder()
                .autohide(true)
                .build();

            // Outer container
            let vbox = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(8)
                .margin_start(8)
                .margin_end(8)
                .margin_top(8)
                .margin_bottom(8)
                .width_request(320)
                .build();

            // TextView (multi-line)
            let buffer = gtk::TextBuffer::builder().text(&current_text).build();
            let text_view = gtk::TextView::builder()
                .buffer(&buffer)
                .wrap_mode(gtk::WrapMode::Word)
                .height_request(120)
                .build();
            vbox.append(&text_view);

            // Button row
            let hbox = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .halign(gtk::Align::End)
                .build();

            let cancel_btn = gtk::Button::builder().label("Cancel").build();
            let save_btn = gtk::Button::builder()
                .label("Save")
                .css_classes(["suggested-action"])
                .build();
            hbox.append(&cancel_btn);
            hbox.append(&save_btn);
            vbox.append(&hbox);

            popover.set_child(Some(&vbox));

            // Save: extract text, call save handler (no-op for Unit 6), popdown.
            let popover_save = popover.clone();
            let save_handler = ui.make_edit_save_handler();
            let buffer_for_save = buffer.clone();
            let guid_for_save = target_guid.clone();
            save_btn.connect_clicked(move |_| {
                let (start, end) = buffer_for_save.bounds();
                let new_text = buffer_for_save
                    .text(&start, &end, false)
                    .to_string();
                if !new_text.trim().is_empty() {
                    if let Some(cb) = save_handler.as_ref() {
                        cb(guid_for_save.clone(), new_text);
                    }
                }
                popover_save.popdown();
            });

            // Cancel: just popdown.
            let popover_cancel = popover.clone();
            cancel_btn.connect_clicked(move |_| {
                popover_cancel.popdown();
            });

            // Unparent the popover when it closes (avoids accumulating hidden
            // children in content_page after Cancel or outside-click). Clear
            // the focus first so GTK doesn't move focus to the next focusable
            // widget (which happens to be the rename_button in the topbar,
            // causing an unwanted blue focus outline to appear there).
            popover.connect_closed(move |p| {
                if let Some(root) = p.root() {
                    root.set_focus(None::<&gtk::Widget>);
                }
                p.unparent();
            });

            // Anchor the popover to the source bubble so it appears next to
            // the message being edited (instead of the default position on
            // content_page, which lands under the compose area). If the
            // bubble is no longer in the widget tree (e.g., the chat was
            // rebuilt between right-click and popover-open), fall back to
            // the default position.
            if let Some(entry) = ui.current_chips.borrow().get(&target_guid) {
                if let Some(rect) = entry.bubble.compute_bounds(&ui.content_page) {
                    // compute_bounds gives graphene::Rect (f32);
                    // set_pointing_to wants gdk::Rectangle (i32).
                    let gdk_rect = gtk::gdk::Rectangle::new(
                        rect.x() as i32,
                        rect.y() as i32,
                        rect.width() as i32,
                        rect.height() as i32,
                    );
                    popover.set_pointing_to(Some(&gdk_rect));
                }
            }

            popover.set_parent(&ui.content_page);
            popover.popup();
        }))
    }

    /// Build a closure suitable as the `on_retry` callback for failed own
    /// text messages. The handler receives `(guid, text)` from the bubble's
    /// context menu, clears the error state via `store.mark_retrying`,
    /// re-invokes `backend.send_text` with the same text and guid, and
    /// updates the store on success (`mark_sent`) or failure
    /// (`SendFailed`).
    pub(super) fn make_retry_handler(&self) -> Option<Rc<RetryHandler>> {
        let ui = self.clone();
        Some(Rc::new(move |guid: String, text: String| {
            let chat = match ui.open_summary.borrow().clone() {
                Some(c) => c,
                None => return,
            };
            let my_handle = match self_handle(&chat.participants, &ui.handles) {
                Some(h) => h,
                None => return,
            };
            let chat_ref = chat_ref_of(&chat);
            let chat_id = chat.id;
            let is_group = chat.is_group;
            let store = ui.store.clone();
            let backend = ui.backend.clone();
            let client = ui.client.clone();

            // Step 1: mark the message as retrying (pending=1, error=NULL).
            let store1 = store.clone();
            let guid1 = guid.clone();
            let ui_step1 = ui.clone();
            gtk_bridge::spawn(
                async move { store1.mark_retrying(&guid1).await },
                move |_| {
                    ui_step1.reload_messages(chat_id, is_group);
                    ui_step1.reload_chats(|_| {});
                    // Step 2: re-invoke the backend send with the same text & guid.
                    let store2 = store.clone();
                    let guid2 = guid.clone();
                    let ui_step2 = ui_step1.clone();
                    gtk_bridge::spawn(
                        async move {
                            backend
                                .send_text(&client, &chat_ref, &my_handle, text, guid2)
                                .await?;
                            Ok::<(), anyhow::Error>(())
                        },
                        move |res| {
                            match res {
                                Ok(()) => {
                                    let store3 = store2.clone();
                                    let guid3 = guid.clone();
                                    let ui_step3 = ui_step2.clone();
                                    gtk_bridge::spawn(
                                        async move { store3.mark_sent(&guid3).await },
                                        move |_| {
                                            ui_step3.reload_messages(chat_id, is_group);
                                            ui_step3.reload_chats(|_| {});
                                        },
                                    );
                                }
                                Err(e) => {
                                    let category =
                                        crate::protocol::categorize_send_error(&e);
                                    let store3 = store2.clone();
                                    let guid3 = guid.clone();
                                    let ui_step3 = ui_step2.clone();
                                    gtk_bridge::spawn(
                                        async move {
                                            store3
                                                .apply(Ingest::SendFailed {
                                                    guid: guid3,
                                                    category,
                                                })
                                                .await
                                        },
                                        move |_| {
                                            ui_step3.reload_messages(chat_id, is_group);
                                            ui_step3.reload_chats(|_| {});
                                        },
                                    );
                                }
                            }
                        },
                    );
                },
            );
        }))
    }

    /// Build a closure suitable as the Save-button callback inside the editor
    /// popover.
    ///
    /// Unit 6: no-op — the editor opens, accepts input, and Save closes it
    /// without dispatching anything. Unit 7 replaces this with code that
    /// calls `send_edit`.
    fn make_edit_save_handler(&self) -> Option<Rc<EditSaveHandler>> {
        #[cfg(feature = "rustpush")]
        {
            let ui = self.clone();
            Some(Rc::new(move |guid, text| {
                ui.send_edit(guid, 0, text);
            }))
        }
        #[cfg(not(feature = "rustpush"))]
        {
            Some(Rc::new(move |_guid, _text| {
                eprintln!("edit save skipped (rustpush feature disabled)");
            }))
        }
    }
}
