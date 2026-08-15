//! Conversations UI, styled after Fractal: an `AdwNavigationSplitView` with an
//! avatar-led sidebar (unread badges) on the left and, on the right, a flat
//! sender-grouped message timeline plus a compose bar.
//!
//! Presentation only — the store, receive loop, and send paths are untouched.
//! Everything here reads from [`crate::store::Store`] and refreshes when the
//! backend pulses the notifier.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::path::PathBuf;
use std::sync::{Arc, Once};
use tokio::sync::oneshot;

use adw::prelude::*;

use crate::gtk_bridge;
use crate::protocol::{Backend, Connection, ImClient, RecvEvent, friendly_category_message};
use crate::store::{
    group_tapbacks_by_target, live_tapbacks, AttachmentKind, AttachmentRecord,
    ChatSummary, Contact, IncomingMessage, Ingest, LiveReactionSummary, MessageLinkPreview,
    NewMessage, Store, StoredAttachment, StoredMessage,
};
#[cfg(feature = "rustpush")]
use crate::store::Tapback;
#[cfg(feature = "rustpush")]
use rustpush::{Reaction, ReactMessageType};

mod avatar;
mod pending;
mod plan;
mod chips;
mod builder;
mod media;
mod chat_list;
mod link_preview;
mod messaging;
mod send;
mod notifications;
mod helpers;
mod text_format;
mod avatar_edit;
mod keychain;

#[allow(unused_imports)]
pub use plan::{ChatUpdatePlan, ChipChange, ReceiptAction, plan_chat_update};
use plan::{extract_target_text, receipt_status};
#[allow(unused_imports)]
use chips::{code_to_emoji, REACTIONS};

pub(crate) use chat_list::ChatSelectionState;

// Re-export extracted free-function helpers so existing call sites in this
// module (and within the crate) resolve without prefix changes.
use helpers::*;
use text_format::*;
pub use avatar_edit::{AvatarEdit, apply_chat_edit};
#[allow(unused_imports)]
pub use keychain::{
    build_bottle_aware_prompt_closure, build_clique_password_dialog,
    build_password_prompt_closure, describe_escrow_metadata_for_user,
};
use builder::*;
use media::*;
use link_preview::*;

/// Debounce duration for coalescing desktop notifications per chat.
const NOTIFICATION_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(1500);
/// Max age before a pending notification fires unconditionally.
const NOTIFICATION_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(5);

/// Callback type for the reaction emoji picker: receives the target message
/// GUID, the reaction index (0-5), and the target message's text (for the
/// wire-level `ams` field).
type ReactionHandler = dyn Fn(String, usize, String);

/// Callback type for the "Edit" menu option on own messages: receives the
/// target message GUID and the current text. The handler is responsible for
/// opening the editor (Unit 6 wires this up).
type EditHandler = dyn Fn(String, String);

/// Callback type for the "Retry" menu option on failed own messages: receives
/// the target message GUID and the text to resend.
type RetryHandler = dyn Fn(String, String);

/// Callback type for the editor's Save button: receives the target message
/// GUID and the new text. Unit 6 leaves this as a no-op; Unit 7 wires the
/// real send.
type EditSaveHandler = dyn Fn(String, String);

/// Default: send read receipts when a chat is viewed.
pub(super) const SEND_READ_RECEIPTS: bool = true;

/// New sender header after this idle gap (5 min), even for the same person.
pub(super) const GROUP_GAP_MS: i64 = 5 * 60 * 1000;
/// How many messages to load per page (initial open and each scroll-up).
pub(super) const PAGE_SIZE: i64 = 20;
/// How long the "New Messages" divider lingers after a chat is opened and read
/// before it dismisses itself.
pub(super) const UNREAD_DIVIDER_TTL_SECS: u64 = 4;

const CSS: &str = "
.unread-badge {
  background-color: @accent_bg_color;
  color: @accent_fg_color;
  border-radius: 999px;
  padding: 0px 7px;
  margin: 4px 2px;
  font-weight: bold;
  font-size: 0.85em;
}
.sender-name {
  font-weight: bold;
  font-size: 0.9em;
  color: @accent_color;
  margin-left: 2px;
}
.unread-marker {
  color: @accent_color;
  font-size: 0.8em;
  font-weight: bold;
}
.bubble {
  border-radius: 18px;
  padding: 6px 12px;
}
.bubble-in {
  background-color: #e7e7ea;
  color: #161616;
}
.bubble-out {
  background-color: #1b7ffb;
  color: #ffffff;
}
.attachment-image {
  border-radius: 14px;
  background-color: #00000010;
}
.live-photo-button {
  min-width: 32px;
  min-height: 32px;
  padding: 4px;
  border-radius: 999px;
  background-color: alpha(#000000, 0.72);
  color: #ffffff;
}
.live-photo-button:hover {
  background-color: alpha(#000000, 0.86);
}
.lightbox-dim {
  background-color: rgba(0, 0, 0, 0.8);
}
/* Empty-state illustration: double the default AdwStatusPage icon size
   (128px) so the artwork reads as a proper hero graphic, not an icon. */
statuspage.empty-hero > scrolledwindow > viewport > box > clamp > box > .icon {
  -gtk-icon-size: 256px;
}
/* Hide the built-in pencil/edit icon on AdwEntryRow — the rows are clearly
   editable, and the icon is a hard-coded widget in the template with no
   Rust API to disable. `-gtk-icon-source: none` doesn't work because the
   icon is set via GtkImage's `icon-name` property, so we use opacity. */
row.entry image.edit-icon {
  opacity: 0;
}
.unread-pill {
  padding: 4px 14px;
  font-size: 0.9em;
}
.typing-dot {
  min-width: 7px;
  min-height: 7px;
  border-radius: 99px;
  background-color: #7c7c80;
  animation: typing-pulse 1.3s infinite ease-in-out;
}
.typing-dot-2 {
  animation-delay: 0.18s;
}
.typing-dot-3 {
  animation-delay: 0.36s;
}
@keyframes typing-pulse {
  0%, 65%, 100% {
    opacity: 0.3;
  }
  32% {
    opacity: 0.95;
  }
}
@keyframes bubble-appear {
  from {
    opacity: 0;
  }
  to {
    opacity: 1;
  }
}
.bubble-appear {
  animation: bubble-appear 0.2s ease-out;
}

/* Reaction chips on message bubbles. Both reaction types use the same
   light grey pill — visible against both the grey incoming bubble and the
   blue sent bubble, and gives every emoji (including the red ‼) a
   neutral background to read clearly against. */
.reaction-chip,
.reaction-chip-self {
  font-size: 0.9em;
  padding: 3px 8px;
  border-radius: 12px;
  background-color: #f0f0f3;
  color: #161616;
}

/* iMessage rich link (sender-generated preview) card. */
.link-preview {
  padding: 8px;
  border-radius: 12px;
  border: 1px solid alpha(currentColor, 0.08);
  background-color: alpha(currentColor, 0.03);
  min-width: 220px;
}
.link-preview:hover {
  background-color: alpha(currentColor, 0.06);
}
.link-preview-thumb {
  border-radius: 8px;
  min-width: 72px;
  min-height: 72px;
  background-color: alpha(currentColor, 0.08);
}
.link-preview-title {
  font-weight: 600;
}
.link-preview-desc {
  color: alpha(currentColor, 0.65);
}
.link-preview-host {
  color: alpha(currentColor, 0.55);
  font-size: 0.85em;
}
.link-preview-placeholder {
  color: alpha(currentColor, 0.55);
  font-style: italic;
}
.link-preview-thumb-fallback {
  border-radius: 8px;
  min-width: 72px;
  min-height: 72px;
  background-color: alpha(currentColor, 0.08);
  color: alpha(currentColor, 0.5);
}
.crop-indicator {
  border-radius: 999px;
  border: 2px solid @accent_bg_color;
  background-color: rgba(255, 255, 255, 0.25);
}
.crop-viewport {
  border: 1px solid alpha(currentColor, 0.4);
}
/* Chat-list row in selection mode when its chat is selected. The accent
   colour is visually distinct against both light and dark themes. */
.chat-row-selected {
  background-color: alpha(@accent_bg_color, 0.25);
  border-left: 4px solid @accent_bg_color;
}
";


/// Cheap-to-clone bundle the UI closures share.
#[derive(Clone)]
struct Ui {
    store: Store,
    backend: Arc<dyn Backend>,
    split: adw::NavigationSplitView,
    content_page: adw::NavigationPage,
    rename_button: gtk::Button,
    chat_list: gtk::ListBox,
    chats: Rc<RefCell<Vec<ChatSummary>>>,
    msg_container: gtk::Box,
    scroller: gtk::ScrolledWindow,
    client: ImClient,
    connection: Connection,
    handles: Vec<String>,
    contacts: Rc<RefCell<Vec<Contact>>>,
    open_summary: Rc<RefCell<Option<ChatSummary>>>,
    // Pagination state for the open chat.
    page_oldest: Rc<RefCell<Option<(i64, i64)>>>,
    page_has_more: Rc<RefCell<bool>>,
    page_loading: Rc<RefCell<bool>>,
    // First-unread anchor for the open chat: (guid, date). The divider is placed
    // before this message; while it isn't loaded, the floating pill is shown.
    unread: Rc<RefCell<Option<(String, i64)>>>,
    unread_marker_shown: Rc<RefCell<bool>>,
    // Handle to the drawn divider widget (so it can be removed in place) and a
    // generation guard for its self-dismiss timer.
    unread_marker: Rc<RefCell<Option<gtk::Widget>>>,
    unread_dismiss_gen: Rc<Cell<u64>>,
    unread_pill: gtk::Button,
    // Compose entry, and outbound-typing bookkeeping: whether we currently have
    // a typing=true outstanding, and a generation guard for the idle-stop timer.
    // `entry` is retained on the Ui for completeness but read-back happens via
    // the per-handler clones captured at build time, so the field itself is
    // unread — kept rather than dropped to avoid churning the struct layout.
    #[allow(dead_code)]
    entry: gtk::Entry,
    typing_sent: Rc<Cell<bool>>,
    typing_idle_gen: Rc<Cell<u64>>,
    // Inbound typing indicator lives as the trailing item in the timeline. We
    // track whether it's active (so it can be re-added after a rebuild clears the
    // container) and hold a handle to the live row (so it can be removed without
    // a rebuild). `typing_gen` guards the auto-expire timer.
    typing_active: Rc<Cell<bool>>,
    typing_row: Rc<RefCell<Option<gtk::Widget>>>,
    typing_gen: Rc<Cell<u64>>,
    // Set when a message supersedes the typing indicator, so the next rebuild
    // fades the new bubble in (in place of the dots) instead of popping it.
    morph_pending: Rc<Cell<bool>>,
    // Whether the window currently has focus. Messages that arrive while it
    // doesn't are held as unread until the user comes back.
    focused: Rc<Cell<bool>>,
    // While a rebuild's layout settles, transient scroll resets must not toggle
    // the bottom-follow. `settling` suppresses that; `settle_gen` lets the latest
    // scroll request own the clear so overlapping rebuilds don't end it early.
    settling: Rc<Cell<bool>>,
    settle_gen: Rc<Cell<u64>>,
    // Coalesces the receive-loop's per-message refresh pulses so a burst (e.g.
    // the backlog drained on startup) collapses into a single sidebar rebuild
    // instead of flickering through dozens.
    refresh_gen: Rc<Cell<u64>>,
    // Desktop notifications: highest message date already considered, and the
    // toplevel window (for raising it when a notification is clicked).
    notify_watermark: Rc<Cell<i64>>,
    window: Rc<RefCell<Option<gtk::Window>>>,
    // Chats we currently have a desktop notification posted for, so we can
    // withdraw it once the chat is read — including reads synced from another
    // device, which clear unread without us opening the chat here.
    notified_chats: Rc<RefCell<HashSet<i64>>>,
    // Debounce registry for pending notifications.
    pending_notifications: Rc<RefCell<pending::PendingNotifications>>,
    // Cleared once on the first chat load, to sweep stale notifications left in
    // the center by a previous session (read elsewhere while we were closed).
    notify_swept: Rc<Cell<bool>>,
    // Live link preview cards currently shown in the open chat, keyed by
    // `(guid, part_idx)`. Lets the in-place `refresh_link_card` find the card
    // and replace it on a placeholder→fillin without rebuilding the whole
    // timeline. Cleared on every `populate_messages` rebuild.
    preview_cards: Rc<RefCell<std::collections::HashMap<(String, i64), gtk::Widget>>>,
    // File the user has picked but not yet sent. While `Some`, the compose
    // area shows a chip with the file name + a remove button. Either path —
    // typing a caption and pressing send, or pressing send with an empty
    // entry — clears this and dispatches backend.send_attachment with the
    // entry text (or None when the entry is empty).
    pending_attachment: Rc<RefCell<Option<PendingAttachment>>>,
    pending_chip: gtk::Box,
    pending_chip_label: gtk::Label,
    pending_chip_icon: gtk::Image,
    compose_outer: gtk::Box,
    /// Swaps the content pane between the empty-state illustration (no chat
    /// open) and the timeline + compose view.
    content_stack: gtk::Stack,
    /// Guids of non-tapback messages currently rendered as bubbles, in order.
    /// Used by `plan_chat_update` to decide between Noop / Append / UpdateReceipt /
    /// Rebuild. Updated after every populate_messages call and after every in-place
    /// update path.
    rendered_guids: Rc<RefCell<Vec<String>>>,
    /// Text currently shown in the receipt label, or `None` if no label is shown.
    /// The placeholder ("\u{200b}") counts as `Some("\u{200b}")`.
    current_receipt_text: Rc<RefCell<Option<String>>>,
    /// Handle to the receipt label widget currently in msg_container, or `None` if
    /// no label is shown. Used for in-place text updates and for removal.
    receipt_label: Rc<RefCell<Option<gtk::Label>>>,
    /// Maps target message guid → ChipEntry. Populated after every populate_messages
    /// rebuild and after every append/prepend, used by `reload_messages` to apply
    /// `UpdateChips` in place without rebuilding the view.
    current_chips: Rc<RefCell<std::collections::HashMap<String, ChipEntry>>>,
    /// Snapshot of the `LiveReactionSummary` maps currently rendered. Used to
    /// compute chip changes (prev_reactions) in `reload_messages`.
    current_reactions: Rc<RefCell<std::collections::BTreeMap<String, Vec<LiveReactionSummary>>>>,
    /// Text currently rendered in each bubble, keyed by guid. Used by
    /// `plan_chat_update` to detect text changes (edits) and pick `EditText`
    /// instead of `Noop` or `Rebuild`. Updated after every populate_messages
    /// rebuild and after every in-place text update.
    current_text: Rc<RefCell<std::collections::HashMap<String, String>>>,
    /// Banner shown at the top of the content pane reflecting the iMessage
    /// identity registration state (re-registering, transient failure, logged
    /// out by Apple).
    reg_banner: adw::Banner,
    /// Guards repeated LoggedOut desktop notifications: set to true once a
    /// notification has been sent for the current LoggedOut episode, reset
    /// back to false when Registered arrives.
    reg_notified: Rc<Cell<bool>>,
    /// Selection mode state for the chat list. When `is_selecting()`, left-click
    /// on a row toggles selection instead of opening the chat. Right-click shows
    /// a context menu with Select/Delete actions.
    chat_selection: Rc<RefCell<ChatSelectionState>>,
}

/// Swap the window over to the messaging UI and start receiving. Called once a
/// live session exists (restored or freshly registered).
pub fn enter_messaging(
    nav: &adw::NavigationView,
    backend: &Arc<dyn Backend>,
    store: Store,
    connection: Connection,
    client: ImClient,
    handles: Vec<String>,
    monitor: &Arc<crate::power::PowerMonitor>,
) {
    install_css();

    // --- sidebar (chat list) ---
    let chat_list = gtk::ListBox::new();
    chat_list.add_css_class("navigation-sidebar");
    chat_list.set_selection_mode(gtk::SelectionMode::Single);
    chat_list.set_activate_on_single_click(true);

    // Compose entry is hoisted out of the `compose` box so the chat-list and
    // message-container click handlers can capture it and clear its own text
    // selection when the user clicks elsewhere.
    let entry = gtk::Entry::builder()
        .hexpand(true)
        .placeholder_text("Message")
        .build();
    // GTK's built-in emoji picker: a dim emoji glyph inside the entry (right
    // side) that opens the chooser and inserts into the text — functional.
    entry.set_show_emoji_icon(true);
    // Gaining focus on the compose box is the reliable signal that the user
    // just clicked into it (the entry's own GestureClick swallows the event
    // for cursor placement, so a Bubble-phase gesture never sees it). Drop
    // any in-progress text selection/cursor in the open message at that point.
    let entry_focus = gtk::EventControllerFocus::new();
    entry_focus.connect_enter(move |_ctrl| deselect_all_labels());
    entry.add_controller(entry_focus);

    // Clicking a chat row must drop any in-progress text selection/cursor in
    // the open message, otherwise the highlight lingers while the user is
    // jumping between chats. Also clear any text selection in the compose
    // entry itself.
    let entry_for_chat_list = entry.clone();
    chat_list.connect_row_activated(move |_, _row| {
        deselect_all_labels();
        defocus_entry(&entry_for_chat_list);
    });
    // Hamburger menu at the end of the sidebar header.
    let main_menu = gtk::gio::Menu::new();
    main_menu.append(Some("Preferences"), Some("menu.preferences"));
    main_menu.append(Some("About"), Some("menu.about"));
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main Menu")
        .menu_model(&main_menu)
        .build();
    // Plus button for new chat at the start of the sidebar header.
    let plus_button = gtk::Button::from_icon_name("list-add-symbolic");
    plus_button.add_css_class("flat");
    plus_button.set_tooltip_text(Some("New Chat"));
    let sidebar = page(
        "Messages",
        &scrolled(&chat_list),
        None,
        Some(plus_button.upcast_ref()),
        Some(menu_button.upcast_ref()),
    );

    // --- content (persistent timeline + compose) ---
    let msg_container = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    // Click anywhere in the message area that ISN'T a selectable label's
    // text — i.e. the bubble background, an attachment, empty timeline
    // space — should drop the in-progress text selection and cursor. The
    // label's internal textview consumes clicks on the text itself, so
    // those clicks never reach this gesture and don't get spuriously cleared.
    // Also clear any text selection the user has made inside the compose
    // entry — they're now interacting with messages, not drafting one.
    let entry_for_msg = entry.clone();
    let msg_container_click = gtk::GestureClick::new();
    msg_container_click.set_propagation_phase(gtk::PropagationPhase::Bubble);
    msg_container_click.connect_released(move |_gesture, _n, _x, _y| {
        log::debug!("msg_container click fired");
        deselect_all_labels();
        defocus_entry(&entry_for_msg);
    });
    msg_container.add_controller(msg_container_click);
    let msg_scroller = scrolled(&msg_container);
    // The container gesture above only sees clicks that hit the container
    // or bubble up from its children. Clicks on the scrolled window's empty
    // viewport (the chat-view background below all messages) target the
    // viewport, not the container, so they never reach that gesture. This
    // one catches them — same bubble phase, same handlers — so the entry
    // selection clears no matter where in the chat view the user clicks.
    let entry_for_scroller = entry.clone();
    let msg_scroller_click = gtk::GestureClick::new();
    msg_scroller_click.set_propagation_phase(gtk::PropagationPhase::Bubble);
    msg_scroller_click.connect_released(move |_gesture, _n, _x, _y| {
        deselect_all_labels();
        defocus_entry(&entry_for_scroller);
    });
    msg_scroller.add_controller(msg_scroller_click);

    // Floating "more unread above" pill, layered over the timeline. Hidden until
    // a chat with not-yet-loaded unread messages is opened.
    let unread_pill = gtk::Button::builder()
        .label("↑ Earlier unread messages")
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
        .margin_top(10)
        .visible(false)
        .build();
    apply_text_scale(&unread_pill, 12.0);
    unread_pill.add_css_class("osd");
    unread_pill.add_css_class("pill");
    unread_pill.add_css_class("unread-pill");

    let msg_overlay = gtk::Overlay::new();
    msg_overlay.set_child(Some(&msg_scroller));
    msg_overlay.add_overlay(&unread_pill);

    let attach = gtk::Button::from_icon_name("text-x-generic-symbolic");
    attach.add_css_class("flat");
    attach.set_tooltip_text(Some("Attach a file"));
    // `entry` is created up top (right after `chat_list`) so the chat-list
    // and message-container click handlers can reach it.
    let send = gtk::Button::from_icon_name("ob-send-symbolic");
    send.add_css_class("circular");
    send.add_css_class("suggested-action");
    send.set_tooltip_text(Some("Send"));

    let compose = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(8)
        .margin_end(8)
        .build();
    compose.append(&attach);
    compose.append(&entry);
    compose.append(&send);

    // Pending-attachment chip row: icon + file name + close button.
    let pending_chip = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(8)
        .margin_end(8)
        .margin_top(4)
        .margin_bottom(0)
        .visible(false)
        .build();
    let pending_chip_icon = gtk::Image::new();
    pending_chip_icon.set_pixel_size(48);
    pending_chip.append(&pending_chip_icon);
    let pending_chip_label = gtk::Label::new(None);
    pending_chip.append(&pending_chip_label);
    let pending_chip_close = gtk::Button::from_icon_name("window-close-symbolic");
    pending_chip_close.add_css_class("flat");
    pending_chip_close.set_valign(gtk::Align::Center);
    pending_chip_close.set_focus_on_click(false);
    pending_chip.append(&pending_chip_close);

    // Outer vertical box: chip row above the compose bar.
    let compose_outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .build();
    compose_outer.append(&pending_chip);
    compose_outer.append(&compose);
    // Hidden until a chat is opened — the compose bar only makes sense
    // when the user is inside a conversation.
    compose_outer.set_visible(false);

    // Rename action in the chat header; only meaningful with a chat open, so it
    // starts insensitive and open_chat enables it.
    let rename_button = gtk::Button::from_icon_name("document-edit-symbolic");
    rename_button.set_tooltip_text(Some("Rename conversation"));
    rename_button.set_sensitive(false);

    // Empty-state illustration shown in the content pane before any chat is
    // opened. Sits behind the same content page as the timeline, swapped in via
    // a Stack. In collapsed (narrow) mode the split view hides the content pane
    // entirely until a chat is opened, so this only appears when both the
    // sidebar and the content pane are visible — the side-by-side layout.
    let empty_state = adw::StatusPage::builder()
        .icon_name("empty-state")
        .description("Pick a conversation from the sidebar to start messaging.")
        .build();
    empty_state.add_css_class("empty-hero");
    let content_stack = gtk::Stack::new();
    content_stack.add_named(&empty_state, Some("empty"));
    content_stack.add_named(&msg_overlay, Some("chat"));
    content_stack.set_visible_child_name("empty");

    // Registration-status banner sits at the top of the content pane, above
    // the stack, so it's visible whether a chat is open or not.
    let reg_banner = adw::Banner::new("");
    reg_banner.set_revealed(false);
    let content_body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    content_body.append(&reg_banner);
    content_body.append(&content_stack);

    let content_page = page(
        "Select a chat",
        &content_body,
        Some(compose_outer.upcast_ref()),
        None,
        Some(rename_button.upcast_ref()),
    );

    // --- split view ---
    let split = adw::NavigationSplitView::new();
    split.set_sidebar(Some(&sidebar));
    split.set_content(Some(&content_page));

    let ui = Ui {
        store: store.clone(),
        backend: backend.clone(),
        split: split.clone(),
        content_page: content_page.clone(),
        rename_button: rename_button.clone(),
        chat_list: chat_list.clone(),
        chats: Rc::new(RefCell::new(Vec::new())),
        msg_container: msg_container.clone(),
        scroller: msg_scroller.clone(),
        client: client.clone(),
        connection: connection.clone(),
        handles: handles.clone(),
        contacts: Rc::new(RefCell::new(Vec::new())),
        open_summary: Rc::new(RefCell::new(None)),
        page_oldest: Rc::new(RefCell::new(None)),
        page_has_more: Rc::new(RefCell::new(false)),
        page_loading: Rc::new(RefCell::new(false)),
        unread: Rc::new(RefCell::new(None)),
        unread_marker_shown: Rc::new(RefCell::new(false)),
        unread_marker: Rc::new(RefCell::new(None)),
        unread_dismiss_gen: Rc::new(Cell::new(0)),
        unread_pill: unread_pill.clone(),
        entry: entry.clone(),
        typing_sent: Rc::new(Cell::new(false)),
        typing_idle_gen: Rc::new(Cell::new(0)),
        typing_active: Rc::new(Cell::new(false)),
        typing_row: Rc::new(RefCell::new(None)),
        typing_gen: Rc::new(Cell::new(0)),
        morph_pending: Rc::new(Cell::new(false)),
        focused: Rc::new(Cell::new(true)),
        settling: Rc::new(Cell::new(false)),
        settle_gen: Rc::new(Cell::new(0)),
        refresh_gen: Rc::new(Cell::new(0)),
        // Start the watermark at "now" so the startup backlog (past-dated) doesn't
        // fire a flood of notifications; only messages arriving live will notify.
        notify_watermark: Rc::new(Cell::new(now_ms())),
        window: Rc::new(RefCell::new(None)),
        notified_chats: Rc::new(RefCell::new(HashSet::new())),
        pending_notifications: Rc::new(RefCell::new(pending::PendingNotifications::new(
            NOTIFICATION_DEBOUNCE,
            NOTIFICATION_MAX_AGE,
        ))),
        notify_swept: Rc::new(Cell::new(false)),
        preview_cards: Rc::new(RefCell::new(std::collections::HashMap::new())),
        pending_attachment: Rc::new(RefCell::new(None)),
        pending_chip: pending_chip.clone(),
        pending_chip_label: pending_chip_label.clone(),
        pending_chip_icon,
        compose_outer: compose_outer.clone(),
        content_stack: content_stack.clone(),
        rendered_guids: Rc::new(RefCell::new(Vec::new())),
        current_receipt_text: Rc::new(RefCell::new(None)),
        receipt_label: Rc::new(RefCell::new(None)),
        current_chips: Rc::new(RefCell::new(std::collections::HashMap::new())),
        current_reactions: Rc::new(RefCell::new(std::collections::BTreeMap::new())),
        current_text: Rc::new(RefCell::new(std::collections::HashMap::new())),
        reg_banner: reg_banner.clone(),
        reg_notified: Rc::new(Cell::new(false)),
        chat_selection: Rc::new(RefCell::new(ChatSelectionState::new())),
    };

    // Sync the compose bar visibility with the split view's content panel.
    // In collapsed (mobile) mode, pressing back hides the content panel —
    // the compose bar should hide with it. In expanded mode this is a no-op
    // because show-content stays true once open_chat sets it.
    {
        let compose_outer = compose_outer.clone();
        split.connect_notify_local(Some("show-content"), move |split, _| {
            compose_outer.set_visible(split.shows_content());
        });
    }

    // Open a chat when its row is activated (or toggle selection in selection mode).
    {
        let ui = ui.clone();
        chat_list.connect_row_activated(move |_list, row| {
            let idx = row.index();
            if idx < 0 {
                return;
            }
            let chat = ui.chats.borrow().get(idx as usize).cloned();
            if let Some(chat) = chat {
                if ui.chat_selection.borrow().is_selecting() {
                    // In selection mode, toggle this chat instead of opening it.
                    ui.chat_selection.borrow_mut().toggle_chat(chat.id);
                    // Update the row's visual selection state.
                    if ui.chat_selection.borrow().is_selected(chat.id) {
                        row.add_css_class("chat-row-selected");
                    } else {
                        row.remove_css_class("chat-row-selected");
                    }
                } else {
                    ui.open_chat(&chat);
                }
            }
        });
    }

    // Rename the open conversation.
    {
        let ui = ui.clone();
        rename_button.connect_clicked(move |_| ui.prompt_edit_chat());
    }

    // New Chat button in the sidebar header.
    {
        let ui = ui.clone();
        plus_button.connect_clicked(move |_| ui.show_new_chat_dialog());
    }

    // Close button on the pending-attachment chip clears it.
    {
        let ui = ui.clone();
        pending_chip_close.connect_clicked(move |_| ui.clear_pending_attachment());
    }

    // Sidebar hamburger menu actions ("menu" group, resolved via the split).
    {
        let actions = gtk::gio::SimpleActionGroup::new();
        let preferences = gtk::gio::SimpleAction::new("preferences", None);
        preferences.connect_activate({
            let ui = ui.clone();
            move |_, _| ui.show_preferences()
        });
        actions.add_action(&preferences);
        let about = gtk::gio::SimpleAction::new("about", None);
        about.connect_activate({
            let ui = ui.clone();
            move |_, _| ui.show_about()
        });
        actions.add_action(&about);
        split.insert_action_group("menu", Some(&actions));
    }

    // Load the previous page when the user scrolls near the top (ignoring the
    // transient resets a rebuild produces while it settles). The same handler
    // also tracks whether the viewport is parked at the bottom of the chat,
    // which the sticky-bottom logic below uses to keep the latest message
    // visible across viewport-size changes (window resize, sidebar collapse)
    // without yanking the user away from older history they're reading.
    {
        let ui = ui.clone();
        let adj = msg_scroller.vadjustment();
        let was_at_bottom = Rc::new(Cell::new(false));

        // value-changed: refresh the parked flag and run the existing
        // pagination check (which is suppressed during rebuild settles).
        let was_at_bottom_v = was_at_bottom.clone();
        let ui_v = ui.clone();
        adj.connect_value_changed(move |a| {
            // 8px of slop for sub-pixel jitter — anything further means the
            // user deliberately scrolled up to read; not "at the bottom".
            let at_bot = a.value() >= a.upper() - a.page_size() - 8.0;
            was_at_bottom_v.set(at_bot);

            if ui_v.settling.get() {
                return;
            }
            // Only a genuine near-top with real scrollback counts — a transient
            // reset during a rebuild collapses upper to the viewport and is ignored.
            if a.value() <= 64.0 && a.upper() > a.page_size() + 4.0 {
                ui_v.maybe_load_older();
            }
        });

        // changed (fires when lower/upper/page-size/step change): sticky-bottom
        // snap. GTK preserves the absolute scroll value when the viewport is
        // reallocated, so a content height that grew under it (reflow on a
        // narrower window, or the sidebar collapsing into a single pane and
        // expanding the content view) leaves the bottom of the viewport cut
        // off below the visible area. Re-snap to the new bottom iff we were
        // parked there and we're not mid-rebuild — scroll_to owns positioning
        // during a rebuild.
        let was_at_bottom_c = was_at_bottom.clone();
        let ui_c = ui.clone();
        // Sticky-bottom re-pin, synchronously inside `changed`.
        //
        // GTK keeps the scroll value at its old absolute position across a
        // viewport reallocation: when the content grows under it (a narrowing
        // resize reflowing text, or — critically — the compose-area chip row
        // appearing/disappearing on attach/clear, which resizes the scrolled
        // window and fires `changed` via the page-size change), the old value
        // is now too LOW and the newest message drops behind the input bar.
        // We re-pin to the new bottom in the same frame `changed` fires.
        //
        // Use the adjustment's own `upper`, NOT a `measure()` of the container.
        // `changed` is emitted by GtkViewport *after* it has configured the
        // adjustment in size_allocate, so `upper` is already the fresh, real
        // content height. The viewport's default `vscroll-policy = MINIMUM`
        // sizes `upper` from the child's minimum height — and a size-requested
        // GtkPicture's minimum height is its *scaled* size (the real on-screen
        // height). A `measure().1` (natural) call instead returns the picture's
        // *intrinsic* (unscaled) height, which is thousands of pixels for a
        // photo. Raising `upper` to that overstated value (as this handler used
        // to) and scrolling to `overstated - page` parks the viewport in empty
        // space past the real content — the chat goes blank and scroll events
        // become no-ops until a rebuild. This was the attach-a-file bug. The
        // EPS guard avoids a no-op set_value when already parked at the bottom.
        adj.connect_changed(move |a| {
            if !ui_c.settling.get() && was_at_bottom_c.get() {
                let page = a.page_size();
                let bottom = (a.upper() - page).max(0.0);
                if (a.value() - bottom).abs() > 0.5 {
                    a.set_value(bottom);
                }
            }
        });
    }

    // Tapping the floating pill jumps straight to the first unread message.
    {
        let ui = ui.clone();
        unread_pill.connect_clicked(move |_| ui.jump_to_first_unread());
    }

    // Compose send (button + Enter).
    {
        let ui = ui.clone();
        let entry = entry.clone();
        send.connect_clicked(move |_| ui.compose_send(&entry));
    }
    {
        let ui = ui.clone();
        let entry2 = entry.clone();
        entry.connect_activate(move |_| ui.compose_send(&entry2));
    }
    // Drive the outbound typing indicator from edits to the compose entry.
    {
        let ui = ui.clone();
        entry.connect_changed(move |e| ui.note_typing_activity(!e.text().trim().is_empty()));
    }

    // Ctrl+V paste-from-clipboard: intercept before the Entry's default handler.
    {
        let paste_ctrl = gtk::EventControllerKey::new();
        paste_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
        let ui = ui.clone();
        paste_ctrl.connect_key_pressed(move |_ctrl, keyval, _keycode, state| {
            let ctrl = state.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            if !ctrl {
                return glib::Propagation::Proceed;
            }
            if keyval != gtk::gdk::Key::v && keyval != gtk::gdk::Key::V {
                return glib::Propagation::Proceed;
            }
            ui.try_attach_from_clipboard()
        });
        entry.add_controller(paste_ctrl);
    }

    // Attach: open the system file picker, then set a pending attachment.
    {
        let ui = ui.clone();
        attach.connect_clicked(move |btn| {
            let dialog = gtk::FileDialog::builder().title("Attach a file").build();
            let win = btn
                .root()
                .and_then(|r| r.downcast::<gtk::Window>().ok());
            let ui = ui.clone();
            dialog.open(win.as_ref(), gtk::gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res {
                    if let Some(path) = file.path() {
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "file".to_string());
                        let mime = guess_mime(&name);
                        ui.set_pending_attachment(PendingAttachment {
                            path,
                            name,
                            mime,
                        });
                    }
                }
            });
        });
    }

    // Track window focus once the UI is mapped into a window. Messages arriving
    // while unfocused stay unread; on return we re-show the chat with the unread
    // divider (and pill, if they're past the loaded window).
    {
        let ui_focus = ui.clone();
        let connected = Rc::new(Cell::new(false));
        split.connect_map(move |w| {
            if connected.replace(true) {
                return;
            }
            if let Some(win) = w.root().and_then(|r| r.downcast::<gtk::Window>().ok()) {
                *ui_focus.window.borrow_mut() = Some(win.clone());
                ui_focus.focused.set(win.is_active());
                let ui2 = ui_focus.clone();
                win.connect_is_active_notify(move |win| {
                    let active = win.is_active();
                    let was = ui2.focused.replace(active);
                    if active && !was {
                        ui2.on_window_focus();
                    }
                });
            }
        });
    }

    // Clicking a desktop notification raises the window and opens the chat it
    // targets (the notification carries the chat id as its action target).
    if let Some(app) = gtk::gio::Application::default() {
        let action = gtk::gio::SimpleAction::new("open-chat", Some(glib::VariantTy::INT64));
        let ui_act = ui.clone();
        action.connect_activate(move |_, param| {
            if let Some(id) = param.and_then(|p| p.get::<i64>()) {
                ui_act.activate_chat(id);
            }
        });
        app.add_action(&action);
    }

    // Host the split view inside the existing navigation stack, wrapped in an
    // overlay so we can layer an enlarged-image lightbox over everything.
    let overlay = gtk::Overlay::new();
    overlay.set_widget_name("lightbox-host");

    // Adaptive layout. Below the breakpoint the split collapses into a single
    // pane: the sidebar is the visible page, activating a chat pushes the chat
    // view over it, and the content header bar gets an automatic back button —
    // the phone-style flow. Above it, the side-by-side split returns.
    //
    // Sizing. AdwNavigationSplitView reports its uncollapsed natural width as
    // `sidebar_nat + content_nat` (see measure_uncollapsed in libadwaita),
    // where sidebar_nat is derived from content via `sidebar_width_fraction`.
    // With our default 0.25 fraction and 180sp min_sidebar_width, that's
    // ~180 + the widest message row — easily 560–610px once a chat with image
    // attachments or max-width text bubbles is open. We size the BreakpointBin
    // and the breakpoint threshold so the bin's allocation is *always* at least
    // the active layout's natural width:
    //   - collapsed natural ≈ max(sidebar page, content page) ≈ max chat row,
    //     widest message row — bounded above by ~430px for typical chats.
    //   - uncollapsed natural ≈ 180 + content ≈ 560–610px.
    // Putting the breakpoint at 620sp keeps the split collapsed for any size
    // where the uncollapsed natural would overflow the bin, and width_request
    // of 440 sets the window minimum above the collapsed natural so we never
    // clip the bottom of the phone-mode range either. AdwBreakpointBin forces
    // its own minimum to 0 when breakpoints are present, so width_request is
    // the only floor — set it carefully.
    //
    // We drive this from a BreakpointBin (rather than the window) so it works
    // under both the real and demo windows without either needing to know
    // about it.
    let bp_bin = adw::BreakpointBin::builder()
        .width_request(440)
        .height_request(294)
        .child(&split)
        .build();
    let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        620.0,
        adw::LengthUnit::Sp,
    ));
    breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
    bp_bin.add_breakpoint(breakpoint);
    overlay.set_child(Some(&bp_bin));

    let host = adw::NavigationPage::builder()
        .title("Bubbles")
        .child(&overlay)
        .build();
    nav.replace(&[host]);

    // --- Contact cache: load persisted cache, then first sidebar render,
    //     then background EDS refresh. ---
    // Load the on-disk cache (from the previous run) into memory *before*
    // the first `reload_chats` so the sidebar renders with contact names
    // immediately — no flash of bare addresses on second+ launch.
    let ui_for_cache = ui.clone();
    let store_for_cache = ui.store.clone();
    gtk_bridge::spawn(
        async move { store_for_cache.all_contacts().await },
        move |result| {
            if let Ok(cached) = result {
                *ui_for_cache.contacts.borrow_mut() = cached;
            }
            ui_for_cache.reload_chats(|_| {});

            // Now refresh from EDS in the background. On success, update the
            // in-memory cache and re-render. On failure, the persisted cache
            // (or empty) stays — graceful degradation.
            let ui_for_contacts = ui_for_cache.clone();
            let store_for_contacts = ui_for_contacts.store.clone();
            gtk_bridge::spawn(
                async move {
                    let source = crate::contacts_eds::EdsContactSource;
                    crate::contacts::refresh_and_collect(&source, &store_for_contacts).await
                },
                move |result| {
                    match result {
                        Ok(contacts) => {
                            *ui_for_contacts.contacts.borrow_mut() = contacts;
                            ui_for_contacts.schedule_refresh();
                        }
                        Err(e) => {
                            eprintln!("contact refresh failed (EDS unavailable?): {e:#}");
                        }
                    }
                },
            );
        },
    );

    // Receive loop -> persist -> pulse -> refresh.
    let (tx, rx) = async_channel::unbounded::<RecvEvent>();
    let kick = backend.start_receiving(&connection, &client, handles, store.clone(), tx);
    // Wire the wake-from-sleep handler to the receive loop's kick signal.
    crate::power::wire_wake_to_receive_loop(monitor, std::sync::Arc::clone(&kick));

    // --- Threshold-gated launch sync ---
    // Run on launch when the gap since `last_alive` exceeds 2 hours.
    // On first launch the file doesn't exist → gap = ∞ → sync runs.
    #[cfg(feature = "rustpush")]
    {
        let backend = backend.clone();
        let store = store.clone();
        crate::runtime::runtime().spawn(async move {
            let state_dir = glib::user_data_dir().join("bubbles");
            let now_unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let threshold_ms = 2 * 60 * 60 * 1000;

            let last_alive = crate::sync::read_last_alive(&state_dir);
            if crate::sync::should_sync(last_alive, now_unix_ms, threshold_ms) {
                let cutoff_ms = last_alive
                    .unwrap_or(now_unix_ms - 48 * 60 * 60 * 1000)
                    .max(now_unix_ms - 48 * 60 * 60 * 1000);
                let sync_result = backend
                    .sync_missed_messages(&store, cutoff_ms, false)
                    .await;
                log::info!("initial sync: {sync_result:?}");
            } else {
                log::info!("skipping initial sync: gap < 2h, trusting push");
            }
        });
    }

    // --- Wake-from-sleep sync gate ---
    // Run a sync on resume if the sleep gap exceeded 2 hours.
    #[cfg(feature = "rustpush")]
    {
        let backend = backend.clone();
        let store = store.clone();
        monitor.on_resume(move || {
            let backend = backend.clone();
            let store = store.clone();
            crate::runtime::runtime().spawn(async move {
                let state_dir = glib::user_data_dir().join("bubbles");
                let now_unix_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let threshold_ms = 2 * 60 * 60 * 1000;

                let last_alive = crate::sync::read_last_alive(&state_dir);
                if crate::sync::should_sync(last_alive, now_unix_ms, threshold_ms) {
                    let cutoff_ms = last_alive
                        .unwrap_or(now_unix_ms - 48 * 60 * 60 * 1000)
                        .max(now_unix_ms - 48 * 60 * 60 * 1000);
                    let sync_result = backend
                        .sync_missed_messages(&store, cutoff_ms, false)
                        .await;
                    log::info!("wake sync: {sync_result:?}");
                } else {
                    log::info!("wake sync: skipped (gap < 2h)");
                }
            });
        });
    }

    // --- Proactive APS refresh on resume ---
    // After wake, sleep 2s for network re-association, then force a fresh
    // APS connection so the receive loop can re-subscribe without waiting
    // for the 60-second keepalive ping.
    #[cfg(feature = "rustpush")]
    {
        let connection = connection.clone();
        monitor.on_resume(move || {
            let connection = connection.clone();
            crate::runtime::runtime().spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                match crate::protocol::rustpush_backend::refresh_aps(&connection).await {
                    Ok(()) => log::info!("APS connection refreshed on resume"),
                    Err(e) => log::warn!("APS refresh on resume failed: {e}"),
                }
            });
        });
    }

    // --- Periodic last_alive save ---
    // Write the current timestamp every 5 minutes so the threshold gate
    // can detect the gap on next launch / wake. Best-effort: crashes lose
    // up to 5 minutes of staleness (negligible vs. the 2-hour threshold).
    #[cfg(feature = "rustpush")]
    {
        crate::runtime::runtime().spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                let state_dir = glib::user_data_dir().join("bubbles");
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                if let Err(e) = crate::sync::write_last_alive(&state_dir, now_ms) {
                    log::warn!("periodic last_alive save failed: {e}");
                }
            }
        });
    }
    // Banner "Sign Out…" button opens the sign-out confirmation dialog.
    let ui_for_banner = ui.clone();
    reg_banner.connect_button_clicked(move |_| ui_for_banner.show_preferences());

    let ui_refresh = ui.clone();
    gtk_bridge::forward(rx, move |ev| match ev {
        RecvEvent::Applied => ui_refresh.schedule_refresh(),
        RecvEvent::LinkPreviewUpdated { guid, part_idx } => {
            ui_refresh.refresh_link_card(&guid, part_idx)
        }
        RecvEvent::Typing {
            chat_key,
            from,
            typing,
            superseded,
        } => ui_refresh.handle_typing(&chat_key, from.as_deref(), typing, superseded),
        RecvEvent::Registration(status) => {
            use crate::protocol::RegistrationStatus;
            match &status {
                RegistrationStatus::Registered => {
                    ui_refresh.reg_banner.set_revealed(false);
                    // Withdraw the registration notification if one was shown.
                    if let Some(app) = gtk::gio::Application::default() {
                        app.withdraw_notification("registration");
                    }
                    ui_refresh.reg_notified.set(false);
                }
                RegistrationStatus::Registering => {
                    ui_refresh
                        .reg_banner
                        .set_title("Re-registering with iMessage…");
                    ui_refresh.reg_banner.set_button_label(None::<&str>);
                    ui_refresh.reg_banner.set_revealed(true);
                }
                RegistrationStatus::TransientFailure {
                    retry_in_s,
                    error,
                } => {
                    ui_refresh.reg_banner.set_title(&format!(
                        "iMessage registration failed — retrying in {retry_in_s}s"
                    ));
                    ui_refresh.reg_banner.set_tooltip_text(Some(error));
                    ui_refresh.reg_banner.set_button_label(None::<&str>);
                    ui_refresh.reg_banner.set_revealed(true);
                }
                RegistrationStatus::LoggedOut { error } => {
                    ui_refresh
                        .reg_banner
                        .set_title("Logged out by Apple — sign in again");
                    ui_refresh.reg_banner.set_button_label(Some("Sign Out…"));
                    ui_refresh.reg_banner.set_tooltip_text(Some(error));
                    ui_refresh.reg_banner.set_revealed(true);
                    // Send a desktop notification once. The `reg_notified` guard
                    // prevents re-sending on every repeated LoggedOut event.
                    if !ui_refresh.reg_notified.replace(true) {
                        if let Some(app) = gtk::gio::Application::default() {
                            let n = gtk::gio::Notification::new("Logged out by Apple");
                            n.set_body(Some(error));
                            app.send_notification(Some("registration"), &n);
                        }
                    }
                }
            }
        }
    });
}

/// A file the user has picked but not yet sent.
#[derive(Clone, Debug)]
struct PendingAttachment {
    path: std::path::PathBuf,
    name: String,
    mime: String,
}

impl Ui {
    /// Prompt to edit the open conversation's name and/or photo.
    /// The photo section is a placeholder — picking a file stashes it but
    /// does not yet apply it (see Unit 4b).
    fn prompt_edit_chat(&self) {
        let Some(chat) = self.open_summary.borrow().clone() else {
            return;
        };
        // Derived title (what the field value falls back to when empty).
        let derived = {
            let mut c = chat.clone();
            c.custom_name = None;
            chat_title(&c, &self.handles, &self.contacts.borrow())
        };

        // --- Name section ---
        let name_label = gtk::Label::builder()
            .label("Name")
            .halign(gtk::Align::Start)
            .build();
        let entry = gtk::Entry::builder()
            .activates_default(true)
            .text(chat.custom_name.clone().unwrap_or_default())
            .build();
        entry.set_placeholder_text(Some(&derived));

        // --- Photo section ---
        let photo_label = gtk::Label::builder()
            .label("Photo")
            .halign(gtk::Align::Start)
            .build();

        let status_label = gtk::Label::builder()
            .label("No photo selected")
            .halign(gtk::Align::Start)
            .build();
        let choose_btn = gtk::Button::builder()
            .label("Choose Photo…")
            .build();
        let remove_btn = gtk::Button::builder()
            .label("Remove Photo")
            .build();

        // Photo edit state shared across closures.
        let state: Rc<RefCell<PhotoEditState>> = Rc::new(RefCell::new(PhotoEditState {
            picked_path: None,
            decoded: None,
            params: None,
            removal_requested: false,
        }));

        // File picker: set up filter and dialog once.
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Images"));
        filter.add_mime_type("image/png");
        filter.add_mime_type("image/jpeg");
        filter.add_mime_type("image/heic");
        filter.add_mime_type("image/heif");

        let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);

        let file_dialog = gtk::FileDialog::builder()
            .title("Choose a chat photo")
            .default_filter(&filter)
            .filters(&filters)
            .build();

        // --- Crop UI widgets ---
        let crop_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();
        let frame = gtk::Frame::builder()
            .width_request(256)
            .height_request(256)
            .build();
        let overlay = gtk::Overlay::new();
        let picture = gtk::Picture::builder()
            .can_shrink(true)
            // Cover: the source fills the 256×256 frame (sides clipped for
            // non-square images).  Pairs with the `connect_get_child_position`
            // math below so the visible circle is aligned with the displayed
            // image instead of the letterbox.
            .content_fit(gtk::ContentFit::Cover)
            .build();
        overlay.set_child(Some(&picture));
        // Viewport: a thin rectangle outline that fills the overlay, so the
        // user can see the photo's full extent.  The circle indicator (drawn
        // on top, below) shows the actual crop inside the viewport.
        let viewport = gtk::Frame::builder()
            .css_classes(["crop-viewport"])
            .build();
        viewport.set_can_target(false);
        overlay.add_overlay(&viewport);
        overlay.set_measure_overlay(&viewport, false);
        overlay.set_clip_overlay(&viewport, false);
        let indicator = gtk::Frame::builder()
            .css_classes(["crop-indicator"])
            .build();
        indicator.set_can_target(false);
        overlay.add_overlay(&indicator);
        // Measure the indicator with the overlay's allocation (so the
        // get_child_position Rectangle sets the actual size, not a
        // separate measure pass) and don't clip it (so the circle can
        // extend past the overlay's bounds when the user drags the crop
        // off the photo's edge).
        overlay.set_measure_overlay(&indicator, false);
        overlay.set_clip_overlay(&indicator, false);
        // Position the indicator at explicit coordinates via
        // `get_child_position`.  This is NOT a CSS margin — it's an
        // absolute `gdk::Rectangle` that GTK uses directly, so negative
        // coordinates are legal and the indicator can extend past the
        // overlay's bounds.  Same pattern as `bubble_with_chip` (which
        // positions a reaction chip half-on, half-off the bubble's edge).
        // The callback reads the current crop state on every layout pass
        // and returns the indicator's rectangle; the call sites just need
        // `overlay.queue_allocate()` after mutating the state to refresh.
        let state_for_position = state.clone();
        let viewport_for_position = viewport.clone();
        overlay.connect_get_child_position(move |overlay, child| {
            // Viewport: fills the overlay so the user can see the photo's
            // full extent (the "rectangle the same size as the photo" that
            // frames the crop circle).
            if child == &viewport_for_position {
                let w = overlay.width();
                let h = overlay.height();
                if w <= 0 || h <= 0 {
                    return None;
                }
                return Some(gtk::gdk::Rectangle::new(0, 0, w, h));
            }
            // Circle: the actual crop, drawn on top of the viewport.
            let s = state_for_position.borrow();
            let (decoded, params) = match (s.decoded.as_ref(), s.params.as_ref()) {
                (Some(d), Some(p)) => (d, p),
                _ => return None,
            };
            let src_w = decoded.width as f64;
            let src_h = decoded.height as f64;
            // Use the overlay's actual allocated size, not a hardcoded 256.
            // The frame is `width_request(256)` — a minimum, not a fixed size;
            // GTK can (and does, when the dialog content is wider) allocate
            // it larger.  Hardcoding 256 here would position the indicator at
            // the top-left of a larger frame, making the visible circle
            // appear off-centre to the left.
            let frame_w = overlay.width() as f64;
            let frame_h = overlay.height() as f64;
            if frame_w <= 0.0 || frame_h <= 0.0 {
                return None;
            }
            let scale = (frame_w / src_w).max(frame_h / src_h);
            let scaled_w = src_w * scale;
            let scaled_h = src_h * scale;
            let x_offset = ((scaled_w - frame_w) / 2.0).max(0.0);
            let y_offset = ((scaled_h - frame_h) / 2.0).max(0.0);
            let display_r = params.r * scale;
            let display_cx = params.cx * scale - x_offset;
            let display_cy = params.cy * scale - y_offset;
            // The circle's diameter is the actual crop in display coords —
            // no clamp to `min(frame_w, frame_h)`.  For a non-square source
            // where `r = min(src_w, src_h) / 2`, the circle can be wider
            // (or taller) than the frame; `set_clip_overlay(&indicator,
            // false)` lets it extend past the overlay's bounds.  The
            // viewport outline shows the full photo extent so the user
            // can see the circle's position relative to the photo.
            let dia = (display_r * 2.0).round().max(1.0) as i32;
            let x = (display_cx - display_r).round() as i32;
            let y = (display_cy - display_r).round() as i32;
            Some(gtk::gdk::Rectangle::new(x, y, dia, dia))
        });
        frame.set_child(Some(&overlay));
        crop_box.append(&frame);
        crop_box.set_visible(false);

        // Show "Remove Photo" when the chat already has a custom avatar.
        let has_existing_avatar = chat
            .custom_avatar_path
            .as_deref()
            .filter(|p| !p.trim().is_empty())
            .is_some();
        remove_btn.set_visible(has_existing_avatar);

        // --- Remove button ---
        {
            let state = state.clone();
            let status = status_label.clone();
            let crop_box = crop_box.clone();
            let remove_btn = remove_btn.clone();
            remove_btn.clone().connect_clicked(move |_| {
                let mut s = state.borrow_mut();
                s.removal_requested = true;
                s.picked_path = None;
                s.decoded = None;
                s.params = None;
                status.set_label("Photo will be removed on save");
                crop_box.set_visible(false);
                if !has_existing_avatar {
                    remove_btn.set_visible(false);
                }
            });
        }

        // --- Drag gesture for panning the crop ---
        //
        // Attached to the `frame` (the 256×256 container), NOT the picture,
        // and configured to claim the button-drag sequence in capture phase.
        //
        // Why: an earlier version attached the gesture only to `picture` and
        // returned `Proceed` from the handler.  That left the button-drag
        // sequence unclaimed, so the window manager (or some upstream
        // handler) won the sequence and dragged the app window instead of
        // panning the crop.  Capturing on the frame + claiming the sequence
        // explicitly + returning `Propagation::Stop` from `drag_update` fixes
        // it.  (Same pattern the scroll controller below already uses.)
        {
            let state = state.clone();
            let overlay = overlay.clone();
            let drag_start: Rc<RefCell<Option<(f64, f64)>>> = Rc::new(RefCell::new(None));
            let gesture = gtk::GestureDrag::new();
            // Run before the event is delivered to the target so the gesture
            // can win the sequence ahead of the window-drag handler.
            gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
            // No other gesture in our group should also handle this drag.
            gesture.set_exclusive(true);

            {
                let drag_start = drag_start.clone();
                let state = state.clone();
                gesture.connect_drag_begin(move |gesture, _start_x, _start_y| {
                    if let Some(ref params) = state.borrow().params {
                        *drag_start.borrow_mut() = Some((params.cx, params.cy));
                    }
                    // Claim the sequence so the window drag handler does
                    // not take over.
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                });
            }
            {
                let drag_start = drag_start.clone();
                let state = state.clone();
                let overlay = overlay.clone();
                gesture.connect_drag_update(move |_gesture, offset_x, offset_y| {
                    // 1. Read current state values without holding a mutable borrow.
                    let (src_w, src_h, r) = {
                        let s = state.borrow();
                        let d = match s.decoded.as_ref() {
                            Some(v) => v,
                            None => return,
                        };
                        let p = match s.params.as_ref() {
                            Some(v) => v,
                            None => return,
                        };
                        (d.width as f64, d.height as f64, p.r)
                    };
                    // Cover scale: matches the picture's content_fit so
                    // display coords map to source coords correctly even for
                    // non-square images.  Uses the overlay's ACTUAL allocated
                    // size (not a hardcoded 256) so the math matches the
                    // get_child_position callback when the dialog's content
                    // box stretches the frame wider than 256.
                    let frame_w = overlay.width() as f64;
                    let frame_h = overlay.height() as f64;
                    let scale = (frame_w / src_w).max(frame_h / src_h);
                    // Drag offsets are relative to the widget; the
                    // conversion to source coords is `offset / scale`.  The
                    // clip offset used by the get_child_position callback
                    // is a constant per-frame, so it cancels out for
                    // relative deltas — no extra compensation needed here.
                    let (start_cx, start_cy) = match *drag_start.borrow() {
                        Some(v) => v,
                        None => return,
                    };
                    let new_cx = (start_cx + offset_x / scale)
                        .clamp(r, src_w - r);
                    let new_cy = (start_cy + offset_y / scale)
                        .clamp(r, src_h - r);
                    // 2. Write the updated params back.
                    if let Some(ref mut params) = state.borrow_mut().params {
                        params.cx = new_cx;
                        params.cy = new_cy;
                    }
                    // 3. Trigger a re-layout so the overlay's
                    // `get_child_position` callback fires and repositions
                    // the indicator at the new crop.
                    overlay.queue_allocate();
                    // Sequence is already claimed in drag_begin, so the
                    // event won't bubble to the window drag handler.
                });
            }
            frame.add_controller(gesture);
        }

        // --- Scroll gesture for zoom ---
        {
            let state = state.clone();
            let overlay = overlay.clone();
            let scroll = gtk::EventControllerScroll::new(
                gtk::EventControllerScrollFlags::VERTICAL,
            );
            scroll.connect_scroll(move |_scroll, _dx, dy| {
                // Read current state values.
                let (src_w, src_h, r, cx, cy) = {
                    let s = state.borrow();
                    let d = match s.decoded.as_ref() {
                        Some(v) => v,
                        None => return glib::Propagation::Proceed,
                    };
                    let p = match s.params.as_ref() {
                        Some(v) => v,
                        None => return glib::Propagation::Proceed,
                    };
                    (d.width as f64, d.height as f64, p.r, p.cx, p.cy)
                };
                let factor = if dy > 0.0 { 1.1 } else { 0.9 };
                let new_r =
                    (r * factor).clamp(32.0, src_w.min(src_h) / 2.0);
                let new_cx = cx.clamp(new_r, src_w - new_r);
                let new_cy = cy.clamp(new_r, src_h - new_r);
                // Write back.
                if let Some(ref mut params) = state.borrow_mut().params {
                    params.r = new_r;
                    params.cx = new_cx;
                    params.cy = new_cy;
                }
                // Trigger a re-layout so the overlay's get_child_position
                // callback fires and repositions the indicator at the new
                // crop.
                overlay.queue_allocate();
                glib::Propagation::Stop
            });
            picture.add_controller(scroll);
        }

        // --- File picker callback ---
        {
            let state = state.clone();
            let status = status_label.clone();
            let picture = picture.clone();
            let crop_box = crop_box.clone();
            let overlay = overlay.clone();
            let remove_btn_for_fp = remove_btn.clone();
            choose_btn.connect_clicked(move |btn| {
                let win = btn
                    .root()
                    .and_then(|r| r.downcast::<gtk::Window>().ok());
                let dialog = file_dialog.clone();
                let state = state.clone();
                let status = status.clone();
                let picture = picture.clone();
                let crop_box = crop_box.clone();
                let overlay = overlay.clone();
                let remove_btn = remove_btn_for_fp.clone();
                dialog.open(win.as_ref(), gtk::gio::Cancellable::NONE, move |res| {
                    if let Ok(file) = res {
                        if let Some(path) = file.path() {
                            let basename = path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "file".to_string());
                            // Decode on the main thread (acceptable for v1).
                            match crate::image::decode_image_rgba(&path, None) {
                                Ok(decoded) => {
                                    let src_w = decoded.width as f64;
                                    let src_h = decoded.height as f64;
                                    let r_val = src_w.min(src_h) / 2.0;
                                    let params_val = crate::image::CropParams {
                                        cx: src_w / 2.0,
                                        cy: src_h / 2.0,
                                        r: r_val,
                                    };
                                    // Store in state
                                    {
                                        let mut s = state.borrow_mut();
                                        s.picked_path = Some(path.clone());
                                        s.decoded = Some(decoded);
                                        s.params = Some(params_val);
                                        s.removal_requested = false;
                                    }
                                    // Show the image in the picture widget
                                    if let Some(texture) =
                                        load_texture(&path.to_string_lossy())
                                    {
                                        picture.set_paintable(Some(&texture));
                                    }
                                    // Trigger a re-layout so the overlay's
                                    // get_child_position callback fires and
                                    // positions the indicator at the
                                    // centered crop.
                                    overlay.queue_allocate();
                                    crop_box.set_visible(true);
                                    status.set_label(&format!("Picked: {basename}"));
                                    remove_btn.set_visible(true);
                                }
                                Err(e) => {
                                    eprintln!("Failed to decode image: {e}");
                                    status.set_label(&format!(
                                        "Failed to load: {basename}"
                                    ));
                                }
                            }
                        }
                    }
                });
            });
        }

        // Photo row: [Choose Photo… button] + [status label] + [Remove Photo]
        let photo_hbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        photo_hbox.append(&choose_btn);
        photo_hbox.append(&status_label);
        photo_hbox.append(&remove_btn);

        // Main extra child: vertical box containing both sections
        let box_ = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_start(12)
            .margin_end(12)
            .margin_top(12)
            .margin_bottom(12)
            .build();
        box_.append(&name_label);
        box_.append(&entry);
        box_.append(&photo_label);
        box_.append(&photo_hbox);
        box_.append(&crop_box);

        let dialog = adw::AlertDialog::new(Some("Edit Chat"), None);
        dialog.set_extra_child(Some(&box_));
        dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");

        let ui = self.clone();
        let chat_id = chat.id;
        let avatars_dir = glib::user_data_dir().join("bubbles").join("avatars");
        dialog.connect_response(None, move |_dlg, resp| {
            if resp != "save" {
                return;
            }
            let trimmed = entry.text().trim().to_string();
            let name = if trimmed.is_empty() { None } else { Some(trimmed) };
            let store = ui.store.clone();
            let ui2 = ui.clone();
            let name_for_db = name.clone();
            let avatars_dir = avatars_dir.clone();

            // Determine AvatarEdit from photo edit state.
            let avatar_edit = {
                let s = state.borrow();
                if s.removal_requested {
                    crate::ui::AvatarEdit::Remove
                } else if let (Some(_path), Some(decoded), Some(params)) =
                    (s.picked_path.as_ref(), s.decoded.as_ref(), s.params.as_ref())
                {
                    match crate::image::render_avatar(decoded, params) {
                        Ok(rendered) => {
                            // Encode the 256×256 rendered RGBA to PNG bytes,
                            // matching the encoding in image::save_png.
                            let stride = rendered.width as usize * 4;
                            let pixbuf_bytes =
                                glib::Bytes::from_owned(rendered.pixels);
                            let pb = gtk::gdk_pixbuf::Pixbuf::from_bytes(
                                &pixbuf_bytes,
                                gtk::gdk_pixbuf::Colorspace::Rgb,
                                true,
                                8,
                                rendered.width as i32,
                                rendered.height as i32,
                                stride as i32,
                            );
                            match pb.save_to_bufferv("png", &[]) {
                                Ok(png_bytes) => {
                                    crate::ui::AvatarEdit::Replace(png_bytes)
                                }
                                Err(e) => {
                                    eprintln!("PNG encode failed: {e}");
                                    crate::ui::AvatarEdit::NoChange
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("render_avatar failed: {e}");
                            crate::ui::AvatarEdit::NoChange
                        }
                    }
                } else {
                    crate::ui::AvatarEdit::NoChange
                }
            };

            let state_for_done = state.clone();
            let avatars_dir_for_done = avatars_dir.clone();
            gtk_bridge::spawn(
                async move {
                    crate::ui::apply_chat_edit(
                        &store,
                        chat_id,
                        &avatars_dir,
                        name_for_db,
                        avatar_edit,
                    )
                    .await
                },
                move |res| {
                    if let Err(e) = res {
                        eprintln!("edit chat error: {e:#}");
                        return;
                    }
                    // Reflect in the open chat's header right away, then rebuild the
                    // sidebar from the DB so its row picks up the new name too.
                    {
                        let mut g = ui2.open_summary.borrow_mut();
                        if let Some(open) = g.as_mut().filter(|o| o.id == chat_id) {
                            open.custom_name = name.clone();
                            // Update the in-memory avatar path to keep the UI
                            // consistent until the sidebar reloads.
                            let s = state_for_done.borrow();
                            if s.removal_requested {
                                open.custom_avatar_path = None;
                            } else if s.picked_path.is_some() {
                                // A Replace was committed — the DB now has the
                                // path to {avatars_dir}/{chat_id}.png.
                                let target =
                                    avatars_dir_for_done.join(format!("{chat_id}.png"));
                                if let Ok(abs) = std::path::absolute(&target) {
                                    open.custom_avatar_path = Some(
                                        abs.to_string_lossy().into_owned(),
                                    );
                                }
                            }
                            // NoChange: leave custom_avatar_path as-is.
                            ui2.content_page
                                .set_title(&chat_title(open, &ui2.handles, &ui2.contacts.borrow()));
                        }
                    }
                    ui2.reload_chats(|_| {});
                },
            );
        });
        dialog.present(Some(&self.split));
    }

    /// Snapshot the current reactions map for computing chip changes on the
    /// next refresh. Returns a clone of the internal map.
    fn collect_current_reactions(&self) -> std::collections::BTreeMap<String, Vec<LiveReactionSummary>> {
        self.current_reactions.borrow().clone()
    }

    /// Coalesce refresh pulses: a burst of inbound messages (notably the backlog
    /// drained on startup) would otherwise rebuild the sidebar once per message,
    /// flickering the hover/selection. Defer and collapse to a single refresh once
    /// the burst settles.
    fn schedule_refresh(&self) {
        let gen = self.refresh_gen.get().wrapping_add(1);
        self.refresh_gen.set(gen);
        let ui = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
            // Only the most recent pulse in the burst actually refreshes.
            if ui.refresh_gen.get() == gen {
                ui.refresh();
            }
        });
    }

    fn refresh(&self) {
        self.reload_chats(|_| {});
        self.process_notifications();
        let open = self.open_summary.borrow().clone();
        if let Some(chat) = open {
            self.reload_messages(chat.id, chat.is_group);
            // Only mark read while we're actually being looked at. Messages that
            // land while the window is in the background stay unread, so the user
            // gets the "new messages" divider when they come back.
            if self.focused.get() {
                self.maybe_send_read(&chat);
            }
        }
    }

}

/// Build the row widgets for a message slice with intra-slice grouping/spacing.
/// Inserts the "new messages" divider before the message whose guid matches
/// `unread_anchor` (if present in this slice). No receipt indicator. Used to
/// prepend an older page or to append new messages; returns the divider widget
/// if it landed here.
///
/// When `prev` is `Some`, the group state is seeded from that message so the
/// first widget in the batch gets the correct spacing relative to its actual
/// predecessor (not a default "new batch" gap). Used by the `Append` path.
///
/// When `divider_prev_date` is `Some`, it overrides the date-divider comparison
/// date that is normally derived from `prev`. (Currently unused — the prepend
/// path passes `None` and instead removes any now-redundant old-top date divider
/// after building, which avoids duplicates without affecting group spacing.)
#[allow(clippy::too_many_arguments)]
fn build_message_widgets(
    msgs: &[StoredMessage],
    is_group: bool,
    unread_anchor: Option<&str>,
    previews: &std::collections::HashMap<(String, i64), MessageLinkPreview>,
    preview_cards: &Rc<RefCell<std::collections::HashMap<(String, i64), gtk::Widget>>>,
    on_reaction: Option<&Rc<ReactionHandler>>,
    on_edit: Option<&Rc<EditHandler>>,
    on_retry: Option<&Rc<RetryHandler>>,
    reactions: &std::collections::BTreeMap<String, Vec<LiveReactionSummary>>,
    prev: Option<&StoredMessage>,
    handles: &[String],
    contacts: &[Contact],
    divider_prev_date: Option<i64>,
) -> (Vec<gtk::Widget>, Option<gtk::Widget>, std::collections::HashMap<String, ChipEntry>) {
    let mut out = Vec::with_capacity(msgs.len());
    let mut marker: Option<gtk::Widget> = None;
    let mut chip_map: std::collections::HashMap<String, ChipEntry> = std::collections::HashMap::new();
    let (mut last_key, mut last_date, mut last_from_me) = match prev {
        Some(p) => (Some(group_key(p)), p.date, Some(p.is_from_me)),
        None => (None, 0i64, None),
    };
    // Date-divider tracking: seeded from `divider_prev_date` when present
    // (prepend path — compares against the adjacent old content's date so
    // duplicate dividers are avoided when the batch shares a date with
    // already-rendered messages below), otherwise from `prev` (append path).
    // When both are `None` (no adjacent context), `should_show_date_divider`
    // handles the first-non-today case correctly and returns `true`.
    let now = now_ms();
    let mut prev_date: Option<i64> = divider_prev_date.or_else(|| prev.map(|p| p.date));
    for m in msgs {
        if marker.is_none() && unread_anchor == Some(m.guid.as_str()) {
            let mk = unread_marker();
            out.push(mk.clone());
            marker = Some(mk);
            last_key = None;
            last_from_me = None;
            // prev_date continues across the unread marker boundary,
            // matching populate_messages behavior.
        }
        // Skip tapback rows — they render as reaction chips on the target message.
        if m.associated_guid.is_some() {
            continue;
        }
        // Insert a centered date divider when crossing into a different
        // calendar date (mirroring populate_messages). When prev_date is None
        // (start of a batch without prior context), should_show_date_divider
        // correctly returns true for the first non-today message.
        if crate::time_format::should_show_date_divider(prev_date, m.date, now) {
            let label = crate::time_format::format_date_label(m.date);
            out.push(date_divider(&label));
        }
        let key = group_key(m);
        let show_header =
            last_key.as_deref() != Some(key.as_str()) || m.date - last_date > GROUP_GAP_MS;
        let side_changed = last_from_me != Some(m.is_from_me);
        let top = if last_from_me.is_none() {
            8
        } else if side_changed {
            16
        } else if show_header {
            8
        } else {
            2
        };
        // Look up the chip for this message in the reactions map (same as
        // populate_messages does for the initial page). Without this, messages
        // loaded by maybe_load_older never get reaction chips.
        let chip = reactions
            .get(&m.guid)
            .map(|chips| reaction_chips_row(chips));
        let ctx = MessageContext { m, show_header, top, previews, preview_cards, handles, contacts };
        let (row, bubble_or_overlay) = message_widget(ctx, is_group, on_reaction, on_edit, on_retry, chip.as_ref());
        let bubble_widget = match &bubble_or_overlay {
            Some(b) => b.clone(),
            None => row.clone(),
        };
        out.push(row);

        // Record chip entry for in-place update support.
        let entry = ChipEntry {
            bubble: bubble_widget,
            chip: chip.clone(),
        };
        chip_map.insert(m.guid.clone(), entry);

        last_key = Some(key);
        last_date = m.date;
        last_from_me = Some(m.is_from_me);
        prev_date = Some(m.date);
    }
    (out, marker, chip_map)
}

/// After a full `populate_messages` rebuild, resync the tracked state to
/// match the new container. `msgs` is the full message list the container
/// was built from (including tapback rows; we filter them).
fn sync_tracked_state_after_rebuild(
    container: &gtk::Box,
    msgs: &[StoredMessage],
    rendered_guids: &Rc<RefCell<Vec<String>>>,
    current_receipt_text: &Rc<RefCell<Option<String>>>,
    receipt_label: &Rc<RefCell<Option<gtk::Label>>>,
    current_text: &Rc<RefCell<std::collections::HashMap<String, String>>>,
) {
    // The old receipt_label handle is now stale (the widget was destroyed by
    // clear_box). Drop it before re-extracting.
    *receipt_label.borrow_mut() = None;
    *current_receipt_text.borrow_mut() = None;
    *rendered_guids.borrow_mut() = msgs
        .iter()
        .filter(|m| m.associated_guid.is_none())
        .map(|m| m.guid.clone())
        .collect();
    *current_text.borrow_mut() = msgs
        .iter()
        .filter(|m| m.associated_guid.is_none())
        .filter_map(|m| m.text.as_ref().map(|t| (m.guid.clone(), t.clone())))
        .collect();
    if let Some(label) = extract_receipt_label(container) {
        let text = label.text().to_string();
        *receipt_label.borrow_mut() = Some(label);
        *current_receipt_text.borrow_mut() = Some(text);
    }
}

/// Walk the bubble widget tree to find the inner `gtk::Label`. The bubble
/// may be a bare `gtk::Box` (no chip) or a `gtk::Overlay` (with chip); in
/// the latter case, the label is the first child of the overlay's wrapped
/// bubble box.
fn find_label_in_bubble(bubble: &gtk::Widget) -> Option<gtk::Label> {
    let inner = if bubble.is::<gtk::Overlay>() {
        bubble.first_child()?
    } else {
        bubble.clone()
    };
    let label_widget = inner.first_child()?;
    label_widget.downcast::<gtk::Label>().ok()
}

/// Walk msg_container to find the receipt label. The typing indicator row
/// is a `gtk::Box`, not a `gtk::Label`, so downcasting is sufficient to
/// distinguish them.
fn extract_receipt_label(container: &gtk::Box) -> Option<gtk::Label> {
    let mut child = container.last_child();
    while let Some(c) = child {
        // Clone before downcast so we can still walk to prev_sibling.
        if let Ok(label) = c.clone().downcast::<gtk::Label>() {
            if label.has_css_class("dim-label") && label.has_css_class("caption") {
                return Some(label);
            }
        }
        child = c.prev_sibling();
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn populate_messages(
    container: &gtk::Box,
    msgs: &[StoredMessage],
    is_group: bool,
    unread_anchor: Option<&str>,
    previews: &std::collections::HashMap<(String, i64), MessageLinkPreview>,
    preview_cards: &Rc<RefCell<std::collections::HashMap<(String, i64), gtk::Widget>>>,
    on_reaction: Option<&Rc<ReactionHandler>>,
    on_edit: Option<&Rc<EditHandler>>,
    on_retry: Option<&Rc<RetryHandler>>,
    reactions: &std::collections::BTreeMap<String, Vec<LiveReactionSummary>>,
    handles: &[String],
    contacts: &[Contact],
) -> (Option<gtk::Widget>, std::collections::HashMap<String, ChipEntry>) {
    clear_box(container);
    // Stale card handles from the previous render are about to be destroyed
    // when their old container is cleared. Drop them so `refresh_link_card`
    // doesn't try to swap into a detached widget.
    preview_cards.borrow_mut().clear();
    let mut last_key: Option<String> = None;
    let mut last_date = 0i64;
    let mut last_from_me: Option<bool> = None;
    let mut marker: Option<gtk::Widget> = None;
    let mut chip_map: std::collections::HashMap<String, ChipEntry> = std::collections::HashMap::new();
    // The single message that carries the Delivered/Read indicator.
    // Skip tapback rows — they render as chips on the target message.
    let last_sent_idx = msgs.iter().rposition(|m| m.is_from_me && m.associated_guid.is_none());

    // Date-divider tracking: the last non-tapback message's timestamp so we
    // can decide whether a new calendar-date divider is needed.
    let now = now_ms();
    let mut prev_date: Option<i64> = None;

    for (i, m) in msgs.iter().enumerate() {
        // Tapback rows are rendered as reaction chips on the target message,
        // not as standalone bubbles. Skip them here.
        if m.associated_guid.is_some() {
            continue;
        }

        // Place the "new messages" divider immediately before the exact first
        // unread message (matched by guid), so it can't drift to the top of a
        // partially-loaded window.
        if marker.is_none() && unread_anchor == Some(m.guid.as_str()) {
            let mk = unread_marker();
            container.append(&mk);
            marker = Some(mk);
            // Start the unread run with a fresh header.
            last_key = None;
            last_from_me = None;
        }

        // Insert a centered date divider before the first message of each
        // non-today calendar date and when crossing into a different calendar
        // date. No divider for an all-today run and no duplicate dividers for
        // consecutive messages on the same date.
        if crate::time_format::should_show_date_divider(prev_date, m.date, now) {
            let label = crate::time_format::format_date_label(m.date);
            container.append(&date_divider(&label));
        }

        let key = group_key(m);
        let show_header =
            last_key.as_deref() != Some(key.as_str()) || m.date - last_date > GROUP_GAP_MS;
        // Bigger gap on a received <-> sent flip, medium for a new same-side
        // group, tight within a group.
        let side_changed = last_from_me != Some(m.is_from_me);
        let top = if last_from_me.is_none() {
            8
        } else if side_changed {
            16
        } else if show_header {
            8
        } else {
            2
        };

        // Reaction chips: the chip is built here and passed into the message
        // widget so it can be placed at the top corner of the bubble
        // (inside `bubble_box`, which is now vertical). The chip's `halign` is
        // set by `bubble_box` based on `own` so it lands at the correct
        // corner for incoming (top-right) vs sent (top-left) messages.
        let chip = reactions
            .get(&m.guid)
            .map(|chips| reaction_chips_row(chips));
        let ctx = MessageContext { m, show_header, top, previews, preview_cards, handles, contacts };
        let (row, bubble_or_overlay) = message_widget(ctx, is_group, on_reaction, on_edit, on_retry, chip.as_ref());
        let bubble_widget = match &bubble_or_overlay {
            Some(b) => b.clone(),
            None => row.clone(),
        };
        container.append(&row);
        // Record chip entry for in-place update support.
        let entry = ChipEntry {
            bubble: bubble_widget,
            chip: chip.clone(),
        };
        chip_map.insert(m.guid.clone(), entry);

        // Delivered/Read indicator: only under the most recent sent message, so
        // it moves forward as new messages are sent and never lingers on older ones.
        if Some(i) == last_sent_idx {
            match receipt_status(m) {
                Some(status) => container.append(&receipt_label(&status)),
                // When the freshly sent message is at the very bottom, reserve the
                // receipt line ahead of time (an invisible, same-height placeholder)
                // so the bubble doesn't bump up the moment "Delivered" arrives.
                None if i == msgs.len() - 1 => {
                    container.append(&receipt_label("\u{200b}"))
                }
                None => {}
            }
        }

        last_key = Some(key);
        last_date = m.date;
        last_from_me = Some(m.is_from_me);
        prev_date = Some(m.date);
    }
    (marker, chip_map)
}

/// A centered date-separator divider with hairlines on each side.
/// Used to separate groups of messages from different calendar dates.
enum ScrollTo {
    Bottom,
    Value(f64),
    Widget(gtk::Widget),
}

/// Photo-editing state shared across the crop-UI closures inside
/// `prompt_edit_chat`.  Not `pub` — internal to this module.
#[allow(dead_code)]
struct PhotoEditState {
    /// The path the file picker returned, if the user picked a photo.
    picked_path: Option<PathBuf>,
    /// The decoded source image (RGBA).  `None` before a pick or after decode failure.
    decoded: Option<crate::image::DecodedRgba>,
    /// The current crop selection in source coordinates.
    params: Option<crate::image::CropParams>,
    /// `true` when the user clicked "Remove Photo".
    removal_requested: bool,
}

/// Reposition and resize the circular crop indicator to match `params` for
/// the given source image dimensions.  Called whenever the crop changes
/// (initial load, drag pan, scroll zoom).
///
/// Uses the same `max` scale as the picture's `ContentFit::Cover`, so for
/// non-square images (where the picture is side-clipped, not letterboxed)
/// the visible circle sits over the displayed image.  The `x_offset` /
/// `y_offset` terms account for the symmetric clip: when the source is
/// scaled up to fill the frame, any excess (above the frame size) is split
/// evenly between the two sides and must be subtracted to land the
/// indicator on the visible image.
///
/// NOTE: this helper is no longer called directly.  The crop UI now uses
/// `GtkOverlay::connect_get_child_position` (set up in `prompt_edit_chat`)
/// which returns an absolute `gdk::Rectangle` for the indicator — not a CSS
/// margin.  That is what allows negative coordinates, so the circle can
/// extend past the overlay's bounds when the user drags the crop off the
/// photo's edge.  Callers that previously invoked this function now just
/// call `overlay.queue_allocate()` to retrigger the layout pass that fires
/// the callback.  This function is kept only for the comment block above
/// the math, which documents the symmetric clip math that the callback
/// reproduces inline.
#[allow(dead_code)]
fn _update_crop_indicator_math_doc(
    _indicator: &gtk::Frame,
    src_w: f64,
    src_h: f64,
    params: &crate::image::CropParams,
) {
    let frame_size = 256.0;
    let scale = (frame_size / src_w).max(frame_size / src_h);
    let scaled_w = src_w * scale;
    let scaled_h = src_h * scale;
    // Symmetric clip from Cover: half the excess on each side.  For a
    // square source, both offsets are 0 and the math reduces to the
    // simple `params.cx * scale` form.
    let x_offset = ((scaled_w - frame_size) / 2.0).max(0.0);
    let y_offset = ((scaled_h - frame_size) / 2.0).max(0.0);
    let display_r = params.r * scale;
    let display_cx = params.cx * scale - x_offset;
    let display_cy = params.cy * scale - y_offset;
    let dia = (display_r * 2.0).round().max(1.0) as i32;
    let x = (display_cx - display_r).round() as i32;
    let y = (display_cy - display_r).round() as i32;
    let _ = (dia, x, y);
}







