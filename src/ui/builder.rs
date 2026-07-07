//! Generic / message widget builders extracted from [`super`](mod.rs).
//! Free functions that construct GTK widgets for the conversation timeline,
//! chat rows, text-scale label helpers, reaction chips, typing indicator,
//! and the CSS installer.

use super::*;
use super::media::{image_widget, video_widget, texture_from_bytes};
use super::link_preview::link_preview_card;

// ---------------------------------------------------------------
// ChipEntry — stored in the chip map
// ---------------------------------------------------------------

/// Maps target message guid → ChipEntry. Populated after every populate_messages
/// rebuild and after every append/prepend, used by `reload_messages` to apply
/// `UpdateChips` in place without rebuilding the view.
#[derive(Clone)]
pub(super) struct ChipEntry {
    /// The bubble widget, or — if the message currently has a chip — the
    /// `gtk::Overlay` wrapping the bubble. Used to find the bubble in the
    /// "add first chip" case (where the bubble is still a plain Box) and to
    /// find the overlay in the "update/remove chip" cases.
    pub(super) bubble: gtk::Widget,
    /// The chip widget, if the message currently has reactions. `None` means
    /// the message was rendered without a chip and we'd need to add one (the
    /// "add first chip" case).
    pub(super) chip: Option<gtk::Widget>,
}

// ---------------------------------------------------------------
// Reaction chips
// ---------------------------------------------------------------

/// A row of small reaction chips overlaid on a message bubble corner. Each chip
/// shows the emoji and, if count > 1, a count. Chips for reactions the current
/// user sent get a distinct visual class.
pub(super) fn reaction_chips_row(chips: &[LiveReactionSummary]) -> gtk::Widget {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .margin_top(0)
        .margin_bottom(0)
        .build();
    row.add_css_class("reaction-chips");
    populate_chips_row(&row, chips);
    row.upcast()
}

/// Clear `row` and re-populate it with one `gtk::Label` per chip.
/// Used for in-place updates when the chip widget already exists.
pub(super) fn populate_chips_row(row: &gtk::Box, chips: &[LiveReactionSummary]) {
    while let Some(child) = row.first_child() {
        row.remove(&child);
    }
    for chip in chips {
        let emoji = chips::code_to_emoji(2000 + chip.reaction_index as i64).unwrap_or("?");
        let text = if chip.count > 1 {
            format!("{} {}", emoji, chip.count)
        } else {
            emoji.to_string()
        };
        let label = gtk::Label::builder()
            .label(&text)
            .build();
        if chip.my_reacted {
            label.add_css_class("reaction-chip-self");
        } else {
            label.add_css_class("reaction-chip");
        }
        row.append(&label);
    }
}

// ---------------------------------------------------------------
// Small timeline helpers
// ---------------------------------------------------------------

pub(super) fn receipt_label(text: &str) -> gtk::Widget {
    let l = gtk::Label::builder()
        .label(text)
        .halign(gtk::Align::End)
        .margin_end(16)
        .margin_top(1)
        .margin_bottom(4)
        .build();
    l.add_css_class("dim-label");
    l.add_css_class("caption");
    apply_text_scale(&l, 10.0);
    l.upcast()
}

/// A centered "New messages" divider with hairlines on each side.
pub(super) fn unread_marker() -> gtk::Widget {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(14)
        .margin_end(14)
        .margin_top(10)
        .margin_bottom(2)
        .build();
    let left = gtk::Separator::new(gtk::Orientation::Horizontal);
    left.set_hexpand(true);
    left.set_valign(gtk::Align::Center);
    let lbl = gtk::Label::builder().label("New messages").build();
    lbl.add_css_class("unread-marker");
    apply_text_scale(&lbl, 11.0);
    let right = gtk::Separator::new(gtk::Orientation::Horizontal);
    right.set_hexpand(true);
    right.set_valign(gtk::Align::Center);
    row.append(&left);
    row.append(&lbl);
    row.append(&right);
    row.upcast()
}

/// A centered date-separator divider with hairlines on each side.
/// Used to separate groups of messages from different calendar dates.
pub(super) fn date_divider(text: &str) -> gtk::Widget {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(14)
        .margin_end(14)
        .margin_top(10)
        .margin_bottom(2)
        .build();
    row.add_css_class("date-divider");
    let left = gtk::Separator::new(gtk::Orientation::Horizontal);
    left.set_hexpand(true);
    left.set_valign(gtk::Align::Center);
    let lbl = gtk::Label::builder().label(text).build();
    lbl.add_css_class("dim-label");
    lbl.add_css_class("caption");
    apply_text_scale(&lbl, 11.0);
    let right = gtk::Separator::new(gtk::Orientation::Horizontal);
    right.set_hexpand(true);
    right.set_valign(gtk::Align::Center);
    row.append(&left);
    row.append(&lbl);
    row.append(&right);
    row.upcast()
}

// ---------------------------------------------------------------
// Text-scale / CSS helpers
// ---------------------------------------------------------------

/// A shared per-base-size CSS provider entry. Each base size (e.g. 10pt, 13pt)
/// gets exactly one provider and one stable class name so that
/// `refresh_chat_text_css` can rewrite the rule in place for all widgets at
/// once.
struct ChatTextProviderEntry {
    base_pt: f64,
    provider: gtk::CssProvider,
}

// Map from base-size key (base_pt × 100 rounded) to the shared provider.
// Lives in a `thread_local!` because `gtk::CssProvider` isn't `Send + Sync`.
thread_local! {
    static CHAT_TEXT_PROVIDERS: RefCell<HashMap<u64, ChatTextProviderEntry>> =
        RefCell::new(HashMap::new());
}

/// Convert a base point size to a stable integer key for the provider map.
fn base_key(base_pt: f64) -> u64 {
    (base_pt * 100.0).round() as u64
}

/// Build the CSS rule string for a given base point size using the current
/// `text_scale::get()` offset. Pure: no GTK, no I/O, no side effects. Used by
/// `apply_text_scale` and `refresh_chat_text_css` to keep the format in one
/// place.
pub(crate) fn chat_text_css(base_pt: f64) -> String {
    let offset = crate::text_scale::get();
    let key = base_key(base_pt);
    let class = format!("text-scale-{}", key);
    format!(".{} {{ font-size: {:.2}pt; }}", class, base_pt + offset)
}

/// Apply the current text size offset to a widget's font via a **shared**
/// per-base-size CSS provider. The offset is in points and added to the base
/// size. All widgets with the same base size share one provider and one CSS
/// class, so a single `refresh_chat_text_css()` call updates every widget on
/// the next paint without a rebuild.
pub(super) fn apply_text_scale(w: &impl IsA<gtk::Widget>, base_pt: f64) {
    use gtk::prelude::*;
    let key = base_key(base_pt);
    let class = format!("text-scale-{}", key);

    CHAT_TEXT_PROVIDERS.with(|p| {
        let mut map = p.borrow_mut();
        map.entry(key).or_insert_with(|| {
            let provider = gtk::CssProvider::new();
            let css = chat_text_css(base_pt);
            provider.load_from_string(&css);
            gtk::style_context_add_provider_for_display(
                &w.display(),
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            ChatTextProviderEntry {
                base_pt,
                provider,
            }
        });
    });

    w.add_css_class(&class);
}

/// Rewrite the CSS rule on every registered per-base-size chat-text provider
/// using the current `text_scale::get()` offset.  All widgets sharing those
/// providers pick up the new size on the next paint — no rebuild needed.
pub(super) fn refresh_chat_text_css() {
    // Collect cloned entries so we don't hold the RefCell borrow across
    // `load_from_string` (which may acquire an internal lock on the provider).
    let entries: Vec<(f64, gtk::CssProvider)> = CHAT_TEXT_PROVIDERS.with(|p| {
        p.borrow()
            .values()
            .map(|e| (e.base_pt, e.provider.clone()))
            .collect()
    });
    for (base_pt, provider) in entries {
        let css = chat_text_css(base_pt);
        provider.load_from_string(&css);
    }
}

/// CSS class assigned to the live chat-text-size preview bubble in the
/// preferences dialog. Stable across value changes so we can rewrite the
/// rule in place instead of stacking providers.
const PREVIEW_CLASS: &str = "text-scale-preview";

/// The one and only CSS provider that drives the live preview. Registered
/// for the display exactly once (the first time the preferences dialog is
/// opened); `refresh_preview_css` just rewrites the rule on the same
/// provider across subsequent opens. `gtk::CssProvider` isn't `Send + Sync`
/// so we can't stash it in a global `OnceLock`; instead we keep it on the
/// main thread via `Rc<RefCell<Option<_>>>` and initialize it on first use.
/// This guarantees we only ever register one provider for the preview
/// class, no matter how many times the dialog is opened.
fn preview_provider_cell() -> Rc<RefCell<Option<gtk::CssProvider>>> {
    thread_local! {
        static CELL: std::cell::OnceCell<Rc<RefCell<Option<gtk::CssProvider>>>> = const { std::cell::OnceCell::new() };
    }
    CELL.with(|c| {
        c.get_or_init(|| {
            let cell: Rc<RefCell<Option<gtk::CssProvider>>> = Rc::new(RefCell::new(None));
            let provider = gtk::CssProvider::new();
            if let Some(display) = gtk::gdk::Display::default() {
                gtk::style_context_add_provider_for_display(
                    &display,
                    &provider,
                    gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
                );
            }
            *cell.borrow_mut() = Some(provider);
            cell
        })
        .clone()
    })
}

/// Rewrite the rule on `PREVIEW_CLASS` to reflect the current offset, using
/// the same 13pt base a real incoming bubble's `body_text` uses. Cheap: it
/// just swaps the rule on the existing provider. The widget is never
/// rebuilt, so the change shows up on the very next paint.
pub(super) fn refresh_preview_css() {
    let offset = crate::text_scale::get();
    let css = format!(
        ".{} {{ font-size: {:.2}pt; }}",
        PREVIEW_CLASS,
        13.0 + offset
    );
    // Clone the Rc so the borrow on the RefCell ends before the load call:
    // we only need a short-lived reference to the provider.
    let provider = preview_provider_cell().borrow().clone();
    if let Some(p) = provider {
        p.load_from_string(&css);
    }
}

// ---------------------------------------------------------------
// Selectable label tracking (click-outside clear, cursor management)
// ---------------------------------------------------------------

// All selectable text labels currently in the message timeline. Held as
// weak refs so labels destroyed on a `populate_messages` rebuild are
// silently skipped. Drives the "click outside the textbox clears the
// highlight/cursor" behavior — the per-label `notify::cursor-position`
// hook clears the *others* when the user clicks into a new label, and the
// click handlers on the message container, chat list, and compose entry
// clear *all* of them when the user clicks anywhere else.
thread_local! {
    static SELECTABLE_LABELS: RefCell<Vec<glib::WeakRef<gtk::Label>>> =
        const { RefCell::new(Vec::new()) };
}

/// Deselect every registered label. Sets the selection bounds to a single
/// point at the current cursor (so any visible highlight disappears without
/// jumping the caret), and yanks focus off the label if it currently holds
/// it — that's what hides the blinking cursor. We only yank focus when the
/// label is the focused widget, so this never steals focus from the compose
/// entry the user is typing in.
pub(super) fn deselect_all_labels() {
    SELECTABLE_LABELS.with(|labels| {
        let mut labels = labels.borrow_mut();
        labels.retain(|weak| {
            if let Some(label) = weak.upgrade() {
                clear_label_selection_and_cursor(&label);
                true
            } else {
                false
            }
        });
    });
}

/// Deselect every registered label except `active`. Used when the user
/// clicks into a different label — the new one takes focus and the old one
/// must drop both its selection and its blinking cursor. Does not touch
/// focus, since the new active label needs it.
pub(super) fn deselect_all_labels_except(active: &gtk::Label) {
    SELECTABLE_LABELS.with(|labels| {
        let mut labels = labels.borrow_mut();
        labels.retain(|weak| {
            if let Some(label) = weak.upgrade() {
                if !std::ptr::eq(label.as_ptr(), active.as_ptr()) {
                    clear_label_selection_and_cursor(&label);
                }
                true
            } else {
                false
            }
        });
    });
}

/// Drop the highlight on `label` (if any) and hide its cursor. A label that
/// isn't focused has no visible cursor, so we skip the focus yank in that
/// case — yanking focus from an unfocused label would steal it from the
/// compose entry, which would be very rude while the user is typing.
fn clear_label_selection_and_cursor(label: &gtk::Label) {
    // Setting start == end at the current cursor position clears the
    // selection while leaving the caret where the user put it; we then
    // move focus off the label to hide the caret itself.
    if label.selection_bounds().is_some() {
        // Setting start == end collapses any highlight to a single point;
        // the caret is then hidden by the focus yank below. We don't bother
        // reading the current cursor position — the selection bounds (the
        // visible highlight) is the only thing the user actually sees, and
        // collapsing it to a point is enough to make it disappear.
        label.select_region(0, 0);
    }
    if label.has_focus() {
        if let Some(root) = label.root() {
            root.set_focus(None::<&gtk::Widget>);
        }
    }
}

/// Register `label` so the click-outside handlers can find and clear it.
/// Also wires up the per-label `notify::cursor-position` hook so that
/// clicking *into* this label (the cursor moves here) automatically clears
/// the previously-highlighted label.
pub(super) fn register_selectable_label(label: &gtk::Label) {
    let weak = label.downgrade();
    SELECTABLE_LABELS.with(|labels| {
        labels.borrow_mut().push(weak);
    });
    // When the cursor moves in this label — i.e. the user just clicked on
    // its text — the previously-focused label must give up its selection
    // and cursor. The "give up" call is in `clear_label_selection_and_cursor`,
    // which only yanks focus if the losing label was the one holding it.
    let label_weak = label.downgrade();
    label.connect_notify_local(Some("cursor-position"), move |_label, _pspec| {
        if let Some(active) = label_weak.upgrade() {
            deselect_all_labels_except(&active);
        }
    });
}

/// Drop focus from the compose `entry` and collapse any active text
/// selection inside it. Called when the user clicks somewhere that isn't
/// the entry — a message, the chat sidebar, the chat-view background — so
/// the blue focus outline disappears and they don't come back to a stale
/// highlight sitting in the draft they're about to overwrite. Yanking
/// focus to NULL is the only way to hide the focus outline; a click on a
/// non-focusable widget (like a Box or the scrolled viewport background)
/// wouldn't otherwise change focus, so the entry would keep its outline.
///
/// Note: `entry.has_focus()` is NOT a reliable gate here. GTK4's
/// `GtkEntry` delegates input focus to an internal `GtkText` child, so
/// `has_focus()` on the entry itself returns `false` even when the entry
/// is the visibly-focused widget. We always yank focus to NULL — it's
/// safe to do so (a no-op when nothing is focused) and avoids the
/// outline lingering after a background click.
pub(super) fn defocus_entry(entry: &gtk::Entry) {
    // Collapse any text selection to a single point at the current cursor.
    // The caret itself is hidden by the focus yank below.
    let pos: i32 = gtk::glib::object::ObjectExt::property(entry, "cursor-position");
    entry.select_region(pos, pos);
    if let Some(root) = entry.root() {
        root.set_focus(None::<&gtk::Widget>);
    }
}

// ---------------------------------------------------------------
// Preview bubble
// ---------------------------------------------------------------

/// Build a small incoming-style chat bubble holding a sample sentence. The
/// text uses [`PREVIEW_CLASS`] so its size is driven by the live CSS rule
/// that `refresh_preview_css` rewrites on every +/- click. Styled to match
/// the real `bubble-in` so the preview is a faithful "what my chats will
/// look like" sample rather than a generic text box.
pub(super) fn build_preview_bubble() -> gtk::Widget {
    let bubble = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .build();
    bubble.add_css_class("bubble");
    bubble.add_css_class("bubble-in");
    // Cap the bubble width so the preview stays compact even at large
    // text sizes — matches the cap on real bubbles so the comparison is
    // honest, not just a wide textarea.
    bubble.set_size_request(160, -1);
    let label = gtk::Label::builder()
        .label("The quick brown fox jumps over the lazy dog.")
        .wrap(true)
        .xalign(0.0)
        .max_width_chars(28)
        .build();
    label.add_css_class(PREVIEW_CLASS);
    bubble.append(&label);
    bubble.upcast()
}

// ---------------------------------------------------------------
// Widgets that depend on Ui (chat_row, message_widget, …)
// ---------------------------------------------------------------

/// A sidebar row: avatar + chat name + unread badge, with right-click context
/// menu and long-press selection gesture.
pub(super) fn chat_row(ui: &Ui, c: &ChatSummary) -> gtk::ListBoxRow {
    let title = chat_title(c, &ui.handles, &ui.contacts.borrow());
    let row = gtk::ListBoxRow::new();
    row.add_css_class("navigation-sidebar-row");
    if ui.chat_selection.borrow().is_selected(c.id) {
        row.add_css_class("chat-row-selected");
    }

    let box_ = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_start(8)
        .margin_end(16)
        .margin_top(8)
        .margin_bottom(8)
        .build();

    let avatar = avatar::AvatarWidget::new(36, &title);
    avatar.widget().set_hexpand(false);

    // Precedence: custom avatar photo (file on disk) → contact photo (EDS) → initials.
    if let Some(path) = chat_avatar_custom_path(c) {
        if let Some(texture) = load_texture(path) {
            avatar.set_custom_image(Some(&texture));
        }
    } else if let Some(bytes) = crate::contacts::chat_avatar_bytes(c, &ui.handles, &ui.contacts.borrow()) {
        if let Some(texture) = texture_from_bytes(bytes) {
            avatar.set_custom_image(Some(&texture));
        }
    }

    box_.append(avatar.widget());

    let title_label = gtk::Label::builder()
        .label(&title)
        .max_width_chars(24)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .single_line_mode(true)
        .build();
    title_label.set_hexpand(true);
    title_label.set_xalign(0.0);
    apply_text_scale(&title_label, 13.0);
    box_.append(&title_label);

    if c.unread > 0 {
        let badge = gtk::Label::new(Some(&c.unread.to_string()));
        badge.add_css_class("unread-badge");
        badge.set_hexpand(false);
        box_.append(&badge);
    }

    row.set_child(Some(&box_));

    // --- Right-click context menu ---
    let gesture_right = gtk::GestureClick::new();
    gesture_right.set_button(3);
    let ui_rc = ui.clone();
    let chat_id = c.id;
    let row_for_popover = row.clone();
    gesture_right.connect_released(move |_gesture, _n, _x, _y| {
        let popover = gtk::Popover::builder()
            .autohide(true)
            .build();

        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_start(4)
            .margin_end(4)
            .margin_top(4)
            .margin_bottom(4)
            .build();

        let selecting = ui_rc.chat_selection.borrow().is_selecting();

        if !selecting {
            // "Select" — enter selection mode and select this chat.
            let select_btn = gtk::Button::builder()
                .label("Select")
                .css_classes(["flat"])
                .build();
            let ui2 = ui_rc.clone();
            let popover2 = popover.clone();
            let row_for_sel = row_for_popover.clone();
            select_btn.connect_clicked(move |_| {
                ui2.chat_selection.borrow_mut().begin_select(chat_id);
                row_for_sel.add_css_class("chat-row-selected");
                popover2.popdown();
            });
            vbox.append(&select_btn);
        }

        // "Delete" (single when not selecting, all selected when selecting).
        let delete_label = if selecting {
            let count = ui_rc.chat_selection.borrow().selected_chat_ids().len();
            format!("Delete ({} selected)", count)
        } else {
            "Delete".to_string()
        };
        let delete_btn = gtk::Button::builder()
            .label(&delete_label)
            .css_classes(["flat"])
            .build();
        let ui2 = ui_rc.clone();
        let popover2 = popover.clone();
        delete_btn.connect_clicked(move |_| {
            if selecting {
                ui2.delete_selected_chats();
            } else {
                ui2.delete_single_chat(chat_id);
            }
            popover2.popdown();
        });
        vbox.append(&delete_btn);

        popover.set_child(Some(&vbox));
        popover.set_parent(&row_for_popover);

        // Unparent on close to avoid accumulating hidden children.
        popover.connect_closed(|p| p.unparent());

        popover.popup();
    });
    row.add_controller(gesture_right);

    // --- Long-press → enter selection mode and select this chat ---
    let long_press = gtk::GestureLongPress::builder()
        .touch_only(false)
        .build();
    let ui_lp = ui.clone();
    let lp_chat_id = c.id;
    let row_lp = row.clone();
    long_press.connect_pressed(move |_gesture, _x, _y| {
        ui_lp
            .chat_selection
            .borrow_mut()
            .begin_select_from_long_press(lp_chat_id);
        row_lp.add_css_class("chat-row-selected");
    });
    row.add_controller(long_press);

    row
}

// ---------------------------------------------------------------
// Message widget helpers
// ---------------------------------------------------------------

/// Per-message rendering context shared by [`message_widget`], [`incoming_message`],
/// and [`own_message`]. Bundles the message itself with the timeline-level
/// state (header flag, sticky-to-bottom offset, link-preview maps) so the
/// per-widget extras (`is_group`, `on_reaction`, `chip`) can stay as direct
/// arguments.
pub(super) struct MessageContext<'a> {
    pub(super) m: &'a StoredMessage,
    pub(super) show_header: bool,
    pub(super) top: i32,
    pub(super) previews: &'a std::collections::HashMap<(String, i64), MessageLinkPreview>,
    pub(super) preview_cards: &'a Rc<RefCell<std::collections::HashMap<(String, i64), gtk::Widget>>>,
    pub(super) handles: &'a [String],
    pub(super) contacts: &'a [Contact],
}

/// One message in the timeline. Incoming messages are grey bubbles on the left
/// (with an avatar, and a sender name in group chats); our own messages are blue
/// bubbles on the right. `previews` is the in-memory map loaded alongside the
/// messages; the renderer reads synchronously from it, so we never hit the
/// store on the GTK main thread. `preview_cards` is the live-widget registry
/// that `refresh_link_card` uses to swap a card in place without rebuilding.
/// Returns `(row_widget, bubble_or_overlay)`.
pub(super) fn message_widget(
    ctx: MessageContext<'_>,
    is_group: bool,
    on_reaction: Option<&Rc<ReactionHandler>>,
    on_edit: Option<&Rc<EditHandler>>,
    on_retry: Option<&Rc<RetryHandler>>,
    chip: Option<&gtk::Widget>,
) -> (gtk::Widget, Option<gtk::Widget>) {
    if ctx.m.is_from_me {
        own_message(ctx, on_edit, on_retry, chip)
    } else {
        incoming_message(ctx, is_group, on_reaction, chip)
    }
}

/// Left: grey bubble, with an avatar + sender name in group chats only.
/// On incoming messages, a right-click gesture opens a popover with the 6
/// standard tapback emoji buttons.
/// Returns `(row_widget, bubble_or_overlay)`.
fn incoming_message(
    ctx: MessageContext<'_>,
    is_group: bool,
    on_reaction: Option<&Rc<ReactionHandler>>,
    chip: Option<&gtk::Widget>,
) -> (gtk::Widget, Option<gtk::Widget>) {
    let MessageContext { m, show_header, top, previews, preview_cards, handles, contacts } = ctx;
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(14)
        .margin_end(56)
        .margin_top(top)
        .halign(gtk::Align::Start)
        .build();

    // Avatars (and their continuation spacer) only in group chats.
    if is_group {
        if show_header {
            let avatar = avatar::AvatarWidget::new(28, &sender_display(m, handles, contacts));
            avatar.widget().set_valign(gtk::Align::Start);
            // Contact photo if the sender is in the address book.
            if let Some(sender) = m.sender.as_deref() {
                if let Some(contact) = crate::contacts::find_contact(contacts, sender) {
                    if let Some(bytes) = &contact.avatar {
                        if !bytes.is_empty() {
                            if let Some(texture) = texture_from_bytes(bytes) {
                                avatar.set_custom_image(Some(&texture));
                            }
                        }
                    }
                }
            }
            row.append(avatar.widget());
        } else {
            let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            spacer.set_size_request(28, -1);
            row.append(&spacer);
        }
    }

    let col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .halign(gtk::Align::Start)
        .build();

    if is_group && show_header {
        let name = gtk::Label::builder()
            .label(sender_display(m, handles, contacts))
            .xalign(0.0)
            .build();
        name.add_css_class("sender-name");
        apply_text_scale(&name, 12.0);
        col.append(&name);
    }

    let line = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();

    // Build the reaction popover early (before the message body) so the text
    // label's extra menu can share the same popover via the show_picker callback.
    let show_picker: Option<Rc<dyn Fn()>> = on_reaction.map(|cb| {
        let popover = gtk::Popover::builder()
            .autohide(true)
            .build();

        let emoji_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .margin_start(4)
            .margin_end(4)
            .margin_top(4)
            .margin_bottom(4)
            .build();

        let target_guid = m.guid.clone();
        let target_text = extract_target_text(m);
        for (i, entry) in REACTIONS.iter().enumerate() {
            let btn = gtk::Button::builder()
                .label(entry.emoji)
                .css_classes(["flat", "circular"])
                .build();
            let cb = cb.clone();
            let guid = target_guid.clone();
            let text = target_text.clone();
            let popover = popover.clone();
            btn.connect_clicked(move |_| {
                cb(guid.clone(), i, text.clone());
                popover.popdown();
            });
            emoji_box.append(&btn);
        }

        popover.set_child(Some(&emoji_box));
        popover.set_parent(&row);

        // Right-click gesture on the row (fires on clicks outside the label).
        let gesture = gtk::GestureClick::new();
        gesture.set_button(3);
        let popover2 = popover.clone();
        gesture.connect_released(move |_gesture, _n, _x, _y| {
            popover2.popup();
        });
        row.add_controller(gesture);

        // Shared show-picker closure — called from both the row gesture and
        // the label's "Reaction" extra menu item.
        let picker: Rc<dyn Fn()> = Rc::new(move || popover.popup());
        picker
    });

    let (body_col, bubble_or_overlay) = message_body(
        m,
        false,
        previews,
        preview_cards,
        show_picker.as_ref(),
        None,
        None,
        chip,
    );
    line.append(&body_col);
    if show_header {
        line.append(&time_label(m));
    }
    col.append(&line);

    row.append(&col);

    (row.upcast(), bubble_or_overlay)
}

/// Right: blue bubble, time to its left on the first bubble of a group.
/// Returns `(row_widget, bubble_or_overlay)`.
fn own_message(
    ctx: MessageContext<'_>,
    on_edit: Option<&Rc<EditHandler>>,
    on_retry: Option<&Rc<RetryHandler>>,
    chip: Option<&gtk::Widget>,
) -> (gtk::Widget, Option<gtk::Widget>) {
    let MessageContext { m, show_header, top, previews, preview_cards, .. } = ctx;
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .margin_start(56)
        .margin_end(14)
        .margin_top(top)
        .halign(gtk::Align::End)
        .build();

    let line = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::End)
        .build();
    if show_header {
        line.append(&time_label(m));
    }

    // Error indicator for failed-send messages.
    if let Some(cat) = m.send_error {
        let icon = gtk::Image::from_icon_name("dialog-error-symbolic");
        icon.add_css_class("error");
        // gtk::Tooltip is the right widget for hover-revealed info: it shows
        // after a short delay, stays visible while the pointer is over the
        // icon, and dismisses when the pointer leaves. A custom popover on
        // an EventControllerMotion enter/leave flashes for a frame and
        // disappears (the popover appearing next to the icon moves the
        // pointer out of the icon's hit area, firing `leave` immediately).
        let tip = friendly_category_message(cat);
        icon.set_tooltip_text(Some(&tip));
        line.append(&icon);
    }

    // Build the optional Edit menu closure for text-only own messages.
    let is_text_only = m.attachments.is_empty()
        && m.text.as_deref().is_some_and(|t| !t.trim().is_empty());
    let show_edit: Option<Rc<dyn Fn()>> = if is_text_only {
        on_edit.map(|cb| -> Rc<dyn Fn()> {
            let cb = cb.clone();
            let target_guid = m.guid.clone();
            let current_text = extract_target_text(m);
            Rc::new(move || cb(target_guid.clone(), current_text.clone()))
        })
    } else {
        None
    };

    // Build the optional Retry menu closure for failed own text messages.
    // Attachment retry is omitted — wiring the stored attachment back
    // through send_attachment is deferred to a follow-up unit.
    let show_retry: Option<Rc<dyn Fn()>> = if m.send_error.is_some() && is_text_only {
        on_retry.map(|cb| -> Rc<dyn Fn()> {
            let cb = cb.clone();
            let target_guid = m.guid.clone();
            let target_text = m.text.clone().unwrap_or_default();
            Rc::new(move || cb(target_guid.clone(), target_text.clone()))
        })
    } else {
        None
    };

    let (body_col, bubble_or_overlay) = message_body(m, true, previews, preview_cards, None, show_edit.as_ref(), show_retry.as_ref(), chip);
    line.append(&body_col);

    row.append(&line);

    (row.upcast(), bubble_or_overlay)
}

/// The visual content of a message: image attachments stacked above an optional
/// text bubble, aligned to the sender's side. A sender-generated link preview
/// (iMessage rich link) is appended below the bubble when the renderer has one
/// in its in-memory map; the card is registered in `preview_cards` so
/// `refresh_link_card` can swap it in place on a placeholder→fillin.
///
/// Returns `(col_widget, bubble_or_overlay)` where the second element is `Some`
/// if a text bubble (with or without chip overlay) was created, `None` for
/// attachment-only messages with no text.
#[allow(clippy::too_many_arguments)]
fn message_body(
    m: &StoredMessage,
    own: bool,
    previews: &std::collections::HashMap<(String, i64), MessageLinkPreview>,
    preview_cards: &Rc<RefCell<std::collections::HashMap<(String, i64), gtk::Widget>>>,
    show_picker: Option<&Rc<dyn Fn()>>,
    show_edit: Option<&Rc<dyn Fn()>>,
    show_retry: Option<&Rc<dyn Fn()>>,
    chip: Option<&gtk::Widget>,
) -> (gtk::Widget, Option<gtk::Widget>) {
    let col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(3)
        .halign(if own {
            gtk::Align::End
        } else {
            gtk::Align::Start
        })
        .build();

    for att in &m.attachments {
        match att.kind() {
            AttachmentKind::Image => {
                if let Some(path) = att.local_path.as_deref() {
                    col.append(&image_widget(path, att.width.zip(att.height)));
                }
            }
            AttachmentKind::Video => {
                if let Some(path) = att.local_path.as_deref() {
                    col.append(&video_widget(path, att.width.zip(att.height)));
                }
            }
            AttachmentKind::Other => {
                col.append(&file_chip(att, own));
            }
        }
    }

    let has_text = m
        .text
        .as_deref()
        .is_some_and(|t| !strip_marker(t).is_empty());
    let is_tapback = m.associated_guid.is_some();
    let bubble_or_overlay: Option<gtk::Widget> = if has_text || is_tapback {
        let bubble = bubble_box(own);
        bubble.append(&bubble_label(&body_text(m), show_picker, show_edit, show_retry));
        let result = bubble_with_chip(&bubble, own, chip);
        col.append(&result);
        Some(result)
    } else if m.attachments.is_empty() {
        let bubble = bubble_box(own);
        bubble.append(&bubble_label("(no text)", show_picker, show_edit, show_retry));
        let result = bubble_with_chip(&bubble, own, chip);
        col.append(&result);
        Some(result)
    } else {
        None
    };

    // Sender-generated link preview (iMessage rich link). The store already
    // cached the thumbnail on disk; the renderer loads it from `image_path`
    // asynchronously to avoid a sync decode on the main thread. Register the
    // card in `preview_cards` so `refresh_link_card` can swap it in place on
    // a placeholder→fillin without rebuilding the timeline.
    if let Some(preview) = previews.get(&(m.guid.clone(), 0)) {
        let card = link_preview_card(preview);
        preview_cards
            .borrow_mut()
            .insert((m.guid.clone(), 0), card.clone());
        col.append(&card);
    }

    (col.upcast(), bubble_or_overlay)
}

// ---------------------------------------------------------------
// File chip
// ---------------------------------------------------------------

/// A bubble with a file icon + name, for non-image (or undecodable) attachments.
fn file_chip(att: &StoredAttachment, own: bool) -> gtk::Widget {
    let bubble = bubble_box(own);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.append(&gtk::Image::from_icon_name("text-x-generic-symbolic"));
    let name = att.name.clone().unwrap_or_else(|| "Attachment".to_string());
    row.append(&gtk::Label::new(Some(&name)));
    bubble.append(&row);
    bubble.upcast()
}

// ---------------------------------------------------------------
// Bubble + chip overlay
// ---------------------------------------------------------------

/// A rounded bubble container; `own` selects blue-on-white vs grey-on-dark.
/// Wrap the bubble in a `gtk::Fixed` so a reaction chip (if provided) can
/// straddle the bubble's top corner — half on, half off (iMessage look).
/// `GtkFixed` lets us position the chip at explicit coordinates; we compute
/// the corner-straddling position from the bubble's allocation via a
/// `connect_allocate` callback so the chip stays anchored as the bubble
/// resizes. GTK CSS can't do this (no `position: absolute`/`top`/`right`),
/// and `set_translate` isn't in the `gtk4 0.11` bindings, so `Fixed` is the
/// only path that works.
/// Wrap the bubble in a `GtkOverlay` so a reaction chip (if provided) can
/// be placed at the bubble's top corner. The overlay's main child is the
/// bubble itself — nothing wider — so the overlay sizes to the bubble. We
/// use `connect_get_child_position` to return a `gdk::Rectangle` for the
/// chip that places its *center* exactly at the bubble's top edge corner,
/// so the chip straddles the edge — half on, half off (the iMessage
/// tapback look). The rectangle is relative to the main child, and
/// negative coordinates are legal here (this is NOT a CSS margin, so it
/// doesn't trigger the negative-margin panic).
///
/// `connect_get_child_position` is a typed wrapper in `gtk4 0.11.3` that
/// uses `connect_raw` internally — it does NOT go through `connect_local`
/// by string name, so it doesn't panic with "Signal not found" the way
/// the earlier `size-allocate` attempts did. The handler re-fires on
/// every re-layout, so the position self-corrects if the first pass is
/// imperfect.
fn bubble_with_chip(bubble: &gtk::Box, own: bool, chip: Option<&gtk::Widget>) -> gtk::Widget {
    let Some(c) = chip else {
        return bubble.clone().upcast();
    };
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(bubble));
    overlay.set_hexpand(false);
    overlay.set_halign(if own {
        gtk::Align::End
    } else {
        gtk::Align::Start
    });
    c.set_valign(gtk::Align::Start);
    c.set_halign(if own {
        gtk::Align::Start
    } else {
        gtk::Align::End
    });
    overlay.add_overlay(c);
    overlay.set_measure_overlay(c, false);
    overlay.set_clip_overlay(c, false);

    // Position the chip so its center is at the bubble's top edge corner.
    // - incoming (own=false): top-RIGHT corner → x = bubble_w - chip_w/2
    // - sent     (own=true):  top-LEFT  corner → x = -chip_w/2
    // y is always -chip_h/2 (above the bubble's top edge).
    let own_side = own;
    // The closure must be `'static`, so clone the bubble (ref-counted by
    // GTK internally — cheap) to move it into the closure.
    let bubble_for_closure = bubble.clone();
    overlay.connect_get_child_position(move |_overlay, child| {
        let (_, chip_w, _, _) = child.measure(gtk::Orientation::Horizontal, -1);
        let (_, chip_h, _, _) = child.measure(gtk::Orientation::Vertical, -1);
        // Prefer the bubble's allocated width (which is the actual rendered
        // width after CSS max-width/wrapping constraints) for accurate chip
        // positioning on wide bubbles. Fall back to natural width on the
        // first layout pass before allocation is known.
        let bubble_w = {
            let a = bubble_for_closure.width();
            if a > 0 {
                a
            } else {
                let (_, natural, _, _) =
                    bubble_for_closure.measure(gtk::Orientation::Horizontal, -1);
                natural
            }
        };
        let y = -(chip_h / 2);
        let x = if own_side {
            -(chip_w / 2)
        } else {
            bubble_w - chip_w / 2
        };
        Some(gtk::gdk::Rectangle::new(x, y, chip_w, chip_h))
    });

    overlay.upcast()
}

/// Wrap `bubble` in a `gtk::Overlay` with `chip` as an overlay child positioned
/// at the bubble's top corner (top-right for incoming, top-left for sent).
/// This is the same logic as `bubble_with_chip` but takes a generic `gtk::Widget`
/// for the bubble (not just `gtk::Box`) so it can work on the in-place update path.
#[allow(deprecated, clippy::unnecessary_cast)]
pub(super) fn wrap_bubble_in_overlay(bubble: &gtk::Widget, chip: &gtk::Widget, own: bool) -> gtk::Widget {
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(bubble));
    overlay.set_hexpand(false);
    overlay.set_halign(if own {
        gtk::Align::End
    } else {
        gtk::Align::Start
    });
    chip.set_valign(gtk::Align::Start);
    chip.set_halign(if own {
        gtk::Align::Start
    } else {
        gtk::Align::End
    });
    overlay.add_overlay(chip);
    overlay.set_measure_overlay(chip, false);
    overlay.set_clip_overlay(chip, false);

    // Position the chip so its center is at the bubble's top edge corner.
    let own_side = own;
    let bubble_for_closure = bubble.clone();
    overlay.connect_get_child_position(move |_overlay, child| {
        let (_, chip_w, _, _) = child.measure(gtk::Orientation::Horizontal, -1);
        let (_, chip_h, _, _) = child.measure(gtk::Orientation::Vertical, -1);
        let bubble_w = {
            let a = bubble_for_closure.width();
            if a > 0 {
                a as i32
            } else {
                let (_, natural, _, _) =
                    bubble_for_closure.measure(gtk::Orientation::Horizontal, -1);
                natural
            }
        };
        let y = -(chip_h / 2);
        let x = if own_side {
            -(chip_w / 2)
        } else {
            bubble_w - chip_w / 2
        };
        Some(gtk::gdk::Rectangle::new(x, y, chip_w, chip_h))
    });

    overlay.upcast()
}

/// Apply a single `ChipChange` in place. The `bubble_or_overlay` is the
/// widget currently in the container for `target_guid` (either the bare
/// bubble Box, or the overlay wrapping the bubble if a chip already exists).
/// `current_chips` is updated to reflect the new state.
pub(super) fn apply_chip_change(
    target_guid: &str,
    new_chips: &[LiveReactionSummary],
    bubble_or_overlay: &gtk::Widget,
    is_from_me: bool,
    current_chips: &Rc<RefCell<std::collections::HashMap<String, ChipEntry>>>,
) {
    let mut chips = current_chips.borrow_mut();

    use std::collections::hash_map::Entry;
    match chips.entry(target_guid.to_string()) {
        Entry::Occupied(mut o) => {
            let has_chip = o.get().chip.is_some();
            let chips_empty = new_chips.is_empty();

            match (has_chip, chips_empty) {
                // "Add first chip" — no chip yet, now has reactions.
                (false, false) => {
                    // The bubble still has its old parent (the message's `col` Box).
                    // GTK4's `gtk_overlay_set_child` asserts when the new child
                    // has a parent that isn't the overlay itself, so we have to
                    // unparent the bubble BEFORE wrapping it. Capture the position
                    // first so the overlay lands in the same spot.
                    let parent_box = bubble_or_overlay
                        .parent()
                        .and_then(|p| p.downcast_ref::<gtk::Box>().cloned())
                        .expect("bubble must have a Box parent for in-place chip add");
                    let prev_sibling = bubble_or_overlay.prev_sibling();
                    parent_box.remove(bubble_or_overlay);
                    let chip = reaction_chips_row(new_chips);
                    let overlay = wrap_bubble_in_overlay(bubble_or_overlay, &chip, is_from_me);
                    match prev_sibling {
                        Some(ref sibling) => {
                            parent_box.insert_child_after(&overlay, Some(sibling))
                        }
                        None => parent_box.prepend(&overlay),
                    }
                    o.insert(ChipEntry {
                        bubble: overlay,
                        chip: Some(chip),
                    });
                }
                // "Update existing chip" — had chip, reactions changed.
                (true, false) => {
                    if let Some(chip_widget) = &o.get().chip {
                        if let Some(box_) = chip_widget.downcast_ref::<gtk::Box>() {
                            populate_chips_row(box_, new_chips);
                        }
                    }
                }
                // "Remove last chip" — had chip, no more reactions.
                (true, true) => {
                    if let Some(chip_widget) = &o.get().chip {
                        if let Some(box_) = chip_widget.downcast_ref::<gtk::Box>() {
                            populate_chips_row(box_, &[]); // clear
                        }
                    }
                }
                // "Noop / state mismatch" — no chip, no chips to show (shouldn't occur)
                (false, true) => {
                    // Nothing to do.
                }
            }
        }
        Entry::Vacant(_) => {
            // Message not found in chip map. This shouldn't happen if the chip map
            // is kept in sync. Log and skip.
            eprintln!("apply_chip_change: no chip entry for {target_guid}, skipping");
        }
    }
}

// ---------------------------------------------------------------
// Bubble + label
// ---------------------------------------------------------------

/// A vertical bubble container for the text label. The reaction chip (if any)
/// is layered on top via a `GtkOverlay` in `message_body` — the overlay wraps
/// only the bubble, so the chip is positioned relative to the bubble's bounds
/// (not the whole message row).
pub(super) fn bubble_box(own: bool) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
    b.add_css_class("bubble");
    b.add_css_class(if own { "bubble-out" } else { "bubble-in" });
    b
}

/// The wrapped, width-capped, left-justified text inside a bubble.
/// URLs in the text are rendered as clickable links that open in the system browser.
fn bubble_label(
    text: &str,
    show_picker: Option<&Rc<dyn Fn()>>,
    show_edit: Option<&Rc<dyn Fn()>>,
    show_retry: Option<&Rc<dyn Fn()>>,
) -> gtk::Label {
    let markup = text_to_markup(text);
    let label = gtk::Label::builder()
        .label(&markup)
        .use_markup(true)
        .wrap(true)
        .xalign(0.0)
        .selectable(true)
        .max_width_chars(40)
        .build();
    apply_text_scale(&label, 13.0);
    label.connect_activate_link(|_, uri| {
        open_uri(uri);
        glib::Propagation::Stop // prevent default handler
    });
    // Register for click-outside clearing and wire up the cursor-moved hook
    // so clicking into this label drops the previous one's highlight + cursor.
    register_selectable_label(&label);

    // Append "Reaction", "Edit", and/or "Retry" items to the label's built-in
    // context menu (Copy / Select All / …) when the corresponding callbacks
    // are wired.
    let menu = gtk::gio::Menu::new();
    let mut has_any = false;

    if let Some(picker) = show_picker {
        let action_group = gtk::gio::SimpleActionGroup::new();
        let open_action = gtk::gio::SimpleAction::new("open", None);
        let picker = Rc::clone(picker);
        open_action.connect_activate(move |_, _| picker());
        action_group.add_action(&open_action);
        label.insert_action_group("reaction", Some(&action_group));
        menu.append(Some("Reaction"), Some("reaction.open"));
        has_any = true;
    }

    if let Some(edit) = show_edit {
        let action_group = gtk::gio::SimpleActionGroup::new();
        let trigger_action = gtk::gio::SimpleAction::new("trigger", None);
        let edit = Rc::clone(edit);
        trigger_action.connect_activate(move |_, _| edit());
        action_group.add_action(&trigger_action);
        label.insert_action_group("edit", Some(&action_group));
        menu.append(Some("Edit"), Some("edit.trigger"));
        has_any = true;
    }

    if let Some(retry) = show_retry {
        let action_group = gtk::gio::SimpleActionGroup::new();
        let trigger_action = gtk::gio::SimpleAction::new("trigger", None);
        let retry = Rc::clone(retry);
        trigger_action.connect_activate(move |_, _| retry());
        action_group.add_action(&trigger_action);
        label.insert_action_group("retry", Some(&action_group));
        menu.append(Some("Retry"), Some("retry.trigger"));
        has_any = true;
    }

    if has_any {
        label.set_extra_menu(Some(&menu));
    }

    label
}

/// A small dim timestamp aligned to the bottom of a bubble.
/// Hover reveals the full local date + time (respecting 12h/24h setting).
pub(super) fn time_label(m: &StoredMessage) -> gtk::Label {
    let l = gtk::Label::builder().label(fmt_time(m.date)).build();
    l.add_css_class("dim-label");
    l.add_css_class("caption");
    l.set_valign(gtk::Align::End);
    l.set_tooltip_text(Some(&crate::time_format::format_full_timestamp(
        m.date,
        crate::time_format::get(),
    )));
    apply_text_scale(&l, 10.0);
    l
}

// ---------------------------------------------------------------
// Scaffolding helpers
// ---------------------------------------------------------------

/// A toolbar-view page: header with `title`, `body` as content, optional bottom
/// bar, and optional widgets packed at the start and end of the header.
pub(super) fn page(
    title: &str,
    body: &impl IsA<gtk::Widget>,
    bottom: Option<&gtk::Widget>,
    header_start: Option<&gtk::Widget>,
    header_end: Option<&gtk::Widget>,
) -> adw::NavigationPage {
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    if let Some(w) = header_start {
        header.pack_start(w);
    }
    if let Some(w) = header_end {
        header.pack_end(w);
    }
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(body));
    if let Some(b) = bottom {
        toolbar.add_bottom_bar(b);
    }
    adw::NavigationPage::builder()
        .title(title)
        .child(&toolbar)
        .build()
}

pub(super) fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(child)
        .build()
}

// ---------------------------------------------------------------
// CSS / typing indicator
// ---------------------------------------------------------------

pub(super) fn install_css() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            // Bundled symbolic icons (e.g. the send arrow). Baked-in absolute
            // path so it resolves regardless of the working directory in dev.
            let theme = gtk::IconTheme::for_display(&display);
            theme.add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icons"));
        }
    });
}

/// A timeline row holding the typing bubble, inset to match an incoming message
/// (same left margin, and the 28px avatar-column spacer in group chats).
pub(super) fn typing_row(is_group: bool) -> gtk::Widget {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(14)
        .margin_end(56)
        .margin_top(8)
        .margin_bottom(2)
        .halign(gtk::Align::Start)
        .build();
    if is_group {
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_size_request(28, -1);
        row.append(&spacer);
    }
    row.append(&typing_bubble());
    row.upcast()
}

/// The grey "three animated dots" bubble shown while the other party types. The
/// pulse is driven by CSS keyframes on the `.typing-dot` class.
fn typing_bubble() -> gtk::Widget {
    let bubble = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Center)
        .build();
    bubble.add_css_class("bubble");
    bubble.add_css_class("bubble-in");
    bubble.add_css_class("typing-bubble");
    for i in 0..3 {
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.set_size_request(8, 8);
        dot.add_css_class("typing-dot");
        match i {
            1 => dot.add_css_class("typing-dot-2"),
            2 => dot.add_css_class("typing-dot-3"),
            _ => {}
        }
        dot.set_valign(gtk::Align::Center);
        bubble.append(&dot);
    }
    bubble.upcast()
}
