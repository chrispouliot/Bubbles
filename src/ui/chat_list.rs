//! Chat list / sidebar logic: selection state, new-chat dialog, delete, preferences,
//! sign-out, about dialog, and the chat-list refresh pipeline.
//!
//! Extracted from `mod.rs` to keep the parent module focused on the message timeline.
//! All types/functions from the parent are available via `use super::*`.

use super::*;

// ---------------------------------------------------------------------------
// ChatSelectionState — pure state machine for the chat-list selection mode
// ---------------------------------------------------------------------------

/// Tracks whether the chat list is in "selecting" mode and which chat ids are
/// currently selected. No widget code, no I/O, no global state.
#[allow(dead_code)]
#[derive(Default)]
pub(crate) struct ChatSelectionState {
    /// Whether the list is in selection mode (shown by checkboxes on rows).
    selecting: bool,
    /// Set of chat ids the user has toggled into the selection.
    selected: HashSet<i64>,
}

#[allow(dead_code)]
impl ChatSelectionState {
    /// Create a fresh state: not selecting, no selected chats.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if the list is in selecting mode.
    pub(crate) fn is_selecting(&self) -> bool {
        self.selecting
    }

    /// Returns `true` if the given chat is currently selected.
    pub(crate) fn is_selected(&self, chat_id: i64) -> bool {
        self.selected.contains(&chat_id)
    }

    /// Returns a snapshot of the currently selected chat ids.
    pub(crate) fn selected_chat_ids(&self) -> Vec<i64> {
        self.selected.iter().copied().collect()
    }

    /// Enter selecting mode and select the given chat.
    pub(crate) fn begin_select(&mut self, chat_id: i64) {
        self.selecting = true;
        self.selected.insert(chat_id);
    }

    /// Enter selecting mode from a long-press (same effect as `begin_select`).
    pub(crate) fn begin_select_from_long_press(&mut self, chat_id: i64) {
        self.selecting = true;
        self.selected.insert(chat_id);
    }

    /// While selecting, toggle the given chat in/out of the selected set.
    /// If the chat is already selected, it is removed; otherwise it is added.
    pub(crate) fn toggle_chat(&mut self, chat_id: i64) {
        if self.selected.contains(&chat_id) {
            self.selected.remove(&chat_id);
            if self.selected.is_empty() {
                self.selecting = false;
            }
        } else {
            self.selected.insert(chat_id);
        }
    }

    /// Exit selecting mode and clear all selected chats.
    pub(crate) fn clear(&mut self) {
        self.selecting = false;
        self.selected.clear();
    }
}

// ---------------------------------------------------------------------------
// impl Ui — chat list / sidebar methods
// ---------------------------------------------------------------------------

impl super::Ui {
    pub(super) fn delete_selected_chats(&self) {
        let ids: Vec<i64> = self.chat_selection.borrow().selected_chat_ids();
        if ids.is_empty() {
            return;
        }
        let was_open_deleted = self
            .open_summary
            .borrow()
            .as_ref()
            .map(|s| ids.contains(&s.id))
            .unwrap_or(false);
        let store = self.store.clone();
        let ui = self.clone();
        gtk_bridge::spawn(
            async move {
                store.delete_chats(ids).await
            },
            move |result| {
                if let Err(e) = result {
                    log::error!("failed to delete chats: {e:#}");
                    return;
                }
                ui.chat_selection.borrow_mut().clear();
                if was_open_deleted {
                    // Close the deleted chat: show the empty state.
                    *ui.open_summary.borrow_mut() = None;
                    ui.content_stack.set_visible_child_name("empty");
                    ui.rename_button.set_sensitive(false);
                    ui.compose_outer.set_visible(false);
                    clear_box(&ui.msg_container);
                }
                ui.reload_chats(|_| {});
            },
        );
    }

    /// Delete a single chat (when not in selection mode).
    pub(super) fn delete_single_chat(&self, chat_id: i64) {
        let store = self.store.clone();
        let ui = self.clone();
        let was_open = self
            .open_summary
            .borrow()
            .as_ref()
            .map(|s| s.id == chat_id)
            .unwrap_or(false);
        gtk_bridge::spawn(
            async move {
                store.delete_chats(vec![chat_id]).await
            },
            move |result| {
                if let Err(e) = result {
                    log::error!("failed to delete chat {chat_id}: {e:#}");
                    return;
                }
                if was_open {
                    *ui.open_summary.borrow_mut() = None;
                    ui.content_stack.set_visible_child_name("empty");
                    ui.rename_button.set_sensitive(false);
                    ui.compose_outer.set_visible(false);
                    clear_box(&ui.msg_container);
                }
                ui.reload_chats(|_| {});
            },
        );
    }

    pub(super) fn reload_chats(&self, on_chats: impl FnOnce(&[ChatSummary]) + 'static) {
        let store = self.store.clone();
        let ui = self.clone();
        gtk_bridge::spawn(async move { store.chats().await }, move |res| {
            let chats = res.unwrap_or_else(|e| {
                eprintln!("chats load error: {e:#}");
                Vec::new()
            });
            on_chats(&chats);
            clear(&ui.chat_list);
            for c in &chats {
                ui.chat_list.append(&chat_row(&ui, c));
            }
            // Keep the open chat highlighted across refreshes.
            if let Some(open) = ui.open_summary.borrow().as_ref() {
                if let Some(i) = chats.iter().position(|c| c.id == open.id) {
                    if let Some(row) = ui.chat_list.row_at_index(i as i32) {
                        ui.chat_list.select_row(Some(&row));
                    }
                }
            }
            // Withdraw notifications for chats that are no longer unread — covers
            // reads synced from another device (receipt or self-sent reply), which
            // clear unread here without us opening the chat.
            let read_off: Vec<i64> = if !ui.notify_swept.replace(true) {
                // First load: also clear any stale notification for an already-read
                // chat, in case it lingered from a previous session.
                chats.iter().filter(|c| c.unread == 0).map(|c| c.id).collect()
            } else {
                let notified = ui.notified_chats.borrow();
                notified
                    .iter()
                    .copied()
                    .filter(|&id| !chats.iter().any(|c| c.id == id && c.unread > 0))
                    .collect()
            };
            for id in read_off {
                ui.withdraw_chat_notification(id);
            }
            *ui.chats.borrow_mut() = chats;
        });
    }

    /// Open the "New Chat" dialog.
    pub(super) fn show_new_chat_dialog(&self) {
        let to_row = adw::EntryRow::new();
        to_row.set_title("To");
        to_row.set_input_purpose(gtk::InputPurpose::Phone);
        to_row.set_show_apply_button(false);

        let name_row = adw::EntryRow::new();
        name_row.set_title("Name");
        name_row.set_show_apply_button(false);

        let msg_row = adw::EntryRow::new();
        msg_row.set_title("Message");
        msg_row.set_activates_default(true);
        msg_row.set_show_apply_button(false);

        let group = adw::PreferencesGroup::new();
        group.add(&to_row);

        // Contact completion popover: shows matching contacts as the user types.
        let completion_list = gtk::ListBox::new();
        completion_list.set_selection_mode(gtk::SelectionMode::Single);
        // Keep keyboard focus on the To entry: the listbox must not grab focus
        // when it appears, otherwise the user can't keep typing the query.
        completion_list.set_can_focus(false);

        let completion_scrolled = gtk::ScrolledWindow::new();
        completion_scrolled.set_child(Some(&completion_list));
        completion_scrolled.set_max_content_height(200);
        completion_scrolled.set_propagate_natural_height(true);
        completion_scrolled.set_propagate_natural_width(true);
        completion_scrolled.set_min_content_width(320);
        completion_scrolled.set_can_focus(false);

        let completion_popover = gtk::Popover::new();
        completion_popover.set_child(Some(&completion_scrolled));
        // autohide=false so the popover doesn't perform a seat grab that steals
        // keyboard input from the To entry. We dismiss it manually on selection,
        // empty query, or click-outside (handled by re-grabbing focus).
        completion_popover.set_autohide(false);
        completion_popover.set_has_arrow(false);
        completion_popover.set_parent(&to_row);

        group.add(&name_row);
        group.add(&msg_row);

        // Error label shown when the recipient is invalid.
        let error_label = gtk::Label::new(Some("Enter a valid phone number or email"));
        error_label.add_css_class("error");
        error_label.set_halign(gtk::Align::Start);
        error_label.set_margin_start(12);
        error_label.set_margin_end(12);
        error_label.set_margin_top(4);
        error_label.set_visible(false);
        group.add(&error_label);

        // Shared state for the contact completion list: the normalized address
        // (`tel:…` / `mailto:…`) for each row, indexed by row position.
        let completion_addrs: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        // Parallel to completion_addrs: the display name for each row, used to
        // pre-fill the Name field as a suggestion when a contact is selected.
        let completion_names: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        // When a contact is selected, the Name field is pre-filled with the
        // contact's display name as a *suggestion*. The suggestion is tracked
        // here so the send handler can distinguish "user left the suggestion
        // as-is" (→ no custom name, fall through to contact name) from "user
        // edited the name" (→ persist as custom_name). An empty suggestion
        // means "no suggestion was made" (e.g. bare-number entry).
        let suggested_name: Rc<Cell<Option<String>>> = Rc::new(Cell::new(None));
        {
            let completion_addrs = completion_addrs.clone();
            let completion_names = completion_names.clone();
            let to_row = to_row.clone();
            let name_row = name_row.clone();
            let suggested_name = suggested_name.clone();
            let completion_popover = completion_popover.clone();
            completion_list.connect_row_activated(move |_list, row| {
                let idx = row.index();
                // Borrow + clone into locals before set_text: set_text fires
                // `changed`, whose handler borrows completion_addrs mutably, so
                // the borrows here must be released first (mirrors chat_list's
                // row_activated at line 728).
                let addr = completion_addrs.borrow().get(idx as usize).cloned();
                let name = completion_names.borrow().get(idx as usize).cloned();
                if let Some(addr) = addr {
                    to_row.set_text(&addr);
                }
                // Pre-fill the Name field with the contact's display name as a
                // suggestion. The send handler checks suggested_name to decide
                // whether to persist it as custom_name or treat it as unset.
                if let Some(name) = name {
                    name_row.set_text(&name);
                    suggested_name.set(Some(name));
                }
                completion_popover.popdown();
            });
        }

        let dialog = adw::AlertDialog::new(Some("New Chat"), Some("Start a conversation with a new contact."));
        dialog.set_extra_child(Some(&group));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("send", "Send");
        dialog.set_response_appearance("send", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("send"));
        dialog.set_close_response("cancel");
        dialog.set_response_enabled("send", false);

        // Send is enabled only when both fields are non-empty AND the recipient
        // is a valid phone or email. This runs on every keystroke (no debounce)
        // so the button can't be re-enabled by typing in the message field while
        // the To field holds invalid input. The debounce below only governs
        // when the error *label* becomes visible.
        let update_send_sensitivity = {
            let dialog = dialog.clone();
            move |to: &str, msg: &str| {
                let to_trimmed = to.trim();
                let recipient_ok =
                    to_trimmed.is_empty() || normalize_recipient(to_trimmed).is_some();
                dialog.set_response_enabled(
                    "send",
                    recipient_ok && !msg.is_empty(),
                );
            }
        };

        // Debounced error-label visibility: the label only shows after the
        // user stops typing for `DEBOUNCE_MS`, not on every keystroke. The
        // pending source is cancelled and replaced on each change.
        let debounce_source: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        const DEBOUNCE_MS: u64 = 600;

          // Enable/disable the Send button based on To + Message fields.
        let contacts = self.contacts.clone();
        {
            let to_row = to_row.clone();
            let msg_row = msg_row.clone();
            let update_send_sensitivity = update_send_sensitivity.clone();
            let debounce_source = debounce_source.clone();
            let error_label = error_label.clone();
            let completion_list = completion_list.clone();
            let completion_popover = completion_popover.clone();
            let completion_addrs = completion_addrs.clone();
            let contacts = contacts.clone();
            to_row.clone().connect_changed(move |_| {
                let to = to_row.text();
                let msg = msg_row.text();
                update_send_sensitivity(&to, &msg);

                // Update contact completion list: search contacts and populate
                // the dropdown with matching entries.
                {
                    let contacts_borrowed = contacts.borrow();
                    let trimmed = to.trim();
                    if !trimmed.is_empty()
                        && trimmed.contains(|c: char| c.is_alphanumeric())
                    {
                        let matches =
                            crate::contacts::search_contacts(&contacts_borrowed, trimmed);
                        if matches.is_empty() {
                            completion_popover.popdown();
                        } else {
                            // Clear existing rows.
                            while let Some(child) = completion_list.first_child() {
                                completion_list.remove(&child);
                            }
                            let mut addrs = completion_addrs.borrow_mut();
                            let mut names = completion_names.borrow_mut();
                            addrs.clear();
                            names.clear();
                            for contact in matches {
                                // Prefer a phone address (iMessage typically
                                // starts from a phone), fall back to email.
                                let addr_value = contact
                                    .addresses
                                    .iter()
                                    .find(|a| a.kind == "phone")
                                    .or_else(|| contact.addresses.first())
                                    .map(|a| a.value.clone())
                                    .unwrap_or_default();
                                addrs.push(addr_value.clone());
                                names.push(contact.display_name.clone());
                                // Build the row: display name + first address.
                                let row = gtk::ListBoxRow::new();
                                let hbox = gtk::Box::builder()
                                    .orientation(gtk::Orientation::Horizontal)
                                    .spacing(8)
                                    .margin_start(12)
                                    .margin_end(12)
                                    .margin_top(6)
                                    .margin_bottom(6)
                                    .build();
                                let name_label = gtk::Label::new(Some(&contact.display_name));
                                name_label.set_hexpand(true);
                                name_label.set_xalign(0.0);
                                let addr_label =
                                    gtk::Label::new(Some(&pretty_addr(&addr_value)));
                                addr_label.set_xalign(0.0);
                                addr_label.add_css_class("dim-label");
                                hbox.append(&name_label);
                                hbox.append(&addr_label);
                                row.set_child(Some(&hbox));
                                completion_list.append(&row);
                            }
                            completion_popover.popup();
                            // The popover's map/focus cycle runs in this
                            // main-loop iteration; restore focus on the next
                            // idle so it takes effect after the popover settles.
                            // grab_focus_without_selecting avoids the select-all
                            // that grab_focus would cause, and set_position puts
                            // the cursor at the end so typing appends.
                            let row = to_row.clone();
                            glib::idle_add_local(move || {
                                row.grab_focus_without_selecting();
                                let len = row.text().len() as i32;
                                row.set_position(len);
                                glib::ControlFlow::Break
                            });
                        }
                    } else {
                        completion_popover.popdown();
                    }
                }

                // Cancel any pending debounce; schedule a new one.
                if let Some(src) = debounce_source.borrow_mut().take() {
                    src.remove();
                }
                let to_row = to_row.clone();
                let error_label = error_label.clone();
                // Separate clone for the inner timeout closure so the outer
                // `debounce_source` stays usable for `Some(src)` below.
                let inner_source = debounce_source.clone();
                let src = glib::timeout_add_local(
                    std::time::Duration::from_millis(DEBOUNCE_MS),
                    move || {
                        *inner_source.borrow_mut() = None;
                        let text = to_row.text();
                        let trimmed = text.trim();
                        let show_error = !trimmed.is_empty()
                            && normalize_recipient(trimmed).is_none();
                        error_label.set_visible(show_error);
                        glib::ControlFlow::Break
                    },
                );
                *debounce_source.borrow_mut() = Some(src);
            });
        }
        {
            let to_row = to_row.clone();
            let msg_row = msg_row.clone();
            let update_send_sensitivity = update_send_sensitivity.clone();
            msg_row.clone().connect_changed(move |_| {
                let to = to_row.text();
                let msg = msg_row.text();
                update_send_sensitivity(&to, &msg);
            });
        }

        let ui = self.clone();
        // `AlertDialog` auto-closes when any response fires, and libadwaita's
        // Rust binding for `close-attempt` doesn't let us block it. So when
        // validation fails, we let the dialog close and re-present it from
        // the `closed` signal — the user sees a brief flicker but the error
        // stays visible and their input is preserved.
        let validation_failed = Rc::new(Cell::new(false));
        let vf_response = validation_failed.clone();
        dialog.connect_response(None, move |dlg, resp| {
            if resp == "send" {
                let recipient = to_row.text();
                if normalize_recipient(&recipient).is_none() {
                    error_label.set_visible(true);
                    vf_response.set(true);
                    return;
                }
                let name = name_row.text();
                let text = msg_row.text();
                // Clear fields before closing so the dialog is fresh if reopened.
                to_row.set_text("");
                name_row.set_text("");
                msg_row.set_text("");
                dlg.close();
                // If the Name field holds the auto-filled suggestion verbatim, treat it as
                // unset — the chat will resolve the contact name dynamically. Only
                // persist a custom_name if the user actually edited the field.
                let suggestion = suggested_name.take();
                let is_suggestion = suggestion.as_deref() == Some(name.as_str());
                let name_owned: Option<String> = if name.is_empty() || is_suggestion {
                    None
                } else {
                    Some(name.to_string())
                };
                ui.create_new_chat(&recipient, &text, name_owned);
            } else {
                dlg.close();
            }
        });

        // Re-present the dialog if validation failed on the last Send click.
        let vf_closed = validation_failed.clone();
        let dialog_ref = dialog.clone();
        let split = self.split.clone();
        dialog.connect_closed(move |_| {
            if vf_closed.replace(false) {
                dialog_ref.present(Some(&split));
            }
        });

        dialog.present(Some(&self.split));
    }

    /// Submit a new chat: ingest the optimistic message, set a custom name,
    /// fire the network send, and open the chat in the messages view.
    pub(super) fn create_new_chat(&self, recipient: &str, text: &str, name: Option<String>) {
        let my_handle = match self.handles.first().cloned() {
            Some(h) => h,
            None => {
                eprintln!("no self handle; cannot create new chat");
                return;
            }
        };

        let (chat_ref, msg) = match new_chat_payload(recipient, text, &my_handle) {
            Some(p) => p,
            None => {
                eprintln!("invalid recipient for new chat: {}", recipient);
                return;
            }
        };

        let new_key = chat_ref.key();
        let guid = msg.guid.clone();
        let text_owned = text.to_string();

        // 1. Persist the optimistic message.
        let store = self.store.clone();
        let ui = self.clone();
        gtk_bridge::spawn(
            async move { store.apply(Ingest::Message(msg)).await },
            move |res| {
                if let Err(e) = res {
                    eprintln!("optimistic insert failed for new chat: {e:#}");
                    return;
                }
                // 2. Load chats and find the newly created one.
                let store = ui.store.clone();
                let ui = ui.clone();
                gtk_bridge::spawn(
                    async move { store.chats().await },
                    move |res| {
                        let chats = res.unwrap_or_else(|e| {
                            eprintln!("chats load error: {e:#}");
                            Vec::new()
                        });
                        let summary = match chats.iter().find(|c| c.key == new_key).cloned() {
                            Some(s) => s,
                            None => {
                                eprintln!("new chat not found in store after insert");
                                return;
                            }
                        };

                        // 3. Optionally set custom name, then open the chat.
                        if let Some(name_owned) = name {
                            let store = ui.store.clone();
                            let chat_id = summary.id;
                            let ui = ui.clone();
                            let summary = summary.clone();
                            let name_for_ui = name_owned.clone();
                            gtk_bridge::spawn(
                                async move {
                                    store
                                        .set_chat_custom_name(chat_id, Some(name_owned))
                                        .await
                                },
                                move |res| {
                                    if let Err(e) = res {
                                        eprintln!("set custom name failed: {e:#}");
                                    }
                                    let mut summary = summary;
                                    summary.custom_name = Some(name_for_ui);
                                    ui.reload_chats(|_| {});
                                    ui.open_chat(&summary);
                                },
                            );
                        } else {
                            ui.reload_chats(|_| {});
                            ui.open_chat(&summary);
                        }
                    },
                );
            },
        );

        // 4. Fire the network send in parallel (best-effort, matches send_text behavior).
        let backend = self.backend.clone();
        let client = self.client.clone();
        gtk_bridge::spawn(
            async move {
                backend
                    .send_text(&client, &chat_ref, &my_handle, text_owned, guid)
                    .await?;
                Ok::<(), anyhow::Error>(())
            },
            move |res| {
                if let Err(e) = res {
                    eprintln!("send failed for new chat: {e:#}");
                }
            },
        );
    }

    /// A scaffold preferences dialog. The "Account" group hosts Sign Out; add
    /// further settings as new groups/rows.
    pub(super) fn show_preferences(&self) {
        let dialog = adw::PreferencesDialog::new();
        let page = adw::PreferencesPage::new();

        // --- Display: chat text size with a live sample-bubble preview ---
        //
        // The slider is gone. Two `circular` stepper buttons (– / +) walk the
        // offset in whole points; a sample chat bubble below the row shows
        // the chosen size in real time, so the user sees exactly what their
        // messages will look like. The bubble updates via a single CSS rule
        // on a stable class — no widget rebuild, no flash, no main-thread
        // store call.
        let display = adw::PreferencesGroup::builder().title("Display").build();

        // Control row: title + stepper buttons.
        let size_row = adw::ActionRow::builder()
            .title("Chat text size")
            .build();

        // The +/− stepper buttons. We hold a handle to each so we can
        // disable the button that would push past the clamp. The tooltip
        // names the step so the user can predict the change before clicking.
        let dec_btn = gtk::Button::from_icon_name("value-decrease-symbolic");
        dec_btn.add_css_class("circular");
        dec_btn.set_tooltip_text(Some("Smaller text (–0.5 pt)"));
        let inc_btn = gtk::Button::from_icon_name("value-increase-symbolic");
        inc_btn.add_css_class("circular");
        inc_btn.set_tooltip_text(Some("Larger text (+0.5 pt)"));

        let stepper = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        stepper.append(&dec_btn);
        stepper.append(&inc_btn);
        size_row.add_suffix(&stepper);

        // Sample-bubble preview row. We build a tiny incoming-style bubble
        // with a sentence of placeholder text. The text size mirrors the
        // chat-text-size offset (base 13pt, same as a real bubble), updated
        // via a single shared CSS provider (`preview_provider`).
        let preview_row = adw::ActionRow::builder()
            .title("Preview")
            .build();
        let preview_bubble = build_preview_bubble();
        preview_row.add_suffix(&preview_bubble);

        // Wire the buttons. Each click clamps to the model's range, writes
        // the new value, refreshes the live preview, updates which buttons
        // are enabled, and asks the open chat to redraw so the user sees
        // the change in their messages too.
        let min = crate::text_scale::MIN_OFFSET;
        let max = crate::text_scale::MAX_OFFSET;
        let refresh_buttons = {
            let dec_btn = dec_btn.clone();
            let inc_btn = inc_btn.clone();
            move |val: f64| {
                dec_btn.set_sensitive(val > min);
                inc_btn.set_sensitive(val < max);
            }
        };
        let apply = {
            let dec_btn = dec_btn.clone();
            let inc_btn = inc_btn.clone();
            let ui = self.clone();
            move |delta: f64| {
                // Add the step, round to 1 decimal to match the persistence
                // format (`{:.1}`) and avoid float drift across many clicks
                // (e.g. starting from 0.1, +0.5 yields 0.6 in math but
                // 0.6000000000000001 in IEEE 754). Clamp after rounding so
                // the disabled-button state reflects the post-clamp value.
                let stepped = crate::text_scale::get() + delta;
                let rounded = (stepped * 10.0).round() / 10.0;
                let new_val = rounded.clamp(
                    crate::text_scale::MIN_OFFSET,
                    crate::text_scale::MAX_OFFSET,
                );
                if (new_val - crate::text_scale::get()).abs() < 1e-9 {
                    return;
                }
                crate::text_scale::set(new_val);
                // Refresh the preview bubble's font size in place.
                refresh_preview_css();
                // Refresh every open-chat message widget in place — no
                // widget rebuild, no chat re-select needed.
                refresh_chat_text_css();
                dec_btn.set_sensitive(new_val > min);
                inc_btn.set_sensitive(new_val < max);
                // Apply the new size to the open chat (if any) so messages
                // pick it up on the next render.
                ui.reload_open_chat();
            }
        };
        dec_btn.connect_clicked({
            let apply = apply.clone();
            move |_| apply(-0.5)
        });
        inc_btn.connect_clicked(move |_| apply(0.5));
        // Initial state: enable/disable buttons based on the loaded value,
        // and push the current value into the preview CSS so it reflects the
        // already-persisted preference on first open.
        refresh_buttons(crate::text_scale::get());
        refresh_preview_css();

        display.add(&size_row);
        display.add(&preview_row);

        // --- 24-hour clock switch ---
        //
        // A single Switch in the Display group. When on, chat-message timestamps
        // render as "13:30"; when off (the default), "01:30 PM". Writes through
        // to time_format::set on every toggle so the open chat (if any) picks
        // up the new format on its next render via reload_open_chat.
        let time_row = adw::ActionRow::builder()
            .title("24-hour time")
            .subtitle("Show message times as 13:30 instead of 01:30 PM")
            .build();
        let time_switch = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .active(matches!(crate::time_format::get(), crate::time_format::TimeFormat::H24))
            .build();
        time_row.add_suffix(&time_switch);
        time_switch.connect_state_set({
            let ui = self.clone();
            move |_, active| {
                let mode = if active {
                    crate::time_format::TimeFormat::H24
                } else {
                    crate::time_format::TimeFormat::AmPm
                };
                crate::time_format::set(mode);
                ui.reload_open_chat();
                glib::Propagation::Proceed
            }
        });
        display.add(&time_row);

        page.add(&display);

        // --- Sync ---
        #[cfg(feature = "rustpush")]
        {
        let sync_group = adw::PreferencesGroup::builder()
            .title("Sync")
            .description("Sync missed messages from iCloud when the app is closed or the device was off.")
            .build();
        let config_path = glib::user_data_dir().join("bubbles").join(crate::sync::CONFIG_FILENAME);
        let initial_config = crate::sync::read_config(&config_path);
        let cloud_sync_switch = adw::SwitchRow::builder()
            .title("Enable cloud sync")
            .subtitle("Fetches missed messages from iCloud via Apple's servers. Disable to skip the sync entirely.")
            .active(initial_config.cloud_sync_enabled)
            .build();
        {
            let config_path = config_path.clone();
            let backend = self.backend.clone();
            let store = self.store.clone();
            cloud_sync_switch.connect_active_notify(move |switch| {
                let new_config = crate::sync::BubblesConfig {
                    cloud_sync_enabled: switch.is_active(),
                };
                if let Err(e) = crate::sync::write_config(&config_path, &new_config) {
                    log::warn!("failed to write config: {e}");
                } else {
                    log::info!("cloud_sync_enabled toggled to {}", switch.is_active());
                }

                // When enabling cloud sync, ensure the iCloud Keychain clique
                // is set up so subsequent syncs can use it.
                #[cfg(feature = "rustpush")]
                if switch.is_active() {
                    let backend = backend.clone();
                    let store = store.clone();
                    crate::runtime::runtime().spawn(async move {
                        // Check for viable escrow bottles to determine
                        // whether to show a bottle-selection prompt.
                        let bottles = backend.get_viable_escrow_bottles().await;
                        match crate::protocol::decide_bottles_lookup_action(bottles) {
                            crate::protocol::BottlesLookupAction::EstablishFirstTime => {
                                // No viable bottles → first-time establish
                                // path (old orchestrator).
                                let password_prompt =
                                    build_password_prompt_closure(None);
                                match crate::protocol::rustpush_backend::orchestrate_sync_now_flow(
                                    &*backend,
                                    &store,
                                    crate::sync::manual_sync_cutoff_ms(now_ms()),
                                    true,
                                    password_prompt,
                                )
                                .await
                                {
                                    Ok(result) => log::info!(
                                        "cloud sync toggle: setup + sync completed ({} messages)",
                                        result.messages_processed
                                    ),
                                    Err(e) => log::error!("cloud sync toggle: {e}"),
                                }
                            }
                            crate::protocol::BottlesLookupAction::ShowBottleSelection(bottles) => {
                                // Bottles exist → show selection dialog and
                                // use the bottle-aware orchestrator.
                                let prompt = build_bottle_aware_prompt_closure(None, bottles);
                                match crate::protocol::rustpush_backend::orchestrate_sync_now_flow_with_bottle_prompt(
                                    &*backend,
                                    &store,
                                    crate::sync::manual_sync_cutoff_ms(now_ms()),
                                    true,
                                    prompt,
                                )
                                .await
                                {
                                    Ok(result) => log::info!(
                                        "cloud sync toggle: setup + sync completed ({} messages)",
                                        result.messages_processed
                                    ),
                                    Err(e) => log::error!("cloud sync toggle: {e}"),
                                }
                            }
                            crate::protocol::BottlesLookupAction::SurfaceError(reason) => {
                                // Bottle lookup unavailable — log the error
                                // and do NOT open the old establish prompt.
                                log::error!(
                                    "cloud sync toggle: bottle lookup unavailable: {reason}",
                                );
                            }
                        }
                    });
                }
            });
        }
        sync_group.add(&cloud_sync_switch);

        // Manual "Sync Now" button — forces a sync regardless of the
        // cloud_sync_enabled toggle (which only gates the automatic launch-gate
        // sync). This is useful when the user wants to pull missed messages
        // immediately, e.g., after a long period offline.
        #[cfg(feature = "rustpush")]
        {
            let sync_now_row = adw::ActionRow::builder()
                .title("Sync Now")
                .subtitle("Fetch missed messages from iCloud right now. Works even if automatic sync is disabled.")
                .build();
            let sync_now_button = gtk::Button::builder()
                .label("Sync Now")
                .halign(gtk::Align::End)
                .valign(gtk::Align::Center)
                .build();
            sync_now_button.add_css_class("pill");
            let sync_now_status = gtk::Label::new(Some(""));
            sync_now_status.add_css_class("dim-label");
            let sync_now_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(12)
                .build();
            sync_now_box.append(&sync_now_status);
            sync_now_box.append(&sync_now_button);
            sync_now_row.add_suffix(&sync_now_box);
            {
                let backend = self.backend.clone();
                let store = self.store.clone();
                let status_label = sync_now_status.clone();
                let button = sync_now_button.clone();
                let _window = self.window.clone();
                sync_now_button.connect_clicked(move |btn| {
                    btn.set_sensitive(false);
                    status_label.set_text("Syncing…");
                    let backend = backend.clone();
                    let store = store.clone();
                    let status_label = status_label.clone();
                    let button = button.clone();
                    // Run the orchestrator on the Tokio runtime (it touches
                    // hyper, which requires a reactor — `glib::spawn_future_local`
                    // doesn't provide one and would panic). Bridge the result
                    // back to the glib main thread for the UI update.
                    // NOTE: both prompt closures discard their parent argument,
                    // so we pass None here (capturing an adw::Window would make
                    // the async block !Send).
                    let (tx, rx) = oneshot::channel();
                    crate::runtime::runtime().spawn(async move {
                        // Fetch viable bottles first to decide which path
                        // to take (bottle-aware vs first-time establish vs
                        // error — the pre-fix bug collapsed errors into the
                        // establish path, showing the old two-textbox prompt).
                        let bottles = backend.get_viable_escrow_bottles().await;
                        let result = match crate::protocol::decide_bottles_lookup_action(bottles) {
                            crate::protocol::BottlesLookupAction::EstablishFirstTime => {
                                // No viable bottles → first-time establish path.
                                let password_prompt =
                                    build_password_prompt_closure(None);
                                crate::protocol::rustpush_backend::orchestrate_sync_now_flow(
                                    &*backend,
                                    &store,
                                    crate::sync::manual_sync_cutoff_ms(now_ms()),
                                    true,
                                    password_prompt,
                                )
                                .await
                            }
                            crate::protocol::BottlesLookupAction::ShowBottleSelection(bottles) => {
                                // Bottles exist → show selection dialog and use
                                // the bottle-aware orchestrator.
                                let prompt = build_bottle_aware_prompt_closure(None, bottles);
                                crate::protocol::rustpush_backend::orchestrate_sync_now_flow_with_bottle_prompt(
                                    &*backend,
                                    &store,
                                    crate::sync::manual_sync_cutoff_ms(now_ms()),
                                    true,
                                    prompt,
                                )
                                .await
                            }
                            crate::protocol::BottlesLookupAction::SurfaceError(reason) => {
                                // Bottle lookup unavailable — surface the error
                                // via the Err path so the status label shows it.
                                Err(reason)
                            }
                        };
                        let _ = tx.send(result);
                    });
                    glib::spawn_future_local(async move {
                        let result = match rx.await {
                            Ok(r) => r,
                            Err(_) => {
                                log::error!("manual sync: tokio task cancelled");
                                status_label.set_text("Sync failed");
                                button.set_sensitive(true);
                                return;
                            }
                        };
                        let summary = match result {
                            Ok(sync_result) => match sync_result.messages_processed {
                                0 => "No new messages".to_string(),
                                1 => "Synced 1 message".to_string(),
                                n => format!("Synced {n} messages"),
                            },
                            Err(e) => {
                                log::error!("manual sync: {e}");
                                status_label.set_text(&format!("Sync failed: {e}"));
                                button.set_sensitive(true);
                                return;
                            }
                        };
                        status_label.set_text(&summary);
                        log::info!("manual sync: {summary}");
                        button.set_sensitive(true);
                    });
                });
            }
            sync_group.add(&sync_now_row);
        }

        page.add(&sync_group);
        }

        // --- Account ---
        let account = adw::PreferencesGroup::builder().title("Account").build();
        let sign_out = gtk::Button::builder()
            .label("Sign Out")
            .halign(gtk::Align::Center)
            .margin_top(8)
            .build();
        sign_out.add_css_class("destructive-action");
        sign_out.add_css_class("pill");
        {
            let ui = self.clone();
            let dialog = dialog.clone();
            sign_out.connect_clicked(move |_| ui.confirm_sign_out(&dialog));
        }
        account.add(&sign_out);
        page.add(&account);

        dialog.add(&page);
        dialog.present(Some(&self.split));
    }

    /// Reload the open chat messages and sidebar so the new preference takes effect.
    pub(super) fn reload_open_chat(&self) {
        self.reload_chats(|_| {});
        if let Some(chat) = self.open_summary.borrow().as_ref() {
            self.reload_messages(chat.id, chat.is_group);
        }
    }

    pub(super) fn confirm_sign_out(&self, prefs: &adw::PreferencesDialog) {
        let confirm = adw::AlertDialog::new(
            Some("Sign Out?"),
            Some("This clears the saved login. The app will close — reopen it to sign in again."),
        );
        confirm.add_responses(&[("cancel", "Cancel"), ("signout", "Sign Out")]);
        confirm.set_response_appearance("signout", adw::ResponseAppearance::Destructive);
        confirm.set_default_response(Some("cancel"));
        confirm.set_close_response("cancel");
        let ui = self.clone();
        let prefs = prefs.clone();
        confirm.connect_response(None, move |_, resp| {
            if resp != "signout" {
                return;
            }
            // Clear persisted credentials, then quit so the live session (receive
            // loop, APNs connection) tears down cleanly. Next launch onboards.
            ui.backend.sign_out();
            prefs.close();
            if let Some(app) = gtk::gio::Application::default() {
                app.quit();
            }
        });
        confirm.present(Some(&self.split));
    }

    pub(super) fn show_about(&self) {
        let about = adw::AboutDialog::builder()
            .application_name("Bubbles")
            .version(env!("CARGO_PKG_VERSION"))
            .build();
        if let Some(id) = gtk::gio::Application::default().and_then(|a| a.application_id()) {
            about.set_application_icon(id.as_str());
        }
        about.present(Some(&self.split));
    }
}

// ---------------------------------------------------------------------------
// Tests — chat list selection state machine
// ---------------------------------------------------------------------------

#[cfg(test)]
mod chat_selection_tests {
    //! Pin the chat-list selection-state helper contract.
    //!
    //! The chat list's right-click "Select" menu item, long-press gesture, and
    //! left-click wiring all share a small piece of pure state: whether the
    //! list is in "selecting" mode and which chat ids are currently in the
    //! selection set. This module pins that state machine so the GTK wiring
    //! can be built against a pure, testable seam — no widget code, no I/O,
    //! no global state.
    //!
    //! Contract (mirrors the unit spec):
    //!
    //! * Initial state: not selecting, no selected chats.
    //! * Choosing `Select` on a chat enters selecting mode and selects it.
    //! * Long-pressing a chat has the same effect as `Select`.
    //! * While selecting, left-clicking a chat toggles it in/out of the set.
    //! * Deleting targets every currently selected chat id.
    //! * Clearing/exiting selection leaves: not selecting, no selected chats.
    //!
    //! The expected red is a compile error: the helper
    //! `super::ChatSelectionState` (with the methods used below) does not
    //! exist yet — adding it is the unit's deliverable. Each test constructs
    //! its own fresh helper, so no test depends on the order of execution or
    //! any shared mutable state.

    use super::*;

    #[test]
    fn initial_state_is_not_selecting_with_no_selected_chats() {
        let s = ChatSelectionState::new();
        assert!(
            !s.is_selecting(),
            "fresh state must not be in selecting mode"
        );
        assert!(
            s.selected_chat_ids().is_empty(),
            "fresh state must have no selected chats"
        );
    }

    #[test]
    fn choosing_select_enters_selecting_mode_and_selects_that_chat() {
        let mut s = ChatSelectionState::new();
        s.begin_select(7);
        assert!(s.is_selecting(), "Select must enter selecting mode");
        assert!(
            s.is_selected(7),
            "Select must mark the chosen chat as selected"
        );
        assert_eq!(s.selected_chat_ids(), vec![7]);
    }

    #[test]
    fn long_press_has_the_same_effect_as_select() {
        let mut s = ChatSelectionState::new();
        s.begin_select_from_long_press(42);
        assert!(
            s.is_selecting(),
            "long-press must enter selecting mode (same as Select)"
        );
        assert!(
            s.is_selected(42),
            "long-press must select the pressed chat"
        );
        assert_eq!(s.selected_chat_ids(), vec![42]);
    }

    #[test]
    fn left_click_while_selecting_toggles_another_chat_into_the_set() {
        let mut s = ChatSelectionState::new();
        s.begin_select(1);
        s.toggle_chat(2);
        assert!(s.is_selected(1), "originally selected chat stays selected");
        assert!(
            s.is_selected(2),
            "left-click on a new chat adds it to the selected set"
        );
        assert_eq!(s.selected_chat_ids().len(), 2);
    }

    #[test]
    fn left_click_while_selecting_toggles_an_already_selected_chat_out() {
        let mut s = ChatSelectionState::new();
        s.begin_select(1);
        s.toggle_chat(2);
        s.toggle_chat(2);
        assert!(s.is_selected(1));
        assert!(
            !s.is_selected(2),
            "left-click on an already-selected chat must remove it"
        );
        assert_eq!(s.selected_chat_ids(), vec![1]);
    }

    #[test]
    fn unselecting_the_only_selected_chat_exits_selecting_mode() {
        // Pin: after entering selection mode and then toggling the only
        // selected chat off, the list must drop out of selecting mode and
        // report an empty selection. Without this, the right-click menu
        // shows "Delete (0 selected)" / "Delete 0" because the UI still
        // believes it is in selection mode with nothing selected.
        let mut s = ChatSelectionState::new();
        s.begin_select(7);
        s.toggle_chat(7);
        assert!(
            !s.is_selecting(),
            "toggling the only selected chat off must exit selecting mode"
        );
        assert!(
            s.selected_chat_ids().is_empty(),
            "toggling the only selected chat off must leave the selected set empty"
        );
    }

    #[test]
    fn unselecting_the_final_chat_in_multi_select_exits_selecting_mode() {
        // Pin the multi-select case: with two chats selected, toggling one
        // off must keep selecting mode (another chat is still selected),
        // but toggling the final one off must drop out of selecting mode.
        let mut s = ChatSelectionState::new();
        s.begin_select(1);
        s.toggle_chat(2);
        s.toggle_chat(1);
        assert!(
            s.is_selecting(),
            "with another chat still selected, toggling one off must keep selecting mode"
        );
        assert_eq!(
            s.selected_chat_ids(),
            vec![2],
            "after toggling one of two selected chats off, exactly the other remains"
        );
        s.toggle_chat(2);
        assert!(
            !s.is_selecting(),
            "toggling the final selected chat off must exit selecting mode"
        );
        assert!(
            s.selected_chat_ids().is_empty(),
            "toggling the final selected chat off must leave the selected set empty"
        );
    }

    #[test]
    fn delete_targets_all_currently_selected_chat_ids() {
        let mut s = ChatSelectionState::new();
        s.begin_select(1);
        s.toggle_chat(2);
        s.toggle_chat(3);
        // Iteration order is an implementation choice; what the contract
        // pins is that the delete path iterates over exactly the full
        // selected set.
        let mut targets = s.selected_chat_ids();
        targets.sort();
        assert_eq!(targets, vec![1, 2, 3]);
    }

    #[test]
    fn clear_leaves_not_selecting_with_no_selected_chats() {
        let mut s = ChatSelectionState::new();
        s.begin_select(1);
        s.toggle_chat(2);
        s.clear();
        assert!(!s.is_selecting(), "clear must exit selecting mode");
        assert!(
            s.selected_chat_ids().is_empty(),
            "clear must drop every selected chat"
        );
        assert!(!s.is_selected(1), "clear must forget every prior selection");
        assert!(!s.is_selected(2), "clear must forget every prior selection");
    }
}
