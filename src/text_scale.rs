//! Persist and restore the chat text size offset (in points) so the user's
//! preference survives app restarts.

use std::path::PathBuf;

use gtk::glib;

const STATE_FILE: &str = "text_scale.txt";
const DEFAULT_OFFSET: f64 = 0.0;
/// Minimum chat-text-size offset the UI is willing to step down to. Used
/// by the +/- stepper to clamp and to disable the "-" button at the floor.
pub const MIN_OFFSET: f64 = -5.0;
/// Maximum chat-text-size offset the UI is willing to step up to. Used
/// by the +/- stepper to clamp and to disable the "+" button at the ceiling.
pub const MAX_OFFSET: f64 = 5.0;

// Per-thread override so parallel tests don't race on the state file.
#[cfg(test)]
thread_local! {
    static TEST_DATA_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Redirect the data directory for the current test thread. Only available
/// in test builds. Each test should call this before the first `get()`/`set()`
/// to isolate its state file from other threads.
#[cfg(test)]
pub(crate) fn set_data_dir_for_tests(path: PathBuf) {
    TEST_DATA_DIR.with(|d| *d.borrow_mut() = Some(path));
}

fn data_dir() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(path) = TEST_DATA_DIR.with(|d| d.borrow().clone()) {
            return path;
        }
    }
    glib::user_data_dir().join("bubbles")
}

fn state_path() -> PathBuf {
    data_dir().join(STATE_FILE)
}

/// Read the saved text size offset (in points), or the default if nothing is
/// saved yet.
pub fn get() -> f64 {
    let data = match std::fs::read_to_string(state_path()) {
        Ok(d) => d,
        Err(_) => return DEFAULT_OFFSET,
    };
    let val: f64 = data.trim().parse().unwrap_or(DEFAULT_OFFSET);
    if (MIN_OFFSET..=MAX_OFFSET).contains(&val) {
        val
    } else {
        DEFAULT_OFFSET
    }
}

/// Save a text size offset (in points) to disk. Creates the parent directory
/// if needed.
pub fn set(val: f64) {
    let clamped = val.clamp(MIN_OFFSET, MAX_OFFSET);
    let path = state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, format!("{:.1}", clamped));
}
