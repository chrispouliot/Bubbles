//! SQLite-backed message store.
//!
//! All logic lives in the sync `*_blocking` / `query_*` functions over a
//! `rusqlite::Connection`; they are unit-tested directly (see `#[cfg(test)]`).
//! The async [`Store`] is a thin `tokio-rusqlite` wrapper used by the app, so
//! DB work happens off the GTK thread and bridges back the same way the rest of
//! the app does.
//!
//! Design choices come straight from the receive spike:
//!   * messages fan out to multiple destinations, so inserts dedupe on `guid`;
//!   * read/delivered receipts reuse the target's guid and carry no chat, so
//!     they're UPDATEs, not inserts;
//!   * a chat is identified by its sorted participant set (self included).

mod model;
pub use model::*;

use std::path::Path;

use anyhow::Result;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};

const DDL: &str = "
CREATE TABLE handle(
  id      INTEGER PRIMARY KEY,
  address TEXT UNIQUE NOT NULL,
  is_me   INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE chat(
  id                INTEGER PRIMARY KEY,
  key               TEXT UNIQUE NOT NULL,
  display_name      TEXT,
  is_group          INTEGER NOT NULL DEFAULT 0,
  service           TEXT,
  last_message_date INTEGER
);
CREATE TABLE chat_participant(
  chat_id   INTEGER NOT NULL REFERENCES chat(id)   ON DELETE CASCADE,
  handle_id INTEGER NOT NULL REFERENCES handle(id) ON DELETE CASCADE,
  PRIMARY KEY(chat_id, handle_id)
);
CREATE TABLE message(
  id               INTEGER PRIMARY KEY,
  guid             TEXT UNIQUE NOT NULL,
  chat_id          INTEGER NOT NULL REFERENCES chat(id) ON DELETE CASCADE,
  sender_handle_id INTEGER REFERENCES handle(id),
  is_from_me       INTEGER NOT NULL DEFAULT 0,
  text             TEXT,
  subject          TEXT,
  service          TEXT,
  date             INTEGER NOT NULL,
  date_delivered   INTEGER,
  date_read        INTEGER,
  effect           TEXT,
  reply_to_guid    TEXT,
  reply_part       TEXT,
  associated_guid  TEXT,
  associated_type  INTEGER,
  item_type        INTEGER NOT NULL DEFAULT 0,
  error            INTEGER
);
CREATE INDEX idx_message_chat_date ON message(chat_id, date);
CREATE INDEX idx_message_assoc     ON message(associated_guid);
CREATE TABLE attachment(
  id            INTEGER PRIMARY KEY,
  message_id    INTEGER NOT NULL REFERENCES message(id) ON DELETE CASCADE,
  guid          TEXT,
  mime_type     TEXT,
  transfer_name TEXT,
  total_bytes   INTEGER,
  local_path    TEXT,
  part_index    INTEGER,
  is_sticker    INTEGER NOT NULL DEFAULT 0
);
";

// Added in v4: sender-generated link previews (iMessage rich links). The
// thumbnail is stored as a file on disk under `$XDG_CACHE_HOME/.../previews/`,
// and this table holds the path plus the textual metadata.
//
// `link_preview` is the URL-keyed cache the fetcher writes to. The table
// ships in the same migration even though nothing populates it today, so the
// schema bump is a single user_version step.
const DDL_V4: &str = "
CREATE TABLE message_link_preview(
  message_guid   TEXT NOT NULL,
  part_idx       INTEGER NOT NULL,
  url            TEXT,
  original_url   TEXT,
  title          TEXT,
  summary        TEXT,
  image_path     TEXT,
  image_width    INTEGER,
  image_height   INTEGER,
  is_placeholder INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(message_guid, part_idx)
);
CREATE INDEX idx_message_link_preview_msg
  ON message_link_preview(message_guid);
CREATE TABLE link_preview(
  url          TEXT PRIMARY KEY,
  title        TEXT,
  description  TEXT,
  site_name    TEXT,
  image_path   TEXT,
  fetched_at   INTEGER NOT NULL,
  status       INTEGER NOT NULL,
  error        TEXT
);
";

// Added in v7: contact cache (address-book lookup for participant display names
// and avatars). `contact_address` rows cascade on contact delete and are
// replaced atomically during upsert.
const DDL_V7: &str = "
CREATE TABLE contact(
    uid           TEXT PRIMARY KEY,
    display_name  TEXT NOT NULL,
    avatar        BLOB
);
CREATE TABLE contact_address(
    contact_uid   TEXT NOT NULL REFERENCES contact(uid) ON DELETE CASCADE,
    value         TEXT NOT NULL,
    kind          TEXT NOT NULL,
    PRIMARY KEY(contact_uid, value)
);
CREATE INDEX idx_contact_address_value ON contact_address(value);
";

// --- sync core (all logic; unit-tested) ---

/// Apply pending migrations and enable FK enforcement on this connection.
pub fn migrate(c: &Connection) -> rusqlite::Result<()> {
    c.pragma_update(None, "foreign_keys", true)?;
    let mut v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v < 1 {
        c.execute_batch(DDL)?;
        v = 1;
    }
    if v < 2 {
        // Track which inbound messages we've already sent a read receipt for.
        c.execute_batch("ALTER TABLE message ADD COLUMN read_sent INTEGER NOT NULL DEFAULT 0;")?;
        v = 2;
    }
    if v < 3 {
        // A user-set name for a conversation, overriding the derived title. Kept
        // separate from `display_name` (Apple's group name), which the chat
        // upsert overwrites; this one is local and survives sync.
        c.execute_batch("ALTER TABLE chat ADD COLUMN custom_name TEXT;")?;
        v = 3;
    }
    if v < 4 {
        // Sender-generated link previews and the URL-keyed cache the
        // fetcher writes to. Both ship in this migration so the schema bump
        // is a single user_version step.
        c.execute_batch(DDL_V4)?;
        v = 4;
    }
    if v < 5 {
        // User-set custom avatar path, a local override that survives chat upsert
        // (like custom_name). Kept separate from any derived avatar.
        c.execute_batch("ALTER TABLE chat ADD COLUMN custom_avatar_path TEXT;")?;
        v = 5;
    }
    if v < 6 {
        // Pending/in-flight send state: tracks messages that haven't been
        // confirmed sent by the server yet.
        c.execute_batch("ALTER TABLE message ADD COLUMN pending INTEGER NOT NULL DEFAULT 0;")?;
        v = 6;
    }
    if v < 7 {
        // Contact cache: address-book lookup for participant display names
        // and avatars.
        c.execute_batch(DDL_V7)?;
        v = 7;
    }
    c.pragma_update(None, "user_version", v)?;
    Ok(())
}

/// The most recent inbound message in `chat_id` we have not yet acknowledged
/// with a read receipt, as `(guid, date)`.
pub fn latest_unread_incoming(c: &Connection, chat_id: i64) -> rusqlite::Result<Option<(String, i64)>> {
    c.query_row(
        "SELECT guid, date FROM message
         WHERE chat_id = ?1 AND is_from_me = 0 AND read_sent = 0
         ORDER BY date DESC, id DESC LIMIT 1",
        params![chat_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// The earliest unacknowledged inbound message — the boundary where the unread
/// run begins. Used to place the "new messages" marker and scroll to it.
pub fn first_unread_incoming(c: &Connection, chat_id: i64) -> rusqlite::Result<Option<(String, i64)>> {
    c.query_row(
        "SELECT guid, date FROM message
         WHERE chat_id = ?1 AND is_from_me = 0 AND read_sent = 0
         ORDER BY date ASC, id ASC LIMIT 1",
        params![chat_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// Mark every inbound message in `chat_id` up to `date` as read-acknowledged.
pub fn mark_read_through(c: &Connection, chat_id: i64, date: i64) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE message SET read_sent = 1
         WHERE chat_id = ?1 AND is_from_me = 0 AND date <= ?2",
        params![chat_id, date],
    )?;
    Ok(())
}

/// Mark as read any inbound message that already has a *later* message we sent in
/// the same chat. Replying after a message proves we saw it, so even if a read
/// receipt never arrives (receipts off, lost, or never echoed) the unread state
/// should clear. Backstops the receipt path and any out-of-order delivery.
pub fn reconcile_implicit_reads(c: &Connection) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE message SET read_sent = 1
         WHERE is_from_me = 0 AND read_sent = 0
           AND EXISTS (
               SELECT 1 FROM message s
               WHERE s.chat_id = message.chat_id
                 AND s.is_from_me = 1
                 AND s.date > message.date
           )",
        [],
    )?;
    Ok(())
}

fn upsert_handle(c: &Connection, address: &str, is_me: bool) -> rusqlite::Result<i64> {
    let addr = address.to_lowercase();
    // `is_me` is sticky: once a handle is known to be ours it stays that way.
    c.execute(
        "INSERT INTO handle(address, is_me) VALUES (?1, ?2)
         ON CONFLICT(address) DO UPDATE SET is_me = handle.is_me | excluded.is_me",
        params![addr, is_me as i64],
    )?;
    c.query_row("SELECT id FROM handle WHERE address = ?1", params![addr], |r| r.get(0))
}

fn upsert_chat(c: &Connection, chat: &ChatRef) -> rusqlite::Result<i64> {
    let key = chat.key();
    // Fill in a name/service we didn't have before, but never clobber with NULL.
    c.execute(
        "INSERT INTO chat(key, display_name, is_group, service) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(key) DO UPDATE SET
            display_name = COALESCE(excluded.display_name, chat.display_name),
            service      = COALESCE(excluded.service, chat.service)",
        params![key, chat.display_name, chat.is_group() as i64, chat.service],
    )?;
    let chat_id: i64 =
        c.query_row("SELECT id FROM chat WHERE key = ?1", params![key], |r| r.get(0))?;
    for p in &chat.participants {
        let hid = upsert_handle(c, p, false)?;
        c.execute(
            "INSERT OR IGNORE INTO chat_participant(chat_id, handle_id) VALUES (?1, ?2)",
            params![chat_id, hid],
        )?;
    }
    Ok(chat_id)
}

fn bump_chat_date(c: &Connection, chat_id: i64, date: i64) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE chat SET last_message_date = MAX(COALESCE(last_message_date, 0), ?2)
         WHERE id = ?1",
        params![chat_id, date],
    )?;
    Ok(())
}

fn insert_message(c: &Connection, m: &IncomingMessage) -> rusqlite::Result<()> {
    let chat_id = upsert_chat(c, &m.chat)?;
    let sender_id = match &m.sender {
        Some(a) => Some(upsert_handle(c, a, m.is_from_me)?),
        None => None,
    };
    c.execute(
        "INSERT OR IGNORE INTO message
           (guid, chat_id, sender_handle_id, is_from_me, text, subject, service, date,
            effect, reply_to_guid, reply_part, item_type, pending)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![
            m.guid, chat_id, sender_id, m.is_from_me as i64, m.text, m.subject, m.service,
            m.date, m.effect, m.reply_to_guid, m.reply_part, m.item_type, m.pending as i64
        ],
    )?;
    // Attach files only on first insert (fan-out duplicates reuse the guid).
    let newly_inserted = c.changes() > 0;
    if newly_inserted && !m.attachments.is_empty() {
        let msg_id = c.last_insert_rowid();
        for a in &m.attachments {
            c.execute(
                "INSERT INTO attachment
                   (message_id, guid, mime_type, transfer_name, total_bytes,
                    local_path, part_index, is_sticker)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    msg_id, a.guid, a.mime, a.name, a.total_bytes,
                    a.local_path, a.part_index, a.is_sticker as i64
                ],
            )?;
        }
    }
    // Sending from any of our devices marks the conversation read up to that
    // point on all of them. Mirror that locally so a reply sent on the phone
    // clears the unread state we picked up here.
    if newly_inserted && m.is_from_me {
        mark_read_through(c, chat_id, m.date)?;
    } else if newly_inserted {
        // Reverse order (our reply was already stored, this incoming arrived late):
        // if a later message we sent exists, this was clearly seen -> read.
        let seen_later: bool = c.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM message
                WHERE chat_id = ?1 AND is_from_me = 1 AND date > ?2)",
            params![chat_id, m.date],
            |r| r.get(0),
        )?;
        if seen_later {
            c.execute("UPDATE message SET read_sent = 1 WHERE guid = ?1", params![m.guid])?;
        }
    }
    bump_chat_date(c, chat_id, m.date)
}

fn insert_tapback(c: &Connection, t: &Tapback) -> rusqlite::Result<()> {
    let chat_id = upsert_chat(c, &t.chat)?;
    let sender_id = match &t.sender {
        Some(a) => Some(upsert_handle(c, a, t.is_from_me)?),
        None => None,
    };
    c.execute(
        "INSERT OR IGNORE INTO message
           (guid, chat_id, sender_handle_id, is_from_me, date,
            associated_guid, associated_type, reply_part, item_type)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0)",
        params![
            t.guid, chat_id, sender_id, t.is_from_me as i64, t.date,
            t.associated_guid, t.associated_type, t.associated_part
        ],
    )?;
    bump_chat_date(c, chat_id, t.date)
}

// --- message-scoped link previews ---

/// Directory where thumbnail files for [`MessageLinkPreview::image_path`] are
/// written. Created on demand by the receiver; the store does not manage it.
pub fn preview_image_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
                .join(".cache")
        });
    base.join("bubbles").join("previews")
}

/// Upsert a sender-generated link preview. The PK is `(message_guid, part_idx)`
/// so a placeholder can be replaced by its fill-in (same key, different row) and
/// a duplicate ingest of the same preview is a no-op.
///
/// Image dimensions are stored best-effort so the renderer can size the card
/// without re-decoding the thumbnail (which is async/expensive on a worker).
fn upsert_message_link_preview(c: &Connection, p: &MessageLinkPreview) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO message_link_preview
           (message_guid, part_idx, url, original_url, title, summary,
            image_path, image_width, image_height, is_placeholder)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(message_guid, part_idx) DO UPDATE SET
            url            = excluded.url,
            original_url   = excluded.original_url,
            title          = excluded.title,
            summary        = excluded.summary,
            image_path     = excluded.image_path,
            image_width    = excluded.image_width,
            image_height   = excluded.image_height,
            is_placeholder = excluded.is_placeholder",
        params![
            p.message_guid,
            p.part_idx,
            p.url,
            p.original_url,
            p.title,
            p.summary,
            p.image_path,
            p.image_width,
            p.image_height,
            p.is_placeholder as i64,
        ],
    )?;
    Ok(())
}

/// Load every link preview whose message is in `guids`. Returns a map keyed by
/// `(guid, part_idx)` so a renderer can look up in O(1) per message part. Used
/// by the UI to batch-fetch the previews for the currently-loaded window in a
/// single round-trip, so per-card blocking reads never happen on the GTK main
/// thread.
pub fn message_link_previews_for(
    c: &Connection,
    guids: &[String],
) -> rusqlite::Result<std::collections::HashMap<(String, i64), MessageLinkPreview>> {
    let mut out = std::collections::HashMap::new();
    if guids.is_empty() {
        return Ok(out);
    }
    let placeholders = guids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT message_guid, part_idx, url, original_url, title, summary,
                image_path, image_width, image_height, is_placeholder
         FROM message_link_preview
         WHERE message_guid IN ({placeholders})"
    );
    let mut stmt = c.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(guids.iter()), |r| {
        Ok(MessageLinkPreview {
            message_guid: r.get(0)?,
            part_idx: r.get(1)?,
            url: r.get(2)?,
            original_url: r.get(3)?,
            title: r.get(4)?,
            summary: r.get(5)?,
            image_path: r.get(6)?,
            image_width: r.get(7)?,
            image_height: r.get(8)?,
            is_placeholder: r.get::<_, i64>(9)? != 0,
        })
    })?;
    for row in rows {
        let p = row?;
        out.insert((p.message_guid.clone(), p.part_idx), p);
    }
    Ok(out)
}

/// Apply one inbound event in a transaction.
pub fn apply_blocking(c: &mut Connection, ingest: Ingest) -> rusqlite::Result<()> {
    let tx = c.transaction()?;
    match ingest {
        Ingest::Message(m) => insert_message(&tx, &m)?,
        Ingest::Tapback(t) => insert_tapback(&tx, &t)?,
        Ingest::LinkPreview(p) => upsert_message_link_preview(&tx, &p)?,
        Ingest::Receipt(Receipt::Delivered { guid, date }) => {
            tx.execute(
                "UPDATE message SET date_delivered = ?1, pending = 0, error = NULL
                 WHERE guid = ?2 AND date_delivered IS NULL",
                params![date, guid],
            )?;
        }
        Ingest::Receipt(Receipt::Read { guid, date }) => {
            // A read receipt means different things depending on whose message it
            // targets. Apple echoes our own read receipts to our other devices, so
            // a receipt against one of *our incoming* messages is a cross-device
            // signal that we read it elsewhere -> clear unread through it. A receipt
            // against an *outgoing* message is the recipient reading us -> date_read.
            let target: Option<(i64, i64, i64)> = tx
                .query_row(
                    "SELECT is_from_me, chat_id, date FROM message WHERE guid = ?1",
                    params![guid],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            match target {
                Some((0, chat_id, msg_date)) => mark_read_through(&tx, chat_id, msg_date)?,
                _ => {
                    tx.execute(
                        "UPDATE message SET date_read = ?1, pending = 0, error = NULL
                         WHERE guid = ?2 AND date_read IS NULL",
                        params![date, guid],
                    )?;
                }
            }
        }
        Ingest::SendFailed { guid, category } => {
            tx.execute(
                "UPDATE message SET error = ?1, pending = 0 WHERE guid = ?2",
                params![category as i64, guid],
            )?;
        }
        Ingest::Edited { guid, text } => {
            tx.execute(
                "UPDATE message SET text = ?1 WHERE guid = ?2",
                params![text, guid],
            )?;
        }
        Ingest::Ignored(_) => {}
    }
    tx.commit()
}

/// Inbound messages newer than `date` (a notification watermark), oldest-first.
/// Excludes our own messages and tapbacks. Used to raise desktop notifications.
pub fn incoming_since(c: &Connection, date: i64) -> rusqlite::Result<Vec<NewMessage>> {
    let mut stmt = c.prepare(
        "SELECT m.chat_id, h.address, m.text, m.date,
                EXISTS(SELECT 1 FROM attachment a WHERE a.message_id = m.id)
         FROM message m
         LEFT JOIN handle h ON h.id = m.sender_handle_id
         WHERE m.is_from_me = 0 AND m.date > ?1 AND m.associated_guid IS NULL
         ORDER BY m.date ASC, m.id ASC",
    )?;
    let rows = stmt.query_map(params![date], |r| {
        Ok(NewMessage {
            chat_id: r.get(0)?,
            sender: r.get(1)?,
            text: r.get(2)?,
            date: r.get(3)?,
            has_attachment: r.get::<_, i64>(4)? != 0,
        })
    })?;
    rows.collect()
}

pub fn query_chats(c: &Connection) -> rusqlite::Result<Vec<ChatSummary>> {
    let mut stmt = c.prepare(
        "SELECT c.id, c.key, c.display_name, c.is_group, c.service, c.last_message_date,
                COALESCE(GROUP_CONCAT(h.address, ';'), ''),
                (SELECT COUNT(*) FROM message m
                   WHERE m.chat_id = c.id AND m.is_from_me = 0 AND m.read_sent = 0),
                c.custom_name,
                c.custom_avatar_path
         FROM chat c
         LEFT JOIN chat_participant cp ON cp.chat_id = c.id
         LEFT JOIN handle h            ON h.id = cp.handle_id
         GROUP BY c.id
         ORDER BY (c.last_message_date IS NULL), c.last_message_date DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        let parts: String = r.get(6)?;
        Ok(ChatSummary {
            id: r.get(0)?,
            key: r.get(1)?,
            display_name: r.get(2)?,
            is_group: r.get::<_, i64>(3)? != 0,
            service: r.get(4)?,
            last_message_date: r.get(5)?,
            participants: if parts.is_empty() {
                vec![]
            } else {
                parts.split(';').map(String::from).collect()
            },
            unread: r.get(7)?,
            custom_name: r.get(8)?,
            custom_avatar_path: r.get(9)?,
        })
    })?;
    rows.collect()
}

/// Columns selected for a `StoredMessage`, in the order [`map_message_row`] expects.
const MSG_COLS: &str = "m.id, m.guid, m.chat_id, h.address, m.is_from_me, m.text, m.subject, \
     m.service, m.date, m.date_delivered, m.date_read, m.effect, m.reply_to_guid, m.reply_part, \
     m.associated_guid, m.associated_type, m.item_type, m.error, m.pending";

fn map_message_row(r: &rusqlite::Row) -> rusqlite::Result<StoredMessage> {
    Ok(StoredMessage {
        id: r.get(0)?,
        guid: r.get(1)?,
        chat_id: r.get(2)?,
        sender: r.get(3)?,
        is_from_me: r.get::<_, i64>(4)? != 0,
        text: r.get(5)?,
        subject: r.get(6)?,
        service: r.get(7)?,
        date: r.get(8)?,
        date_delivered: r.get(9)?,
        date_read: r.get(10)?,
        effect: r.get(11)?,
        reply_to_guid: r.get(12)?,
        reply_part: r.get(13)?,
        associated_guid: r.get(14)?,
        associated_type: r.get(15)?,
        item_type: r.get(16)?,
        send_error: SendErrorCategory::from_i64(r.get::<_, Option<i64>>(17)?),
        pending: r.get::<_, i64>(18)? != 0,
        attachments: Vec::new(),
    })
}

/// Attachments for a specific set of message ids, grouped by message.
fn load_attachments(
    c: &Connection,
    ids: &[i64],
) -> rusqlite::Result<std::collections::HashMap<i64, Vec<StoredAttachment>>> {
    let mut by_msg: std::collections::HashMap<i64, Vec<StoredAttachment>> =
        std::collections::HashMap::new();
    if ids.is_empty() {
        return Ok(by_msg);
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT a.message_id, a.mime_type, a.transfer_name, a.local_path, a.is_sticker
         FROM attachment a WHERE a.message_id IN ({placeholders})
         ORDER BY a.part_index ASC, a.id ASC"
    );
    let mut astmt = c.prepare(&sql)?;
    let arows = astmt.query_map(params_from_iter(ids.iter()), |r| {
        Ok((
            r.get::<_, i64>(0)?,
            StoredAttachment {
                mime: r.get(1)?,
                name: r.get(2)?,
                local_path: r.get(3)?,
                is_sticker: r.get::<_, i64>(4)? != 0,
            },
        ))
    })?;
    for row in arows {
        let (mid, att) = row?;
        by_msg.entry(mid).or_default().push(att);
    }
    Ok(by_msg)
}

fn attach_to(c: &Connection, messages: &mut [StoredMessage]) -> rusqlite::Result<()> {
    let ids: Vec<i64> = messages.iter().map(|m| m.id).collect();
    let mut by_msg = load_attachments(c, &ids)?;
    for m in messages.iter_mut() {
        if let Some(atts) = by_msg.remove(&m.id) {
            m.attachments = atts;
        }
    }
    Ok(())
}

/// Full-chat message list (ascending). Used by the unit tests below; in
/// production the UI drives paged/windowed access (`messages_page`,
/// `messages_from`) instead, so this whole-message helper is test-only.
#[cfg(test)]
pub fn query_messages(c: &Connection, chat_id: i64) -> rusqlite::Result<Vec<StoredMessage>> {
    let mut stmt = c.prepare(&format!(
        "SELECT {MSG_COLS}
         FROM message m LEFT JOIN handle h ON h.id = m.sender_handle_id
         WHERE m.chat_id = ?1
         ORDER BY m.date ASC, m.id ASC"
    ))?;
    let rows = stmt.query_map(params![chat_id], map_message_row)?;
    let mut messages = rows.collect::<rusqlite::Result<Vec<StoredMessage>>>()?;
    attach_to(c, &mut messages)?;
    Ok(messages)
}

/// One page of a chat's messages, newest-first window returned in ascending
/// (oldest-first) order ready to render. `before` is the `(date, id)` cursor of
/// the oldest message already loaded; pass `None` for the most recent page.
/// Loads attachments only for the page's messages.
pub fn query_messages_page(
    c: &Connection,
    chat_id: i64,
    before: Option<(i64, i64)>,
    limit: i64,
) -> rusqlite::Result<Vec<StoredMessage>> {
    let (has_before, bd, bid) = match before {
        Some((d, i)) => (1i64, d, i),
        None => (0, 0, 0),
    };
    let mut stmt = c.prepare(&format!(
        "SELECT {MSG_COLS}
         FROM message m LEFT JOIN handle h ON h.id = m.sender_handle_id
         WHERE m.chat_id = ?1
           AND (?2 = 0 OR (m.date < ?3 OR (m.date = ?3 AND m.id < ?4)))
         ORDER BY m.date DESC, m.id DESC
         LIMIT ?5"
    ))?;
    let rows = stmt.query_map(params![chat_id, has_before, bd, bid, limit], map_message_row)?;
    let mut messages = rows.collect::<rusqlite::Result<Vec<StoredMessage>>>()?;
    messages.reverse(); // DESC window -> ascending for display
    attach_to(c, &mut messages)?;
    Ok(messages)
}

/// All messages at or after the `(date, id)` cursor, ascending. Used to rebuild
/// the currently-loaded window (cursor = oldest shown) on refresh, so newly
/// arrived messages appear without collapsing any older pages already loaded.
/// `None` loads the whole chat.
pub fn query_messages_from(
    c: &Connection,
    chat_id: i64,
    since: Option<(i64, i64)>,
) -> rusqlite::Result<Vec<StoredMessage>> {
    let (has_since, sd, sid) = match since {
        Some((d, i)) => (1i64, d, i),
        None => (0, 0, 0),
    };
    let mut stmt = c.prepare(&format!(
        "SELECT {MSG_COLS}
         FROM message m LEFT JOIN handle h ON h.id = m.sender_handle_id
         WHERE m.chat_id = ?1
           AND (?2 = 0 OR (m.date > ?3 OR (m.date = ?3 AND m.id >= ?4)))
         ORDER BY m.date ASC, m.id ASC"
    ))?;
    let rows = stmt.query_map(params![chat_id, has_since, sd, sid], map_message_row)?;
    let mut messages = rows.collect::<rusqlite::Result<Vec<StoredMessage>>>()?;
    attach_to(c, &mut messages)?;
    Ok(messages)
}

/// All tapback/reaction rows for a chat, ordered by date ascending.
/// Only rows with `associated_guid IS NOT NULL` are reactions.
pub fn query_tapbacks_for_chat(c: &Connection, chat_id: i64) -> rusqlite::Result<Vec<Tapback>> {
    let mut stmt = c.prepare(
        "SELECT m.guid, h.address, m.is_from_me, m.date,
                m.associated_guid, m.associated_type, m.reply_part
         FROM message m
         LEFT JOIN handle h ON h.id = m.sender_handle_id
         WHERE m.chat_id = ?1 AND m.associated_guid IS NOT NULL
         ORDER BY m.date ASC, m.id ASC"
    )?;
    let rows = stmt.query_map(params![chat_id], |r| {
        Ok(Tapback {
            guid: r.get(0)?,
            chat: ChatRef::default(),
            sender: r.get(1)?,
            is_from_me: r.get::<_, i64>(2)? != 0,
            date: r.get(3)?,
            associated_guid: r.get(4)?,
            associated_type: r.get(5)?,
            associated_part: r.get(6)?,
        })
    })?;
    rows.collect()
}

// --- contact cache ---

/// Upsert a contact. If a contact with the same `uid` already exists, replace
/// its display name, avatar, and address set atomically (old addresses are
/// deleted before the new ones are inserted).
#[allow(dead_code)]
pub fn upsert_contact(c: &Connection, contact: &Contact) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO contact(uid, display_name, avatar) VALUES (?1, ?2, ?3)
         ON CONFLICT(uid) DO UPDATE SET
            display_name = excluded.display_name,
            avatar      = excluded.avatar",
        params![contact.uid, contact.display_name, contact.avatar],
    )?;
    // Replace the address set: delete all old rows, insert new ones.
    c.execute(
        "DELETE FROM contact_address WHERE contact_uid = ?1",
        params![contact.uid],
    )?;
    for addr in &contact.addresses {
        c.execute(
            "INSERT INTO contact_address(contact_uid, value, kind) VALUES (?1, ?2, ?3)",
            params![contact.uid, addr.value, addr.kind],
        )?;
    }
    Ok(())
}

/// Load all addresses for a contact (used internally by
/// [`lookup_contact_by_addr`]).
#[allow(dead_code)]
fn load_contact_addresses(c: &Connection, uid: &str) -> rusqlite::Result<Vec<ContactAddress>> {
    let mut stmt = c.prepare(
        "SELECT value, kind FROM contact_address WHERE contact_uid = ?1 ORDER BY kind, value",
    )?;
    let rows = stmt.query_map(params![uid], |r| {
        Ok(ContactAddress {
            value: r.get(0)?,
            kind: r.get(1)?,
        })
    })?;
    rows.collect()
}

/// Look up a contact by one of its normalized address strings. Returns
/// `None` when no contact has that address. The returned contact carries
/// its full address set (not just the matched address).
#[allow(dead_code)]
pub fn lookup_contact_by_addr(c: &Connection, addr: &str) -> rusqlite::Result<Option<Contact>> {
    let maybe = c
        .query_row(
            "SELECT c.uid, c.display_name, c.avatar
             FROM contact c
             JOIN contact_address a ON a.contact_uid = c.uid
             WHERE a.value = ?1",
            params![addr],
            |r| {
                Ok(Contact {
                    uid: r.get(0)?,
                    display_name: r.get(1)?,
                    avatar: r.get(2)?,
                    addresses: Vec::new(), // filled below
                })
            },
        )
        .optional()?;
    match maybe {
        Some(mut contact) => {
            contact.addresses = load_contact_addresses(c, &contact.uid)?;
            Ok(Some(contact))
        }
        None => Ok(None),
    }
}

/// Remove every row from both contact-cache tables. Subsequent lookups
/// return `None`.
#[allow(dead_code)]
pub fn clear_contacts(c: &Connection) -> rusqlite::Result<()> {
    c.execute_batch("DELETE FROM contact_address; DELETE FROM contact;")?;
    Ok(())
}

/// Load every cached contact, with full address sets. Used at startup to
/// populate the in-memory contact list from the persisted cache so the
/// sidebar renders with contact names immediately (before the background
/// EDS refresh updates it).
#[allow(dead_code)]
pub fn query_all_contacts(c: &Connection) -> rusqlite::Result<Vec<Contact>> {
    let mut stmt = c.prepare("SELECT uid, display_name, avatar FROM contact")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<Vec<u8>>>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (uid, display_name, avatar) = row?;
        let addresses = load_contact_addresses(c, &uid)?;
        out.push(Contact {
            uid,
            display_name,
            avatar,
            addresses,
        });
    }
    Ok(out)
}

// --- local chat deletion ---

/// Delete the selected chats and cascade-delete their messages,
/// chat_participant rows, and attachments (via existing FK CASCADE).
/// After deletion, sweep handles that are no longer referenced by
/// any remaining chat_participant or message sender.
/// Empty `chat_ids` is a no-op.
#[allow(dead_code)]
pub fn delete_chats_blocking(c: &mut Connection, chat_ids: &[i64]) -> rusqlite::Result<()> {
    if chat_ids.is_empty() {
        return Ok(());
    }
    let tx = c.transaction()?;
    let placeholders = chat_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!("DELETE FROM chat WHERE id IN ({placeholders})");
    tx.execute(&sql, params_from_iter(chat_ids))?;
    // Sweep handles no longer referenced by any chat_participant or message sender.
    tx.execute_batch(
        "DELETE FROM handle
         WHERE id NOT IN (SELECT handle_id FROM chat_participant)
           AND id NOT IN (SELECT sender_handle_id FROM message WHERE sender_handle_id IS NOT NULL)",
    )?;
    tx.commit()
}

// --- async wrapper (used by the app) ---

/// Async handle to the message database. Cloneable; all clones share the one
/// underlying connection/thread.
#[derive(Clone)]
pub struct Store {
    conn: tokio_rusqlite::Connection,
}

impl Store {
    /// Open (creating if needed) the DB at `path` and run migrations.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = tokio_rusqlite::Connection::open(path.as_ref().to_owned()).await?;
        conn.call(|c| {
            migrate(c)?;
            // Clear any unread that a later sent message already implies we read,
            // in case those events were missed while this device was offline.
            reconcile_implicit_reads(c)?;
            // Repair: rows with delivery evidence (date_delivered or date_read)
            // should never be in-flight or failed — clear both states.  This
            // handles the case where a message was delivered but a late
            // SendFailed or a crash left stale state behind.
            c.execute(
                "UPDATE message SET pending = 0, error = NULL
                 WHERE (date_delivered IS NOT NULL OR date_read IS NOT NULL)
                   AND (pending = 1 OR error IS NOT NULL)",
                [],
            )?;
            // Orphan sweep: any message still pending without delivery evidence
            // (e.g. after a crash before send completion) is marked as failed
            // so it's not left in-flight forever.
            c.execute(
                "UPDATE message SET pending = 0, error = ?1
                 WHERE pending = 1
                   AND date_delivered IS NULL
                   AND date_read IS NULL",
                params![SendErrorCategory::Other as i64],
            )?;
            Ok(())
        })
        .await?;
        Ok(Self { conn })
    }

    /// Open an in-memory DB (tests / ephemeral use).
    pub async fn open_in_memory() -> Result<Self> {
        let conn = tokio_rusqlite::Connection::open_in_memory().await?;
        conn.call(|c| {
            migrate(c)?;
            Ok(())
        })
        .await?;
        Ok(Self { conn })
    }

    /// Mark a pending message as successfully sent (clear its pending flag).
    pub async fn mark_sent(&self, guid: &str) -> Result<()> {
        let guid = guid.to_owned();
        self.conn
            .call(move |c| {
                c.execute(
                    "UPDATE message SET pending = 0 WHERE guid = ?1",
                    params![guid],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Reset a failed message back to in-flight: pending=1, error=NULL.
    pub async fn mark_retrying(&self, guid: &str) -> Result<()> {
        let guid = guid.to_owned();
        self.conn
            .call(move |c| {
                c.execute(
                    "UPDATE message SET pending = 1, error = NULL WHERE guid = ?1",
                    params![guid],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    pub async fn apply(&self, ingest: Ingest) -> Result<()> {
        self.conn
            .call(move |c| {
                apply_blocking(c, ingest)?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    pub async fn chats(&self) -> Result<Vec<ChatSummary>> {
        Ok(self.conn.call(|c| Ok(query_chats(c)?)).await?)
    }

    /// Set (or, with `None`/empty, clear) a conversation's user-given name.
    pub async fn set_chat_custom_name(&self, chat_id: i64, name: Option<String>) -> Result<()> {
        let name = name.filter(|n| !n.trim().is_empty());
        self.conn
            .call(move |c| {
                c.execute(
                    "UPDATE chat SET custom_name = ?1 WHERE id = ?2",
                    params![name, chat_id],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Set (or, with `None`/empty, clear) a conversation's custom avatar path.
    /// This is a local override and survives chat upsert (like custom_name).
    #[allow(dead_code)]
    pub async fn set_chat_custom_avatar(&self, chat_id: i64, path: Option<String>) -> Result<()> {
        let path = path.filter(|p| !p.trim().is_empty());
        self.conn
            .call(move |c| {
                c.execute(
                    "UPDATE chat SET custom_avatar_path = ?1 WHERE id = ?2",
                    params![path, chat_id],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Inbound messages newer than `date`, for desktop notifications.
    pub async fn incoming_since(&self, date: i64) -> Result<Vec<NewMessage>> {
        Ok(self.conn.call(move |c| Ok(incoming_since(c, date)?)).await?)
    }

    /// One page of a chat's messages (ascending). `before` is the `(date, id)`
    /// cursor of the oldest message already shown; `None` fetches the newest page.
    pub async fn messages_page(
        &self,
        chat_id: i64,
        before: Option<(i64, i64)>,
        limit: i64,
    ) -> Result<Vec<StoredMessage>> {
        Ok(self
            .conn
            .call(move |c| Ok(query_messages_page(c, chat_id, before, limit)?))
            .await?)
    }

    /// Rebuilds the loaded window: all messages at/after `since` (oldest shown),
    /// ascending. `None` loads the whole chat.
    pub async fn messages_from(
        &self,
        chat_id: i64,
        since: Option<(i64, i64)>,
    ) -> Result<Vec<StoredMessage>> {
        Ok(self
            .conn
            .call(move |c| Ok(query_messages_from(c, chat_id, since)?))
            .await?)
    }

    /// All tapback/reaction rows for a chat, ordered by date ascending.
    pub async fn tapbacks_for_chat(&self, chat_id: i64) -> Result<Vec<Tapback>> {
        Ok(self
            .conn
            .call(move |c| Ok(query_tapbacks_for_chat(c, chat_id)?))
            .await?)
    }

    pub async fn latest_unread_incoming(&self, chat_id: i64) -> Result<Option<(String, i64)>> {
        Ok(self
            .conn
            .call(move |c| Ok(latest_unread_incoming(c, chat_id)?))
            .await?)
    }

    pub async fn first_unread_incoming(&self, chat_id: i64) -> Result<Option<(String, i64)>> {
        Ok(self
            .conn
            .call(move |c| Ok(first_unread_incoming(c, chat_id)?))
            .await?)
    }

    pub async fn mark_read_through(&self, chat_id: i64, date: i64) -> Result<()> {
        self.conn
            .call(move |c| {
                mark_read_through(c, chat_id, date)?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Batch-load link previews for a window of messages. The UI calls this
    /// alongside `messages_page` / `messages_from` so per-card blocking reads
    /// never happen on the GTK main thread.
    pub async fn message_link_previews_for(
        &self,
        guids: Vec<String>,
    ) -> Result<std::collections::HashMap<(String, i64), MessageLinkPreview>> {
        Ok(self
            .conn
            .call(move |c| Ok(message_link_previews_for(c, &guids)?))
            .await?)
    }

    /// Remove every row from both contact-cache tables.
    #[allow(dead_code)]
    pub async fn clear_contacts(&self) -> Result<()> {
        self.conn
            .call(move |c| {
                clear_contacts(c)?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Upsert a contact (replace if same uid).
    #[allow(dead_code)]
    pub async fn upsert_contact(&self, contact: Contact) -> Result<()> {
        self.conn
            .call(move |c| {
                upsert_contact(c, &contact)?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Look up a contact by one of its normalized address strings.
    #[allow(dead_code)]
    pub async fn lookup_contact_by_addr(&self, addr: String) -> Result<Option<Contact>> {
        Ok(self
            .conn
            .call(move |c| Ok(lookup_contact_by_addr(c, &addr)?))
            .await?)
    }

    /// Load every cached contact into memory. Used at startup to populate
    /// the UI's in-memory contact list from the persisted cache so the
    /// sidebar renders with contact names immediately.
    #[allow(dead_code)]
    pub async fn all_contacts(&self) -> Result<Vec<Contact>> {
        Ok(self
            .conn
            .call(|c| Ok(query_all_contacts(c)?))
            .await?)
    }

    /// Delete the selected chats and cascade-delete their messages,
    /// chat_participant rows, and attachments. Sweeps orphaned handles.
    #[allow(dead_code)]
    pub async fn delete_chats(&self, chat_ids: Vec<i64>) -> Result<()> {
        self.conn
            .call(move |c| {
                delete_chats_blocking(c, &chat_ids)?;
                Ok(())
            })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (target_guid, target_part, sender, is_from_me, reaction_index) — the
    /// identity tuple used to compare `LiveTapback`s in the helper closure below.
    type LiveTapbackKey = (String, Option<String>, Option<String>, bool, u8);

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c
    }

    fn chat_1to1() -> ChatRef {
        ChatRef {
            participants: vec![
                "mailto:chrispouliot@icloud.com".into(),
                "mailto:asd@icloud.com".into(),
            ],
            display_name: None,
            service: Some("iMessage".into()),
        }
    }

    fn msg(guid: &str, date: i64) -> IncomingMessage {
        IncomingMessage {
            guid: guid.into(),
            chat: chat_1to1(),
            sender: Some("mailto:asd@icloud.com".into()),
            is_from_me: false,
            text: Some("Hello me".into()),
            date,
            ..Default::default()
        }
    }

    // An outgoing message (sent by us, possibly synced from another device).
    fn sent(guid: &str, date: i64) -> IncomingMessage {
        IncomingMessage {
            guid: guid.into(),
            chat: chat_1to1(),
            sender: Some("mailto:me@icloud.com".into()),
            is_from_me: true,
            text: Some("From me".into()),
            date,
            ..Default::default()
        }
    }

    #[test]
    fn dedupes_duplicate_delivery() {
        let mut c = db();
        // Same guid twice (the fan-out duplicate seen in the spike).
        apply_blocking(&mut c, Ingest::Message(msg("G1", 1000))).unwrap();
        apply_blocking(&mut c, Ingest::Message(msg("G1", 1000))).unwrap();

        let chats = query_chats(&c).unwrap();
        assert_eq!(chats.len(), 1, "one chat");
        assert!(!chats[0].is_group);
        let msgs = query_messages(&c, chats[0].id).unwrap();
        assert_eq!(msgs.len(), 1, "duplicate guid collapsed to one row");
        assert_eq!(msgs[0].text.as_deref(), Some("Hello me"));
        assert_eq!(chats[0].last_message_date, Some(1000));
        assert_eq!(chats[0].participants.len(), 2);
    }

    #[test]
    fn receipt_updates_existing_message() {
        let mut c = db();
        // A read receipt against one of *our* messages = the recipient read it.
        apply_blocking(&mut c, Ingest::Message(sent("G2", 2000))).unwrap();
        apply_blocking(&mut c, Ingest::Receipt(Receipt::Read { guid: "G2".into(), date: 2500 }))
            .unwrap();

        let chats = query_chats(&c).unwrap();
        let msgs = query_messages(&c, chats[0].id).unwrap();
        assert_eq!(msgs.len(), 1, "receipt did not insert a new row");
        assert_eq!(msgs[0].date_read, Some(2500));
        assert_eq!(msgs[0].date_delivered, None);
    }

    #[test]
    fn read_receipt_on_incoming_clears_unread_cross_device() {
        let mut c = db();
        // Three incoming messages, all unread.
        apply_blocking(&mut c, Ingest::Message(msg("I1", 1000))).unwrap();
        apply_blocking(&mut c, Ingest::Message(msg("I2", 1100))).unwrap();
        apply_blocking(&mut c, Ingest::Message(msg("I3", 1200))).unwrap();
        let chats = query_chats(&c).unwrap();
        assert_eq!(chats[0].unread, 3, "all unread before sync");

        // We read up to I2 on another device; Apple echoes that read receipt to us.
        apply_blocking(&mut c, Ingest::Receipt(Receipt::Read { guid: "I2".into(), date: 1500 }))
            .unwrap();

        let chats = query_chats(&c).unwrap();
        assert_eq!(chats[0].unread, 1, "I1+I2 cleared, I3 still unread");
        // It must not have written date_read onto our incoming row.
        let msgs = query_messages(&c, chats[0].id).unwrap();
        assert!(msgs.iter().all(|m| m.date_read.is_none()));
    }

    #[test]
    fn self_sent_message_marks_conversation_read() {
        let mut c = db();
        apply_blocking(&mut c, Ingest::Message(msg("I1", 1000))).unwrap();
        apply_blocking(&mut c, Ingest::Message(msg("I2", 1100))).unwrap();
        let chats = query_chats(&c).unwrap();
        assert_eq!(chats[0].unread, 2);

        // A reply we sent (here or synced from the phone) clears prior unread.
        apply_blocking(&mut c, Ingest::Message(sent("S1", 1200))).unwrap();
        let chats = query_chats(&c).unwrap();
        assert_eq!(chats[0].unread, 0, "sending implies the chat was read");

        // An incoming that arrives *after* our reply stays unread.
        apply_blocking(&mut c, Ingest::Message(msg("I3", 1300))).unwrap();
        let chats = query_chats(&c).unwrap();
        assert_eq!(chats[0].unread, 1);
    }

    #[test]
    fn out_of_order_send_then_incoming_marks_read() {
        let mut c = db();
        // Our reply is stored first; an earlier incoming arrives late (reordering).
        apply_blocking(&mut c, Ingest::Message(sent("S1", 1200))).unwrap();
        apply_blocking(&mut c, Ingest::Message(msg("I1", 1000))).unwrap();
        let chats = query_chats(&c).unwrap();
        assert_eq!(chats[0].unread, 0, "a later send implies the late incoming was seen");

        // But an incoming after the send is genuinely new.
        apply_blocking(&mut c, Ingest::Message(msg("I2", 1300))).unwrap();
        let chats = query_chats(&c).unwrap();
        assert_eq!(chats[0].unread, 1);
    }

    #[test]
    fn incoming_since_returns_only_newer_inbound() {
        let mut c = db();
        apply_blocking(&mut c, Ingest::Message(msg("I1", 1000))).unwrap();
        apply_blocking(&mut c, Ingest::Message(sent("S1", 1100))).unwrap();
        apply_blocking(&mut c, Ingest::Message(msg("I2", 1200))).unwrap();
        apply_blocking(&mut c, Ingest::Message(msg("I3", 1300))).unwrap();

        // Only inbound messages strictly after the watermark, oldest-first.
        let got = incoming_since(&c, 1100).unwrap();
        assert_eq!(got.len(), 2, "I2 and I3 (not the sent one, not I1)");
        assert_eq!(got[0].date, 1200);
        assert_eq!(got[1].date, 1300);
        assert_eq!(got[0].text.as_deref(), Some("Hello me"));
        assert!(got.iter().all(|m| m.sender.as_deref() == Some("mailto:asd@icloud.com")));
    }

    #[test]
    fn reconcile_clears_stale_unread_before_a_send() {
        let mut c = db();
        // Simulate state persisted before the implicit-read logic existed: an
        // incoming and a later send both stored, but the unread flag still set.
        apply_blocking(&mut c, Ingest::Message(msg("I1", 1000))).unwrap();
        apply_blocking(&mut c, Ingest::Message(sent("S1", 1200))).unwrap();
        c.execute("UPDATE message SET read_sent = 0 WHERE is_from_me = 0", [])
            .unwrap();
        let chats = query_chats(&c).unwrap();
        assert_eq!(chats[0].unread, 1, "forced stale unread");

        reconcile_implicit_reads(&c).unwrap();
        let chats = query_chats(&c).unwrap();
        assert_eq!(chats[0].unread, 0, "sweep cleared unread that a later send implies");
    }

    #[test]
    fn receipt_for_unknown_guid_is_noop() {
        let mut c = db();
        apply_blocking(&mut c, Ingest::Receipt(Receipt::Delivered { guid: "nope".into(), date: 1 }))
            .unwrap();
        assert!(query_chats(&c).unwrap().is_empty());
    }

    #[test]
    fn distinct_participant_sets_are_distinct_chats() {
        let mut c = db();
        apply_blocking(&mut c, Ingest::Message(msg("A", 10))).unwrap();

        let mut m = msg("B", 20);
        m.chat = ChatRef {
            participants: vec!["tel:+15551234".into(), "mailto:asd@icloud.com".into()],
            display_name: None,
            service: Some("iMessage".into()),
        };
        apply_blocking(&mut c, Ingest::Message(m)).unwrap();

        assert_eq!(query_chats(&c).unwrap().len(), 2, "different sets -> different chats");
    }

    #[test]
    fn key_is_order_and_case_insensitive() {
        let a = ChatRef {
            participants: vec!["mailto:B@x.com".into(), "mailto:a@x.com".into()],
            ..Default::default()
        };
        let b = ChatRef {
            participants: vec!["mailto:a@x.com".into(), "mailto:b@x.com".into()],
            ..Default::default()
        };
        assert_eq!(a.key(), b.key());
    }

    #[test]
    fn group_detection() {
        let g = ChatRef {
            participants: vec!["a".into(), "b".into(), "c".into()],
            ..Default::default()
        };
        assert!(g.is_group());
    }

    #[test]
    fn first_unread_is_earliest_unacked() {
        let mut c = db();
        apply_blocking(&mut c, Ingest::Message(msg("U1", 100))).unwrap();
        apply_blocking(&mut c, Ingest::Message(msg("U2", 200))).unwrap();
        apply_blocking(&mut c, Ingest::Message(msg("U3", 300))).unwrap();
        let chat_id = query_chats(&c).unwrap()[0].id;

        let (guid, date) = first_unread_incoming(&c, chat_id).unwrap().unwrap();
        assert_eq!((guid.as_str(), date), ("U1", 100), "earliest unread");

        // Ack through the middle; boundary advances to the next unread.
        mark_read_through(&c, chat_id, 200).unwrap();
        let (guid, _) = first_unread_incoming(&c, chat_id).unwrap().unwrap();
        assert_eq!(guid, "U3");

        mark_read_through(&c, chat_id, 300).unwrap();
        assert!(first_unread_incoming(&c, chat_id).unwrap().is_none());
    }

    #[test]
    fn attachments_round_trip() {
        let mut c = db();
        let mut m = msg("IMG1", 100);
        m.text = None;
        m.attachments = vec![AttachmentRecord {
            mime: Some("image/jpeg".into()),
            name: Some("photo.jpg".into()),
            local_path: Some("/tmp/photo.jpg".into()),
            part_index: Some(0),
            ..Default::default()
        }];
        apply_blocking(&mut c, Ingest::Message(m.clone())).unwrap();
        // Fan-out duplicate must not double-insert attachments.
        apply_blocking(&mut c, Ingest::Message(m)).unwrap();

        let chat_id = query_chats(&c).unwrap()[0].id;
        let msgs = query_messages(&c, chat_id).unwrap();
        assert_eq!(msgs.len(), 1, "deduped message");
        let atts = &msgs[0].attachments;
        assert_eq!(atts.len(), 1, "single attachment, not duplicated");
        assert!(atts[0].is_image());
        assert_eq!(atts[0].local_path.as_deref(), Some("/tmp/photo.jpg"));
    }

    #[test]
    fn unread_count_reflects_read_sent() {
        let mut c = db();
        apply_blocking(&mut c, Ingest::Message(msg("N1", 100))).unwrap();
        apply_blocking(&mut c, Ingest::Message(msg("N2", 200))).unwrap();
        let summary = &query_chats(&c).unwrap()[0];
        assert_eq!(summary.unread, 2, "two unacked inbound messages");

        mark_read_through(&c, summary.id, 200).unwrap();
        assert_eq!(query_chats(&c).unwrap()[0].unread, 0, "cleared after read");
    }

    #[test]
    fn read_sent_tracking() {
        let mut c = db();
        apply_blocking(&mut c, Ingest::Message(msg("U1", 100))).unwrap();
        apply_blocking(&mut c, Ingest::Message(msg("U2", 200))).unwrap();
        let cid = query_chats(&c).unwrap()[0].id;

        let unread = latest_unread_incoming(&c, cid).unwrap();
        assert_eq!(unread.map(|(g, _)| g), Some("U2".to_string()), "newest unread");

        mark_read_through(&c, cid, 200).unwrap();
        assert!(
            latest_unread_incoming(&c, cid).unwrap().is_none(),
            "all acknowledged"
        );
    }

    #[test]
    fn tapback_stored_with_association() {
        let mut c = db();
        apply_blocking(&mut c, Ingest::Message(msg("T1", 100))).unwrap();
        apply_blocking(
            &mut c,
            Ingest::Tapback(Tapback {
                guid: "R1".into(),
                chat: chat_1to1(),
                sender: Some("mailto:asd@icloud.com".into()),
                is_from_me: false,
                date: 150,
                associated_guid: "T1".into(),
                associated_part: Some("0".into()),
                associated_type: 2000, // Heart, add
            }),
        )
        .unwrap();

        let chats = query_chats(&c).unwrap();
        let msgs = query_messages(&c, chats[0].id).unwrap();
        assert_eq!(msgs.len(), 2);
        let react = msgs.iter().find(|m| m.guid == "R1").unwrap();
        assert_eq!(react.associated_guid.as_deref(), Some("T1"));
        assert_eq!(react.associated_type, Some(2000));
    }

    #[test]
    fn paginates_newest_first_then_scrolls_back() {
        let mut c = db();
        // 25 messages with increasing dates.
        for i in 0..25 {
            apply_blocking(&mut c, Ingest::Message(msg(&format!("M{i}"), 1000 + i)))
                .unwrap();
        }
        let chats = query_chats(&c).unwrap();
        let cid = chats[0].id;

        // Newest page of 10, returned ascending.
        let page1 = query_messages_page(&c, cid, None, 10).unwrap();
        assert_eq!(page1.len(), 10);
        assert_eq!(page1.first().unwrap().date, 1015);
        assert_eq!(page1.last().unwrap().date, 1024, "last is the newest message");

        // Scroll up from the oldest currently shown.
        let cursor = (page1[0].date, page1[0].id);
        let page2 = query_messages_page(&c, cid, Some(cursor), 10).unwrap();
        assert_eq!(page2.len(), 10);
        assert_eq!(page2.last().unwrap().date, 1014, "ends just before the cursor");
        assert_eq!(page2.first().unwrap().date, 1005);

        // Final partial page.
        let cursor = (page2[0].date, page2[0].id);
        let page3 = query_messages_page(&c, cid, Some(cursor), 10).unwrap();
        assert_eq!(page3.len(), 5, "only 5 older messages remain");
        assert_eq!(page3.first().unwrap().date, 1000);

        // Exhausted.
        let cursor = (page3[0].date, page3[0].id);
        let page4 = query_messages_page(&c, cid, Some(cursor), 10).unwrap();
        assert!(page4.is_empty());
    }

    #[test]
    fn messages_from_reloads_window_and_new_arrivals() {
        let mut c = db();
        for i in 0..10 {
            apply_blocking(&mut c, Ingest::Message(msg(&format!("W{i}"), 2000 + i)))
                .unwrap();
        }
        let chats = query_chats(&c).unwrap();
        let cid = chats[0].id;
        // Simulate having scrolled so the oldest shown is the 5th message.
        let all = query_messages(&c, cid).unwrap();
        let cursor = (all[4].date, all[4].id);
        let window = query_messages_from(&c, cid, Some(cursor)).unwrap();
        assert_eq!(window.len(), 6, "messages 5..10 inclusive of the cursor");
        assert_eq!(window.first().unwrap().date, 2004);

        // A new message arrives; the same cursor still yields the window + it.
        apply_blocking(&mut c, Ingest::Message(msg("W-new", 2100))).unwrap();
        let window2 = query_messages_from(&c, cid, Some(cursor)).unwrap();
        assert_eq!(window2.len(), 7);
        assert_eq!(window2.last().unwrap().guid, "W-new");
    }

    #[test]
    fn page_carries_attachments() {
        let mut c = db();
        let mut m = msg("A1", 500);
        m.text = None;
        m.attachments = vec![AttachmentRecord {
            guid: Some("att-1".into()),
            mime: Some("image/jpeg".into()),
            name: Some("cat.jpg".into()),
            total_bytes: Some(1234),
            local_path: Some("/tmp/cat.jpg".into()),
            part_index: Some(0),
            is_sticker: false,
        }];
        apply_blocking(&mut c, Ingest::Message(m)).unwrap();
        let chats = query_chats(&c).unwrap();
        let page = query_messages_page(&c, chats[0].id, None, 10).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].attachments.len(), 1);
        assert_eq!(page[0].attachments[0].name.as_deref(), Some("cat.jpg"));
    }

    // --- link preview tests ---

    fn preview(guid: &str, part: i64) -> MessageLinkPreview {
        MessageLinkPreview {
            message_guid: guid.into(),
            part_idx: part,
            url: Some("https://example.com/".into()),
            original_url: Some("https://example.com".into()),
            title: Some("Example".into()),
            summary: Some("The example page.".into()),
            image_path: Some("/tmp/example.jpg".into()),
            image_width: Some(400),
            image_height: Some(300),
            is_placeholder: false,
        }
    }

    #[test]
    fn message_link_preview_round_trip() {
        let mut c = db();
        let p = preview("G-PREVIEW", 0);
        apply_blocking(&mut c, Ingest::LinkPreview(p.clone())).unwrap();
        let out = message_link_previews_for(&c, &["G-PREVIEW".into()]).unwrap();
        let got = out.get(&("G-PREVIEW".into(), 0)).expect("row present");
        assert_eq!(got.url.as_deref(), Some("https://example.com/"));
        assert_eq!(got.title.as_deref(), Some("Example"));
        assert_eq!(got.summary.as_deref(), Some("The example page."));
        assert_eq!(got.image_path.as_deref(), Some("/tmp/example.jpg"));
        assert_eq!(got.image_width, Some(400));
        assert_eq!(got.image_height, Some(300));
        assert!(!got.is_placeholder);
        assert!(!got.is_sparse());
    }

    #[test]
    fn message_link_preview_batches_across_guids() {
        let mut c = db();
        // Two messages, each with two parts; only the requested guids come back.
        for g in ["G-A", "G-B", "G-C"] {
            apply_blocking(&mut c, Ingest::LinkPreview(preview(g, 0))).unwrap();
            apply_blocking(&mut c, Ingest::LinkPreview(preview(g, 1))).unwrap();
        }
        let guids = vec!["G-A".into(), "G-C".into()];
        let out = message_link_previews_for(&c, &guids).unwrap();
        assert_eq!(out.len(), 4, "two parts for each of the two requested guids");
        assert!(out.contains_key(&("G-A".into(), 0)));
        assert!(out.contains_key(&("G-A".into(), 1)));
        assert!(out.contains_key(&("G-C".into(), 0)));
        assert!(out.contains_key(&("G-C".into(), 1)));
        // G-B was not requested; nothing for it in the result.
        assert!(!out.keys().any(|(g, _)| g == "G-B"));
    }

    #[test]
    fn message_link_preview_empty_guids_yields_empty_map() {
        let c = db();
        let out = message_link_previews_for(&c, &[]).unwrap();
        assert!(out.is_empty(), "no guids, no rows");
    }

    #[test]
    fn message_link_preview_placeholder_to_fillin() {
        let mut c = db();
        // Placeholder first.
        let mut p = preview("G-PH", 0);
        p.title = None;
        p.summary = None;
        p.is_placeholder = true;
        apply_blocking(&mut c, Ingest::LinkPreview(p)).unwrap();
        let placeholder =
            message_link_previews_for(&c, &["G-PH".into()])
                .unwrap()
                .remove(&("G-PH".into(), 0))
                .unwrap();
        assert!(placeholder.is_placeholder);
        assert!(placeholder.is_sparse(), "no title, no summary -> sparse");

        // Fill-in upserts on the same (guid, part_idx), replacing the row.
        let fill = MessageLinkPreview {
            title: Some("Real Title".into()),
            summary: Some("Real description.".into()),
            is_placeholder: false,
            ..preview("G-PH", 0)
        };
        apply_blocking(&mut c, Ingest::LinkPreview(fill)).unwrap();
        let updated =
            message_link_previews_for(&c, &["G-PH".into()])
                .unwrap()
                .remove(&("G-PH".into(), 0))
                .unwrap();
        assert!(!updated.is_placeholder, "fill-in cleared the placeholder flag");
        assert_eq!(updated.title.as_deref(), Some("Real Title"));
        assert_eq!(updated.summary.as_deref(), Some("Real description."));
        assert!(!updated.is_sparse());
    }

    #[test]
    fn message_link_preview_distinct_part_indices() {
        let mut c = db();
        apply_blocking(&mut c, Ingest::LinkPreview(preview("G-MULTI", 0))).unwrap();
        let mut p1 = preview("G-MULTI", 1);
        p1.url = Some("https://example.com/2".into());
        p1.title = Some("Second".into());
        apply_blocking(&mut c, Ingest::LinkPreview(p1)).unwrap();
        let out = message_link_previews_for(&c, &["G-MULTI".into()]).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[&("G-MULTI".into(), 0)].title.as_deref(),
            Some("Example")
        );
        assert_eq!(
            out[&("G-MULTI".into(), 1)].title.as_deref(),
            Some("Second")
        );
    }

    // --- live tapback set (cancellation/aggregation) ---

    // Pin the cancellation/aggregation of live tapbacks. No DB, no env, no I/O:
    // builds Tapback values directly and calls the helper.
    //
    // Helper contract: `pub fn live_tapbacks(tapbacks: &[Tapback]) -> Vec<LiveTapback>`
    // where `LiveTapback` is:
    //   pub struct LiveTapback {
    //       pub target_guid: String,
    //       pub target_part: Option<String>,
    //       pub sender: Option<String>,
    //       pub is_from_me: bool,
    //       pub reaction_index: u8,   // 0..=5
    //   }
    //
    // Cancellation rule: `associated_type` 2000..=2005 = add, 3000..=3005 = remove.
    // The lower digit is the reaction index (0..=5). For each
    // (target_guid, target_part, sender, reaction_index) the most recent event
    // by `date` wins; an "add" followed by a "remove" from the same sender on
    // the same target cancels out; a "remove" with no prior add produces no
    // live entry. Assertions are order-independent (fingerprint via BTreeSet).
    #[test]
    fn tapbacks_live_set() {
        use std::collections::BTreeSet;

        let sender_a = Some("mailto:a@x.com".to_string());
        let sender_b = Some("mailto:b@x.com".to_string());
        let chat = chat_1to1();

        // Build a Tapback with the fields the helper actually consults; the
        // `chat` is shared and irrelevant to the cancellation logic.
        let mk = |guid: &str,
                  target: &str,
                  part: Option<&str>,
                  sender: Option<String>,
                  is_from_me: bool,
                  date: i64,
                  kind: i64|
         -> Tapback {
            Tapback {
                guid: guid.into(),
                chat: chat.clone(),
                sender,
                is_from_me,
                date,
                associated_guid: target.into(),
                associated_part: part.map(String::from),
                associated_type: kind,
            }
        };

        // Project a Vec<LiveTapback> to a BTreeSet so the assertions do not
        // depend on the helper's output ordering.
        let fingerprint = |v: &[LiveTapback]| -> BTreeSet<LiveTapbackKey> {
            v.iter()
                .map(|l| {
                    (
                        l.target_guid.clone(),
                        l.target_part.clone(),
                        l.sender.clone(),
                        l.is_from_me,
                        l.reaction_index,
                    )
                })
                .collect()
        };

        // 1. Empty input -> empty live set.
        assert!(live_tapbacks(&[]).is_empty(), "empty input -> empty live set");

        // 2. A single "add" of reaction 0 (heart) from sender A on target T1
        //    -> exactly one live entry with the right target/part/sender/idx.
        let add_heart_a = mk("RA1", "T1", Some("0"), sender_a.clone(), false, 1000, 2000);
        let live = live_tapbacks(std::slice::from_ref(&add_heart_a));
        assert_eq!(live.len(), 1, "single add -> one live entry");
        assert_eq!(live[0].target_guid, "T1");
        assert_eq!(live[0].target_part.as_deref(), Some("0"));
        assert_eq!(live[0].sender, sender_a);
        assert!(!live[0].is_from_me, "is_from_me mirrors the winning event");
        assert_eq!(live[0].reaction_index, 0, "lower digit of 2000 = 0");

        // 3. "add" then later "remove" (same target/sender/idx) -> cancelled.
        let rem_heart_a = mk("RR1", "T1", Some("0"), sender_a.clone(), false, 1100, 3000);
        let live = live_tapbacks(&[add_heart_a.clone(), rem_heart_a.clone()]);
        assert!(
            live.is_empty(),
            "add+remove on the same (target, sender, idx) cancels"
        );

        // 4. "remove" then later "add" -> one live entry (last event wins).
        let live = live_tapbacks(&[rem_heart_a.clone(), add_heart_a.clone()]);
        assert_eq!(
            fingerprint(&live),
            BTreeSet::from([(
                "T1".to_string(),
                Some("0".to_string()),
                sender_a.clone(),
                false,
                0u8,
            )]),
            "remove+add on the same (target, sender, idx) -> live"
        );

        // 5. Multiple senders each adding the same reaction on the same target
        //    -> one live entry per sender.
        let add_heart_b = mk("RB1", "T1", Some("0"), sender_b.clone(), false, 1200, 2000);
        let live = live_tapbacks(&[add_heart_a.clone(), add_heart_b.clone()]);
        assert_eq!(live.len(), 2, "one live entry per sender");
        assert_eq!(
            fingerprint(&live),
            BTreeSet::from([
                ("T1".to_string(), Some("0".to_string()), sender_a.clone(), false, 0u8),
                ("T1".to_string(), Some("0".to_string()), sender_b.clone(), false, 0u8),
            ]),
            "two senders, same target and reaction -> two live entries"
        );

        // 6. Same sender adding two different reactions on the same target
        //    -> two live entries (different indices don't merge).
        let add_thumb_a = mk("RA2", "T1", Some("0"), sender_a.clone(), false, 1300, 2002);
        let live = live_tapbacks(&[add_heart_a.clone(), add_thumb_a.clone()]);
        assert_eq!(
            fingerprint(&live),
            BTreeSet::from([
                ("T1".to_string(), Some("0".to_string()), sender_a.clone(), false, 0u8),
                ("T1".to_string(), Some("0".to_string()), sender_a.clone(), false, 2u8),
            ]),
            "different reaction indices don't merge"
        );

        // 7. Different targets are not merged.
        let add_heart_t2 = mk("RA-T2", "T2", Some("0"), sender_a.clone(), false, 1400, 2000);
        let live = live_tapbacks(&[add_heart_a.clone(), add_heart_t2.clone()]);
        assert_eq!(
            fingerprint(&live),
            BTreeSet::from([
                ("T1".to_string(), Some("0".to_string()), sender_a.clone(), false, 0u8),
                ("T2".to_string(), Some("0".to_string()), sender_a.clone(), false, 0u8),
            ]),
            "different targets are not merged"
        );

        // 8. A "remove" with no prior add produces no live entry.
        let orphan_remove = mk(
            "R-ORPHAN",
            "T3",
            Some("0"),
            sender_a.clone(),
            false,
            1500,
            3000,
        );
        assert!(
            live_tapbacks(&[orphan_remove]).is_empty(),
            "remove with no prior add -> no live entry"
        );
    }

    // --- per-target reaction summary (chat-timeline chips) ---

    // Pin the per-target grouping of *live* tapbacks for the chat-timeline
    // reaction-chip renderer. Pure data transformation: no DB, no env, no I/O.
    // Builds `Tapback` values directly, runs them through `live_tapbacks` to
    // apply add/remove cancellation, then hands the result to
    // `group_tapbacks_by_target`.
    //
    // Helper contract:
    //   pub struct LiveReactionSummary {
    //       pub reaction_index: u8,  // 0..=5
    //       pub count: usize,
    //       pub my_reacted: bool,
    //   }
    //   pub fn group_tapbacks_by_target(
    //       live: Vec<LiveTapback>
    //   ) -> BTreeMap<String, Vec<LiveReactionSummary>>
    //
    // Grouping: by `target_guid` (outer map key), then by `reaction_index`
    // (one entry per reaction type). `count` is the number of *distinct
    // senders* in that group; `my_reacted` is true if any of them has
    // `is_from_me: true`. Inner `Vec` ordering is implementation-defined, so
    // assertions fingerprint rather than depend on order.
    #[test]
    fn group_tapbacks_by_target_chips() {
        use std::collections::{BTreeMap, BTreeSet};

        let sender_a = Some("mailto:a@x.com".to_string());
        let sender_b = Some("mailto:b@x.com".to_string());
        let sender_me = Some("mailto:me@x.com".to_string());
        let chat = chat_1to1();

        // Same closure shape as in `tapbacks_live_set` for visual consistency.
        let mk = |guid: &str,
                  target: &str,
                  part: Option<&str>,
                  sender: Option<String>,
                  is_from_me: bool,
                  date: i64,
                  kind: i64|
         -> Tapback {
            Tapback {
                guid: guid.into(),
                chat: chat.clone(),
                sender,
                is_from_me,
                date,
                associated_guid: target.into(),
                associated_part: part.map(String::from),
                associated_type: kind,
            }
        };

        // Project the result to a BTreeSet of (target, reaction_index, count,
        // my_reacted) tuples so assertions don't depend on the inner Vec's
        // ordering. Outer BTreeMap keys are already sorted.
        let fp = |m: &BTreeMap<String, Vec<LiveReactionSummary>>|
         -> BTreeSet<(String, u8, usize, bool)> {
            m.iter()
                .flat_map(|(target, entries)| {
                    entries
                        .iter()
                        .map(move |e| (target.clone(), e.reaction_index, e.count, e.my_reacted))
                })
                .collect()
        };

        // 1. Empty input -> empty map.
        let out = group_tapbacks_by_target(Vec::new());
        assert!(out.is_empty(), "empty input -> empty BTreeMap");

        // 2. One target, one sender, one reaction -> one entry
        //    {reaction_index: 0, count: 1, my_reacted: false}.
        let a_heart_t1 = mk("R1", "T1", Some("0"), sender_a.clone(), false, 1000, 2000);
        let out = group_tapbacks_by_target(live_tapbacks(std::slice::from_ref(&a_heart_t1)));
        assert_eq!(out.len(), 1, "one target with one reaction -> one map entry");
        let v = out.get("T1").expect("target T1 is in the map");
        assert_eq!(v.len(), 1, "one reaction type -> one inner entry");
        assert_eq!(v[0].reaction_index, 0);
        assert_eq!(v[0].count, 1);
        assert!(!v[0].my_reacted);

        // 3. One target, two senders, same reaction -> one entry,
        //    count: 2, my_reacted: false.
        let b_heart_t1 = mk("R2", "T1", Some("0"), sender_b.clone(), false, 1100, 2000);
        let out =
            group_tapbacks_by_target(live_tapbacks(&[a_heart_t1.clone(), b_heart_t1.clone()]));
        assert_eq!(out.len(), 1, "still one target");
        let v = &out["T1"];
        assert_eq!(v.len(), 1, "two senders on same reaction type collapse to one entry");
        assert_eq!(v[0].reaction_index, 0);
        assert_eq!(v[0].count, 2);
        assert!(!v[0].my_reacted);

        // 4. One target, one sender, two different reactions -> two entries
        //    with count: 1 each, distinct reaction_index.
        let a_thumb_t1 = mk("R3", "T1", Some("0"), sender_a.clone(), false, 1200, 2002);
        let out = group_tapbacks_by_target(live_tapbacks(&[a_heart_t1.clone(), a_thumb_t1.clone()]));
        let v = &out["T1"];
        assert_eq!(v.len(), 2, "two different reaction types -> two inner entries");
        let by_idx: BTreeMap<u8, &LiveReactionSummary> =
            v.iter().map(|e| (e.reaction_index, e)).collect();
        assert_eq!(by_idx.len(), 2, "distinct reaction_index per entry");
        for (idx, e) in &by_idx {
            assert_eq!(e.count, 1, "one sender per reaction type");
            assert!(!e.my_reacted);
            assert!(*idx == 0 || *idx == 2, "only the two reaction types we sent");
        }
        assert!(by_idx.contains_key(&0) && by_idx.contains_key(&2));

        // 5. Two different targets -> two map entries, not merged.
        let a_heart_t2 = mk("R4", "T2", Some("0"), sender_a.clone(), false, 1300, 2000);
        let out = group_tapbacks_by_target(live_tapbacks(&[
            a_heart_t1.clone(),
            a_heart_t2.clone(),
        ]));
        assert_eq!(out.len(), 2, "two targets -> two map keys");
        assert!(out.contains_key("T1") && out.contains_key("T2"));
        assert_eq!(
            fp(&out),
            BTreeSet::from([
                ("T1".to_string(), 0u8, 1usize, false),
                ("T2".to_string(), 0u8, 1usize, false),
            ]),
            "different targets are not merged"
        );

        // 6. my_reacted: true when the (only) sender is is_from_me.
        let me_heart_t1 = mk("R5", "T1", Some("0"), sender_me.clone(), true, 1400, 2000);
        let out = group_tapbacks_by_target(live_tapbacks(std::slice::from_ref(&me_heart_t1)));
        let v = &out["T1"];
        assert_eq!(v.len(), 1);
        assert!(v[0].my_reacted, "is_from_me -> my_reacted: true");
        assert_eq!(v[0].count, 1);
        assert_eq!(v[0].reaction_index, 0);

        // 7. my_reacted: true when at least one of multiple senders is
        //    is_from_me; entry collapses across the two senders.
        let out = group_tapbacks_by_target(live_tapbacks(&[
            a_heart_t1.clone(),
            me_heart_t1.clone(),
        ]));
        let v = &out["T1"];
        assert_eq!(v.len(), 1, "still one entry per reaction type");
        assert_eq!(v[0].reaction_index, 0);
        assert_eq!(v[0].count, 2, "two distinct senders -> count: 2");
        assert!(
            v[0].my_reacted,
            "one of the senders is is_from_me -> my_reacted: true"
        );
    }

    #[test]
    fn tapbacks_for_chat_query() {
        let mut c = db();
        // Insert a base message so the chat exists.
        apply_blocking(&mut c, Ingest::Message(msg("T1", 100))).unwrap();
        let chat_id = query_chats(&c).unwrap()[0].id;

        // Insert two tapback rows with different dates.
        apply_blocking(
            &mut c,
            Ingest::Tapback(Tapback {
                guid: "R1".into(),
                chat: chat_1to1(),
                sender: Some("mailto:a@x.com".into()),
                is_from_me: false,
                date: 150,
                associated_guid: "T1".into(),
                associated_part: Some("0".into()),
                associated_type: 2000,
            }),
        )
        .unwrap();
        apply_blocking(
            &mut c,
            Ingest::Tapback(Tapback {
                guid: "R2".into(),
                chat: chat_1to1(),
                sender: Some("mailto:b@x.com".into()),
                is_from_me: true,
                date: 160,
                associated_guid: "T1".into(),
                associated_part: Some("0".into()),
                associated_type: 2002,
            }),
        )
        .unwrap();

        // Also insert a non-tapback message (no associated_guid) — should be excluded.
        apply_blocking(&mut c, Ingest::Message(msg("T2", 200))).unwrap();

        let tapbacks = query_tapbacks_for_chat(&c, chat_id).unwrap();
        assert_eq!(tapbacks.len(), 2, "only tapback rows returned, not T2");
        assert_eq!(tapbacks[0].date, 150, "ordered by date ASC");
        assert_eq!(tapbacks[1].date, 160);
        assert_eq!(tapbacks[0].associated_guid, "T1");
        assert_eq!(tapbacks[0].associated_type, 2000);
        assert_eq!(tapbacks[0].sender.as_deref(), Some("mailto:a@x.com"));
        assert!(!tapbacks[0].is_from_me);
        assert_eq!(tapbacks[1].associated_type, 2002);
        assert!(tapbacks[1].is_from_me);
    }

    #[test]
    fn send_failed() {
        let mut c = db();
        // Each sub-case creates its own outgoing message (unique guid) then marks
        // it as failed and asserts the round-trip.  All share the same chat (from
        // the `sent()` helper) so `chat_id` is obtained once.
        apply_blocking(&mut c, Ingest::Message(sent("BASE", 0))).unwrap();
        let chat_id = query_chats(&c).unwrap()[0].id;

        let cases: [(i64, &str, SendErrorCategory); 3] = [
            (1000, "SEND-FAIL-TIMEOUT",       SendErrorCategory::Timeout),
            (2000, "SEND-FAIL-CONNECTION",    SendErrorCategory::ConnectionLost),
            (3000, "SEND-FAIL-OTHER",         SendErrorCategory::Other),
        ];
        for (date, guid, category) in cases {
            apply_blocking(&mut c, Ingest::Message(sent(guid, date))).unwrap();

            apply_blocking(
                &mut c,
                Ingest::SendFailed {
                    guid: guid.into(),
                    category,
                },
            )
            .unwrap();

            let msgs = query_messages(&c, chat_id).unwrap();
            let msg = msgs.iter().find(|m| m.guid == guid).unwrap();
            assert_eq!(msg.send_error, Some(category));
        }

        // A message that never received a SendFailed has send_error: None.
        let msgs = query_messages(&c, chat_id).unwrap();
        let base = msgs.iter().find(|m| m.guid == "BASE").unwrap();
        assert_eq!(base.send_error, None);
    }

    // --- custom chat avatar photo (v5) ---

    #[tokio::test]
    async fn set_chat_custom_avatar_round_trip() {
        let store = Store::open_in_memory().await.unwrap();
        store
            .apply(Ingest::Message(msg("AVATAR-RT", 1000)))
            .await
            .unwrap();
        let chat_id = store.chats().await.unwrap()[0].id;
        store
            .set_chat_custom_avatar(chat_id, Some("/some/path/avatar.png".into()))
            .await
            .unwrap();
        let chats = store.chats().await.unwrap();
        let chat = chats.into_iter().find(|c| c.id == chat_id).unwrap();
        assert_eq!(
            chat.custom_avatar_path.as_deref(),
            Some("/some/path/avatar.png")
        );
    }

    #[tokio::test]
    async fn set_chat_custom_avatar_clears_with_none() {
        let store = Store::open_in_memory().await.unwrap();
        store
            .apply(Ingest::Message(msg("AVATAR-CLEAR", 1000)))
            .await
            .unwrap();
        let chat_id = store.chats().await.unwrap()[0].id;

        // Set a path first.
        store
            .set_chat_custom_avatar(chat_id, Some("/path/to/avatar.png".into()))
            .await
            .unwrap();
        let chats = store.chats().await.unwrap();
        let chat = chats.into_iter().find(|c| c.id == chat_id).unwrap();
        assert!(
            chat.custom_avatar_path.is_some(),
            "path should be set before the clear"
        );

        // Clear with None.
        store
            .set_chat_custom_avatar(chat_id, None)
            .await
            .unwrap();
        let chats = store.chats().await.unwrap();
        let chat = chats.into_iter().find(|c| c.id == chat_id).unwrap();
        assert!(
            chat.custom_avatar_path.is_none(),
            "None should clear the path"
        );

        // Empty string should also clear it (mirrors set_chat_custom_name).
        store
            .set_chat_custom_avatar(chat_id, Some("/other/path.png".into()))
            .await
            .unwrap();
        store
            .set_chat_custom_avatar(chat_id, Some("".into()))
            .await
            .unwrap();
        let chats = store.chats().await.unwrap();
        let chat = chats.into_iter().find(|c| c.id == chat_id).unwrap();
        assert!(
            chat.custom_avatar_path.is_none(),
            "empty string should normalize to None"
        );
    }

    #[tokio::test]
    async fn existing_chat_has_no_custom_avatar_path_by_default() {
        let store = Store::open_in_memory().await.unwrap();
        store
            .apply(Ingest::Message(msg("AVATAR-DEFAULT", 1000)))
            .await
            .unwrap();
        let chats = store.chats().await.unwrap();
        let chat = chats.into_iter().find(|c| !c.participants.is_empty()).unwrap();
        assert!(
            chat.custom_avatar_path.is_none(),
            "fresh chat should have no custom avatar path"
        );
    }

    #[tokio::test]
    async fn custom_avatar_path_survives_chat_upsert() {
        let store = Store::open_in_memory().await.unwrap();
        // First message creates the chat.
        store
            .apply(Ingest::Message(msg("UPSERT-1", 1000)))
            .await
            .unwrap();
        let chat_id = store.chats().await.unwrap()[0].id;

        // Set a custom avatar path.
        store
            .set_chat_custom_avatar(chat_id, Some("/path/survives.png".into()))
            .await
            .unwrap();

        // A second message re-upserts the same chat (same ChatRef -> same key).
        store
            .apply(Ingest::Message(msg("UPSERT-2", 2000)))
            .await
            .unwrap();

        let chats = store.chats().await.unwrap();
        let chat = chats.into_iter().find(|c| c.id == chat_id).unwrap();
        assert_eq!(
            chat.custom_avatar_path.as_deref(),
            Some("/path/survives.png"),
            "custom avatar path must survive a chat upsert"
        );
    }

    #[test]
    fn migration_v4_to_v5_adds_column() {
        let c = Connection::open_in_memory().unwrap();
        // Apply v1–v4 schema manually, setting user_version to 4 between steps.
        c.execute_batch(DDL).unwrap();
        c.pragma_update(None, "user_version", 1).unwrap();
        c.execute_batch(
            "ALTER TABLE message ADD COLUMN read_sent INTEGER NOT NULL DEFAULT 0;",
        )
        .unwrap();
        c.pragma_update(None, "user_version", 2).unwrap();
        c.execute_batch("ALTER TABLE chat ADD COLUMN custom_name TEXT;")
            .unwrap();
        c.pragma_update(None, "user_version", 3).unwrap();
        c.execute_batch(DDL_V4).unwrap();
        c.pragma_update(None, "user_version", 4).unwrap();

        // Now run the full migrate — this should add v5.
        migrate(&c).unwrap();

        // Assert the column exists.
        let col_count: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('chat') WHERE name = 'custom_avatar_path'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            col_count, 1,
            "custom_avatar_path column must exist on chat after v5 migration"
        );

        // Assert user_version bumped through all migrations applied from a v4
        // base (v5, v6, v7, …). Update this expected value whenever a new
        // migration arm is added to `migrate()`.
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 7, "user_version must be 7 after migration from a v4 base");
    }

    #[test]
    fn edited_ingest_updates_message_text() {
        let mut c = db();
        apply_blocking(&mut c, Ingest::Message(sent("G1", 1000))).unwrap();
        apply_blocking(
            &mut c,
            Ingest::Edited {
                guid: "G1".into(),
                text: "Edited body text".into(),
            },
        )
        .unwrap();

        let chats = query_chats(&c).unwrap();
        let msgs = query_messages(&c, chats[0].id).unwrap();
        assert_eq!(msgs.len(), 1, "edit did not insert a new row");
        assert_eq!(msgs[0].text.as_deref(), Some("Edited body text"));
        assert_eq!(msgs[0].guid, "G1");
        assert!(msgs[0].is_from_me, "edit preserved is_from_me");
        assert_eq!(msgs[0].date, 1000, "edit did not change date");
    }

    #[test]
    fn edited_ingest_on_unknown_guid_is_a_noop() {
        let mut c = db();
        apply_blocking(&mut c, Ingest::Message(sent("EXISTS", 1000))).unwrap();
        apply_blocking(
            &mut c,
            Ingest::Edited {
                guid: "MISSING".into(),
                text: "nope".into(),
            },
        )
        .unwrap();

        let chats = query_chats(&c).unwrap();
        let msgs = query_messages(&c, chats[0].id).unwrap();
        assert_eq!(msgs.len(), 1, "edit did not insert a phantom row");
        assert_eq!(msgs[0].guid, "EXISTS");
        assert_eq!(msgs[0].text.as_deref(), Some("From me"));
    }

    // -------------------------------------------------------------------
    // Pending/in-flight send state (v6)
    // -------------------------------------------------------------------
    //
    // Contract (symbols do not exist yet — compile errors = intentional red):
    //   1. IncomingMessage.pending: bool (default false via ..Default::default())
    //   2. StoredMessage.pending: bool (read back from queries)
    //   3. Store::mark_sent(guid: &str) -> async Result<()> clears pending
    //   4. Ingest::SendFailed on a pending message clears pending AND sets error
    //   5. Store::open orphan-sweep: pending messages at open get marked as failed
    //
    // All fail to compile until the new field/method lands.

    #[tokio::test]
    async fn pending_send_state_message_with_pending_true() {
        let store = Store::open_in_memory().await.unwrap();
        let mut m = sent("PEND-TRUE", 1000);
        m.pending = true;
        store.apply(Ingest::Message(m)).await.unwrap();
        let chat_id = store.chats().await.unwrap()[0].id;
        let msgs = store.messages_page(chat_id, None, 10).await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].pending);
    }

    #[tokio::test]
    async fn pending_send_state_message_with_pending_false() {
        let store = Store::open_in_memory().await.unwrap();
        let m = sent("PEND-FALSE", 2000);
        store.apply(Ingest::Message(m)).await.unwrap();
        let chat_id = store.chats().await.unwrap()[0].id;
        let msgs = store.messages_page(chat_id, None, 10).await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(!msgs[0].pending);
    }

    #[tokio::test]
    async fn pending_send_state_mark_sent() {
        let store = Store::open_in_memory().await.unwrap();
        let mut m = sent("MARK-SENT", 3000);
        m.pending = true;
        store.apply(Ingest::Message(m)).await.unwrap();
        store.mark_sent("MARK-SENT").await.unwrap();
        let chat_id = store.chats().await.unwrap()[0].id;
        let msgs = store.messages_page(chat_id, None, 10).await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(!msgs[0].pending);
    }

    #[tokio::test]
    async fn pending_send_state_send_failed_clears_pending() {
        let store = Store::open_in_memory().await.unwrap();
        let mut m = sent("FAIL-PEND", 4000);
        m.pending = true;
        store.apply(Ingest::Message(m)).await.unwrap();
        store
            .apply(Ingest::SendFailed {
                guid: "FAIL-PEND".into(),
                category: SendErrorCategory::Timeout,
            })
            .await
            .unwrap();
        let chat_id = store.chats().await.unwrap()[0].id;
        let msgs = store.messages_page(chat_id, None, 10).await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(!msgs[0].pending);
        assert_eq!(msgs[0].send_error, Some(SendErrorCategory::Timeout));
    }

    #[tokio::test]
    async fn pending_send_state_orphan_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("bubbles.db");

        // First open: insert one pending and one non-pending (sent) message.
        let store = Store::open(&db_path).await.unwrap();
        let mut pending_msg = sent("PEND-SWEEP", 5000);
        pending_msg.pending = true;
        store.apply(Ingest::Message(pending_msg)).await.unwrap();
        store.apply(Ingest::Message(sent("SENT-SWEEP", 6000))).await.unwrap();
        let chat_id = store.chats().await.unwrap()[0].id;
        let msgs = store.messages_page(chat_id, None, 10).await.unwrap();
        assert_eq!(msgs.len(), 2, "both messages inserted on first open");
        drop(store);

        // Reopen — the orphan sweep must mark the pending message as failed.
        let store = Store::open(&db_path).await.unwrap();
        let msgs = store.messages_page(chat_id, None, 10).await.unwrap();
        assert_eq!(msgs.len(), 2, "reopen sees both messages");

        let pend = msgs.iter().find(|m| m.guid == "PEND-SWEEP").unwrap();
        assert!(!pend.pending, "orphan sweep cleared pending");
        assert_eq!(pend.send_error, Some(SendErrorCategory::Other));

        let sent = msgs.iter().find(|m| m.guid == "SENT-SWEEP").unwrap();
        assert!(!sent.pending, "non-pending message not pending");
        assert_eq!(sent.send_error, None, "non-pending message not marked failed");
    }

    #[tokio::test]
    async fn pending_send_state_mark_retrying() {
        let store = Store::open_in_memory().await.unwrap();
        let mut m = sent("RETRY-GUID", 7000);
        m.pending = true;
        store.apply(Ingest::Message(m)).await.unwrap();
        // Fail the message first.
        store
            .apply(Ingest::SendFailed {
                guid: "RETRY-GUID".into(),
                category: SendErrorCategory::Timeout,
            })
            .await
            .unwrap();
        let chat_id = store.chats().await.unwrap()[0].id;

        // Confirm it is failed before retry.
        let msgs = store.messages_page(chat_id, None, 10).await.unwrap();
        let msg = msgs.iter().find(|m| m.guid == "RETRY-GUID").unwrap();
        assert!(!msg.pending);
        assert_eq!(msg.send_error, Some(SendErrorCategory::Timeout));
        assert_eq!(msg.text.as_deref(), Some("From me"));

        // Retry: reset to in-flight.
        store.mark_retrying("RETRY-GUID").await.unwrap();

        let msgs = store.messages_page(chat_id, None, 10).await.unwrap();
        let msg = msgs.iter().find(|m| m.guid == "RETRY-GUID").unwrap();
        assert!(msg.pending, "retry sets pending back to true");
        assert_eq!(msg.send_error, None, "retry clears send_error");
        assert_eq!(msg.text.as_deref(), Some("From me"), "other fields intact");
    }

    #[tokio::test]
    async fn pending_send_state_delivered_receipt_clears_pending() {
        let store = Store::open_in_memory().await.unwrap();
        let mut m = sent("PEND-DEL-GUID", 8000);
        m.pending = true;
        store.apply(Ingest::Message(m)).await.unwrap();

        store
            .apply(Ingest::Receipt(Receipt::Delivered {
                guid: "PEND-DEL-GUID".into(),
                date: 8500,
            }))
            .await
            .unwrap();

        let chat_id = store.chats().await.unwrap()[0].id;
        let msgs = store.messages_page(chat_id, None, 10).await.unwrap();
        let msg = msgs.iter().find(|m| m.guid == "PEND-DEL-GUID").unwrap();
        // Fails today: Delivered receipt does not clear pending.
        assert!(!msg.pending, "Delivered receipt must clear pending");
        assert_eq!(
            msg.date_delivered,
            Some(8500),
            "date_delivered must be set"
        );
    }

    #[tokio::test]
    async fn pending_send_state_read_receipt_clears_pending() {
        let store = Store::open_in_memory().await.unwrap();
        let mut m = sent("PEND-READ-GUID", 9000);
        m.pending = true;
        store.apply(Ingest::Message(m)).await.unwrap();

        store
            .apply(Ingest::Receipt(Receipt::Read {
                guid: "PEND-READ-GUID".into(),
                date: 9500,
            }))
            .await
            .unwrap();

        let chat_id = store.chats().await.unwrap()[0].id;
        let msgs = store.messages_page(chat_id, None, 10).await.unwrap();
        let msg = msgs.iter().find(|m| m.guid == "PEND-READ-GUID").unwrap();
        // Fails today: Read receipt does not clear pending.
        assert!(!msg.pending, "Read receipt must clear pending");
        assert_eq!(msg.date_read, Some(9500), "date_read must be set");
    }

    #[tokio::test]
    async fn pending_send_state_receipt_clears_stale_error() {
        let store = Store::open_in_memory().await.unwrap();
        let mut m = sent("PEND-ERR-GUID", 10000);
        m.pending = true;
        store.apply(Ingest::Message(m)).await.unwrap();

        // Fail the message.
        store
            .apply(Ingest::SendFailed {
                guid: "PEND-ERR-GUID".into(),
                category: SendErrorCategory::Other,
            })
            .await
            .unwrap();

        // Now a Delivered receipt arrives — this should clear the stale error.
        store
            .apply(Ingest::Receipt(Receipt::Delivered {
                guid: "PEND-ERR-GUID".into(),
                date: 10500,
            }))
            .await
            .unwrap();

        let chat_id = store.chats().await.unwrap()[0].id;
        let msgs = store.messages_page(chat_id, None, 10).await.unwrap();
        let msg = msgs.iter().find(|m| m.guid == "PEND-ERR-GUID").unwrap();
        // Fails today: receipt does not clear error (a delivered message cannot be failed).
        assert!(
            msg.send_error.is_none(),
            "Delivered receipt must clear send_error"
        );
        assert!(!msg.pending, "pending must be false");
        assert_eq!(
            msg.date_delivered,
            Some(10500),
            "date_delivered must be set"
        );
    }

    #[tokio::test]
    async fn pending_send_state_orphan_sweep_spares_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("bubbles.db");

        let store = Store::open(&db_path).await.unwrap();
        // Two pending messages.
        let mut m1 = sent("SWEEP-DEL-GUID", 11000);
        m1.pending = true;
        store.apply(Ingest::Message(m1)).await.unwrap();
        let mut m2 = sent("SWEEP-ORPHAN-GUID", 12000);
        m2.pending = true;
        store.apply(Ingest::Message(m2)).await.unwrap();

        // Deliver a receipt for the first one.
        store
            .apply(Ingest::Receipt(Receipt::Delivered {
                guid: "SWEEP-DEL-GUID".into(),
                date: 11500,
            }))
            .await
            .unwrap();

        drop(store);

        // Reopen — the orphan sweep must not clobber the delivered row.
        let store = Store::open(&db_path).await.unwrap();
        let chat_id = store.chats().await.unwrap()[0].id;
        let msgs = store.messages_page(chat_id, None, 10).await.unwrap();
        assert_eq!(msgs.len(), 2, "both messages survive");

        let delivered = msgs
            .iter()
            .find(|m| m.guid == "SWEEP-DEL-GUID")
            .unwrap();
        // Fails today: orphan sweep marks the delivered message as failed.
        assert!(
            delivered.send_error.is_none(),
            "delivered message must not be marked failed"
        );
        assert!(!delivered.pending, "delivered message not pending");
        assert_eq!(
            delivered.date_delivered,
            Some(11500),
            "date_delivered preserved"
        );

        let orphan = msgs
            .iter()
            .find(|m| m.guid == "SWEEP-ORPHAN-GUID")
            .unwrap();
        // Existing sweep behaviour: undelivered pending message is marked failed.
        assert!(!orphan.pending, "orphan sweep cleared pending");
        assert_eq!(
            orphan.send_error,
            Some(SendErrorCategory::Other),
            "orphan sweep marked undelivered as failed"
        );
    }

    #[tokio::test]
    async fn pending_send_state_open_repairs_failed_but_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("bubbles.db");

        // Build a row that has both delivery evidence AND an error.
        let store = Store::open(&db_path).await.unwrap();
        let mut m = sent("REPAIR-GUID", 13000);
        m.pending = true;
        store.apply(Ingest::Message(m)).await.unwrap();

        // Delivered receipt arrives first.
        store
            .apply(Ingest::Receipt(Receipt::Delivered {
                guid: "REPAIR-GUID".into(),
                date: 13500,
            }))
            .await
            .unwrap();

        // SendFiled always wins at apply time — it's fresh evidence. This creates
        // the bad row: error=Some(Other) + date_delivered=Some(13500).
        store
            .apply(Ingest::SendFailed {
                guid: "REPAIR-GUID".into(),
                category: SendErrorCategory::Other,
            })
            .await
            .unwrap();

        drop(store);

        // Reopen — the open-time repair must clear error on rows that have
        // delivery evidence.
        let store = Store::open(&db_path).await.unwrap();
        let chat_id = store.chats().await.unwrap()[0].id;
        let msgs = store.messages_page(chat_id, None, 10).await.unwrap();
        let msg = msgs.iter().find(|m| m.guid == "REPAIR-GUID").unwrap();
        // Fails today: open does not repair error on delivered rows.
        assert!(
            msg.send_error.is_none(),
            "open must clear send_error on a row with delivery evidence"
        );
        assert!(!msg.pending, "pending cleared by reopen");
        assert_eq!(msg.date_delivered, Some(13500), "date_delivered preserved");
    }

    // -------------------------------------------------------------------
    // Contact cache (v7) — compile errors = intentional red until the
    // Contact struct, ContactAddress struct, DDL_V7, migration arm, and
    // sync functions (upsert_contact, lookup_contact_by_addr, clear_contacts)
    // land.  All fail to compile because none of those symbols exist.
    //
    // Contract (symbols do not exist yet — compile errors = intentional red):
    //   pub struct ContactAddress {
    //       pub value: String,   // normalized tel:+… / mailto:…
    //       pub kind: String,    // "phone" or "email"
    //   }
    //   pub struct Contact {
    //       pub uid: String,
    //       pub display_name: String,
    //       pub avatar: Option<Vec<u8>>,
    //       pub addresses: Vec<ContactAddress>,
    //   }
    //   pub fn upsert_contact(c: &Connection, contact: &Contact) -> rusqlite::Result<()>
    //   pub fn lookup_contact_by_addr(c: &Connection, addr: &str) -> rusqlite::Result<Option<Contact>>
    //   pub fn clear_contacts(c: &Connection) -> rusqlite::Result<()>
    // -------------------------------------------------------------------

    #[test]
    fn contact_lookup_returns_none_when_empty() {
        let c = db();
        let got = lookup_contact_by_addr(&c, "tel:+15555550100").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn upsert_contact_then_lookup_by_phone_finds_it() {
        let c = db();
        let contact = Contact {
            uid: "u1".into(),
            display_name: "Alice".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "tel:+15555550100".into(),
                kind: "phone".into(),
            }],
        };
        upsert_contact(&c, &contact).unwrap();
        let got = lookup_contact_by_addr(&c, "tel:+15555550100")
            .unwrap()
            .expect("contact should be found by phone");
        assert_eq!(got.display_name, "Alice");
        assert_eq!(got.uid, "u1");
    }

    #[test]
    fn upsert_contact_then_lookup_by_email_finds_it() {
        let c = db();
        let contact = Contact {
            uid: "u2".into(),
            display_name: "Bob".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "mailto:bob@example.com".into(),
                kind: "email".into(),
            }],
        };
        upsert_contact(&c, &contact).unwrap();
        let got = lookup_contact_by_addr(&c, "mailto:bob@example.com")
            .unwrap()
            .expect("contact should be found by email");
        assert_eq!(got.display_name, "Bob");
        assert_eq!(got.uid, "u2");
    }

    #[test]
    fn upsert_contact_replaces_existing_same_uid() {
        let c = db();
        // First upsert: Alice with phone +1 and an avatar.
        let alice = Contact {
            uid: "u1".into(),
            display_name: "Alice".into(),
            avatar: Some(vec![0x89, 0x50, 0x4e, 0x47]), // fake PNG header bytes
            addresses: vec![ContactAddress {
                value: "tel:+1".into(),
                kind: "phone".into(),
            }],
        };
        upsert_contact(&c, &alice).unwrap();

        // Second upsert: same uid u1, now Bob with phone +2 and no avatar.
        let bob = Contact {
            uid: "u1".into(),
            display_name: "Bob".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "tel:+2".into(),
                kind: "phone".into(),
            }],
        };
        upsert_contact(&c, &bob).unwrap();

        // Old address (tel:+1) must be gone — full replacement.
        let old = lookup_contact_by_addr(&c, "tel:+1").unwrap();
        assert!(old.is_none(), "old address must be replaced");

        // New address resolves to Bob with no avatar.
        let got = lookup_contact_by_addr(&c, "tel:+2")
            .unwrap()
            .expect("new address must resolve");
        assert_eq!(got.display_name, "Bob");
        assert!(got.avatar.is_none(), "avatar must be replaced with None");
        assert_eq!(got.uid, "u1");
    }

    #[test]
    fn lookup_by_unknown_address_returns_none() {
        let c = db();
        let contact = Contact {
            uid: "u1".into(),
            display_name: "Charlie".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "mailto:charlie@example.com".into(),
                kind: "email".into(),
            }],
        };
        upsert_contact(&c, &contact).unwrap();
        let got = lookup_contact_by_addr(&c, "tel:+999").unwrap();
        assert!(got.is_none(), "unknown address must not match");
    }

    #[test]
    fn clear_contacts_wipes_cache() {
        let c = db();
        let a = Contact {
            uid: "u1".into(),
            display_name: "Alice".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "tel:+1".into(),
                kind: "phone".into(),
            }],
        };
        let b = Contact {
            uid: "u2".into(),
            display_name: "Bob".into(),
            avatar: None,
            addresses: vec![ContactAddress {
                value: "tel:+2".into(),
                kind: "phone".into(),
            }],
        };
        upsert_contact(&c, &a).unwrap();
        upsert_contact(&c, &b).unwrap();

        clear_contacts(&c).unwrap();

        let got_a = lookup_contact_by_addr(&c, "tel:+1").unwrap();
        assert!(got_a.is_none(), "first contact wiped");
        let got_b = lookup_contact_by_addr(&c, "tel:+2").unwrap();
        assert!(got_b.is_none(), "second contact wiped");
    }

    #[test]
    fn two_contacts_distinct_addresses_resolve_independently() {
        let c = db();
        let alice = Contact {
            uid: "u1".into(),
            display_name: "Alice".into(),
            avatar: None,
            addresses: vec![
                ContactAddress {
                    value: "tel:+1111".into(),
                    kind: "phone".into(),
                },
                ContactAddress {
                    value: "mailto:alice@example.com".into(),
                    kind: "email".into(),
                },
            ],
        };
        let bob = Contact {
            uid: "u2".into(),
            display_name: "Bob".into(),
            avatar: None,
            addresses: vec![
                ContactAddress {
                    value: "tel:+2222".into(),
                    kind: "phone".into(),
                },
                ContactAddress {
                    value: "mailto:bob@example.com".into(),
                    kind: "email".into(),
                },
            ],
        };
        upsert_contact(&c, &alice).unwrap();
        upsert_contact(&c, &bob).unwrap();

        let got_alice = lookup_contact_by_addr(&c, "tel:+1111")
            .unwrap()
            .expect("Alice by phone");
        assert_eq!(got_alice.display_name, "Alice");
        assert_eq!(got_alice.uid, "u1");
        assert_eq!(got_alice.addresses.len(), 2, "Alice has both addresses");

        let got_alice_email = lookup_contact_by_addr(&c, "mailto:alice@example.com")
            .unwrap()
            .expect("Alice by email");
        assert_eq!(got_alice_email.display_name, "Alice");
        assert_eq!(got_alice_email.uid, "u1");

        let got_bob = lookup_contact_by_addr(&c, "tel:+2222")
            .unwrap()
            .expect("Bob by phone");
        assert_eq!(got_bob.display_name, "Bob");
        assert_eq!(got_bob.uid, "u2");

        let got_bob_email = lookup_contact_by_addr(&c, "mailto:bob@example.com")
            .unwrap()
            .expect("Bob by email");
        assert_eq!(got_bob_email.display_name, "Bob");
        assert_eq!(got_bob_email.uid, "u2");
    }

    #[test]
    fn migration_bumps_user_version_to_7() {
        let c = db();
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 7, "migration must bump user_version to 7");
    }

    // -------------------------------------------------------------------
    // Local chat deletion — compile errors = intentional red until the
    // `delete_chats_blocking` function lands. The test fails to compile
    // because the symbol does not exist; that compile error is the
    // expected "red" for a fresh spec.
    //
    // Contract:
    //   pub fn delete_chats_blocking(
    //       c: &mut Connection,
    //       chat_ids: &[i64],
    //   ) -> rusqlite::Result<()>
    //
    // Behavior:
    //   * Deletes the selected chat rows and cascades to their messages,
    //     chat_participant rows, and attachments via the existing FK
    //     CASCADE on chat/message/attachment.
    //   * Sweeps handles orphaned by the deletion (no longer referenced
    //     by any chat_participant or any message.sender_handle_id).
    //   * Preserves handles still referenced by remaining chats or
    //     remaining messages.
    //   * Non-selected chats and their messages are untouched.
    //   * Empty `chat_ids` input is a no-op and returns Ok(()).
    // -------------------------------------------------------------------

    #[test]
    fn delete_chats_removes_selected_chats_and_cascades() {
        let mut c = db();

        // Chat A: self + alice. The "self" handle is shared with chat B
        // (it must survive A's deletion). Alice is unique to A — she must
        // be swept as an orphan after A is deleted.
        let chat_a_ref = ChatRef {
            participants: vec![
                "mailto:chrispouliot@icloud.com".into(),
                "mailto:alice@example.com".into(),
            ],
            display_name: None,
            service: Some("iMessage".into()),
        };
        let mut m_a = msg("A1", 1000);
        m_a.chat = chat_a_ref;
        m_a.sender = Some("mailto:alice@example.com".into());
        m_a.attachments = vec![AttachmentRecord {
            guid: Some("att-a".into()),
            mime: Some("image/jpeg".into()),
            name: Some("a.jpg".into()),
            total_bytes: Some(1024),
            local_path: Some("/tmp/a.jpg".into()),
            part_index: Some(0),
            is_sticker: false,
        }];
        apply_blocking(&mut c, Ingest::Message(m_a)).unwrap();

        // Chat B: self + bob. Two messages, one sender. The "self" handle
        // is shared with A; bob is unique to B and must survive (still
        // used by chat B's participant row and by both messages' sender).
        let chat_b_ref = ChatRef {
            participants: vec![
                "mailto:chrispouliot@icloud.com".into(),
                "mailto:bob@example.com".into(),
            ],
            display_name: None,
            service: Some("iMessage".into()),
        };
        let mut m_b1 = msg("B1", 2000);
        m_b1.chat = chat_b_ref.clone();
        m_b1.sender = Some("mailto:bob@example.com".into());
        apply_blocking(&mut c, Ingest::Message(m_b1)).unwrap();
        let mut m_b2 = msg("B2", 2100);
        m_b2.chat = chat_b_ref;
        m_b2.sender = Some("mailto:bob@example.com".into());
        apply_blocking(&mut c, Ingest::Message(m_b2)).unwrap();

        // Resolve chat ids by participant.
        let chats_before = query_chats(&c).unwrap();
        assert_eq!(chats_before.len(), 2, "two chats exist before deletion");
        let chat_a = chats_before
            .iter()
            .find(|ch| ch.participants.iter().any(|p| p.contains("alice")))
            .expect("chat A exists");
        let chat_b = chats_before
            .iter()
            .find(|ch| ch.participants.iter().any(|p| p.contains("bob")))
            .expect("chat B exists");
        let chat_a_id = chat_a.id;
        let chat_b_id = chat_b.id;

        // Sanity: chat A's message has its attachment present.
        let msgs_a_before = query_messages(&c, chat_a_id).unwrap();
        assert_eq!(msgs_a_before.len(), 1);
        assert_eq!(
            msgs_a_before[0].attachments.len(),
            1,
            "chat A has its attachment before deletion"
        );

        // --- ACT: delete chat A only ---
        delete_chats_blocking(&mut c, &[chat_a_id]).unwrap();

        // Chat A is gone; chat B remains.
        let chats_after = query_chats(&c).unwrap();
        assert_eq!(
            chats_after.len(),
            1,
            "selected chat removed, the other remains"
        );
        assert!(
            chats_after.iter().all(|ch| ch.id != chat_a_id),
            "deleted chat A is not in the list"
        );
        assert!(
            chats_after.iter().any(|ch| ch.id == chat_b_id),
            "remaining chat B is preserved"
        );

        // Cascade: messages of chat A are gone.
        let msgs_a_after = query_messages(&c, chat_a_id).unwrap();
        assert!(
            msgs_a_after.is_empty(),
            "messages of deleted chat A cascaded out"
        );

        // Cascade: chat_participant rows of chat A are gone.
        let cp_count: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM chat_participant WHERE chat_id = ?1",
                params![chat_a_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            cp_count, 0,
            "chat_participant rows of deleted chat cascaded"
        );

        // Cascade: attachments of chat A's message are gone.
        let att_count: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM attachment a
                 JOIN message m ON a.message_id = m.id
                 WHERE m.chat_id = ?1",
                params![chat_a_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            att_count, 0,
            "attachments of deleted chat cascaded"
        );

        // Orphan sweep: handle "alice" was only used by chat A (and its
        // message, which is gone) — it must be swept.
        let alice_count: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM handle WHERE address = 'mailto:alice@example.com'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(alice_count, 0, "orphan handle alice swept");

        // Preserved: handle "self" is still used by chat B's participants.
        let self_count: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM handle WHERE address = 'mailto:chrispouliot@icloud.com'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            self_count, 1,
            "self handle preserved (still in chat B)"
        );

        // Preserved: handle "bob" is still used by chat B (chat_participant
        // and as message sender for both B1 and B2).
        let bob_count: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM handle WHERE address = 'mailto:bob@example.com'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bob_count, 1, "bob handle preserved (still in chat B)");

        // Remaining chat B's data is fully intact.
        let msgs_b_after = query_messages(&c, chat_b_id).unwrap();
        assert_eq!(
            msgs_b_after.len(),
            2,
            "remaining chat B's two messages preserved"
        );

        // --- ACT: empty id input — no-op, succeeds ---
        delete_chats_blocking(&mut c, &[]).unwrap();

        // Nothing changed.
        let chats_final = query_chats(&c).unwrap();
        assert_eq!(
            chats_final.len(),
            1,
            "empty delete did not remove anything"
        );
        assert!(
            chats_final.iter().any(|ch| ch.id == chat_b_id),
            "chat B still present after empty delete"
        );
        let msgs_b_final = query_messages(&c, chat_b_id).unwrap();
        assert_eq!(
            msgs_b_final.len(),
            2,
            "chat B's messages still intact after empty delete"
        );
    }
}
