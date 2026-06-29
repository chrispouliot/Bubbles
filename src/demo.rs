//! A self-contained demo sandbox for exercising the messaging UI without any
//! network or onboarding. Enabled with `BUBBLES_DEMO=1`.
//!
//! It opens an in-memory store, seeds a handful of chats (including one with 50
//! unread, to exercise pagination and the "earlier unread" pill), and hands the
//! messaging UI the no-op [`StubBackend`]. Sends go through the optimistic path
//! and simply land in the in-memory store — nothing leaves the machine, and
//! nothing persists between runs.

use std::sync::Arc;

use crate::protocol::stub::StubBackend;
use crate::protocol::{Backend, Connection, ImClient};
use crate::runtime;
use crate::store::{ChatRef, IncomingMessage, Ingest, Receipt, Store};

/// Our own handle in the demo. Excluded from chat titles; shown as "You".
const ME: &str = "mailto:me@demo.test";
const ALICE: &str = "mailto:Alice";
const BOB: &str = "mailto:Bob";
const CAROL: &str = "mailto:Carol";
const NEWS: &str = "mailto:News Bot";

/// Minutes between messages within a chat.
const STEP: i64 = 60_000;

/// Build a window that drops straight into a seeded, offline messaging UI.
pub fn build_demo_window(app: &adw::Application) -> adw::ApplicationWindow {
    let store = runtime::runtime().block_on(async {
        let store = Store::open_in_memory()
            .await
            .expect("open in-memory demo store");
        seed(&store).await;
        store
    });

    let backend: Arc<dyn Backend> = Arc::new(StubBackend::default());
    let connection = Connection::new(());
    let client = ImClient::new(());
    let handles = vec![ME.to_string()];

    let nav = adw::NavigationView::new();
    crate::ui::enter_messaging(&nav, &backend, store, connection, client, handles);

    adw::ApplicationWindow::builder()
        .application(app)
        .title("Bubbles — Demo")
        .default_width(920)
        .default_height(640)
        .content(&nav)
        .build()
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

async fn seed(store: &Store) {
    let now = now_ms();
    let hour = 3_600_000i64;

    // --- News feed: 5 read + 50 unread = 55 total; the headline test case for
    //     pagination + the "earlier unread" pill. Newest chat, so it's on top.
    let mut news: Vec<(bool, &str, String)> = Vec::with_capacity(55);
    for i in 1..=55 {
        news.push((false, NEWS, format!("News item #{i} — lorem ipsum dolor sit amet.")));
    }
    let news: Vec<(bool, &str, &str)> =
        news.iter().map(|(m, s, t)| (*m, *s, t.as_str())).collect();
    seed_chat(store, "news", &[NEWS], None, now - STEP, &news, 5, false).await;

    // --- Group chat with one unread; exercises sender names + group avatars.
    let trip: &[(bool, &str, &str)] = &[
        (false, ALICE, "Who's driving Saturday?"),
        (true, ME, "I can drive if we leave by 9"),
        (false, BOB, "Works for me"),
        (false, CAROL, "I'll bring snacks 🍫"),
        (true, ME, "Perfect, see you all then"),
        (false, ALICE, "Actually can we make it 9:30?"),
        (false, BOB, "+1 for 9:30"),
    ];
    seed_chat(
        store,
        "trip",
        &[ALICE, BOB, CAROL],
        Some("Weekend Trip"),
        now - 2 * hour,
        trip,
        6,
        false,
    )
    .await;

    // --- 1:1 with two unread at the tail; first-unread lands inside the initial
    //     page, so the divider shows inline on open (no pill).
    let alice: &[(bool, &str, &str)] = &[
        (false, ALICE, "Hey! Are we still on for lunch?"),
        (true, ME, "Yes! 12:30 at the usual spot?"),
        (false, ALICE, "Perfect"),
        (true, ME, "Great, see you there"),
        (false, ALICE, "Running 5 min late, sorry!"),
        (true, ME, "No worries 🙂"),
        (false, ALICE, "Here now, grabbing a table"),
        (false, ALICE, "Got the window seat"),
    ];
    seed_chat(store, "alice", &[ALICE], None, now - 5 * hour, alice, 6, false).await;

    // --- 1:1 fully read, ending on our own message with a Read receipt.
    let bob: &[(bool, &str, &str)] = &[
        (false, BOB, "Did you see the game last night?"),
        (true, ME, "Yeah, what a finish"),
        (false, BOB, "Unreal. Same time next week?"),
        (true, ME, "I'm in"),
        (false, BOB, "👍"),
        (true, ME, "Bringing the good snacks this time"),
    ];
    seed_chat(store, "bob", &[BOB], None, now - 26 * hour, bob, 6, true).await;
}

/// Seed one chat. `msgs` is `(from_me, sender_handle, text)` ordered oldest→newest;
/// `last_at` is the final message's timestamp. The first `read_count` messages
/// are marked read; the rest stay unread. If `read_receipt_last_sent`, the most
/// recent outgoing message gets a Read receipt so the UI shows "Read HH:MM".
#[allow(clippy::too_many_arguments)] // Demo helper, all 4 call sites in this file pass positionally.
async fn seed_chat(
    store: &Store,
    tag: &str,
    others: &[&str],
    display: Option<&str>,
    last_at: i64,
    msgs: &[(bool, &str, &str)],
    read_count: usize,
    read_receipt_last_sent: bool,
) {
    let mut participants: Vec<String> = others.iter().map(|s| s.to_string()).collect();
    participants.push(ME.to_string());
    let chat = ChatRef {
        participants: participants.clone(),
        display_name: display.map(|s| s.to_string()),
        service: Some("iMessage".into()),
    };

    let n = msgs.len() as i64;
    let base = last_at - (n - 1).max(0) * STEP;
    let mut last_sent: Option<(String, i64)> = None;

    for (i, (from_me, sender, text)) in msgs.iter().enumerate() {
        let date = base + i as i64 * STEP;
        let guid = format!("demo-{tag}-{i}");
        let im = IncomingMessage {
            guid: guid.clone(),
            chat: chat.clone(),
            sender: Some(if *from_me { ME.to_string() } else { sender.to_string() }),
            is_from_me: *from_me,
            text: Some(text.to_string()),
            service: Some("iMessage".into()),
            date,
            ..Default::default()
        };
        let _ = store.apply(Ingest::Message(im)).await;
        if *from_me {
            last_sent = Some((guid, date));
        }
    }

    // Resolve the chat id by its normalized key to mark the read run.
    let key = chat.key();
    let chats = store.chats().await.unwrap_or_default();
    if let Some(c) = chats.iter().find(|c| c.key == key) {
        if read_count > 0 {
            let cutoff = base + (read_count as i64 - 1) * STEP;
            let _ = store.mark_read_through(c.id, cutoff).await;
        }
    }

    if read_receipt_last_sent {
        if let Some((guid, date)) = last_sent {
            let _ = store
                .apply(Ingest::Receipt(Receipt::Read { guid, date: date + 30_000 }))
                .await;
        }
    }
}
