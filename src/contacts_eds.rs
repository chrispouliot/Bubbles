#![cfg(target_os = "linux")]
//! Implementation of [`ContactSource`](crate::contacts::ContactSource) that
//! fetches contacts from the Evolution Data Server over D-Bus.
//!
//! # Architecture
//!
//! 1. Enumerate address-book sources via `org.freedesktop.DBus.ObjectManager`
//!    on `org.gnome.evolution.dataserver.Sources5` at
//!    `/org/gnome/evolution/dataserver/SourceManager`, filtering for sources
//!    whose GKeyFile `Data` contains an `[Address Book]` group (any backend —
//!    local, Google, webdav, etc.).
//! 2. For each matching source UID, call `OpenAddressBook(uid)` on the EDS
//!    `AddressBookFactory` to obtain a dynamic book object path and bus name.
//! 3. Call `Open()` then `GetContactList("#t")` on each resulting
//!    `org.gnome.evolution.dataserver.AddressBook` interface.
//! 4. Parse each vCard string into a [`Contact`] and merge across all books.
//!
//! When EDS is not running the D-Bus call fails with `ServiceUnknown` and the
//! error propagates to [`refresh_cache`](crate::contacts::refresh_cache), which
//! leaves the contact cache untouched.  This is a graceful degradation — the UI
//! will work with the bare-address fallback until EDS is available.

#![allow(dead_code)]

use anyhow::Context;
use async_trait::async_trait;
use zbus::{fdo::ObjectManagerProxy, Connection};

use crate::contacts::ContactSource;
use crate::store::{Contact, ContactAddress};

// ---------------------------------------------------------------------------
// D-Bus proxy declarations (zbus 4.x proc-macro)
// ---------------------------------------------------------------------------

/// Proxy for the EDS address-book factory — used to open a book given its
/// source UID.
#[zbus::proxy(
    interface = "org.gnome.evolution.dataserver.AddressBookFactory",
    default_service = "org.gnome.evolution.dataserver.AddressBook10",
    default_path = "/org/gnome/evolution/dataserver/AddressBookFactory",
    gen_blocking = false,
)]
trait EDSAddressBookFactory {
    /// Open an address book by its source UID, returning a dynamic
    /// `(object_path, bus_name)`. EDS sends the object path as a plain
    /// string (`s`), not a D-Bus object-path signature (`o`), so both
    /// elements are `String`.
    async fn open_address_book(
        &self,
        source_uid: &str,
    ) -> zbus::Result<(String, String)>;
}

/// Proxy for a single opened address book — used to fetch contacts.
#[zbus::proxy(
    interface = "org.gnome.evolution.dataserver.AddressBook",
    gen_blocking = false,
)]
trait EDSAddressBook {
    /// Must be called before any query.  Returns the initial property set
    /// (ignored).
    async fn open(&self) -> zbus::Result<Vec<String>>;

    /// Search contacts by sexp query.  Pass `"#t"` to match all contacts.
    /// Returns an array of vCard 3.0 strings.
    async fn get_contact_list(&self, query: &str) -> zbus::Result<Vec<String>>;
}

// ---------------------------------------------------------------------------
// EdsContactSource
// ---------------------------------------------------------------------------

/// A [`ContactSource`] that reads contacts from the Evolution Data Server
/// (EDS) over the session D-Bus.
pub struct EdsContactSource;

impl Default for EdsContactSource {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl ContactSource for EdsContactSource {
    /// Fetch all contacts from the local EDS address book.
    ///
    /// Returns an error if EDS is not running (or no local address book is
    /// configured), which the caller
    /// [`refresh_cache`](crate::contacts::refresh_cache) treats as a graceful
    /// degradation — the existing cache is left untouched.
    async fn fetch_all(&self) -> anyhow::Result<Vec<Contact>> {
        let conn = Connection::session()
            .await
            .context("failed to connect to D-Bus session bus")?;

        let factory = EDSAddressBookFactoryProxy::new(&conn)
            .await
            .context("failed to connect to EDS AddressBookFactory")?;

        let uids = find_address_book_uids(&conn)
            .await
            .context("failed to discover EDS address-book sources")?;

        if uids.is_empty() {
            anyhow::bail!("no EDS address books found");
        }

        let mut all: Vec<Contact> = Vec::new();

        for uid in &uids {
            // Best-effort per-book: a failing GOA book shouldn't abort the whole fetch.
            match Self::fetch_book(&conn, &factory, uid).await {
                Ok(contacts) => all.extend(contacts),
                Err(e) => eprintln!("EDS: skipping address book '{uid}': {e:#}"),
            }
        }

        Ok(all)
    }
}

impl EdsContactSource {
    /// Open and query a single address book by source UID.
    async fn fetch_book(
        conn: &Connection,
        factory: &EDSAddressBookFactoryProxy<'static>,
        uid: &str,
    ) -> anyhow::Result<Vec<Contact>> {
        let (book_path, book_bus_name) = factory
            .open_address_book(uid)
            .await
            .with_context(|| format!("OpenAddressBook failed for '{uid}'"))?;

        let book = EDSAddressBookProxy::builder(conn)
            .destination(book_bus_name.as_str())
            .context("invalid book bus name")?
            .path(book_path.as_str())
            .context("invalid book object path")?
            .build()
            .await
            .with_context(|| format!("failed to build AddressBook proxy for '{uid}'"))?;

        book.open()
            .await
            .with_context(|| format!("EDS Open() failed for '{uid}'"))?;

        let vcards = book
            .get_contact_list("#t")
            .await
            .with_context(|| format!("GetContactList failed for '{uid}'"))?;

        let mut contacts = Vec::with_capacity(vcards.len());
        for vcard in vcards {
            if let Some(c) = parse_vcard(&vcard) {
                contacts.push(c);
            }
        }
        Ok(contacts)
    }
}

// ---------------------------------------------------------------------------
// Address-book source discovery (ObjectManager on SourceManager)
// ---------------------------------------------------------------------------

/// Enumerate EDS address-book source UIDs via the `ObjectManager` interface
/// on `Sources5` at `/org/gnome/evolution/dataserver/SourceManager`.
///
/// Returns the UIDs of every source whose GKeyFile `Data` contains an
/// `[Address Book]` group, regardless of backend (local, Google, webdav, etc.).
async fn find_address_book_uids(conn: &Connection) -> anyhow::Result<Vec<String>> {
    let om = ObjectManagerProxy::builder(conn)
        .destination("org.gnome.evolution.dataserver.Sources5")
        .context("invalid Sources5 bus name")?
        .path("/org/gnome/evolution/dataserver/SourceManager")
        .context("invalid SourceManager object path")?
        .build()
        .await
        .context("failed to create ObjectManager proxy for EDS SourceManager")?;

    let objects = om
        .get_managed_objects()
        .await
        .context("EDS SourceManager GetManagedObjects failed")?;

    let mut uids = Vec::new();
    for interfaces in objects.values() {
        for (iface_name, props) in interfaces {
            if iface_name.as_str() != "org.gnome.evolution.dataserver.Source" {
                continue;
            }
            let uid: &str = match props.get("UID").and_then(|v| v.try_into().ok()) {
                Some(s) => s,
                None => continue,
            };
            let data: &str = match props.get("Data").and_then(|v| v.try_into().ok()) {
                Some(s) => s,
                None => continue,
            };
            if is_address_book(data) {
                uids.push(uid.to_owned());
            }
        }
    }
    Ok(uids)
}

/// Check whether a `Data` GKeyFile string represents an address-book source:
/// it must contain a `[Address Book]` group. Backend (`local`, `google`,
/// `webdav`, …) is not filtered — any address book the user has configured
/// is included so the integration matches what Gnome Contacts shows.
fn is_address_book(data: &str) -> bool {
    for line in data.lines() {
        let line = line.trim();
        if line == "[Address Book]" {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// vCard 3.0 parser
// ---------------------------------------------------------------------------

/// Parse a vCard 3.0 string into a [`Contact`].
///
/// Returns `None` if the vCard lacks a `UID` property (every EDS contact
/// should have one, but we defend against malformed data).
fn parse_vcard(vcard: &str) -> Option<Contact> {
    let unfolded = unfold_vcard(vcard);

    let mut uid: Option<String> = None;
    let mut display_name: Option<String> = None;
    let mut avatar: Option<Vec<u8>> = None;
    let mut addresses: Vec<ContactAddress> = Vec::new();

    for line in unfolded.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Skip BEGIN / END / VERSION.
        if line.starts_with("BEGIN:") || line.starts_with("END:") || line.starts_with("VERSION:")
        {
            continue;
        }

        // Split header (name + params) from value at the first colon.
        let colon = line.find(':')?;
        let header = &line[..colon];
        let value = &line[colon + 1..];

        // Property name is everything before the first semicolon (if any).
        let name = header.split(';').next().unwrap_or(header);

        match name {
            "FN" => {
                display_name = Some(value.to_owned());
            }
            "UID" => {
                uid = Some(value.to_owned());
            }
            "TEL" => {
                addresses.push(ContactAddress {
                    value: format!("tel:{}", value),
                    kind: "phone".into(),
                });
            }
            "EMAIL" => {
                addresses.push(ContactAddress {
                    value: format!("mailto:{}", value),
                    kind: "email".into(),
                });
            }
            "PHOTO" => {
                // vCard 3.0 uses ENCODING=b for base64.
                let upper_header = header.to_uppercase();
                if upper_header.contains("ENCODING=B") {
                    let decoded = glib::base64_decode(value);
                    if !decoded.is_empty() {
                        avatar = Some(decoded);
                    }
                }
            }
            _ => {}
        }
    }

    // Every contact must have at least a UID.
    Some(Contact {
        uid: uid?,
        display_name: display_name.unwrap_or_default(),
        avatar,
        addresses,
    })
}

/// Unfold continuation lines in a vCard 3.0 string.
///
/// A line beginning with a single space (or tab) is a continuation of the
/// previous line — strip the leading whitespace and append.
fn unfold_vcard(vcard: &str) -> String {
    let mut result = String::new();
    let mut carry = String::new();

    for line in vcard.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            // Continuation: strip leading whitespace, append to previous line.
            carry.push_str(line.trim_start());
        } else {
            // Flush the carried line first.
            if !carry.is_empty() {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(&carry);
            }
            carry = line.to_owned();
        }
    }

    // Flush the final carried line.
    if !carry.is_empty() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&carry);
    }

    result
}
