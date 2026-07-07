//! Pure reaction-table data and lookup helpers (no GTK widgets).
//!
//! Extracted from `mod.rs` so the emoji table and its tests live in their own
//! module without pulling in the rest of the UI.

/// A single entry in the Apple tapback reaction table.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct ReactionEntry {
    pub(crate) emoji: &'static str,
    pub(crate) label: &'static str,
}

/// The 6 standard Apple tapback reactions, indexed 0..=5.
/// Add codes are 2000 + index; remove codes are 3000 + index.
#[allow(dead_code)]
pub(crate) const REACTIONS: [ReactionEntry; 6] = [
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
pub(crate) fn code_to_emoji(code: i64) -> Option<&'static str> {
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
