//! Avatar editing types and the persisted `apply_chat_edit` function.
//!
//! Extracted from [`super`](mod.rs). Re-exported at `crate::ui::AvatarEdit` and
//! `crate::ui::apply_chat_edit` to preserve the existing API surface.

use std::path::Path;

use crate::store::Store;

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

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod avatar_save_tests {
    use super::*;
    use tempfile::TempDir;

    use crate::store::{ChatRef, IncomingMessage, Ingest};

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
