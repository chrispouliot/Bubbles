//! Message timeline logic: opening chats, reloading/populating messages,
//! older-message paging, unread divider/pill behavior, scroll-to helpers,
//! and read-state management.
//!
//! Extracted from `mod.rs` to keep the parent module focused on orchestration.
//! All types/functions from the parent are available via `use super::*`.

use super::*;

impl super::Ui {
    /// Replace a single link preview card in place. Driven by the
    /// `RecvEvent::LinkPreviewUpdated` event from the receive loop. The full
    /// `reload_messages` path is forbidden on this event (it would flicker and
    /// jump scroll); we walk the tracked card map, find the live widget, and
    /// swap in a freshly-built one built from the new store row. No-op when the
    /// chat is closed or the message isn't currently on screen.
    pub(super) fn refresh_link_card(&self, guid: &str, part_idx: i64) {
        let key = (guid.to_string(), part_idx);
        let Some(old) = self.preview_cards.borrow().get(&key).cloned() else {
            return;
        };
        // The card's parent is the inner `col` `gtk::Box` from `message_body`.
        // `Widget::parent()` returns a generic `Widget`; downcast to Box for
        // remove/append. If the parent isn't a Box (shouldn't happen with our
        // own builders), the registration is stale — drop it and bail.
        let Some(parent_box) = old.parent().and_then(|p| p.downcast::<gtk::Box>().ok()) else {
            self.preview_cards.borrow_mut().remove(&key);
            return;
        };
        let store = self.store.clone();
        let guid_for_async = guid.to_string();
        let guid_for_lookup = guid.to_string();
        let key_for_async = key.clone();
        let ui = self.clone();
        gtk_bridge::spawn(
            async move {
                store
                    .message_link_previews_for(vec![guid_for_async])
                    .await
            },
            move |res| {
                let previews = res.unwrap_or_default();
                let Some(p) = previews.get(&(guid_for_lookup, part_idx)).cloned() else {
                    // The row was deleted between the receive and the read; the
                    // card is still on screen with stale data. Just drop the
                    // registration; a later refresh will sort it out.
                    ui.preview_cards.borrow_mut().remove(&key_for_async);
                    return;
                };
                let new_card = link_preview_card(&p);
                // Replace: drop the old widget, register the new one. GTK will
                // dispose the old widget when we remove it from its parent.
                parent_box.remove(&old);
                parent_box.append(&new_card);
                ui.preview_cards
                    .borrow_mut()
                    .insert((p.message_guid.clone(), part_idx), new_card);
            },
        );
    }

    pub(super) fn open_chat(&self, chat: &ChatSummary) {
        // Switching away: stop any outbound typing on the previous chat, and clear
        // the inbound indicator (it belongs to the chat we're leaving).
        let prev = self.open_summary.borrow().clone();
        if self.typing_sent.replace(false) {
            if let Some(p) = prev.as_ref().filter(|p| p.id != chat.id) {
                self.send_typing(p, false);
            }
        }
        self.hide_typing_indicator();
        self.clear_pending_attachment();
        *self.open_summary.borrow_mut() = Some(chat.clone());
        self.content_page.set_title(&chat_title(chat, &self.handles, &self.contacts.borrow()));
        self.rename_button.set_sensitive(true);
        self.split.set_show_content(true);
        self.compose_outer.set_visible(true);
        // Drop the empty-state illustration now that a real conversation is
        // loaded into the content pane.
        self.content_stack.set_visible_child_name("chat");
        // Opening the chat means reading it — cancel any pending notification.
        self.pending_notifications.borrow_mut().cancel(chat.id);
        self.withdraw_chat_notification(chat.id);

        let store = self.store.clone();
        let backend = self.backend.clone();
        let client = self.client.clone();
        let ui = self.clone();
        let chat_id = chat.id;
        let is_group = chat.is_group;
        let chat_ref = chat_ref_of(chat);
        let my_handle = self_handle(&chat.participants, &self.handles);

        gtk_bridge::spawn(
            async move {
                // Capture the unread boundary BEFORE acking, then load and ack.
                let first = store.first_unread_incoming(chat_id).await.ok().flatten();
                let latest = store.latest_unread_incoming(chat_id).await.ok().flatten();
                let msgs = store
                    .messages_page(chat_id, None, PAGE_SIZE)
                    .await
                    .unwrap_or_default();
                // Batch-load previews with the messages (single round-trip,
                // off the GTK main thread). The renderer reads from this map
                // synchronously so it never blocks on a store call.
                let previews = store
                    .message_link_previews_for(msgs.iter().map(|m| m.guid.clone()).collect())
                    .await
                    .unwrap_or_default();
                if let Some((_, date)) = &latest {
                    let _ = store.mark_read_through(chat_id, *date).await;
                }
                // Fetch tapbacks and group for reaction chips.
                let tapbacks = store.tapbacks_for_chat(chat_id).await.unwrap_or_default();
                let live = live_tapbacks(&tapbacks);
                let reactions = group_tapbacks_by_target(live);
                (msgs, previews, first, latest.map(|(g, _)| g), reactions)
            },
            move |(msgs, previews, first, receipt_guid, reactions)| {
                // Reset pagination for the newly opened chat.
                *ui.page_oldest.borrow_mut() = msgs.first().map(|m| (m.date, m.id));
                *ui.page_has_more.borrow_mut() = msgs.len() as i64 >= PAGE_SIZE;
                *ui.page_loading.borrow_mut() = false;
                *ui.unread.borrow_mut() = first.clone();

                let anchor = first.as_ref().map(|(g, _)| g.as_str());
                let on_reaction = ui.make_reaction_handler();
                let on_edit = ui.make_edit_handler();
                let on_retry = ui.make_retry_handler();
                let (marker, chip_map) = populate_messages(&ui.msg_container, &msgs, is_group, anchor, &previews, &ui.preview_cards, on_reaction.as_ref(), on_edit.as_ref(), on_retry.as_ref(), &reactions, &ui.handles, &ui.contacts.borrow());
                *ui.current_chips.borrow_mut() = chip_map;
                *ui.current_reactions.borrow_mut() = reactions.clone();
                *ui.unread_marker_shown.borrow_mut() = marker.is_some();
                *ui.unread_marker.borrow_mut() = marker.clone();
                ui.update_unread_pill();
                ui.refresh_typing_row(is_group);
                sync_tracked_state_after_rebuild(
                    &ui.msg_container,
                    &msgs,
                    &ui.rendered_guids,
                    &ui.current_receipt_text,
                    &ui.receipt_label,
                    &ui.current_text,
                );

                let to = match &marker {
                    Some(w) => ScrollTo::Widget(w.clone()),
                    None => ScrollTo::Bottom,
                };
                ui.scroll_to(to);
                // The divider has done its job — showed where you left off and
                // scrolled there. Dismiss it shortly so it doesn't linger over
                // messages you've now read.
                ui.arm_unread_dismiss();
                if SEND_READ_RECEIPTS {
                    if let (Some(guid), Some(handle)) = (receipt_guid, my_handle) {
                        backend.send_receipt(&client, &chat_ref, &handle, true, guid);
                    }
                }
                // The chat is now read; refresh the sidebar to clear its badge.
                ui.reload_chats(|_| {});
            },
        );
    }

    /// Reload the open chat's messages in place (after sends/receives while
    /// viewing). Follows the bottom only if already there; otherwise holds
    /// position so reading history isn't interrupted.
    pub(super) fn reload_messages(&self, chat_id: i64, is_group: bool) {
        let store = self.store.clone();
        let ui = self.clone();
        // Rebuild only the window currently loaded (oldest shown -> now), so a
        // new message doesn't collapse history the user scrolled up to read.
        let since = *self.page_oldest.borrow();
        // While the window is backgrounded, recompute the unread boundary so the
        // "New Messages" divider appears live — you can glance at the background
        // window and see it. While focused, keep the existing anchor: the chat is
        // being read and the divider self-dismisses.
        let recompute_unread = !self.focused.get();
        gtk_bridge::spawn(
            async move {
                let msgs = store.messages_from(chat_id, since).await;
                let previews = if let Ok(msgs) = &msgs {
                    store
                        .message_link_previews_for(msgs.iter().map(|m| m.guid.clone()).collect())
                        .await
                        .unwrap_or_default()
                } else {
                    Default::default()
                };
                let first = if recompute_unread {
                    Some(store.first_unread_incoming(chat_id).await.ok().flatten())
                } else {
                    None
                };
                // Fetch tapbacks and group for reaction chips.
                let tapbacks = store.tapbacks_for_chat(chat_id).await.unwrap_or_default();
                let live = live_tapbacks(&tapbacks);
                let reactions = group_tapbacks_by_target(live);
                (msgs, previews, first, reactions)
            },
            move |(res, previews, first, reactions)| {
                let msgs = res.unwrap_or_else(|e| {
                    eprintln!("messages load error: {e:#}");
                    Vec::new()
                });
                if let Some(first) = first {
                    *ui.unread.borrow_mut() = first;
                }
                let adj = ui.scroller.vadjustment();
                let at_bottom = adj.value() + adj.page_size() >= adj.upper() - 80.0;
                let prev = adj.value();

                // Read tracked state and decide how to update the view.
                let prev_guids = ui.rendered_guids.borrow().clone();
                let prev_receipt = ui.current_receipt_text.borrow().clone();
                let prev_reactions = ui.collect_current_reactions();
                let plan = plan_chat_update(
                    &prev_guids,
                    prev_receipt.as_deref(),
                    &prev_reactions,
                    &ui.current_text.borrow(),
                    &msgs,
                    &reactions,
                );

                // Remove any stale typing row that was left in the container by
                // the typing-indicator path.  For the Rebuild path this is a no-op
                // (clear_box handles it), for the in-place paths it is essential.
                ui.remove_typing_row();

                match plan {
                    ChatUpdatePlan::Noop => {
                        ui.refresh_typing_row(is_group);
                        ui.update_unread_pill();
                    }
                    ChatUpdatePlan::UpdateReceipt { new_text } => {
                        if let Some(label) = ui.receipt_label.borrow().as_ref() {
                            label.set_text(&new_text);
                        }
                        *ui.current_receipt_text.borrow_mut() = Some(new_text);
                        ui.refresh_typing_row(is_group);
                        ui.update_unread_pill();
                        let to = if at_bottom {
                            ScrollTo::Bottom
                        } else {
                            ScrollTo::Value(prev)
                        };
                        ui.scroll_to(to);
                    }
                    ChatUpdatePlan::EditText { guid, new_text } => {
                        if let Some(entry) = ui.current_chips.borrow().get(&guid) {
                            if let Some(label) = find_label_in_bubble(&entry.bubble) {
                                label.set_markup(&text_to_markup(&new_text));
                            }
                        }
                        ui.current_text.borrow_mut().insert(guid, new_text);
                        ui.refresh_typing_row(is_group);
                        ui.update_unread_pill();
                    }
                    ChatUpdatePlan::Append { new_tail, receipt } => {
                        let on_reaction = ui.make_reaction_handler();
                        let on_edit = ui.make_edit_handler();
                        let on_retry = ui.make_retry_handler();
                        // Seed the group state from the last previously-rendered
                        // message so the first appended widget gets correct spacing.
                        let prev_msg = prev_guids
                            .last()
                            .and_then(|g| msgs.iter().find(|m| &m.guid == g));
                        let anchor = if !*ui.unread_marker_shown.borrow() {
                            ui.unread.borrow().as_ref().map(|(g, _)| g.clone())
                        } else {
                            None
                        };
                        let (widgets, marker, chip_map) = build_message_widgets(
                            &new_tail,
                            is_group,
                            anchor.as_deref(),
                            &previews,
                            &ui.preview_cards,
                            on_reaction.as_ref(),
                            on_edit.as_ref(),
                            on_retry.as_ref(),
                            &reactions,
                            prev_msg,
                            &ui.handles,
                            &ui.contacts.borrow(),
                            None,
                        );
                        ui.current_chips.borrow_mut().extend(chip_map);
                        *ui.current_reactions.borrow_mut() = reactions.clone();
                        for w in &widgets {
                            ui.msg_container.append(w);
                        }
                        if let Some(w) = marker {
                            *ui.unread_marker.borrow_mut() = Some(w.clone());
                            *ui.unread_marker_shown.borrow_mut() = true;
                            ui.update_unread_pill();
                        }
                        // Handle the receipt action.
                        match receipt {
                            ReceiptAction::Keep => {}
                            ReceiptAction::Set(text) => {
                                if let Some(old) = ui.receipt_label.borrow_mut().take() {
                                    ui.msg_container.remove(&old);
                                }
                                *ui.receipt_label.borrow_mut() = None;
                                *ui.current_receipt_text.borrow_mut() = None;
                                let label_widget = receipt_label(&text);
                                ui.msg_container.append(&label_widget);
                                if let Ok(label) =
                                    label_widget.downcast::<gtk::Label>()
                                {
                                    *ui.receipt_label.borrow_mut() = Some(label);
                                }
                                *ui.current_receipt_text.borrow_mut() = Some(text);
                            }
                            ReceiptAction::Remove => {
                                if let Some(old) = ui.receipt_label.borrow_mut().take() {
                                    ui.msg_container.remove(&old);
                                }
                                *ui.receipt_label.borrow_mut() = None;
                                *ui.current_receipt_text.borrow_mut() = None;
                            }
                        }
                        // Update the rendered-guid list to include the new messages.
                        let mut new_guids = prev_guids;
                        for m in &new_tail {
                            new_guids.push(m.guid.clone());
                        }
                        *ui.rendered_guids.borrow_mut() = new_guids;
                        // Also register text for the new bubbles.
                        for m in &new_tail {
                            if let Some(text) = &m.text {
                                ui.current_text.borrow_mut().insert(m.guid.clone(), text.clone());
                            }
                        }
                        ui.refresh_typing_row(is_group);
                        ui.update_unread_pill();
                        // morph_pending: if the typing dots were superseded by
                        // an incoming message, fade the newly-arrived bubble in.
                        if ui.morph_pending.replace(false) {
                            if let Some(last_msg) = widgets.last() {
                                last_msg.add_css_class("bubble-appear");
                            }
                        }
                        let to = if at_bottom {
                            ScrollTo::Bottom
                        } else {
                            ScrollTo::Value(prev)
                        };
                        ui.scroll_to(to);
                    }
                    ChatUpdatePlan::UpdateChips { changes } => {
                        // Build a quick guid → is_from_me lookup from the new message list.
                        let own_lookup: std::collections::HashMap<String, bool> = msgs.iter()
                            .filter(|m| m.associated_guid.is_none())
                            .map(|m| (m.guid.clone(), m.is_from_me))
                            .collect();

                        // Snapshot the chip map so we can iterate without holding the borrow.
                        let chips_snapshot: Vec<(String, gtk::Widget)> = ui.current_chips.borrow()
                            .iter()
                            .map(|(g, e)| (g.clone(), e.bubble.clone()))
                            .collect();

                        for change in changes {
                            let target = &change.target_guid;
                            let bubble_or_overlay = chips_snapshot.iter()
                                .find(|(g, _)| g == target)
                                .map(|(_, w)| w.clone());
                            if let Some(bubble) = bubble_or_overlay {
                                let is_from_me = own_lookup.get(target).copied().unwrap_or(false);
                                apply_chip_change(
                                    target,
                                    &change.new_chips,
                                    &bubble,
                                    is_from_me,
                                    &ui.current_chips,
                                );
                            } else {
                                // First reaction on a message we don't have a bubble for.
                                // This shouldn't normally happen if the chip map is kept
                                // in sync (every message in view has a bubble entry). If
                                // it does, just log and skip.
                                eprintln!("UpdateChips: no bubble entry for {target}, skipping");
                            }
                        }
                        // Update tracked reactions so the next plan correctly computes
                        // prev_reactions for removal detection.
                        *ui.current_reactions.borrow_mut() = reactions.clone();
                        ui.refresh_typing_row(is_group);
                        ui.update_unread_pill();
                    }
                    ChatUpdatePlan::Rebuild => {
                        let anchor = ui.unread.borrow().as_ref().map(|(g, _)| g.clone());
                        let on_reaction = ui.make_reaction_handler();
                        let on_edit = ui.make_edit_handler();
                        let on_retry = ui.make_retry_handler();
                        let (marker, chip_map) = populate_messages(
                            &ui.msg_container,
                            &msgs,
                            is_group,
                            anchor.as_deref(),
                            &previews,
                            &ui.preview_cards,
                            on_reaction.as_ref(),
                            on_edit.as_ref(),
                            on_retry.as_ref(),
                            &reactions,
                            &ui.handles,
                            &ui.contacts.borrow(),
                        );
                        *ui.current_chips.borrow_mut() = chip_map;
                        *ui.current_reactions.borrow_mut() = reactions.clone();
                        *ui.unread_marker_shown.borrow_mut() = marker.is_some();
                        *ui.unread_marker.borrow_mut() = marker.clone();
                        ui.update_unread_pill();
                        ui.refresh_typing_row(is_group);
                        sync_tracked_state_after_rebuild(
                            &ui.msg_container,
                            &msgs,
                            &ui.rendered_guids,
                            &ui.current_receipt_text,
                            &ui.receipt_label,
                            &ui.current_text,
                        );
                        if ui.morph_pending.replace(false) {
                            if let Some(last) = ui.msg_container.last_child() {
                                last.add_css_class("bubble-appear");
                            }
                        }
                        let to = if at_bottom {
                            ScrollTo::Bottom
                        } else {
                            ScrollTo::Value(prev)
                        };
                        ui.scroll_to(to);
                    }
                }
            },
        );
    }

    /// Fetch the page just before the oldest currently-shown message, prepend it,
    /// and keep the viewport anchored on the same message.
    pub(super) fn maybe_load_older(&self) {
        if *self.page_loading.borrow() || !*self.page_has_more.borrow() {
            return;
        }
        let chat = match self.open_summary.borrow().clone() {
            Some(c) => c,
            None => return,
        };
        let cursor = match *self.page_oldest.borrow() {
            Some(c) => c,
            None => return,
        };
        *self.page_loading.borrow_mut() = true;

        // Capture the oldest currently-rendered message's date so that after
        // building the prepend batch we can detect when the batch's newest
        // non-today message shares the same calendar date — and remove the
        // now-redundant date divider from the old content.
        let old_oldest_date = self.page_oldest.borrow().map(|(date, _id)| date);

        let store = self.store.clone();
        let ui = self.clone();
        let chat_id = chat.id;
        let is_group = chat.is_group;

        gtk_bridge::spawn(
            async move {
                let older = store.messages_page(chat_id, Some(cursor), PAGE_SIZE).await;
                let previews = if let Ok(older) = &older {
                    store
                        .message_link_previews_for(older.iter().map(|m| m.guid.clone()).collect())
                        .await
                        .unwrap_or_default()
                } else {
                    Default::default()
                };
                // Also fetch tapbacks for the chat so older messages (prepended
                // by this page) get reaction chips. Without this, `build_message_widgets`
                // would have no `reactions` map to look up, and old tapbacks would
                // never render when the user scrolls up.
                let tapbacks = store.tapbacks_for_chat(chat_id).await.unwrap_or_default();
                let reactions = group_tapbacks_by_target(live_tapbacks(&tapbacks));
                (older, previews, reactions)
            },
            move |(res, previews, reactions)| {
                let older = res.unwrap_or_default();
                // Bail if the user switched chats while we were loading.
                let still_open = ui
                    .open_summary
                    .borrow()
                    .as_ref()
                    .is_some_and(|c| c.id == chat_id);
                if !still_open {
                    *ui.page_loading.borrow_mut() = false;
                    return;
                }
                if older.is_empty() {
                    *ui.page_has_more.borrow_mut() = false;
                    *ui.page_loading.borrow_mut() = false;
                    return;
                }

                *ui.page_oldest.borrow_mut() = older.first().map(|m| (m.date, m.id));
                *ui.page_has_more.borrow_mut() = older.len() as i64 >= PAGE_SIZE;

                // Capture the anchor right before we change the height — not
                // before the async load, since the user may have scrolled while
                // it was in flight.
                let adj = ui.scroller.vadjustment();
                let old_value = adj.value();
                // Anchor on the *actual* position of the current top message rather
                // than a measured height. measure() returns natural sizes, and a
                // GtkPicture's natural height is the image's intrinsic (unscaled)
                // height — so with photos in the batch the measured delta overshot
                // and, accumulating across batches, flung the view downward.
                // compute_bounds gives the true post-layout shift instead.
                let anchor_widget = ui.msg_container.first_child();
                let anchor_old_y = anchor_widget
                    .as_ref()
                    .and_then(|w| w.compute_bounds(&ui.msg_container))
                    .map(|b| b.y() as f64)
                    .unwrap_or(0.0);
                // Minimum-size baseline for the pre-layout estimate below. Minimum
                // sizes track actual allocation (a photo's minimum is its scaled
                // size, not its huge intrinsic natural size), unlike natural sizes.
                let anchor_width = ui.msg_container.width();
                let old_min_h = if anchor_width > 0 {
                    ui.msg_container
                        .measure(gtk::Orientation::Vertical, anchor_width)
                        .0 as f64
                } else {
                    adj.upper()
                };

                // Prepend in reverse so the batch keeps its order at the top. If
                // this page contains the first unread, the divider slots in here
                // and the floating pill is dismissed.
                let anchor = ui.unread.borrow().as_ref().map(|(g, _)| g.clone());
                let on_reaction = ui.make_reaction_handler();
                let on_edit = ui.make_edit_handler();
                let on_retry = ui.make_retry_handler();
                let (widgets, marker, chip_map) = build_message_widgets(
                    &older,
                    is_group,
                    anchor.as_deref(),
                    &previews,
                    &ui.preview_cards,
                    on_reaction.as_ref(),
                    on_edit.as_ref(),
                    on_retry.as_ref(),
                    &reactions,
                    None,
                    &ui.handles,
                    &ui.contacts.borrow(),
                    None, // divider_prev_date: let the batch render its own dividers
                );
                ui.current_chips.borrow_mut().extend(chip_map);
                *ui.current_reactions.borrow_mut() = reactions.clone();
                // If the batch's newest non-today calendar date matches the old
                // oldest message's date, the old top date divider is now redundant,
                // because the batch already provides one for that date farther up
                // the timeline.  Remove it so the date appears only once.
                if let Some(old_d) = old_oldest_date {
                    let now = now_ms();
                    let newest_nontoday = older.iter().rev().find(|m| {
                        crate::time_format::should_show_date_divider(None, m.date, now)
                    });
                    if let Some(nn) = newest_nontoday {
                        let same_date =
                            !crate::time_format::should_show_date_divider(Some(old_d), nn.date, now);
                        if same_date {
                            if let Some(first) = ui.msg_container.first_child() {
                                if first.has_css_class("date-divider") {
                                    ui.msg_container.remove(&first);
                                }
                            }
                        }
                    }
                }
                // Capture the old content's first child *before* prepending batch
                // widgets, so we can insert a boundary divider between the batch
                // and the existing timeline when they span different calendar days.
                let old_first_child = ui.msg_container.first_child();
                for w in widgets.into_iter().rev() {
                    ui.msg_container.prepend(&w);
                }
                // After prepending, if the newest real message in the batch and
                // the oldest previously-rendered real message are on different
                // calendar days, insert a date divider at the boundary.  This
                // covers the case where existing content starts with today and
                // therefore had no top divider — the boundary now needs one.
                // We do NOT duplicate an already-present top divider (one that
                // was already on the old content and survived the removal above).
                if let Some(old_d) = old_oldest_date {
                    if let Some(newest_batch) =
                        older.iter().rev().find(|m| m.associated_guid.is_none())
                    {
                        let now = now_ms();
                        if crate::time_format::should_show_date_divider(
                            Some(newest_batch.date),
                            old_d,
                            now,
                        ) {
                            if let Some(ref first) = old_first_child {
                                if !first.has_css_class("date-divider") {
                                    let label = crate::time_format::format_date_label(old_d);
                                    ui.msg_container.insert_before(
                                        &date_divider(&label),
                                        Some(first),
                                    );
                                }
                            }
                        }
                    }
                }
                if marker.is_some() {
                    *ui.unread_marker_shown.borrow_mut() = true;
                    ui.update_unread_pill();
                }

                // Anchor *synchronously*, before returning to the main loop, so no
                // frame can paint the prepended batch at the old scroll value. The
                // async callback may run after this frame's update phase, in which
                // case the tick below wouldn't fire until the next frame — leaving
                // one painted flash. The minimum-size measurement is available now
                // (it forces a re-measure including the new rows), and minimum sizes
                // match actual allocation, so this first anchor is already correct.
                {
                    let width = ui.msg_container.width();
                    let new_min = if width > 0 {
                        ui.msg_container
                            .measure(gtk::Orientation::Vertical, width)
                            .0 as f64
                    } else {
                        adj.upper()
                    };
                    if new_min > adj.upper() {
                        adj.set_upper(new_min);
                    }
                    adj.set_value(old_value + (new_min - old_min_h).max(0.0));
                }

                // Re-anchor before the frame paints. We can't predict the height
                // with container.measure() — it returns natural sizes, and a
                // GtkPicture's natural height is the photo's intrinsic (unscaled)
                // height, so any batch with images overshot and (compounding across
                // batches) flung the view down. Instead we watch the anchor message's
                // real position and shift by exactly how far it moved once layout
                // reflects the prepend.
                let scroller = ui.scroller.clone();
                let container = ui.msg_container.clone();
                let loading = ui.page_loading.clone();
                let frames = Cell::new(0u32);
                let stable = Cell::new(0u32);
                let last_shift = Cell::new(f64::NAN);
                ui.scroller.add_tick_callback(move |_w, _clock| {
                    let adj = scroller.vadjustment();
                    let actual = anchor_widget
                        .as_ref()
                        .and_then(|w| w.compute_bounds(&container))
                        .map(|b| (b.y() as f64 - anchor_old_y).max(0.0));
                    // Once layout reflects the prepend, compute_bounds is exact.
                    // Until then it still reads the old position (shift ~0), which
                    // would paint one unanchored frame — the flash. Fall back to a
                    // minimum-size measurement of the added height so that first
                    // frame is already anchored.
                    let shift = match actual {
                        Some(s) if s > 0.5 => s,
                        _ => {
                            let width = container.width();
                            let new_min = if width > 0 {
                                container.measure(gtk::Orientation::Vertical, width).0 as f64
                            } else {
                                adj.upper()
                            };
                            (new_min - old_min_h).max(0.0)
                        }
                    };
                    adj.set_value(old_value + shift);
                    // Re-assert until the shift settles (layout done), so a pre-layout
                    // frame can't leave us anchored short.
                    if (shift - last_shift.get()).abs() < 0.5 {
                        stable.set(stable.get() + 1);
                    } else {
                        stable.set(0);
                    }
                    last_shift.set(shift);
                    frames.set(frames.get() + 1);
                    if stable.get() >= 4 || frames.get() >= 24 {
                        *loading.borrow_mut() = false;
                        glib::ControlFlow::Break
                    } else {
                        glib::ControlFlow::Continue
                    }
                });
            },
        );
    }

    /// Load everything from the first unread message down to now, render with the
    /// divider in place, and scroll to it. Backs the floating pill.
    pub(super) fn jump_to_first_unread(&self) {
        let chat = match self.open_summary.borrow().clone() {
            Some(c) => c,
            None => return,
        };
        let (guid, date) = match self.unread.borrow().clone() {
            Some(u) => u,
            None => return,
        };
        let store = self.store.clone();
        let ui = self.clone();
        let chat_id = chat.id;
        let is_group = chat.is_group;
        gtk_bridge::spawn(
            async move {
                let msgs = store.messages_from(chat_id, Some((date, 0))).await;
                let previews = if let Ok(msgs) = &msgs {
                    store
                        .message_link_previews_for(msgs.iter().map(|m| m.guid.clone()).collect())
                        .await
                        .unwrap_or_default()
                } else {
                    Default::default()
                };
                // Fetch tapbacks and group for reaction chips.
                let tapbacks = store.tapbacks_for_chat(chat_id).await.unwrap_or_default();
                let live = live_tapbacks(&tapbacks);
                let reactions = group_tapbacks_by_target(live);
                (msgs, previews, reactions)
            },
            move |(res, previews, reactions)| {
                let msgs = res.unwrap_or_default();
                let still_open = ui
                    .open_summary
                    .borrow()
                    .as_ref()
                    .is_some_and(|c| c.id == chat_id);
                if !still_open {
                    return;
                }
                *ui.page_oldest.borrow_mut() = msgs.first().map(|m| (m.date, m.id));
                // Read history still sits above the first unread.
                *ui.page_has_more.borrow_mut() = true;
                *ui.page_loading.borrow_mut() = false;
                let on_reaction = ui.make_reaction_handler();
                let on_edit = ui.make_edit_handler();
                let on_retry = ui.make_retry_handler();
                let (marker, chip_map) = populate_messages(
                    &ui.msg_container,
                    &msgs,
                    is_group,
                    Some(guid.as_str()),
                    &previews,
                    &ui.preview_cards,
                    on_reaction.as_ref(),
                    on_edit.as_ref(),
                    on_retry.as_ref(),
                    &reactions,
                    &ui.handles,
                    &ui.contacts.borrow(),
                );
                *ui.current_chips.borrow_mut() = chip_map;
                *ui.current_reactions.borrow_mut() = reactions.clone();
                *ui.unread_marker_shown.borrow_mut() = marker.is_some();
                *ui.unread_marker.borrow_mut() = marker.clone();
                ui.update_unread_pill();
                ui.refresh_typing_row(is_group);
                sync_tracked_state_after_rebuild(
                    &ui.msg_container,
                    &msgs,
                    &ui.rendered_guids,
                    &ui.current_receipt_text,
                    &ui.receipt_label,
                    &ui.current_text,
                );
                let to = match &marker {
                    Some(w) => ScrollTo::Widget(w.clone()),
                    None => ScrollTo::Bottom,
                };
                ui.scroll_to(to);
            },
        );
    }

    /// Show the pill only when unread messages exist that aren't yet on screen.
    fn update_unread_pill(&self) {
        let show = self.unread.borrow().is_some() && !*self.unread_marker_shown.borrow();
        self.unread_pill.set_visible(show);
    }

    /// Remove the "New Messages" divider and forget the unread boundary, so a
    /// later refresh won't redraw it. Safe to call repeatedly.
    pub(super) fn dismiss_unread_divider(&self) {
        self.unread_dismiss_gen
            .set(self.unread_dismiss_gen.get().wrapping_add(1));
        if let Some(w) = self.unread_marker.borrow_mut().take() {
            self.msg_container.remove(&w);
        }
        *self.unread.borrow_mut() = None;
        *self.unread_marker_shown.borrow_mut() = false;
        self.update_unread_pill();
    }

    /// Arm a one-shot timer that dismisses the divider after a short dwell.
    /// Always bumps the generation (invalidating any timer from a previously
    /// opened chat); only schedules when a divider is actually on screen.
    pub(super) fn arm_unread_dismiss(&self) {
        let gen = self.unread_dismiss_gen.get().wrapping_add(1);
        self.unread_dismiss_gen.set(gen);
        if self.unread_marker.borrow().is_none() {
            return;
        }
        let ui = self.clone();
        glib::timeout_add_local_once(
            std::time::Duration::from_secs(UNREAD_DIVIDER_TTL_SECS),
            move || {
                if ui.unread_dismiss_gen.get() == gen {
                    ui.dismiss_unread_divider();
                }
            },
        );
    }

    /// Ack the newest unread inbound message (implicitly marking earlier ones read).
    pub(super) fn maybe_send_read(&self, chat: &ChatSummary) {
        if !SEND_READ_RECEIPTS {
            return;
        }
        let Some(my_handle) = self_handle(&chat.participants, &self.handles) else {
            return;
        };
        let chat_ref = chat_ref_of(chat);
        let chat_id = chat.id;
        let store = self.store.clone();
        let backend = self.backend.clone();
        let client = self.client.clone();
        let ui = self.clone();
        gtk_bridge::spawn(
            async move {
                match store.latest_unread_incoming(chat_id).await {
                    Ok(Some((guid, date))) => {
                        let _ = store.mark_read_through(chat_id, date).await;
                        Some(guid)
                    }
                    _ => None,
                }
            },
            move |guid| {
                if let Some(guid) = guid {
                    backend.send_receipt(&client, &chat_ref, &my_handle, true, guid);
                    // Something was just marked read; clear its sidebar badge.
                    ui.reload_chats(|_| {});
                }
            },
        );
    }

    /// Scroll the timeline to `to` after a rebuild, reliably. The content height
    /// settles over several allocation passes, and setting the adjustment during
    /// those passes gets overridden by GtkScrolledWindow. So instead we re-assert
    /// the target on the frame clock (post-layout) until the height stops changing
    /// — which fixes "opens a notch above the last message until you nudge it".
    pub(super) fn scroll_to(&self, to: ScrollTo) {
        // Suppress older-page loads from the rebuild's transient scroll resets.
        self.settling.set(true);
        let gen = self.settle_gen.get().wrapping_add(1);
        self.settle_gen.set(gen);
        {
            let settling = self.settling.clone();
            let settle_gen = self.settle_gen.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
                if settle_gen.get() == gen {
                    settling.set(false);
                }
            });
        }

        let scroller = self.scroller.clone();
        let container = self.msg_container.clone();
        let frames = Cell::new(0u32);
        let stable = Cell::new(0u32);
        let last_h = Cell::new(f64::NAN);
        self.scroller.add_tick_callback(move |_w, _clock| {
            let adj = scroller.vadjustment();
            let page = adj.page_size();
            // The tick runs in the frame's update phase, before layout, so on the
            // first tick after a rebuild adj.upper() is still the *old* chat's
            // height — targeting it flashes the previous scroll position. Measure
            // the container instead (recomputes for the new content immediately),
            // so the first painted frame already sits at the right place.
            //
            // Use the *minimum* height (`.0`), not the natural height (`.1`). A
            // GtkPicture's natural height is the image's intrinsic (unscaled)
            // size — thousands of pixels for a photo — while its minimum height
            // is the scaled size we set via `set_size_request`. The viewport's
            // default MINIMUM scroll policy sizes `upper` from that same
            // minimum, so measuring minimum here matches the value the viewport
            // will configure `upper` to a moment later. Using natural instead
            // would overshoot, inflate `upper` past the real content, and park
            // the viewport in empty space (the attach-a-file bug, same class).
            let width = container.width();
            let content_h = if width > 0 {
                container.measure(gtk::Orientation::Vertical, width).0 as f64
            } else {
                adj.upper()
            };
            let bottom = (content_h - page).max(0.0);
            let value = match &to {
                ScrollTo::Bottom => bottom,
                ScrollTo::Value(v) => v.min(bottom),
                ScrollTo::Widget(w) => w
                    .compute_bounds(&container)
                    .map(|b| (b.y() as f64 - 8.0).max(0.0))
                    .unwrap_or(bottom),
            };
            // Push the upper to the measured height first; otherwise set_value is
            // clamped against the stale (pre-layout) upper and lands short. Layout
            // will set the same upper a moment later, so this just wins the frame.
            if content_h > adj.upper() {
                adj.set_upper(content_h);
            }
            adj.set_value(value);

            // Stop once the height has been stable for a few frames (settled), or
            // after a hard cap so we never re-assert indefinitely.
            if (content_h - last_h.get()).abs() < 0.5 {
                stable.set(stable.get() + 1);
            } else {
                stable.set(0);
            }
            last_h.set(content_h);
            frames.set(frames.get() + 1);
            if stable.get() >= 4 || frames.get() >= 24 {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }
}
