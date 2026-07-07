//! Text-formatting helpers: Pango-markup-safe URL linkification,
//! markup escaping, and system-browser URI opening.
//!
//! Extracted from [`super`](mod.rs). Pure text transformations — no GTK widget
//! construction, no global state beyond the cached URL regex.

use regex::Regex;
use std::sync::OnceLock;

/// Regex that matches URLs at word boundaries.
static URL_RE: OnceLock<Regex> = OnceLock::new();

fn url_re() -> &'static Regex {
    URL_RE.get_or_init(|| {
        Regex::new(r#"(?i)\b(?:https?://|www\.)[^\s<>'"{}\[\]()]+[^\s<>'"{}\[\]()\.,:;!?)]"#).unwrap()
    })
}

/// Convert plain text containing URLs into Pango markup with clickable `<a>` tags.
/// Non-URL text is escaped for markup safety.
pub(super) fn text_to_markup(text: &str) -> String {
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
pub(super) fn open_uri(uri: &str) {
    let uri = if uri.starts_with("www.") && !uri.starts_with("http") {
        format!("https://{uri}")
    } else {
        uri.to_string()
    };
    if let Err(e) = gtk::gio::AppInfo::launch_default_for_uri(&uri, None::<&gtk::gio::AppLaunchContext>) {
        eprintln!("failed to open URI {}: {e}", uri);
    }
}
