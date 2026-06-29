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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once, OnceLock};

use adw::prelude::*;
use regex::Regex;

use crate::gtk_bridge;
use crate::protocol::{Backend, Connection, ImClient, RecvEvent, friendly_category_message};
use crate::store::{
    group_tapbacks_by_target, live_tapbacks, AttachmentKind, AttachmentRecord, ChatRef,
    ChatSummary, IncomingMessage, Ingest, LiveReactionSummary, MessageLinkPreview, Store,
    StoredAttachment, StoredMessage,
};
#[cfg(feature = "rustpush")]
use crate::store::Tapback;
#[cfg(feature = "rustpush")]
use rustpush::{Reaction, ReactMessageType};
use tokio::sync::oneshot;

mod avatar;

/// Callback type for the reaction emoji picker: receives the target message
/// GUID, the reaction index (0-5), and the target message's text (for the
/// wire-level `ams` field).
type ReactionHandler = dyn Fn(String, usize, String);

/// Callback type for the "Edit" menu option on own messages: receives the
/// target message GUID and the current text. The handler is responsible for
/// opening the editor (Unit 6 wires this up).
type EditHandler = dyn Fn(String, String);

/// Callback type for the editor's Save button: receives the target message
/// GUID and the new text. Unit 6 leaves this as a no-op; Unit 7 wires the
/// real send.
type EditSaveHandler = dyn Fn(String, String);

/// Default: send read receipts when a chat is viewed.
const SEND_READ_RECEIPTS: bool = true;

/// New sender header after this idle gap (5 min), even for the same person.
const GROUP_GAP_MS: i64 = 5 * 60 * 1000;
/// How many messages to load per page (initial open and each scroll-up).
const PAGE_SIZE: i64 = 20;
/// How long the "New Messages" divider lingers after a chat is opened and read
/// before it dismisses itself.
const UNREAD_DIVIDER_TTL_SECS: u64 = 4;

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
";

/// Regex that matches URLs at word boundaries.
static URL_RE: OnceLock<Regex> = OnceLock::new();

fn url_re() -> &'static Regex {
    URL_RE.get_or_init(|| {
        Regex::new(r#"(?i)\b(?:https?://|www\.)[^\s<>'"{}\[\]()]+[^\s<>'"{}\[\]()\.,:;!?)]"#).unwrap()
    })
}

/// Convert plain text containing URLs into Pango markup with clickable <a> tags.
/// Non-URL text is escaped for markup safety.
fn text_to_markup(text: &str) -> String {
    let re = url_re();
    let mut result = String::with_capacity(text.len() + 64);
    let mut last_end = 0;
    for m in re.find_iter(text) {
        // Escape and append text before this URL
        if m.start() > last_end {
            result.push_str(&escape_markup(&text[last_end..m.start()]));
        }
        // Append the URL as a clickable link
        let url = m.as_str();
        result.push_str(&format!(
            r#"<a href="{}">{}</a>"#,
            escape_markup_attr(url),
            escape_markup(url)
        ));
        last_end = m.end();
    }
    // Append any remaining text
    if last_end < text.len() {
        result.push_str(&escape_markup(&text[last_end..]));
    }
    result
}

/// Escape a string for safe inclusion inside Pango markup.
fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escape a string for safe inclusion inside an XML attribute value.
fn escape_markup_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Open a URI in the system browser.
/// Uses GIO which routes through xdg-desktop-portal inside Flatpak,
/// and launches the default handler directly outside Flatpak.
fn open_uri(uri: &str) {
    let uri = if uri.starts_with("www.") && !uri.starts_with("http") {
        format!("https://{uri}")
    } else {
        uri.to_string()
    };
    if let Err(e) = gtk::gio::AppInfo::launch_default_for_uri(&uri, None::<&gtk::gio::AppLaunchContext>) {
        eprintln!("failed to open URI {}: {e}", uri);
    }
}

/// Maps target message guid → ChipEntry. Populated after every populate_messages
/// rebuild and after every append/prepend, used by `reload_messages` to apply
/// `UpdateChips` in place without rebuilding the view.
#[derive(Clone)]
struct ChipEntry {
    /// The bubble widget, or — if the message currently has a chip — the
    /// `gtk::Overlay` wrapping the bubble. Used to find the bubble in the
    /// "add first chip" case (where the bubble is still a plain Box) and to
    /// find the overlay in the "update/remove chip" cases.
    bubble: gtk::Widget,
    /// The chip widget, if the message currently has reactions. `None` means
    /// the message was rendered without a chip and we'd need to add one (the
    /// "add first chip" case).
    chip: Option<gtk::Widget>,
}

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

    let content_page = page(
        "Select a chat",
        &content_stack,
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

    // Open a chat when its row is activated.
    {
        let ui = ui.clone();
        chat_list.connect_row_activated(move |_list, row| {
            let idx = row.index();
            if idx < 0 {
                return;
            }
            let chat = ui.chats.borrow().get(idx as usize).cloned();
            if let Some(chat) = chat {
                ui.open_chat(&chat);
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

    ui.reload_chats(|_| {});

    // Receive loop -> persist -> pulse -> refresh.
    let (tx, rx) = async_channel::unbounded::<RecvEvent>();
    let kick = backend.start_receiving(&connection, &client, handles, store.clone(), tx);
    // Wire the wake-from-sleep handler to the receive loop's kick signal.
    let monitor = std::sync::Arc::new(crate::power::PowerMonitor::new());
    crate::power::wire_wake_to_receive_loop(&monitor, std::sync::Arc::clone(&kick));

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

    // Subscribe to OS resume events (Linux only).
    #[cfg(target_os = "linux")]
    crate::power::spawn_dbus_power_monitor(std::sync::Arc::clone(&monitor));

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
    /// Set the pending attachment and show the chip. The chip's label is
    /// updated to the file name.
    fn set_pending_attachment(&self, att: PendingAttachment) {
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
    fn clear_pending_attachment(&self) {
        self.pending_chip.set_visible(false);
        self.pending_chip_label.set_text("");
        self.pending_chip_icon.set_paintable(None::<&gtk::gdk::Paintable>);
        *self.pending_attachment.borrow_mut() = None;
    }

    /// Inspect the default clipboard and, if it carries a file URI or a
    /// supported image mime, attach the first item via `set_pending_attachment`.
    /// Returns `Propagation::Stop` when we initiate an attach (so the entry's
    /// default text paste is suppressed) and `Propagation::Proceed` otherwise.
    fn try_attach_from_clipboard(&self) -> glib::Propagation {
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

    fn reload_chats(&self, on_chats: impl FnOnce(&[ChatSummary]) + 'static) {
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
                ui.chat_list.append(&chat_row(c, &ui.handles));
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
    fn show_new_chat_dialog(&self) {
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
        {
            let to_row = to_row.clone();
            let msg_row = msg_row.clone();
            let update_send_sensitivity = update_send_sensitivity.clone();
            let debounce_source = debounce_source.clone();
            let error_label = error_label.clone();
            to_row.clone().connect_changed(move |_| {
                let to = to_row.text();
                let msg = msg_row.text();
                update_send_sensitivity(&to, &msg);

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
                let name_owned: Option<String> = if name.is_empty() {
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
    fn create_new_chat(&self, recipient: &str, text: &str, name: Option<String>) {
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
    fn show_preferences(&self) {
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
                        let password_prompt =
                            build_password_prompt_closure(None);
                        match crate::protocol::rustpush_backend::orchestrate_sync_now_flow(
                            &*backend,
                            &store,
                            i64::MIN,
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
                let window = self.window.clone();
                sync_now_button.connect_clicked(move |btn| {
                    btn.set_sensitive(false);
                    status_label.set_text("Syncing…");
                    let backend = backend.clone();
                    let store = store.clone();
                    let status_label = status_label.clone();
                    let button = button.clone();
                    // Pre-build the password prompt closure while on the GTK
                    // main thread, so we can capture the parent window.
                    let parent = window
                        .borrow()
                        .as_ref()
                        .and_then(|w| w.downcast_ref::<adw::Window>().cloned());
                    let password_prompt =
                        build_password_prompt_closure(parent.as_ref());
                    // Run the orchestrator on the Tokio runtime (it touches
                    // hyper, which requires a reactor — `glib::spawn_future_local`
                    // doesn't provide one and would panic). Bridge the result
                    // back to the glib main thread for the UI update.
                    let (tx, rx) = oneshot::channel();
                    crate::runtime::runtime().spawn(async move {
                        let result = crate::protocol::rustpush_backend::orchestrate_sync_now_flow(
                            &*backend,
                            &store,
                            i64::MIN,
                            true,
                            password_prompt,
                        )
                        .await;
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
    fn reload_open_chat(&self) {
        self.reload_chats(|_| {});
        if let Some(chat) = self.open_summary.borrow().as_ref() {
            self.reload_messages(chat.id, chat.is_group);
        }
    }

    fn confirm_sign_out(&self, prefs: &adw::PreferencesDialog) {
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

    fn show_about(&self) {
        let about = adw::AboutDialog::builder()
            .application_name("Bubbles")
            .version(env!("CARGO_PKG_VERSION"))
            .build();
        if let Some(id) = gtk::gio::Application::default().and_then(|a| a.application_id()) {
            about.set_application_icon(id.as_str());
        }
        about.present(Some(&self.split));
    }

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
            chat_title(&c, &self.handles)
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
                                .set_title(&chat_title(open, &ui2.handles));
                        }
                    }
                    ui2.reload_chats(|_| {});
                },
            );
        });
        dialog.present(Some(&self.split));
    }

    /// React to compose-entry edits: send a typing start when text first appears,
    /// a stop when it's cleared, and re-arm an idle timer that stops after a pause
    /// (so we don't leave the other side showing dots if the user walks away).
    fn note_typing_activity(&self, typing_now: bool) {
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

    fn send_typing(&self, chat: &ChatSummary, typing: bool) {
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
    fn handle_typing(&self, chat_key: &str, from: Option<&str>, typing: bool, superseded: bool) {
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

    fn hide_typing_indicator(&self) {
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

    fn remove_typing_row(&self) {
        if let Some(row) = self.typing_row.borrow_mut().take() {
            self.msg_container.remove(&row);
        }
    }

    /// A timeline rebuild clears the container (and our row with it); drop the
    /// stale handle and re-append a fresh bubble if typing is still active, so a
    /// refresh mid-typing doesn't drop the indicator.
    fn refresh_typing_row(&self, is_group: bool) {
        *self.typing_row.borrow_mut() = None;
        if self.typing_active.get() {
            self.append_typing_row(is_group);
        }
    }

    /// Replace a single link preview card in place. Driven by the
    /// `RecvEvent::LinkPreviewUpdated` event from the receive loop. The full
    /// `reload_messages` path is forbidden on this event (it would flicker and
    /// jump scroll); we walk the tracked card map, find the live widget, and
    /// swap in a freshly-built one built from the new store row. No-op when the
    /// chat is closed or the message isn't currently on screen.
    fn refresh_link_card(&self, guid: &str, part_idx: i64) {
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
    fn open_chat(&self, chat: &ChatSummary) {
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
        self.content_page.set_title(&chat_title(chat, &self.handles));
        self.rename_button.set_sensitive(true);
        self.split.set_show_content(true);
        self.compose_outer.set_visible(true);
        // Drop the empty-state illustration now that a real conversation is
        // loaded into the content pane.
        self.content_stack.set_visible_child_name("chat");
        // Opening the chat means reading it — clear any pending notification.
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
                let (marker, chip_map) = populate_messages(&ui.msg_container, &msgs, is_group, anchor, &previews, &ui.preview_cards, on_reaction.as_ref(), on_edit.as_ref(), &reactions);
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
    fn reload_messages(&self, chat_id: i64, is_group: bool) {
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
                        // Seed the group state from the last previously-rendered
                        // message so the first appended widget gets correct spacing.
                        let prev_msg = prev_guids
                            .last()
                            .and_then(|g| msgs.iter().find(|m| &m.guid == g));
                        let (widgets, _marker, chip_map) = build_message_widgets(
                            &new_tail,
                            is_group,
                            None,
                            &previews,
                            &ui.preview_cards,
                            on_reaction.as_ref(),
                            on_edit.as_ref(),
                            &reactions,
                            prev_msg,
                        );
                        ui.current_chips.borrow_mut().extend(chip_map);
                        *ui.current_reactions.borrow_mut() = reactions.clone();
                        for w in &widgets {
                            ui.msg_container.append(w);
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
                        let (marker, chip_map) = populate_messages(
                            &ui.msg_container,
                            &msgs,
                            is_group,
                            anchor.as_deref(),
                            &previews,
                            &ui.preview_cards,
                            on_reaction.as_ref(),
                            on_edit.as_ref(),
                            &reactions,
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
    fn maybe_load_older(&self) {
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
                let (widgets, marker, chip_map) = build_message_widgets(
                    &older,
                    is_group,
                    anchor.as_deref(),
                    &previews,
                    &ui.preview_cards,
                    on_reaction.as_ref(),
                    on_edit.as_ref(),
                    &reactions,
                    None,
                );
                ui.current_chips.borrow_mut().extend(chip_map);
                *ui.current_reactions.borrow_mut() = reactions.clone();
                for w in widgets.into_iter().rev() {
                    ui.msg_container.prepend(&w);
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
    fn jump_to_first_unread(&self) {
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
                let (marker, chip_map) = populate_messages(
                    &ui.msg_container,
                    &msgs,
                    is_group,
                    Some(guid.as_str()),
                    &previews,
                    &ui.preview_cards,
                    on_reaction.as_ref(),
                    on_edit.as_ref(),
                    &reactions,
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
    fn dismiss_unread_divider(&self) {
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
    fn arm_unread_dismiss(&self) {
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

    fn compose_send(&self, entry: &gtk::Entry) {
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
            let backend = self.backend.clone();
            let client = self.client.clone();
            let connection = self.connection.clone();
            let ui = self.clone();
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
                            if let Err(e) = res {
                                eprintln!("attachment send failed: {e:#}");
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
                        if let Err(e) = res {
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
    fn make_reaction_handler(&self) -> Option<Rc<ReactionHandler>> {
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
    fn make_edit_handler(&self) -> Option<Rc<EditHandler>> {
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

    /// Snapshot the current reactions map for computing chip changes on the
    /// next refresh. Returns a clone of the internal map.
    fn collect_current_reactions(&self) -> std::collections::BTreeMap<String, Vec<LiveReactionSummary>> {
        self.current_reactions.borrow().clone()
    }

    /// Ack the newest unread inbound message (implicitly marking earlier ones read).
    fn maybe_send_read(&self, chat: &ChatSummary) {
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

    /// Raise a desktop notification for each chat that received new messages,
    /// unless that chat is the one currently open *and* focused. Coalesces per
    /// chat (id-keyed), so new messages replace the prior notification rather
    /// than stacking, and a watermark ensures each message notifies once.
    fn process_notifications(&self) {
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
                ui.notify_watermark.set(max_date);

                let open_id = ui.open_summary.borrow().as_ref().map(|c| c.id);
                let focused = ui.focused.get();
                for chat_id in order {
                    let (sender, preview, count) = per_chat.remove(&chat_id).unwrap();
                    // Don't notify for the chat the user is actively viewing.
                    if focused && open_id == Some(chat_id) {
                        ui.withdraw_chat_notification(chat_id);
                        continue;
                    }
                    let summary = ui.chats.borrow().iter().find(|c| c.id == chat_id).cloned();
                    let (title, is_group) = match &summary {
                        Some(c) => (chat_title(c, &ui.handles), c.is_group),
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
                    ui.show_chat_notification(chat_id, &title, &body);
                }
            },
        );
    }

    fn show_chat_notification(&self, chat_id: i64, title: &str, body: &str) {
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

    fn withdraw_chat_notification(&self, chat_id: i64) {
        if let Some(app) = gtk::gio::Application::default() {
            app.withdraw_notification(&format!("chat-{chat_id}"));
        }
        self.notified_chats.borrow_mut().remove(&chat_id);
        crate::tray::set_unread(!self.notified_chats.borrow().is_empty());
    }

    /// Open the chat a clicked notification targets, raising the window first.
    fn activate_chat(&self, chat_id: i64) {
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
    fn on_window_focus(&self) {
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
                } else {
                    // Already read (e.g. on another device): clear any divider
                    // still lingering from that session.
                    ui.dismiss_unread_divider();
                }
            },
        );
    }

    /// Scroll the timeline to `to` after a rebuild, reliably. The content height
    /// settles over several allocation passes, and setting the adjustment during
    /// those passes gets overridden by GtkScrolledWindow. So instead we re-assert
    /// the target on the frame clock (post-layout) until the height stops changing
    /// — which fixes "opens a notch above the last message until you nudge it".
    fn scroll_to(&self, to: ScrollTo) {
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

/// Build the row widgets for a message slice with intra-slice grouping/spacing.
/// Inserts the "new messages" divider before the message whose guid matches
/// `unread_anchor` (if present in this slice). No receipt indicator. Used to
/// prepend an older page or to append new messages; returns the divider widget
/// if it landed here.
///
/// When `prev` is `Some`, the group state is seeded from that message so the
/// first widget in the batch gets the correct spacing relative to its actual
/// predecessor (not a default "new batch" gap). Used by the `Append` path.
#[allow(clippy::too_many_arguments)]
fn build_message_widgets(
    msgs: &[StoredMessage],
    is_group: bool,
    unread_anchor: Option<&str>,
    previews: &std::collections::HashMap<(String, i64), MessageLinkPreview>,
    preview_cards: &Rc<RefCell<std::collections::HashMap<(String, i64), gtk::Widget>>>,
    on_reaction: Option<&Rc<ReactionHandler>>,
    on_edit: Option<&Rc<EditHandler>>,
    reactions: &std::collections::BTreeMap<String, Vec<LiveReactionSummary>>,
    prev: Option<&StoredMessage>,
) -> (Vec<gtk::Widget>, Option<gtk::Widget>, std::collections::HashMap<String, ChipEntry>) {
    let mut out = Vec::with_capacity(msgs.len());
    let mut marker: Option<gtk::Widget> = None;
    let mut chip_map: std::collections::HashMap<String, ChipEntry> = std::collections::HashMap::new();
    let (mut last_key, mut last_date, mut last_from_me) = match prev {
        Some(p) => (Some(group_key(p)), p.date, Some(p.is_from_me)),
        None => (None, 0i64, None),
    };
    for m in msgs {
        if marker.is_none() && unread_anchor == Some(m.guid.as_str()) {
            let mk = unread_marker();
            out.push(mk.clone());
            marker = Some(mk);
            last_key = None;
            last_from_me = None;
        }
        // Skip tapback rows — they render as reaction chips on the target message.
        if m.associated_guid.is_some() {
            continue;
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
        let ctx = MessageContext { m, show_header, top, previews, preview_cards };
        let (row, bubble_or_overlay) = message_widget(ctx, is_group, on_reaction, on_edit, chip.as_ref());
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
    reactions: &std::collections::BTreeMap<String, Vec<LiveReactionSummary>>,
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
        let ctx = MessageContext { m, show_header, top, previews, preview_cards };
        let (row, bubble_or_overlay) = message_widget(ctx, is_group, on_reaction, on_edit, chip.as_ref());
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
    }
    (marker, chip_map)
}

/// A row of small reaction chips overlaid on a message bubble corner. Each chip
/// shows the emoji and, if count > 1, a count. Chips for reactions the current
/// user sent get a distinct visual class.
fn reaction_chips_row(chips: &[LiveReactionSummary]) -> gtk::Widget {
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
fn populate_chips_row(row: &gtk::Box, chips: &[LiveReactionSummary]) {
    while let Some(child) = row.first_child() {
        row.remove(&child);
    }
    for chip in chips {
        let emoji = code_to_emoji(2000 + chip.reaction_index as i64).unwrap_or("?");
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

/// "Read 16:06" if read, else "Delivered" if delivered, else nothing.
fn receipt_status(m: &StoredMessage) -> Option<String> {
    if let Some(d) = m.date_read {
        Some(format!("Read {}", fmt_time(d)))
    } else if m.date_delivered.is_some() {
        Some("Delivered".to_string())
    } else {
        None
    }
}

fn receipt_label(text: &str) -> gtk::Widget {
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
fn unread_marker() -> gtk::Widget {
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

/// Where to land the timeline after (re)populating.
enum ScrollTo {
    Bottom,
    Value(f64),
    Widget(gtk::Widget),
}

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
fn apply_text_scale(w: &impl IsA<gtk::Widget>, base_pt: f64) {
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
fn refresh_chat_text_css() {
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
fn refresh_preview_css() {
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
fn deselect_all_labels() {
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
fn deselect_all_labels_except(active: &gtk::Label) {
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
fn register_selectable_label(label: &gtk::Label) {
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
fn defocus_entry(entry: &gtk::Entry) {
    // Collapse any text selection to a single point at the current cursor.
    // The caret itself is hidden by the focus yank below.
    let pos: i32 = gtk::glib::object::ObjectExt::property(entry, "cursor-position");
    entry.select_region(pos, pos);
    if let Some(root) = entry.root() {
        root.set_focus(None::<&gtk::Widget>);
    }
}

/// Build a small incoming-style chat bubble holding a sample sentence. The
/// text uses [`PREVIEW_CLASS`] so its size is driven by the live CSS rule
/// that `refresh_preview_css` rewrites on every +/- click. Styled to match
/// the real `bubble-in` so the preview is a faithful "what my chats will
/// look like" sample rather than a generic text box.
fn build_preview_bubble() -> gtk::Widget {
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

/// Return the custom avatar path for a chat if one is set and non-empty.
/// Returns `None` for `None`, empty string, or whitespace-only values.
/// Does NOT trim the returned path — leading whitespace is preserved for
/// non-empty paths so that callers receive the raw stored value.
fn chat_avatar_custom_path(c: &ChatSummary) -> Option<&str> {
    c.custom_avatar_path.as_deref().filter(|p| !p.trim().is_empty())
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

/// What should happen to the chat's custom avatar during this edit?
#[allow(dead_code)]
#[derive(Clone)]
pub enum AvatarEdit {
    /// Don't touch the avatar file or the column. Used when the user opened
    /// the dialog and clicked Save without changing the photo.
    NoChange,
    /// Write `bytes` to `avatars_dir/{chat_id}.png` and set the column to the
    /// absolute path. Overwrites any existing file at that path.
    Replace(Vec<u8>),
    /// Delete `avatars_dir/{chat_id}.png` if it exists, and clear the column.
    /// Idempotent: succeeds even if no file was there.
    Remove,
}

/// Apply a chat name/avatar edit: write the avatar file (if changing), then
/// write both `custom_name` and `custom_avatar_path` to the store.
///
/// The avatar file is written BEFORE the store columns, so a successful DB
/// write implies a successful file write. On failure the caller is responsible
/// for cleaning up the file if needed.
#[allow(dead_code)]
pub async fn apply_chat_edit(
    store: &Store,
    chat_id: i64,
    avatars_dir: &Path,
    name: Option<String>,
    avatar: AvatarEdit,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let name = name.filter(|n| !n.trim().is_empty());

    // Ensure the avatars directory exists before any file operation.  The
    // production caller passes `glib::user_data_dir().join("bubbles")
    // .join("avatars")` which may not exist on first run — without this,
    // `std::fs::write` below would fail with ENOENT.  `create_dir_all` is
    // idempotent (succeeds if the directory already exists) and creates
    // any missing parent directories.
    std::fs::create_dir_all(avatars_dir)?;

    match avatar {
        AvatarEdit::Replace(bytes) => {
            let target = avatars_dir.join(format!("{chat_id}.png"));
            // Write atomically: write to a temp file, then rename so a partial
            // write never leaves a half-written file at the target path.
            let tmp = avatars_dir.join(format!("{chat_id}.png.tmp"));
            std::fs::write(&tmp, &bytes)?;
            if let Err(e) = std::fs::rename(&tmp, &target) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
            let abs_path = std::path::absolute(&target)?;
            store.set_chat_custom_name(chat_id, name).await?;
            store
                .set_chat_custom_avatar(chat_id, Some(abs_path.to_string_lossy().into_owned()))
                .await?;
        }
        AvatarEdit::Remove => {
            let target = avatars_dir.join(format!("{chat_id}.png"));
            // Delete the file if it exists; ignore NotFound (idempotent).
            match std::fs::remove_file(&target) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
            store.set_chat_custom_name(chat_id, name).await?;
            store.set_chat_custom_avatar(chat_id, None).await?;
        }
        AvatarEdit::NoChange => {
            store.set_chat_custom_name(chat_id, name).await?;
        }
    }

    Ok(())
}

/// A sidebar row: avatar + chat name + unread badge.
fn chat_row(c: &ChatSummary, handles: &[String]) -> gtk::ListBoxRow {
    let title = chat_title(c, handles);
    let row = gtk::ListBoxRow::new();
    row.add_css_class("navigation-sidebar-row");

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

    // Override with custom avatar photo if one is set and loadable.
    if let Some(path) = chat_avatar_custom_path(c) {
        if let Some(texture) = load_texture(path) {
            avatar.set_custom_image(Some(&texture));
        }
    }

    box_.append(avatar.widget());

    let title_label = gtk::Label::new(Some(&title));
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
    row
}

/// Per-message rendering context shared by [`message_widget`], [`incoming_message`],
/// and [`own_message`]. Bundles the message itself with the timeline-level
/// state (header flag, sticky-to-bottom offset, link-preview maps) so the
/// per-widget extras (`is_group`, `on_reaction`, `chip`) can stay as direct
/// arguments.
struct MessageContext<'a> {
    m: &'a StoredMessage,
    show_header: bool,
    top: i32,
    previews: &'a std::collections::HashMap<(String, i64), MessageLinkPreview>,
    preview_cards: &'a Rc<RefCell<std::collections::HashMap<(String, i64), gtk::Widget>>>,
}

/// One message in the timeline. Incoming messages are grey bubbles on the left
/// (with an avatar, and a sender name in group chats); our own messages are blue
/// bubbles on the right. `previews` is the in-memory map loaded alongside the
/// messages; the renderer reads synchronously from it, so we never hit the
/// store on the GTK main thread. `preview_cards` is the live-widget registry
/// that `refresh_link_card` uses to swap a card in place without rebuilding.
/// Returns `(row_widget, bubble_or_overlay)`.
fn message_widget(
    ctx: MessageContext<'_>,
    is_group: bool,
    on_reaction: Option<&Rc<ReactionHandler>>,
    on_edit: Option<&Rc<EditHandler>>,
    chip: Option<&gtk::Widget>,
) -> (gtk::Widget, Option<gtk::Widget>) {
    if ctx.m.is_from_me {
        own_message(ctx, on_edit, chip)
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
    let MessageContext { m, show_header, top, previews, preview_cards } = ctx;
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
            let avatar = avatar::AvatarWidget::new(28, &sender_display(m));
            avatar.widget().set_valign(gtk::Align::Start);
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
            .label(sender_display(m))
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
    chip: Option<&gtk::Widget>,
) -> (gtk::Widget, Option<gtk::Widget>) {
    let MessageContext { m, show_header, top, previews, preview_cards } = ctx;
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

    let (body_col, bubble_or_overlay) = message_body(m, true, previews, preview_cards, None, show_edit.as_ref(), chip);
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
fn message_body(
    m: &StoredMessage,
    own: bool,
    previews: &std::collections::HashMap<(String, i64), MessageLinkPreview>,
    preview_cards: &Rc<RefCell<std::collections::HashMap<(String, i64), gtk::Widget>>>,
    show_picker: Option<&Rc<dyn Fn()>>,
    show_edit: Option<&Rc<dyn Fn()>>,
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
                    col.append(&image_widget(path));
                }
            }
            AttachmentKind::Video => {
                if let Some(path) = att.local_path.as_deref() {
                    col.append(&video_widget(path));
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
        bubble.append(&bubble_label(&body_text(m), show_picker, show_edit));
        let result = bubble_with_chip(&bubble, own, chip);
        col.append(&result);
        Some(result)
    } else if m.attachments.is_empty() {
        let bubble = bubble_box(own);
        bubble.append(&bubble_label("(no text)", show_picker, show_edit));
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

/// iMessage marks attachment positions in the text stream with U+FFFC; drop it
/// (and surrounding whitespace) so attachment-only messages read as empty.
fn strip_marker(s: &str) -> String {
    s.replace('\u{FFFC}', "").trim().to_string()
}

/// Load a texture from `path`. HEIC/HEIF files are decoded via libheif-rs;
/// all other formats are decoded via gdk-pixbuf, with EXIF orientation applied
/// to the decoded RGBA pixels before wrapping in a `MemoryTexture`.
fn load_texture(path: &str) -> Option<gtk::gdk::Texture> {
    if is_heic_path(path) {
        let decoded = crate::image::decode_heic_to_rgba(std::path::Path::new(path))
            .inspect_err(|e| log::warn!("load_texture: HEIC decode failed for {path}: {e}"))
            .ok()?;
        let w = decoded.width;
        let h = decoded.height;
        let bytes = gtk::glib::Bytes::from_owned(decoded.pixels);
        return Some(gtk::gdk::MemoryTexture::new(
            w as i32,
            h as i32,
            gtk::gdk::MemoryFormat::R8g8b8a8,
            &bytes,
            w as usize * 4,
        )
        .upcast());
    }

    // JPEG (and other non-HEIC) path: decode to RGBA, read EXIF orientation,
    // apply the transform, and wrap in a MemoryTexture.
    let file_bytes = std::fs::read(path)
        .inspect_err(|e| log::warn!("load_texture: read failed for {path}: {e}"))
        .ok()?;
    let orientation = crate::image::read_exif_orientation(&file_bytes).unwrap_or(1);

    // Decode from memory via gdk-pixbuf (handles JPEG, PNG, etc.)
    let loader = gtk::gdk_pixbuf::PixbufLoader::new();
    loader
        .write(&file_bytes)
        .inspect_err(|e| log::warn!("load_texture: pixbuf loader write failed for {path}: {e}"))
        .ok()?;
    loader
        .close()
        .inspect_err(|e| log::warn!("load_texture: pixbuf loader close failed for {path}: {e}"))
        .ok()?;
    let pb = match loader.pixbuf() {
        Some(p) => p,
        None => {
            log::warn!("load_texture: pixbuf loader returned no pixbuf for {path}");
            return None;
        }
    };

    let w = pb.width() as u32;
    let h = pb.height() as u32;
    let nch = pb.n_channels() as usize;
    let stride = pb.rowstride() as usize;
    let src = pb.read_pixel_bytes();
    let src = src.as_ref();

    // Copy to tightly-packed RGBA (strip stride padding)
    let mut pixels = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h as usize {
        let row = &src[y * stride..y * stride + w as usize * nch];
        for px in row.chunks_exact(nch) {
            pixels.push(px[0]);
            pixels.push(px[1]);
            pixels.push(px[2]);
            pixels.push(if nch == 4 { px[3] } else { 0xff });
        }
    }

    let decoded = crate::image::DecodedRgba {
        width: w,
        height: h,
        pixels,
    };
    let oriented = crate::image::apply_orientation(decoded, orientation);

    let w = oriented.width;
    let h = oriented.height;
    let bytes = gtk::glib::Bytes::from_owned(oriented.pixels);
    Some(gtk::gdk::MemoryTexture::new(
        w as i32,
        h as i32,
        gtk::gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        w as usize * 4,
    )
    .upcast())
}

/// Returns `true` when the path has a `.heic` or `.heif` extension
/// (case-insensitive).
fn is_heic_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".heic") || lower.ends_with(".heif")
}

/// An image widget that decodes on a background thread and swaps in the
/// finished texture when ready.  Returns a placeholder immediately so the
/// chat opens without blocking.
fn image_widget(path: &str) -> gtk::Widget {
    const CHAT_THUMBNAIL_MAX_EDGE: u32 = 1024;

    let pic = gtk::Picture::new();
    let (max_w, max_h) = (260.0, 340.0);
    pic.set_size_request(max_w as i32, max_h as i32);
    pic.set_content_fit(gtk::ContentFit::Contain);
    pic.set_overflow(gtk::Overflow::Hidden);
    pic.add_css_class("attachment-image");
    pic.set_cursor_from_name(Some("pointer"));

    // Owned for the 'static decode callback below.
    let path_string = path.to_string();

    // Schedule background decode via the image scheduler.
    let weak = pic.downgrade();
    crate::image::schedule_image_loads(vec![std::path::PathBuf::from(path)], Some(CHAT_THUMBNAIL_MAX_EDGE), {
        move |result| {
            if let Some(pic) = weak.upgrade() {
                match result {
                    Ok(decoded) => {
                        let w = decoded.width as i32;
                        let h = decoded.height as i32;
                        let bytes = glib::Bytes::from_owned(decoded.pixels);
                        let texture = gtk::gdk::MemoryTexture::new(
                            w,
                            h,
                            gtk::gdk::MemoryFormat::R8g8b8a8,
                            &bytes,
                            w as usize * 4,
                        )
                        .upcast::<gtk::gdk::Texture>();
                        pic.set_paintable(Some(&texture));
                        // Re-size based on actual image dimensions, capped.
                        let scale = (max_w / w.max(1) as f64)
                            .min(max_h / h.max(1) as f64)
                            .min(1.0);
                        pic.set_size_request(
                            (w as f64 * scale).round() as i32,
                            (h as f64 * scale).round() as i32,
                        );
                        log::debug!("image thumbnail decoded: {w}x{h} for {path_string}");
                    }
                    Err(e) => log::warn!("image thumbnail decode failed for {path_string}: {e}"),
                }
            }
        }
    });

    // Click to enlarge: find the lightbox host overlay and layer the full image.
    let gesture = gtk::GestureClick::new();
    let path_owned = path.to_string();
    let pic_weak = pic.downgrade();
    gesture.connect_released(move |_, _, _, _| {
        log::debug!("image click handler fired for {path_owned}");
        let Some(pic) = pic_weak.upgrade() else {
            log::debug!("image click: pic_weak.upgrade() returned None for {path_owned}");
            return;
        };
        let Some(host) = find_lightbox_host(pic.upcast_ref()) else {
            log::debug!("image click: find_lightbox_host returned None for {path_owned}");
            return;
        };
        show_lightbox(&host, &path_owned);
    });
    pic.add_controller(gesture);

    pic.upcast()
}

/// A video thumbnail widget that decodes a single frame on a background thread
/// and swaps in the finished texture when ready.  Returns a placeholder with a
/// centered play-button overlay immediately so the chat opens without blocking.
fn video_widget(path: &str) -> gtk::Widget {
    const CHAT_THUMBNAIL_MAX_EDGE: u32 = 1024;

    let pic = gtk::Picture::new();
    let (max_w, max_h) = (260.0, 340.0);
    pic.set_size_request(max_w as i32, max_h as i32);
    pic.set_content_fit(gtk::ContentFit::Contain);
    pic.set_overflow(gtk::Overflow::Hidden);
    pic.add_css_class("attachment-image");
    pic.set_cursor_from_name(Some("pointer"));

    // Play-button overlay on top of the thumbnail.
    let play_icon = gtk::Image::from_icon_name("media-playback-start-symbolic");
    play_icon.set_pixel_size(48);
    play_icon.set_can_target(false);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&pic));
    overlay.add_overlay(&play_icon);
    play_icon.set_halign(gtk::Align::Center);
    play_icon.set_valign(gtk::Align::Center);

    // Schedule background decode via the video scheduler.
    let weak = pic.downgrade();
    // Owned for the 'static decode callback below.
    let path_string = path.to_string();
    crate::video::schedule_video_thumbnails(
        vec![std::path::PathBuf::from(path)],
        CHAT_THUMBNAIL_MAX_EDGE,
        {
            move |result| {
                if let Some(pic) = weak.upgrade() {
                    match result {
                        Ok(decoded) => {
                            let w = decoded.width as i32;
                            let h = decoded.height as i32;
                            let bytes = glib::Bytes::from_owned(decoded.pixels);
                            let texture = gtk::gdk::MemoryTexture::new(
                                w,
                                h,
                                gtk::gdk::MemoryFormat::R8g8b8a8,
                                &bytes,
                                w as usize * 4,
                            )
                            .upcast::<gtk::gdk::Texture>();
                            pic.set_paintable(Some(&texture));
                            // Re-size based on actual image dimensions, capped.
                            let scale = (max_w / w.max(1) as f64)
                                .min(max_h / h.max(1) as f64)
                                .min(1.0);
                            pic.set_size_request(
                                (w as f64 * scale).round() as i32,
                                (h as f64 * scale).round() as i32,
                            );
                            log::debug!("video thumbnail decoded: {w}x{h} for {path_string}");
                        }
                        Err(e) => log::warn!("video thumbnail decode failed for {path_string}: {e}"),
                    }
                }
            }
        },
    );

    // Click to enlarge: find the lightbox host overlay and open the video
    // lightbox.
    let gesture = gtk::GestureClick::new();
    let path_owned = path.to_string();
    let overlay_weak = overlay.downgrade();
    gesture.connect_released(move |_, _, _, _| {
        log::debug!("video click handler fired for {path_owned}");
        let Some(overlay) = overlay_weak.upgrade() else {
            log::debug!("video click: overlay_weak.upgrade() returned None for {path_owned}");
            return;
        };
        let Some(host) = find_lightbox_host(overlay.upcast_ref()) else {
            log::debug!("video click: find_lightbox_host returned None for {path_owned}");
            return;
        };
        show_video_lightbox(&host, &path_owned);
    });
    overlay.add_controller(gesture);

    overlay.upcast()
}

/// Walk up from `w` to the named overlay we wrap the messaging UI in.
fn find_lightbox_host(w: &gtk::Widget) -> Option<gtk::Overlay> {
    let mut cur = w.parent();
    while let Some(p) = cur {
        if p.widget_name().as_str() == "lightbox-host" {
            return p.downcast::<gtk::Overlay>().ok();
        }
        cur = p.parent();
    }
    None
}

/// Layer a dimmed, centered, full-size image over the UI. Click anywhere or
/// press Escape to dismiss.
fn show_lightbox(host: &gtk::Overlay, path: &str) {
    log::debug!("show_lightbox: opening {path}");
    let Some(texture) = load_texture(path) else {
        log::warn!("show_lightbox: load_texture returned None for {path}");
        return;
    };

    let dim = gtk::Box::new(gtk::Orientation::Vertical, 0);
    dim.add_css_class("lightbox-dim");
    dim.set_hexpand(true);
    dim.set_vexpand(true);
    dim.set_focusable(true);

    let pic = gtk::Picture::new();
    pic.set_paintable(Some(&texture));
    pic.set_content_fit(gtk::ContentFit::ScaleDown);
    pic.set_can_shrink(true);
    pic.set_hexpand(true);
    pic.set_vexpand(true);
    pic.set_margin_top(32);
    pic.set_margin_bottom(32);
    pic.set_margin_start(32);
    pic.set_margin_end(32);
    dim.append(&pic);

    // Click anywhere on the dim layer dismisses.
    let click = gtk::GestureClick::new();
    let host_c = host.clone();
    let dim_c = dim.clone();
    click.connect_released(move |_, _, _, _| host_c.remove_overlay(&dim_c));
    dim.add_controller(click);

    // Escape dismisses.
    let keys = gtk::EventControllerKey::new();
    let host_k = host.clone();
    let dim_k = dim.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            host_k.remove_overlay(&dim_k);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    dim.add_controller(keys);

    host.add_overlay(&dim);
    dim.grab_focus();
}

/// Open the fullscreen viewer for a video at `path`. Auto-plays with audio.
/// Click the video to toggle play/pause. Click outside (dim area) or press
/// Escape to dismiss (stopping the audio pipeline).
fn show_video_lightbox(host: &gtk::Overlay, path: &str) {
    use gstreamer as gst;
    use gst::prelude::{ElementExt, ElementExtManual};

    // Initialise gstreamer and register the static gtk4paintablesink plugin
    // exactly once per process (see src/video.rs).
    crate::video::ensure_gst_init();

    // Build a playbin pipeline that auto-demuxes and handles audio.  The
    // video sink is a gtk4paintablesink whose Paintable feeds our Picture.
    let playbin = gst::ElementFactory::make("playbin")
        .name("playbin")
        .build()
        .expect("Failed to create playbin");

    // Simple file:// URI — see note in render(), not worth a url crate dep.
    let uri = format!("file://{}", path);
    playbin.set_property("uri", &uri);

    let video_sink = gst::ElementFactory::make("gtk4paintablesink")
        .name("video-sink")
        .build()
        .expect("Failed to create gtk4paintablesink");
    playbin.set_property("video-sink", &video_sink);

    // Obtain the Paintable that the sink renders into.
    let paintable: gtk::gdk::Paintable = video_sink.property("paintable");

    // --- UI: dim layer + picture + dismissal pattern (mirrors show_lightbox) ---

    let dim = gtk::Box::new(gtk::Orientation::Vertical, 0);
    dim.add_css_class("lightbox-dim");
    dim.set_hexpand(true);
    dim.set_vexpand(true);
    dim.set_focusable(true);

    let pic = gtk::Picture::new();
    pic.set_paintable(Some(&paintable));
    pic.set_content_fit(gtk::ContentFit::ScaleDown);
    pic.set_can_shrink(true);
    pic.set_hexpand(true);
    pic.set_vexpand(true);
    pic.set_margin_top(32);
    pic.set_margin_bottom(32);
    pic.set_margin_start(32);
    pic.set_margin_end(32);
    dim.append(&pic);

    // Auto-play.
    playbin
        .set_state(gst::State::Playing)
        .expect("Failed to start video playback");

    // Click on the video picture toggles play/pause.
    let toggle_gesture = gtk::GestureClick::new();
    toggle_gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
    let pb_toggle = playbin.clone();
    toggle_gesture.connect_released(move |_, _, _, _| {
        let new_state = match pb_toggle.current_state() {
            gst::State::Playing => gst::State::Paused,
            _ => gst::State::Playing,
        };
        let _ = pb_toggle.set_state(new_state);
    });
    pic.add_controller(toggle_gesture);

    // Click on the dim background (outside the picture) dismisses.
    // Default Bubble phase fires for clicks on the dim's empty area;
    // clicks on the picture are caught in Capture phase by toggle_gesture above.
    let dismiss_gesture = gtk::GestureClick::new();
    let host_c = host.clone();
    let dim_c = dim.clone();
    let pb_dismiss = playbin.clone();
    dismiss_gesture.connect_released(move |_, _, _, _| {
        let _ = pb_dismiss.set_state(gst::State::Null);
        host_c.remove_overlay(&dim_c);
    });
    dim.add_controller(dismiss_gesture);

    // Escape key dismisses.
    let keys = gtk::EventControllerKey::new();
    let host_k = host.clone();
    let dim_k = dim.clone();
    let pb_esc = playbin.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            let _ = pb_esc.set_state(gst::State::Null);
            host_k.remove_overlay(&dim_k);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    dim.add_controller(keys);

    host.add_overlay(&dim);
    dim.grab_focus();
}

// --- link preview card ---

/// Best-effort extraction of a host label from a URL for the small "example.com"
/// caption at the bottom of the card. We try to render something readable even
/// when the URL is malformed or uses a non-default scheme.
fn host_caption(url: &str) -> String {
    // Strip the scheme.
    let after_scheme = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url);
    // Drop the path, query, and fragment; keep the host (and optional :port).
    let host_port = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    if host_port.is_empty() {
        url.to_string()
    } else {
        host_port.to_string()
    }
}

/// The sender's preview is sparse (`is_placeholder == true` or both title and
/// summary are empty). Render a compact "loading preview…" state instead of an
/// empty card — it's what the user actually sees while waiting for the fill-in
/// or for a sender that ships only a thumbnail + URL.
fn link_preview_placeholder_card(p: &MessageLinkPreview) -> gtk::Widget {
    let card = gtk::Button::builder()
        .has_frame(false)
        .halign(gtk::Align::Start)
        .build();
    card.add_css_class("link-preview");
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    row.append(&link_preview_thumb(p));
    let text_col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();
    let label = gtk::Label::builder()
        .label("Loading preview…")
        .xalign(0.0)
        .build();
    label.add_css_class("link-preview-placeholder");
    text_col.append(&label);
    if let Some(u) = p.url.as_deref().or(p.original_url.as_deref()) {
        let host = gtk::Label::builder().label(host_caption(u)).xalign(0.0).build();
        host.add_css_class("link-preview-host");
        text_col.append(&host);
    }
    row.append(&text_col);
    card.set_child(Some(&row));
    // Clicking the placeholder opens the URL too (best UX while we wait).
    if let Some(u) = p.url.as_deref().or(p.original_url.as_deref()) {
        let url = u.to_string();
        card.connect_clicked(move |_| open_uri(&url));
        card.set_cursor_from_name(Some("pointer"));
    }
    card.upcast()
}

/// A 72×72 rounded thumbnail, loaded from `image_path` on disk. The
/// thumbnail bytes were just written there by the link-preview ingest, so
/// the synchronous read is fast and fresh. If the cached image can't be
/// decoded (HEIC on a system without gdk-pixbuf HEIC, or the file was
/// deleted), the cell is filled with a neutral chain-link icon.
fn link_preview_thumb(p: &MessageLinkPreview) -> gtk::Widget {
    if let Some(path) = p.image_path.as_deref() {
        if let Ok(texture) = gtk::gdk::Texture::from_filename(path) {
            let pic = gtk::Picture::new();
            pic.set_paintable(Some(&texture));
            // Cover-fit: thumbnail may be a different aspect ratio than the box.
            pic.set_content_fit(gtk::ContentFit::Cover);
            pic.set_size_request(72, 72);
            pic.set_can_shrink(true);
            pic.set_overflow(gtk::Overflow::Hidden);
            pic.add_css_class("link-preview-thumb");
            return pic.upcast();
        }
    }
    // Fallback: neutral chain icon in a rounded box the same size as the thumb.
    let box_ = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    box_.set_size_request(72, 72);
    box_.add_css_class("link-preview-thumb-fallback");
    let icon = gtk::Image::from_icon_name("insert-link-symbolic");
    icon.set_pixel_size(32);
    box_.append(&icon);
    box_.upcast()
}

/// A link-preview card for an inbound `MessageLinkPreview` — the sender's static
/// snapshot, already downloaded. Clicking opens the URL via the system browser.
fn link_preview_card(p: &MessageLinkPreview) -> gtk::Widget {
    // Sparse (placeholder, or title+summary both empty): render the compact
    // loading state, not an empty card shell.
    if p.is_sparse() {
        return link_preview_placeholder_card(p);
    }
    let card = gtk::Button::builder()
        .has_frame(false)
        .halign(gtk::Align::Start)
        .build();
    card.add_css_class("link-preview");

    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .build();
    row.append(&link_preview_thumb(p));

    let text_col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();

    let title_text = p.title.clone().unwrap_or_default();
    if !title_text.is_empty() {
        let title = gtk::Label::builder()
            .label(&title_text)
            .xalign(0.0)
            .max_width_chars(40)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .single_line_mode(true)
            .build();
        title.add_css_class("link-preview-title");
        apply_text_scale(&title, 13.0);
        text_col.append(&title);
    }
    let summary_text = p.summary.clone().unwrap_or_default();
    if !summary_text.is_empty() {
        let summary = gtk::Label::builder()
            .label(&summary_text)
            .xalign(0.0)
            .max_width_chars(60)
            .wrap(true)
            .lines(2)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        summary.add_css_class("link-preview-desc");
        apply_text_scale(&summary, 11.0);
        text_col.append(&summary);
    }
    if let Some(u) = p.url.as_deref().or(p.original_url.as_deref()) {
        let host = gtk::Label::builder()
            .label(host_caption(u))
            .xalign(0.0)
            .build();
        host.add_css_class("link-preview-host");
        apply_text_scale(&host, 10.0);
        text_col.append(&host);
    }
    row.append(&text_col);
    card.set_child(Some(&row));

    // Open the URL when clicked. Use the original URL (what the sender typed)
    // when it differs from the canonical one — that's the link the sender
    // intended the user to follow.
    if let Some(u) = p.original_url.as_deref().or(p.url.as_deref()) {
        let url = u.to_string();
        card.connect_clicked(move |_| open_uri(&url));
        card.set_cursor_from_name(Some("pointer"));
    }

    card.upcast()
}

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
fn wrap_bubble_in_overlay(bubble: &gtk::Widget, chip: &gtk::Widget, own: bool) -> gtk::Widget {
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
fn apply_chip_change(
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

/// A vertical bubble container for the text label. The reaction chip (if any)
/// is layered on top via a `GtkOverlay` in `message_body` — the overlay wraps
/// only the bubble, so the chip is positioned relative to the bubble's bounds
/// (not the whole message row).
fn bubble_box(own: bool) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
    b.add_css_class("bubble");
    b.add_css_class(if own { "bubble-out" } else { "bubble-in" });
    b
}

/// The wrapped, width-capped, left-justified text inside a bubble.
/// URLs in the text are rendered as clickable links that open in the system browser.
fn bubble_label(text: &str, show_picker: Option<&Rc<dyn Fn()>>, show_edit: Option<&Rc<dyn Fn()>>) -> gtk::Label {
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

    // Append "Reaction" and/or "Edit" items to the label's built-in context
    // menu (Copy / Select All / …) when the corresponding callbacks are wired.
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

    if has_any {
        label.set_extra_menu(Some(&menu));
    }

    label
}

/// A small dim timestamp aligned to the bottom of a bubble.
fn time_label(m: &StoredMessage) -> gtk::Label {
    let l = gtk::Label::builder().label(fmt_time(m.date)).build();
    l.add_css_class("dim-label");
    l.add_css_class("caption");
    l.set_valign(gtk::Align::End);
    apply_text_scale(&l, 10.0);
    l
}

// --- scaffolding helpers ---

/// A toolbar-view page: header with `title`, `body` as content, optional bottom
/// bar, and optional widgets packed at the start and end of the header.
fn page(
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

fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(child)
        .build()
}

fn guess_mime(name: &str) -> String {
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
fn parse_uri_list(text: &str) -> Vec<std::path::PathBuf> {
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
                    result.push(std::path::PathBuf::from(s));
                }
            }
        }
    }
    result
}

fn install_css() {
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

// --- formatting helpers ---

/// A timeline row holding the typing bubble, inset to match an incoming message
/// (same left margin, and the 28px avatar-column spacer in group chats).
fn typing_row(is_group: bool) -> gtk::Widget {
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

fn chat_title(c: &ChatSummary, handles: &[String]) -> String {
    // A user-set name wins over everything.
    if let Some(n) = &c.custom_name {
        if !n.trim().is_empty() {
            return n.clone();
        }
    }
    if let Some(n) = &c.display_name {
        if !n.is_empty() {
            return n.clone();
        }
    }
    let is_me = |p: &str| handles.iter().any(|h| h.as_str().eq_ignore_ascii_case(p));
    let others: Vec<String> = c
        .participants
        .iter()
        .filter(|p| !is_me(p.as_str()))
        .map(|p| pretty_addr(p))
        .collect();
    if !others.is_empty() {
        return others.join(", ");
    }
    // Note-to-self (only our own handle) or empty: show what we have.
    let all: Vec<String> = c.participants.iter().map(|p| pretty_addr(p)).collect();
    if all.is_empty() {
        c.key.clone()
    } else {
        all.join(", ")
    }
}

fn sender_display(m: &StoredMessage) -> String {
    if m.is_from_me {
        "You".to_string()
    } else {
        m.sender
            .as_deref()
            .map(pretty_addr)
            .unwrap_or_else(|| "Unknown".to_string())
    }
}

/// An iMessage-style guid (uppercased UUID v4) for optimistic local inserts.
fn new_guid() -> String {
    glib::uuid_string_random().to_string().to_uppercase()
}

/// Unix epoch milliseconds, matching the backend's message timestamps.
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Build a `(ChatRef, IncomingMessage)` for the "start a new chat" path.
///
/// Normalises the recipient string into a `mailto:` or `tel:` URI and returns
/// an outbound message ready for `Ingest::Message`.
/// Normalise a raw recipient string into a `mailto:` or `tel:` URI (or `None` if invalid).
fn normalize_recipient(recipient: &str) -> Option<String> {
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

fn new_chat_payload(
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
    };

    Some((msg.chat.clone(), msg))
}

/// Normalise a raw phone string into a `tel:` URI (or `None` if invalid).
fn normalize_phone(raw: &str) -> Option<String> {
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

fn group_key(m: &StoredMessage) -> String {
    if m.is_from_me {
        "\0me".to_string()
    } else {
        m.sender.clone().unwrap_or_default()
    }
}

fn body_text(m: &StoredMessage) -> String {
    match (&m.text, &m.associated_guid) {
        (Some(t), _) if !strip_marker(t).is_empty() => strip_marker(t),
        // Tapback rows are rendered as reaction chips on the target message;
        // suppress the placeholder bubble body.
        (_, Some(_)) => String::new(),
        _ => "(no text)".to_string(),
    }
}

fn fmt_time(ms: i64) -> String {
    crate::time_format::format_time(ms, crate::time_format::get())
}

fn pretty_addr(a: &str) -> String {
    a.strip_prefix("mailto:")
        .or_else(|| a.strip_prefix("tel:"))
        .unwrap_or(a)
        .to_string()
}

/// Our own address within a conversation, used as the sender for outbound items.
fn self_handle(participants: &[String], handles: &[String]) -> Option<String> {
    participants
        .iter()
        .find(|p| {
            handles
                .iter()
                .any(|h| h.as_str().eq_ignore_ascii_case(p.as_str()))
        })
        .cloned()
}

fn chat_ref_of(c: &ChatSummary) -> ChatRef {
    ChatRef {
        participants: c.participants.clone(),
        display_name: c.display_name.clone(),
        service: c.service.clone(),
    }
}

fn clear(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

fn clear_box(b: &gtk::Box) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}

/// Extract the wire-level `ams` text for a reaction from a stored message.
/// Prefers the message body text, falls back to the first attachment's
/// filename, and returns `""` when both are absent.
fn extract_target_text(m: &StoredMessage) -> String {
    m.text
        .clone()
        .filter(|t| !t.is_empty())
        .or_else(|| m.attachments.first().and_then(|a| a.name.clone()))
        .unwrap_or_default()
}

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
    // The function is pure (no GTK, no I/O, no side effects) — the format is:
    //   `format!($".<class> {{ font-size: {:.2}pt; }}", base_pt + offset)`
    //
    // Expected red (compile error): `super::chat_text_css` does not exist yet.
    // The worker must add `pub(crate) fn chat_text_css(base_pt: f64) -> String`
    // in this module.

    #[test]
    fn chat_text_css_single_base_reflects_offset() {
        let tmp = tempfile::tempdir().unwrap();
        crate::text_scale::set_data_dir_for_tests(tmp.path().to_path_buf());

        // Reset to default offset.
        crate::text_scale::set(0.0);

        let css = super::chat_text_css(13.0);
        assert!(
            css.contains("font-size: 13.00pt"),
            "at offset 0.0, expected 13.00pt, got: {css}"
        );

        // Change the offset — the function must reflect the new size.
        crate::text_scale::set(2.0);
        let css = super::chat_text_css(13.0);
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

        let css10 = super::chat_text_css(10.0);
        let css12 = super::chat_text_css(12.0);
        let css13 = super::chat_text_css(13.0);
        assert!(css10.contains("font-size: 10.00pt"), "base 10 at offset 0");
        assert!(css12.contains("font-size: 12.00pt"), "base 12 at offset 0");
        assert!(css13.contains("font-size: 13.00pt"), "base 13 at offset 0");

        // Change offset — all bases reflect the change.
        crate::text_scale::set(1.5);
        let css10 = super::chat_text_css(10.0);
        let css12 = super::chat_text_css(12.0);
        let css13 = super::chat_text_css(13.0);
        assert!(css10.contains("font-size: 11.50pt"), "base 10 after +1.5");
        assert!(css12.contains("font-size: 13.50pt"), "base 12 after +1.5");
        assert!(css13.contains("font-size: 14.50pt"), "base 13 after +1.5");
    }
}

/// A single entry in the Apple tapback reaction table.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
struct ReactionEntry {
    emoji: &'static str,
    label: &'static str,
}

/// The 6 standard Apple tapback reactions, indexed 0..=5.
/// Add codes are 2000 + index; remove codes are 3000 + index.
#[allow(dead_code)]
const REACTIONS: [ReactionEntry; 6] = [
    ReactionEntry { emoji: "\u{2764}\u{FE0F}",  label: "Loved" },
    ReactionEntry { emoji: "\u{1F44D}\u{FE0F}", label: "Liked" },
    ReactionEntry { emoji: "\u{1F44E}\u{FE0F}", label: "Disliked" },
    ReactionEntry { emoji: "\u{1F604}\u{FE0F}", label: "Laughed at" },
    ReactionEntry { emoji: "\u{203C}\u{FE0F}",  label: "Emphasized" },
    ReactionEntry { emoji: "\u{2753}\u{FE0F}",  label: "Questioned" },
];

/// Look up the emoji string for an Apple tapback code.
/// Accepts both add (2000..=2005) and remove (3000..=3005) codes.
#[allow(dead_code)]
fn code_to_emoji(code: i64) -> Option<&'static str> {
    let idx = match code {
        2000..=2005 => code - 2000,
        3000..=3005 => code - 3000,
        _ => return None,
    };
    Some(REACTIONS[idx as usize].emoji)
}

/// Look up the friendly label for an Apple tapback code.
/// Accepts both add (2000..=2005) and remove (3000..=3005) codes.
#[allow(dead_code)]
fn code_to_label(code: i64) -> Option<&'static str> {
    let idx = match code {
        2000..=2005 => code - 2000,
        3000..=3005 => code - 3000,
        _ => return None,
    };
    Some(REACTIONS[idx as usize].label)
}

#[cfg(test)]
mod reaction_tests {
    use super::*;

    #[test]
    fn reaction_table() {
        // Apple code 2000 + index is "add reaction"; 3000 + index is "remove reaction".
        // Each entry: (add_code, emoji_str, label). The emoji string always
        // carries the U+FE0F variation selector — required for ‼ (U+203C) and
        // ❓ (U+2753) to render as emoji rather than text, and conventional
        // for the other four.
        let add_expected: [(i64, &str, &str); 6] = [
            (2000, "\u{2764}\u{FE0F}",  "Loved"),       // heart + VS
            (2001, "\u{1F44D}\u{FE0F}", "Liked"),       // thumbs up + VS
            (2002, "\u{1F44E}\u{FE0F}", "Disliked"),    // thumbs down + VS
            (2003, "\u{1F604}\u{FE0F}", "Laughed at"),  // smile/laugh + VS
            (2004, "\u{203C}\u{FE0F}",  "Emphasized"),  // double exclamation + VS (required)
            (2005, "\u{2753}\u{FE0F}",  "Questioned"),  // question mark + VS (required)
        ];

        // 1. Lookup of add codes (2000-2005) returns the correct emoji and a non-empty label.
        for (code, emoji, expected_label) in add_expected.iter().copied() {
            assert_eq!(
                code_to_emoji(code),
                Some(emoji),
                "code_to_emoji({}) should return {:?}",
                code,
                emoji,
            );
            let label = code_to_label(code)
                .unwrap_or_else(|| panic!("code_to_label({}) returned None", code));
            assert!(
                !label.is_empty(),
                "code_to_label({}) returned an empty label",
                code,
            );
            assert_eq!(
                label, expected_label,
                "code_to_label({}) mismatch",
                code,
            );
        }

        // 2. Lookup of remove codes (3000-3005) returns the SAME emoji as the
        //    corresponding add code, and a non-empty label. This pins the
        //    2000/3000 unification behavior.
        for (add_code, emoji, _) in add_expected.iter().copied() {
            let remove_code = add_code + 1000;
            assert_eq!(
                code_to_emoji(remove_code),
                Some(emoji),
                "code_to_emoji({}) should match the add-code emoji {:?}",
                remove_code,
                emoji,
            );
            let label = code_to_label(remove_code)
                .unwrap_or_else(|| panic!("code_to_label({}) returned None", remove_code));
            assert!(
                !label.is_empty(),
                "code_to_label({}) returned an empty label",
                remove_code,
            );
        }

        // 3. Out-of-range codes return None (both helpers must reject them).
        for &bad in &[1999i64, 2006, 2999, 3006, 0, 9999] {
            assert_eq!(
                code_to_emoji(bad),
                None,
                "code_to_emoji({}) should be None",
                bad,
            );
            assert_eq!(
                code_to_label(bad),
                None,
                "code_to_label({}) should be None",
                bad,
            );
        }

        // 4. All 6 labels are distinct.
        let labels: Vec<&str> = add_expected.iter().map(|(_, _, l)| *l).collect();
        let mut sorted = labels.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            6,
            "all 6 reaction labels should be distinct, got: {:?}",
            labels,
        );
    }
}

// ── chat-update plan types and stub ──────────────────────────────────

/// What to do with the chat-bubble container on the next refresh.
///
/// The caller (the GTK refresh path) inspects this and applies the minimal
/// update needed instead of rebuilding the entire bubble list from scratch.
#[derive(Debug)]
pub enum ChatUpdatePlan {
    Noop,
    UpdateReceipt { new_text: String },
    UpdateChips { changes: Vec<ChipChange> },
    /// A single bubble's text changed (an edit). The UI updates the label
    /// in place without rebuilding the view.
    EditText { guid: String, new_text: String },
    Append {
        new_tail: Vec<StoredMessage>,
        receipt: ReceiptAction,
    },
    Rebuild,
}

/// A single reaction-chip update for one target message.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ChipChange {
    pub target_guid: String,
    pub new_chips: Vec<LiveReactionSummary>,
}

/// What to do with the existing receipt label underneath the last sent
/// message.
#[derive(Debug)]
pub enum ReceiptAction {
    Keep,
    Set(String),
    Remove,
}

/// Decide what display update is needed given the previously-rendered state
/// and the new message list from the DB.
///
/// This is the pure decision function that lets the chat view avoid a full
/// rebuild when messages are merely appended or receipts change.  The caller
/// (the GTK refresh path) acts on the returned action.
pub fn plan_chat_update(
    prev_guids: &[String],
    prev_receipt: Option<&str>,
    prev_reactions: &std::collections::BTreeMap<String, Vec<LiveReactionSummary>>,
    prev_text: &std::collections::HashMap<String, String>,
    new_msgs: &[StoredMessage],
    new_reactions: &std::collections::BTreeMap<String, Vec<LiveReactionSummary>>,
) -> ChatUpdatePlan {
    // 1. Compute the non-tapback guid set from new_msgs.
    let new_guids: Vec<String> = new_msgs
        .iter()
        .filter(|m| m.associated_guid.is_none())
        .map(|m| m.guid.clone())
        .collect();

    // 2. Compute desired new receipt state.
    let new_receipt = compute_receipt_state(new_msgs);

    // 3. Compute chip changes.
    let chip_changes = compute_chip_changes(prev_reactions, new_reactions);

    let prev_len = prev_guids.len();

    // 4. Decision tree.
    if prev_guids.is_empty() && !new_guids.is_empty() {
        return ChatUpdatePlan::Rebuild;
    }

    if new_guids == prev_guids {
        // Same set of non-tapback guids → in-place update possible.
        let receipt_changed = new_receipt.as_deref() != prev_receipt;
        let chips_changed = !chip_changes.is_empty();
        // Build new_text map for the non-tapback rows.
        let new_text: std::collections::HashMap<String, String> = new_msgs
            .iter()
            .filter(|m| m.associated_guid.is_none())
            .filter_map(|m| m.text.as_ref().map(|t| (m.guid.clone(), t.clone())))
            .collect();
        // Find the first guid whose text changed (single EditText per plan;
        // further changes are picked up on the next refresh).
        let text_change: Option<(String, String)> = prev_text
            .iter()
            .find_map(|(guid, old_text)| {
                new_text.get(guid).and_then(|new_t| {
                    if new_t != old_text {
                        Some((guid.clone(), new_t.clone()))
                    } else {
                        None
                    }
                })
            });
        let text_changed = text_change.is_some();
        match (receipt_changed, chips_changed, text_changed) {
            (false, false, false) => ChatUpdatePlan::Noop,
            (true, false, false) => match new_receipt {
                Some(text) => ChatUpdatePlan::UpdateReceipt { new_text: text },
                None => ChatUpdatePlan::Rebuild,
            },
            (false, true, false) => ChatUpdatePlan::UpdateChips {
                changes: chip_changes,
            },
            (false, false, true) => {
                let (guid, new_text) = text_change.unwrap();
                ChatUpdatePlan::EditText { guid, new_text }
            }
            // Any other combination → multiple changes, fall through to Rebuild.
            _ => ChatUpdatePlan::Rebuild,
        }
    } else if new_guids.len() > prev_len
        && new_guids[..prev_len]
            .iter()
            .zip(prev_guids.iter())
            .all(|(a, b)| a == b)
    {
        // Strict extension at the end: new_guids starts with prev_guids and
        // has more items.  Chip changes are IGNORED per spec (documented
        // limitation — the chip update will be picked up on the next refresh).
        let new_tail: Vec<StoredMessage> = new_msgs
            .iter()
            .filter(|m| m.associated_guid.is_none())
            .skip(prev_len)
            .cloned()
            .collect();

        let receipt = match (prev_receipt, new_receipt.as_deref()) {
            (Some(_), None) => ReceiptAction::Remove,
            (None, Some(text)) => ReceiptAction::Set(text.to_string()),
            (Some(old), Some(new)) if old != new => ReceiptAction::Set(new.to_string()),
            _ => ReceiptAction::Keep,
        };

        ChatUpdatePlan::Append { new_tail, receipt }
    } else {
        ChatUpdatePlan::Rebuild
    }
}

/// Compare two reaction-chip maps and produce a list of changes.
///
/// An entry in `new` that is absent from `prev` (or has different chips) is a
/// change with the new chips.  An entry present only in `prev` is a removal
/// (empty chips).  The order of the returned vector is unspecified — callers
/// sort it when asserting.
fn compute_chip_changes(
    prev: &std::collections::BTreeMap<String, Vec<LiveReactionSummary>>,
    new: &std::collections::BTreeMap<String, Vec<LiveReactionSummary>>,
) -> Vec<ChipChange> {
    let mut changes = Vec::new();
    // Check for new or changed chips.
    for (guid, new_chips) in new {
        let prev_chips = prev.get(guid);
        if prev_chips != Some(new_chips) {
            changes.push(ChipChange {
                target_guid: guid.clone(),
                new_chips: new_chips.clone(),
            });
        }
    }
    // Check for removed chips (target guid no longer in new).
    for guid in prev.keys() {
        if !new.contains_key(guid) {
            changes.push(ChipChange {
                target_guid: guid.clone(),
                new_chips: vec![],
            });
        }
    }
    changes
}

/// Compute the desired receipt state from the full message list (including
/// any trailing tapback rows). Mirrors the logic in `populate_messages`.
fn compute_receipt_state(msgs: &[StoredMessage]) -> Option<String> {
    let last_sent_idx = msgs
        .iter()
        .rposition(|m| m.is_from_me && m.associated_guid.is_none())?;
    let m = &msgs[last_sent_idx];
    if let Some(text) = receipt_status(m) {
        return Some(text);
    }
    // No real receipt yet. Placeholder only if the last sent is the very
    // last message in the list (including any trailing tapbacks).
    if last_sent_idx == msgs.len() - 1 {
        Some("\u{200b}".to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod extract_target_text_tests {
    //! Pins the pure helper that picks the wire-level `ams` (target text) for
    //! a reaction. The iPhone uses `ams` to render the reaction chip in the
    //! chat list, and `amk` (`p:N/{guid}`) to attach the chip to the right
    //! target message part. The helper must prefer the message's own text,
    //! fall back to the first attachment's filename, and produce `""` only
    //! when both are missing.
    use super::*;

    /// Build a `StoredMessage` with only the fields the helper inspects set.
    /// All other fields get the values a fresh, unstored message would have
    /// (zero ids, `None` for optionals, empty strings, no is_sticker).
    fn message_with(
        text: Option<&str>,
        attachments: Vec<StoredAttachment>,
    ) -> StoredMessage {
        StoredMessage {
            id: 0,
            guid: String::new(),
            chat_id: 0,
            sender: None,
            is_from_me: false,
            text: text.map(str::to_string),
            subject: None,
            service: None,
            date: 0,
            date_delivered: None,
            date_read: None,
            effect: None,
            reply_to_guid: None,
            reply_part: None,
            associated_guid: None,
            associated_type: None,
            item_type: 0,
            send_error: None,
            attachments,
        }
    }

    /// Build a `StoredAttachment` with only `name` (and the `is_sticker`
    /// default `false`) set. The other fields are irrelevant to the helper.
    fn attachment(name: Option<&str>) -> StoredAttachment {
        StoredAttachment {
            mime: None,
            name: name.map(str::to_string),
            local_path: None,
            is_sticker: false,
        }
    }

    #[test]
    fn extract_target_text_returns_text_when_no_attachments() {
        let m = message_with(Some("Hello world"), vec![]);
        assert_eq!(extract_target_text(&m), "Hello world");
    }

    #[test]
    fn extract_target_text_prefers_text_over_attachment_name() {
        // The message has a caption AND a filename; the helper should pick
        // the caption (what the sender actually wrote) over the filename
        // (the system-supplied attachment name).
        let m = message_with(Some("Check this out"), vec![attachment(Some("photo.jpg"))]);
        assert_eq!(extract_target_text(&m), "Check this out");
    }

    #[test]
    fn extract_target_text_falls_back_to_attachment_name_when_text_is_none() {
        // A media-only message (no caption) — the helper must still produce
        // a non-empty `ams` for the iPhone by using the attachment's
        // filename, so the reaction chip has something to display.
        let m = message_with(None, vec![attachment(Some("photo.jpg"))]);
        assert_eq!(extract_target_text(&m), "photo.jpg");
    }

    #[test]
    fn extract_target_text_falls_back_to_attachment_name_when_text_is_empty() {
        // An empty caption is semantically the same as no caption — must
        // NOT be returned as a one-character-or-zero-length `ams`. Falls
        // through to the filename so the chip renders something.
        let m = message_with(Some(""), vec![attachment(Some("photo.jpg"))]);
        assert_eq!(extract_target_text(&m), "photo.jpg");
    }

    #[test]
    fn extract_target_text_returns_empty_when_text_none_and_attachment_has_no_name() {
        // Last resort before the empty fallback: media with no caption and
        // no filename. Returning `""` matches the pre-fix behavior, so the
        // iPhone still gets a valid (if content-less) `ams` field rather
        // than a missing one.
        let m = message_with(None, vec![attachment(None)]);
        assert_eq!(extract_target_text(&m), "");
    }

    #[test]
    fn extract_target_text_returns_empty_when_no_text_and_no_attachments() {
        let m = message_with(None, vec![]);
        assert_eq!(extract_target_text(&m), "");
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

#[cfg(test)]
mod plan_chat_update_tests {
    //! Pins the behaviour of [`super::plan_chat_update`] — the pure decision
    //! function that compares previously-rendered state against the new message
    //! list and returns one of four actions so the GTK side can avoid a full
    //! rebuild.
    //!
    //! All tests construct their own fixtures and call
    //! [`super::plan_chat_update`] directly.  No GTK initialisation needed.
    use super::*;
    use std::collections::BTreeMap;
    use std::collections::HashMap;

    // ── test helpers ────────────────────────────────────────────────

    /// Minimum `StoredMessage` with the identity-relevant fields set to
    /// something useful; everything else zero / `None`.
    fn m(guid: &str, is_from_me: bool, date: i64) -> StoredMessage {
        StoredMessage {
            id: 0,
            guid: guid.to_string(),
            chat_id: 0,
            sender: None,
            is_from_me,
            text: None,
            subject: None,
            service: None,
            date,
            date_delivered: None,
            date_read: None,
            effect: None,
            reply_to_guid: None,
            reply_part: None,
            associated_guid: None,
            associated_type: None,
            item_type: 0,
            send_error: None,
            attachments: vec![],
        }
    }

    fn delivered(mut m: StoredMessage, date: i64) -> StoredMessage {
        m.date_delivered = Some(date);
        m
    }

    fn read(mut m: StoredMessage, date: i64) -> StoredMessage {
        m.date_read = Some(date);
        m
    }

    fn tapback(mut m: StoredMessage, target: &str) -> StoredMessage {
        m.associated_guid = Some(target.to_string());
        m
    }

    /// Shorthand to build a `Vec<String>` from string slices.
    fn guids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// Empty reaction map shorthand.
    fn no_reactions() -> BTreeMap<String, Vec<LiveReactionSummary>> {
        BTreeMap::new()
    }

    /// Empty text map shorthand.
    fn no_text() -> HashMap<String, String> {
        HashMap::new()
    }

    /// Like `m` but with explicit text.
    fn m_text(guid: &str, is_from_me: bool, date: i64, text: &str) -> StoredMessage {
        StoredMessage {
            text: Some(text.to_string()),
            ..m(guid, is_from_me, date)
        }
    }

    // ── tests ──────────────────────────────────────────────────────

    // ── assertion helpers ──────────────────────────────────────────

    /// Assert the result is `Noop`.
    fn assert_noop(result: ChatUpdatePlan) {
        assert!(matches!(result, ChatUpdatePlan::Noop), "expected Noop, got {result:?}");
    }

    /// Assert the result is `Rebuild`.
    fn assert_rebuild(result: ChatUpdatePlan) {
        assert!(matches!(result, ChatUpdatePlan::Rebuild), "expected Rebuild, got {result:?}");
    }

    /// Assert the result is `UpdateReceipt` with exactly `expected_text`.
    fn assert_update_receipt(result: ChatUpdatePlan, expected_text: &str) {
        match result {
            ChatUpdatePlan::UpdateReceipt { new_text } => {
                assert_eq!(new_text, expected_text, "UpdateReceipt text mismatch");
            }
            other => panic!("expected UpdateReceipt, got {other:?}"),
        }
    }

    /// Assert the result is `Append` with the given tail guids and
    /// `ReceiptAction::Keep`.
    fn assert_append_keep(result: ChatUpdatePlan, expected_tail_guids: &[&str]) {
        match result {
            ChatUpdatePlan::Append { new_tail, receipt } => {
                let tail_guids: Vec<&str> =
                    new_tail.iter().map(|m| m.guid.as_str()).collect();
                assert_eq!(tail_guids, expected_tail_guids, "Append tail guids mismatch");
                assert!(
                    matches!(receipt, ReceiptAction::Keep),
                    "expected Keep receipt, got {receipt:?}",
                );
            }
            other => panic!("expected Append, got {other:?}"),
        }
    }

    /// Assert the result is `Append` with the given tail guids and
    /// `ReceiptAction::Set(expected)`.
    fn assert_append_set(result: ChatUpdatePlan, expected_tail_guids: &[&str], expected_text: &str) {
        match result {
            ChatUpdatePlan::Append { new_tail, receipt } => {
                let tail_guids: Vec<&str> =
                    new_tail.iter().map(|m| m.guid.as_str()).collect();
                assert_eq!(tail_guids, expected_tail_guids, "Append tail guids mismatch");
                match receipt {
                    ReceiptAction::Set(text) => {
                        assert_eq!(text, expected_text, "Append Set receipt text mismatch");
                    }
                    other => panic!("expected Set({expected_text:?}), got {other:?}"),
                }
            }
            other => panic!("expected Append, got {other:?}"),
        }
    }

    /// Assert the result is `Append` with the given tail guids and
    /// `ReceiptAction::Remove`.
    fn assert_append_remove(result: ChatUpdatePlan, expected_tail_guids: &[&str]) {
        match result {
            ChatUpdatePlan::Append { new_tail, receipt } => {
                let tail_guids: Vec<&str> =
                    new_tail.iter().map(|m| m.guid.as_str()).collect();
                assert_eq!(tail_guids, expected_tail_guids, "Append tail guids mismatch");
                assert!(
                    matches!(receipt, ReceiptAction::Remove),
                    "expected Remove receipt, got {receipt:?}",
                );
            }
            other => panic!("expected Append, got {other:?}"),
        }
    }

    // ── individual tests ───────────────────────────────────────────

    #[test]
    fn plan_chat_update_noop_when_guids_and_receipt_unchanged() {
        let prev = guids(&["A", "B"]);
        let new = vec![
            m("A", false, 1000),
            delivered(m("B", true, 2000), 3000),
        ];
        assert_noop(plan_chat_update(&prev, Some("Delivered"), &no_reactions(), &no_text(), &new, &no_reactions()));
    }

    #[test]
    fn plan_chat_update_update_receipt_from_none_to_delivered() {
        let prev = guids(&["A", "B"]);
        let new = vec![
            m("A", false, 1000),
            delivered(m("B", true, 2000), 3000),
        ];
        // prev_receipt was the zero-width placeholder (sent message at bottom
        // with no real receipt yet).
        assert_update_receipt(
            plan_chat_update(&prev, Some("\u{200b}"), &no_reactions(), &no_text(), &new, &no_reactions()),
            "Delivered",
        );
    }

    #[test]
    fn plan_chat_update_update_receipt_delivered_to_read() {
        let prev = guids(&["A", "B"]);
        let new = vec![
            m("A", false, 1000),
            read(delivered(m("B", true, 2000), 3000), 4000),
        ];
        match plan_chat_update(&prev, Some("Delivered"), &no_reactions(), &no_text(), &new, &no_reactions()) {
            ChatUpdatePlan::UpdateReceipt { new_text } => {
                assert!(
                    new_text.starts_with("Read "),
                    "expected Read …, got {new_text:?}",
                );
                assert!(!new_text.is_empty(), "Read text must not be empty");
            }
            other => panic!("expected UpdateReceipt, got {other:?}"),
        }
    }

    #[test]
    fn plan_chat_update_append_incoming_message_keeps_receipt_when_last_sent_unchanged() {
        let prev = guids(&["A", "B"]);
        let new = vec![
            m("A", false, 1000),
            delivered(m("B", true, 2000), 3000),
            m("C", false, 4000),
        ];
        assert_append_keep(
            plan_chat_update(&prev, Some("Delivered"), &no_reactions(), &no_text(), &new, &no_reactions()),
            &["C"],
        );
    }

    #[test]
    fn plan_chat_update_append_sent_message_adds_placeholder() {
        let prev = guids(&["A"]);
        let new = vec![m("A", false, 1000), m("B", true, 2000)];
        assert_append_set(
            plan_chat_update(&prev, None, &no_reactions(), &no_text(), &new, &no_reactions()),
            &["B"],
            "\u{200b}",
        );
    }

    #[test]
    fn plan_chat_update_append_removes_placeholder_when_new_incoming_after_last_sent() {
        let prev = guids(&["A", "B"]);
        let new = vec![
            m("A", false, 1000),
            m("B", true, 2000),  // last sent, no real receipt, was at end → placeholder
            m("C", false, 3000), // new incoming after last sent
        ];
        assert_append_remove(
            plan_chat_update(&prev, Some("\u{200b}"), &no_reactions(), &no_text(), &new, &no_reactions()),
            &["C"],
        );
    }

    #[test]
    fn plan_chat_update_append_multiple_new_messages() {
        let prev = guids(&["A"]);
        let new = vec![
            m("A", false, 1000),
            m("B", false, 2000),
            m("C", false, 3000),
            m("D", false, 4000),
        ];
        assert_append_keep(
            plan_chat_update(&prev, None, &no_reactions(), &no_text(), &new, &no_reactions()),
            &["B", "C", "D"],
        );
    }

    #[test]
    fn plan_chat_update_rebuild_on_deletion() {
        let prev = guids(&["A", "B", "C"]);
        let new = vec![m("A", false, 1000), m("C", false, 3000)];
        assert_rebuild(plan_chat_update(&prev, None, &no_reactions(), &no_text(), &new, &no_reactions()));
    }

    #[test]
    fn plan_chat_update_rebuild_on_reorder() {
        let prev = guids(&["A", "B"]);
        let new = vec![m("B", false, 2000), m("A", false, 1000)];
        assert_rebuild(plan_chat_update(&prev, None, &no_reactions(), &no_text(), &new, &no_reactions()));
    }

    #[test]
    fn plan_chat_update_edit_returns_edit_text() {
        // The plan function now compares text in addition to guids/receipt. A
        // message body change (edit) returns EditText when no other state
        // changed. The UI then updates that one bubble's label in place.
        let prev = guids(&["A", "B", "C"]);
        let prev_text: HashMap<String, String> = [
            ("A", "old A"),
            ("B", "old B"),
            ("C", "old C"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let new = vec![
            m("A", false, 1000),
            // B's text changed (edit)
            m_text("B", false, 2000, "new B"),
            m("C", false, 3000),
        ];
        match plan_chat_update(&prev, None, &no_reactions(), &prev_text, &new, &no_reactions()) {
            ChatUpdatePlan::EditText { guid, new_text } => {
                assert_eq!(guid, "B", "EditText targets the changed message");
                assert_eq!(new_text, "new B", "EditText carries the new text");
            }
            other => panic!("expected EditText, got {other:?}"),
        }
    }

    #[test]
    fn plan_chat_update_no_text_change_is_noop() {
        // Even with prev_text supplied, no actual text change returns Noop.
        let prev = guids(&["A", "B"]);
        let prev_text: HashMap<String, String> = [
            ("A", "same A"),
            ("B", "same B"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let new = vec![
            m_text("A", false, 1000, "same A"),
            m_text("B", false, 2000, "same B"),
        ];
        assert_noop(plan_chat_update(&prev, None, &no_reactions(), &prev_text, &new, &no_reactions()));
    }

    #[test]
    fn plan_chat_update_rebuild_when_prev_guids_empty_with_messages() {
        let prev: Vec<String> = vec![];
        let new = vec![m("A", false, 1000), m("B", false, 2000)];
        assert_rebuild(plan_chat_update(&prev, None, &no_reactions(), &no_text(), &new, &no_reactions()));
    }

    #[test]
    fn plan_chat_update_noop_when_new_list_has_only_extra_tapback_rows() {
        // Tapback rows (associated_guid.is_some()) are filtered out of the
        // guid set.  Adding only tapbacks leaves the non-tapback guid set
        // unchanged, so the plan is Noop.  The reaction chips themselves are
        // not detected by this function — they will be stale until the next
        // real refresh, which is acceptable for the send-flash fix.
        let prev = guids(&["A", "B"]);
        let new = vec![
            delivered(m("A", false, 1000), 0),
            delivered(m("B", true, 2000), 3000),
            tapback(m("T1", false, 1500), "A"),
        ];
        assert_noop(plan_chat_update(&prev, Some("Delivered"), &no_reactions(), &no_text(), &new, &no_reactions()));
    }

    #[test]
    fn plan_chat_update_append_tapback_does_not_show_in_tail() {
        // A tapback row in new_msgs does not count as a non-tapback message,
        // so it must not appear in Append's new_tail.
        let prev = guids(&["A"]);
        let new = vec![
            m("A", false, 1000),
            m("B", false, 2000),
            tapback(m("T1", false, 1500), "A"),
        ];
        assert_append_keep(
            plan_chat_update(&prev, None, &no_reactions(), &no_text(), &new, &no_reactions()),
            &["B"],
        );
    }

    // ── reaction helpers ──────────────────────────────────────────────

    /// Build a single `LiveReactionSummary`.
    fn chip(index: u8, count: usize, my: bool) -> LiveReactionSummary {
        LiveReactionSummary {
            reaction_index: index,
            count,
            my_reacted: my,
        }
    }

    /// Build a reaction map from a slice of (guid, chips) pairs.
    fn rmap(pairs: &[(&str, Vec<LiveReactionSummary>)]) -> BTreeMap<String, Vec<LiveReactionSummary>> {
        pairs.iter().map(|(g, c)| (g.to_string(), c.clone())).collect()
    }

    /// Assert the result is `UpdateChips` with the given changes (order-insensitive).
    fn assert_update_chips(result: ChatUpdatePlan, expected: Vec<ChipChange>) {
        match result {
            ChatUpdatePlan::UpdateChips { mut changes } => {
                changes.sort_by(|a, b| a.target_guid.cmp(&b.target_guid));
                let mut expected = expected;
                expected.sort_by(|a, b| a.target_guid.cmp(&b.target_guid));
                assert_eq!(changes, expected, "UpdateChips changes mismatch");
            }
            other => panic!("expected UpdateChips, got {other:?}"),
        }
    }

    // ── reaction chip tests ───────────────────────────────────────────

    #[test]
    fn plan_chat_update_update_chips_only_when_reactions_change() {
        // prev has reactions empty, new has a reaction on A — chips differ.
        let prev = guids(&["A", "B"]);
        let new = vec![
            m("A", false, 1000),
            m("B", false, 2000),
        ];
        let prev_r = no_reactions();
        let new_r = rmap(&[("A", vec![chip(0, 1, false)])]);
        assert_update_chips(
            plan_chat_update(&prev, None, &prev_r, &no_text(), &new, &new_r),
            vec![ChipChange {
                target_guid: "A".to_string(),
                new_chips: vec![chip(0, 1, false)],
            }],
        );
    }

    #[test]
    fn plan_chat_update_update_chips_when_existing_chip_gains_reaction() {
        let prev = guids(&["A"]);
        let new = vec![m("A", false, 1000)];
        let prev_r = rmap(&[("A", vec![chip(0, 1, false)])]);
        let new_r = rmap(&[("A", vec![chip(0, 1, false), chip(1, 1, false)])]);
        assert_update_chips(
            plan_chat_update(&prev, None, &prev_r, &no_text(), &new, &new_r),
            vec![ChipChange {
                target_guid: "A".to_string(),
                new_chips: vec![chip(0, 1, false), chip(1, 1, false)],
            }],
        );
    }

    #[test]
    fn plan_chat_update_update_chips_when_reaction_removed() {
        let prev = guids(&["A"]);
        let new = vec![m("A", false, 1000)];
        let prev_r = rmap(&[("A", vec![chip(0, 1, false)])]);
        // Key is absent in new_reactions — should treat as empty.
        let new_r = no_reactions();
        assert_update_chips(
            plan_chat_update(&prev, None, &prev_r, &no_text(), &new, &new_r),
            vec![ChipChange {
                target_guid: "A".to_string(),
                new_chips: vec![],
            }],
        );
    }

    #[test]
    fn plan_chat_update_noop_when_reactions_unchanged() {
        let prev = guids(&["A"]);
        let new = vec![m("A", false, 1000)];
        let r = rmap(&[("A", vec![chip(0, 1, false)])]);
        assert_noop(plan_chat_update(&prev, None, &r, &no_text(), &new, &r));
    }

    #[test]
    fn plan_chat_update_update_chips_multiple_targets() {
        let prev = guids(&["A", "B"]);
        let new = vec![
            m("A", false, 1000),
            m("B", false, 2000),
        ];
        let prev_r = rmap(&[("A", vec![chip(0, 1, false)])]);
        let new_r = rmap(&[("A", vec![chip(0, 1, false)]), ("B", vec![chip(1, 2, true)])]);
        // Only B changed (A is the same).
        assert_update_chips(
            plan_chat_update(&prev, None, &prev_r, &no_text(), &new, &new_r),
            vec![ChipChange {
                target_guid: "B".to_string(),
                new_chips: vec![chip(1, 2, true)],
            }],
        );
    }

    #[test]
    fn plan_chat_update_rebuild_when_both_receipt_and_chips_change() {
        // Guids unchanged, but both receipt (Delivered→Read) and chips (added
        // laugh) changed.  The safe fallback is Rebuild.
        let prev = guids(&["A", "B"]);
        let new = vec![
            m("A", false, 1000),
            read(delivered(m("B", true, 2000), 3000), 4000),
        ];
        let prev_r = rmap(&[("A", vec![chip(0, 1, false)])]);
        let new_r = rmap(&[("A", vec![chip(0, 1, false), chip(3, 1, false)])]);
        assert_rebuild(plan_chat_update(
            &prev,
            Some("Delivered"),
            &prev_r,
            &no_text(),
            &new,
            &new_r,
        ));
    }

    #[test]
    fn plan_chat_update_append_ignores_chip_changes() {
        // A new message B arrives.  Even though chips changed on A, the plan
        // should return Append (not Rebuild, not UpdateChips).  Chip changes
        // in the Append case are ignored per spec — they will be picked up on
        // the next refresh.
        let prev = guids(&["A"]);
        let new = vec![
            m("A", false, 1000),
            m("B", false, 2000),
        ];
        let prev_r = rmap(&[("A", vec![chip(0, 1, false)])]);
        let new_r = rmap(&[("A", vec![chip(0, 1, false), chip(1, 1, false)])]);
        assert_append_keep(
            plan_chat_update(&prev, None, &prev_r, &no_text(), &new, &new_r),
            &["B"],
        );
    }
} // mod plan_chat_update_tests

#[cfg(test)]
mod avatar_save_tests {
    use super::*;
    use tempfile::TempDir;

    /// Ingest a 1:1 message to create a chat and return its id.
    async fn ingest_chat(store: &Store) -> i64 {
        store
            .apply(Ingest::Message(IncomingMessage {
                guid: "avatar-save-test".into(),
                chat: ChatRef {
                    participants: vec![
                        "mailto:alice@example.com".into(),
                        "mailto:bob@example.com".into(),
                    ],
                    display_name: None,
                    service: Some("iMessage".into()),
                },
                sender: Some("mailto:bob@example.com".into()),
                is_from_me: false,
                text: Some("Hello".into()),
                date: 1000,
                ..Default::default()
            }))
            .await
            .unwrap();
        store.chats().await.unwrap().remove(0).id
    }

    // ── test 1: Replace writes both file and DB columns ────────────────────

    #[tokio::test]
    async fn apply_chat_edit_writes_avatar_file_and_db_columns() {
        let store = Store::open_in_memory().await.unwrap();
        let chat_id = ingest_chat(&store).await;
        let tmp = TempDir::new().unwrap();

        let result = apply_chat_edit(
            &store,
            chat_id,
            tmp.path(),
            Some("New Name".into()),
            AvatarEdit::Replace(vec![1, 2, 3, 4]),
        )
        .await;

        assert!(result.is_ok());

        let expected_path = tmp.path().join(format!("{chat_id}.png"));
        assert!(expected_path.exists(), "avatar file should exist on disk");
        let written = std::fs::read(&expected_path).unwrap();
        assert_eq!(written, vec![1, 2, 3, 4], "file content should match the bytes passed to Replace");

        let chats = store.chats().await.unwrap();
        let chat = chats.into_iter().find(|c| c.id == chat_id).unwrap();
        assert_eq!(chat.custom_name.as_deref(), Some("New Name"));
        assert_eq!(
            chat.custom_avatar_path.as_deref(),
            Some(expected_path.to_str().unwrap()),
            "DB custom_avatar_path should be the absolute path to the file"
        );
    }

    // ── test 2: Remove clears DB column and deletes file ──────────────────

    #[tokio::test]
    async fn apply_chat_edit_remove_clears_db_and_deletes_file() {
        let store = Store::open_in_memory().await.unwrap();
        let chat_id = ingest_chat(&store).await;
        let tmp = TempDir::new().unwrap();

        // Write an avatar file and set the DB column as pre-condition.
        let avatar_path = tmp.path().join(format!("{chat_id}.png"));
        std::fs::write(&avatar_path, vec![1, 2, 3, 4]).unwrap();
        store
            .set_chat_custom_avatar(chat_id, Some(avatar_path.to_str().unwrap().into()))
            .await
            .unwrap();
        store
            .set_chat_custom_name(chat_id, Some("Original".into()))
            .await
            .unwrap();

        let result = apply_chat_edit(&store, chat_id, tmp.path(), None, AvatarEdit::Remove).await;

        assert!(result.is_ok());
        assert!(!avatar_path.exists(), "Remove should delete the avatar file");

        let chats = store.chats().await.unwrap();
        let chat = chats.into_iter().find(|c| c.id == chat_id).unwrap();
        assert!(
            chat.custom_avatar_path.is_none(),
            "Remove should clear custom_avatar_path in the DB"
        );
        assert!(
            chat.custom_name.is_none(),
            "name=None should clear custom_name"
        );
    }

    // ── test 3: Remove is idempotent when no file exists ──────────────────

    #[tokio::test]
    async fn apply_chat_edit_remove_idempotent_when_no_file() {
        let store = Store::open_in_memory().await.unwrap();
        let chat_id = ingest_chat(&store).await;
        let tmp = TempDir::new().unwrap();

        // No avatar file exists at all — Remove should still succeed.
        let result =
            apply_chat_edit(&store, chat_id, tmp.path(), Some("Name".into()), AvatarEdit::Remove)
                .await;

        assert!(result.is_ok(), "Remove should succeed even when no file exists");

        let chats = store.chats().await.unwrap();
        let chat = chats.into_iter().find(|c| c.id == chat_id).unwrap();
        assert_eq!(chat.custom_name.as_deref(), Some("Name"));
        assert!(
            chat.custom_avatar_path.is_none(),
            "custom_avatar_path should remain None"
        );
    }

    // ── test 4: Replace overwrites an existing file ───────────────────────

    #[tokio::test]
    async fn apply_chat_edit_replace_overwrites_existing_file() {
        let store = Store::open_in_memory().await.unwrap();
        let chat_id = ingest_chat(&store).await;
        let tmp = TempDir::new().unwrap();

        // Write old content first.
        let avatar_path = tmp.path().join(format!("{chat_id}.png"));
        std::fs::write(&avatar_path, vec![10, 20, 30, 40]).unwrap();

        let result = apply_chat_edit(
            &store,
            chat_id,
            tmp.path(),
            None,
            AvatarEdit::Replace(vec![1, 2, 3, 4]),
        )
        .await;

        assert!(result.is_ok());

        let written = std::fs::read(&avatar_path).unwrap();
        assert_eq!(
            written,
            vec![1, 2, 3, 4],
            "Replace should overwrite file with new bytes"
        );

        let chats = store.chats().await.unwrap();
        let chat = chats.into_iter().find(|c| c.id == chat_id).unwrap();
        assert_eq!(
            chat.custom_avatar_path.as_deref(),
            Some(avatar_path.to_str().unwrap()),
            "Replace should set the DB column to the absolute path"
        );
    }

    // ── test 5: NoChange leaves everything as-is ──────────────────────────

    #[tokio::test]
    async fn apply_chat_edit_no_change_does_not_touch_file_or_db() {
        let store = Store::open_in_memory().await.unwrap();
        let chat_id = ingest_chat(&store).await;
        let tmp = TempDir::new().unwrap();

        // Pre-set both columns via the store (no file needed).
        store
            .set_chat_custom_name(chat_id, Some("Original".into()))
            .await
            .unwrap();
        store
            .set_chat_custom_avatar(chat_id, Some("/path/to/old.png".into()))
            .await
            .unwrap();

        let result = apply_chat_edit(
            &store,
            chat_id,
            tmp.path(),
            Some("Original".into()),
            AvatarEdit::NoChange,
        )
        .await;

        assert!(result.is_ok());

        let chats = store.chats().await.unwrap();
        let chat = chats.into_iter().find(|c| c.id == chat_id).unwrap();
        assert_eq!(
            chat.custom_name.as_deref(),
            Some("Original"),
            "NoChange should not alter custom_name"
        );
        assert_eq!(
            chat.custom_avatar_path.as_deref(),
            Some("/path/to/old.png"),
            "NoChange should not alter custom_avatar_path"
        );
    }

    // ── test 6: Whitespace-only name normalizes to None ───────────────────

    #[tokio::test]
    async fn apply_chat_edit_empty_name_normalizes_to_none() {
        let store = Store::open_in_memory().await.unwrap();
        let chat_id = ingest_chat(&store).await;
        let tmp = TempDir::new().unwrap();

        let result = apply_chat_edit(
            &store,
            chat_id,
            tmp.path(),
            Some("   ".into()),
            AvatarEdit::NoChange,
        )
        .await;

        assert!(result.is_ok());

        let chats = store.chats().await.unwrap();
        let chat = chats.into_iter().find(|c| c.id == chat_id).unwrap();
        assert!(
            chat.custom_name.is_none(),
            "whitespace-only name should normalize to None"
        );
    }

    // ── test 7: Replace with valid PNG bytes round-trips through decode ──

    #[tokio::test]
    async fn apply_chat_edit_with_replace_bytes_writes_a_valid_png() {
        let store = Store::open_in_memory().await.unwrap();
        let chat_id = ingest_chat(&store).await;
        let tmp = TempDir::new().unwrap();

        // Build a 2×2 RGBA image and save it to a temp file via save_png.
        let src = crate::image::DecodedRgba {
            width: 2,
            height: 2,
            pixels: vec![
                255, 0, 0, 255,    // (0,0) red
                0, 255, 0, 255,    // (1,0) green
                0, 0, 255, 255,    // (0,1) blue
                128, 128, 128, 128, // (1,1) semi-transparent grey
            ],
        };
        let png_temp = TempDir::new().unwrap();
        let png_path = png_temp.path().join("seed.png");
        crate::image::save_png(&src, &png_path).unwrap();
        let png_bytes = std::fs::read(&png_path).unwrap();

        let result = apply_chat_edit(
            &store,
            chat_id,
            tmp.path(),
            None,
            AvatarEdit::Replace(png_bytes),
        )
        .await;
        assert!(result.is_ok());

        let avatar_path = tmp.path().join(format!("{chat_id}.png"));
        let decoded = crate::image::decode_image_rgba(&avatar_path, None)
            .expect("apply_chat_edit should write a decodable PNG");
        assert_eq!(decoded.width, src.width, "width should round-trip");
        assert_eq!(decoded.height, src.height, "height should round-trip");
        assert_eq!(decoded.pixels, src.pixels, "pixels should round-trip through apply_chat_edit");
    }

    // ── test 8: Replace auto-creates avatars_dir if missing ───────────────

    #[tokio::test]
    async fn apply_chat_edit_creates_avatars_dir_if_missing() {
        let store = Store::open_in_memory().await.unwrap();
        let chat_id = ingest_chat(&store).await;
        let tmp = TempDir::new().unwrap();

        let avatars_dir = tmp.path().join("avatars");
        assert!(
            !avatars_dir.exists(),
            "avatars subdirectory should not exist before the call"
        );

        let result = apply_chat_edit(
            &store,
            chat_id,
            &avatars_dir,
            None,
            AvatarEdit::Replace(vec![1, 2, 3, 4]),
        )
        .await;

        assert!(result.is_ok(), "apply_chat_edit should create avatars_dir and succeed");

        assert!(
            avatars_dir.exists(),
            "avatars subdirectory should have been auto-created"
        );

        let avatar_path = avatars_dir.join(format!("{chat_id}.png"));
        assert!(avatar_path.exists(), "avatar file should exist");
        let written = std::fs::read(&avatar_path).unwrap();
        assert_eq!(written, vec![1, 2, 3, 4], "file content should match the bytes passed to Replace");
    }
}

/// Build an `adw::AlertDialog` for prompting the user for their iCloud
/// account password (the "device password" used by
/// `Backend::setup_keychain_clique`). The dialog is pre-populated with
/// the expected title, body, and a password entry. The caller is
/// responsible for presenting the dialog (`dialog.present(Some(parent))`)
/// and wiring the response handling.
///
/// The dialog has:
/// - Title: "iCloud Account Password"
/// - Body: "Enter your iCloud account password to set up iCloud Keychain
///   on this device. This is required to sync iMessage history from
///   iCloud."
/// - A password entry widget (`gtk::PasswordEntry`)
/// - "Cancel" button (response id: "cancel")
/// - "Set Up" button (response id: "suggested", default appearance)
#[allow(dead_code)]
pub fn build_clique_password_dialog(_parent: Option<&adw::Window>) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::new(
        Some("iCloud Account Password"),
        Some("Enter your iCloud account password to set up iCloud Keychain on this device. This is required to sync iMessage history from iCloud."),
    );

    let password_entry = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .build();
    password_entry.set_placeholder_text(Some("Password"));

    dialog.set_extra_child(Some(&password_entry));
    dialog.add_responses(&[("cancel", "Cancel"), ("suggested", "Set Up")]);
    dialog.set_response_appearance("suggested", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("suggested"));
    dialog.set_close_response("cancel");

    dialog
}

/// Build a closure suitable for the `password_prompt` parameter of
/// `orchestrate_sync_now_flow`. The closure, when called, presents the
/// iCloud Keychain password dialog, waits for the user's response, and
/// returns the entered password (or `None` if the user cancelled).
///
/// `parent` is the parent window the dialog should be modal to.
///
/// The closure is `FnOnce` and synchronous — when called from a thread
/// where blocking is acceptable (e.g., a tokio worker thread spawned
/// via `crate::runtime::runtime().spawn`), it presents the dialog on
/// the GTK main thread and blocks the calling thread until the user
/// responds. This relies on glib's main-loop iteration during the
/// blocking wait to drive the response callback.
///
/// # Thread safety
///
/// The returned closure is `Send` (captures no non-Send types) so it
/// can be moved into a tokio task spawned via `runtime().spawn()`.
/// Inside, it uses `glib::MainContext::default().invoke` to schedule
/// the dialog presentation on the GTK main thread (this is the
/// thread-safe way to do GTK work from a non-GTK thread — `spawn_local`
/// panics with "already acquired by another thread" because the main
/// context is owned by the GTK main thread), and then blocks the
/// calling tokio worker thread on `rx.blocking_recv()` until the
/// response callback fires on the main thread.
#[allow(dead_code)]
pub fn build_password_prompt_closure(
    _parent: Option<&adw::Window>,
) -> impl FnOnce() -> Option<String> {
    // Discard the parent reference to avoid capturing a non-Send type.
    // The dialog is presented without an explicit parent window.
    let _ = _parent;
    move || {
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<String>>();
        // `glib::MainContext::default().invoke(...)` is the cross-thread way
        // to run GTK code from a tokio worker. It posts a sync closure to
        // the main context and blocks the calling thread until the main
        // thread has executed it. The closure is just "present the
        // dialog" — non-blocking from the main thread's perspective.
        // The user response comes later via the dialog's response signal,
        // which the main thread drives through its main loop while our
        // tokio thread blocks on `rx.blocking_recv()`.
        glib::MainContext::default().invoke(move || {
            let dialog = build_clique_password_dialog(None);
            // Wrap `tx` in a `RefCell<Option<...>>` so it can be taken
            // once inside an `Fn` closure (signal handlers are `Fn`).
            let tx = std::cell::RefCell::new(Some(tx));
            dialog.connect_response(None, move |dialog, response_id| {
                let password = if response_id == "suggested" {
                    dialog
                        .extra_child()
                        .and_downcast::<gtk::PasswordEntry>()
                        .map(|entry| entry.text().to_string())
                } else {
                    None
                };
                if let Some(tx) = tx.borrow_mut().take() {
                    let _ = tx.send(password);
                }
            });
            dialog.present(None::<&gtk::Window>);
        });
        rx.blocking_recv().ok().flatten()
    }
}

#[cfg(test)]
mod clique_password_dialog_tests {
    use super::*;

    /// Compile-time signature check: `build_clique_password_dialog` must exist
    /// with the expected signature. GTK widget instantiation requires a display,
    /// so this test only verifies the function is callable with the right types.
    #[test]
    fn build_clique_password_dialog_exists() {
        let _: fn(Option<&adw::Window>) -> adw::AlertDialog = build_clique_password_dialog;
    }

    /// Compile-time signature check: `build_password_prompt_closure` must exist
    /// with the expected signature. GTK dialog interaction requires a display,
    /// so this test only verifies the function is callable with the right types
    /// and returns a closure matching the `FnOnce() -> Option<String>` contract
    /// required by `orchestrate_sync_now_flow`.
    #[test]
    fn build_password_prompt_closure_exists() {
        fn _assert_prompt_type(_: impl FnOnce() -> Option<String>) {}
        let closure = build_password_prompt_closure(None);
        _assert_prompt_type(closure);
    }
}

