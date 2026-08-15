//! Image/video/thumbnail/lightbox helpers extracted from [`super`](mod.rs).
//! Decodes textures, manages thumbnail caches, builds image/video widgets,
//! and controls the lightbox overlay.

use super::*;

/// Decode raw image bytes (PNG/JPEG/etc.) into a `gdk::Texture` for avatar display.
pub(super) fn texture_from_bytes(bytes: &[u8]) -> Option<gtk::gdk::Texture> {
    gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from(bytes)).ok()
}

/// Load a texture from `path`. HEIC/HEIF files are decoded via libheif-rs;
/// all other formats are decoded via gdk-pixbuf, with EXIF orientation applied
/// to the decoded RGBA pixels before wrapping in a `MemoryTexture`.
pub(super) fn load_texture(path: &str) -> Option<gtk::gdk::Texture> {
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

#[derive(Clone)]
struct CachedThumbnail {
    texture: gtk::gdk::Texture,
    width: i32,
    height: i32,
}

thread_local! {
    static THUMBNAIL_CACHE: RefCell<HashMap<String, CachedThumbnail>> = RefCell::new(HashMap::new());
}

pub(super) fn thumbnail_size(width: i32, height: i32, max_w: f64, max_h: f64) -> (i32, i32) {
    let scale = (max_w / width.max(1) as f64)
        .min(max_h / height.max(1) as f64)
        .min(1.0);
    (
        (width as f64 * scale).round() as i32,
        (height as f64 * scale).round() as i32,
    )
}

fn cached_thumbnail(path: &str) -> Option<CachedThumbnail> {
    THUMBNAIL_CACHE.with(|cache| cache.borrow().get(path).cloned())
}

fn store_thumbnail(path: &str, texture: &gtk::gdk::Texture, width: i32, height: i32) {
    const MAX_CACHED_THUMBNAILS: usize = 128;

    THUMBNAIL_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= MAX_CACHED_THUMBNAILS && !cache.contains_key(path) {
            if let Some(key) = cache.keys().next().cloned() {
                cache.remove(&key);
            }
        }
        cache.insert(
            path.to_string(),
            CachedThumbnail {
                texture: texture.clone(),
                width,
                height,
            },
        );
    });
}

/// After a thumbnail decode changes the widget's size request, shift the
/// scroll position only if the widget is entirely above the visible viewport.
fn maybe_adjust_scroll_for_thumbnail(pic: &gtk::Widget, old_h: i32, new_h: i32) {
    let delta = new_h - old_h;
    if delta == 0 {
        return;
    }
    let mut cur = pic.parent();
    while let Some(p) = cur {
        if let Some(sw) = p.downcast_ref::<gtk::ScrolledWindow>() {
            let should_anchor = pic
                .compute_bounds(sw)
                .map(|r| (r.y() as f64 + r.height() as f64) <= 0.0)
                .unwrap_or(false);
            if should_anchor {
                let adj = sw.vadjustment();
                adj.set_value((adj.value() + delta as f64).max(0.0));
            }
            break;
        }
        cur = p.parent();
    }
}

/// An image widget that decodes on a background thread and swaps in the
/// finished texture when ready.  Returns a placeholder immediately so the
/// chat opens without blocking.
pub(super) fn image_widget(path: &str, dimensions: Option<(i32, i32)>) -> gtk::Widget {
    image_widget_with_motion(path, dimensions, None)
}

/// An image widget with an optional paired motion file.  The still remains the
/// primary click target; when a motion file is present, a separate corner
/// button opens the video lightbox without changing the still's click behavior.
pub(super) fn live_photo_widget(
    still_path: &str,
    motion_path: &str,
    dimensions: Option<(i32, i32)>,
) -> gtk::Widget {
    image_widget_with_motion(still_path, dimensions, Some(motion_path))
}

fn image_widget_with_motion(
    path: &str,
    dimensions: Option<(i32, i32)>,
    motion_path: Option<&str>,
) -> gtk::Widget {
    const CHAT_THUMBNAIL_MAX_EDGE: u32 = 1024;
    const MAX_W: f64 = 260.0;
    const MAX_H: f64 = 340.0;

    let pic = gtk::Picture::new();
    pic.set_size_request(MAX_W as i32, MAX_H as i32);
    pic.set_content_fit(gtk::ContentFit::Contain);
    pic.set_overflow(gtk::Overflow::Hidden);
    pic.add_css_class("attachment-image");
    pic.set_cursor_from_name(Some("pointer"));

    // Owned for the 'static decode callback below.
    let path_string = path.to_string();

    if let Some((width, height)) = dimensions {
        let (thumb_w, thumb_h) = thumbnail_size(width, height, MAX_W, MAX_H);
        pic.set_size_request(thumb_w, thumb_h);
    }

    let has_cached_thumbnail = if let Some(cached) = cached_thumbnail(path) {
        pic.set_size_request(cached.width, cached.height);
        pic.set_paintable(Some(&cached.texture));
        true
    } else {
        false
    };

    // Schedule background decode via the image scheduler.
    if !has_cached_thumbnail {
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
                            let (new_w, new_h) = thumbnail_size(w, h, MAX_W, MAX_H);

                            let old_h = pic.height_request();
                            pic.set_size_request(new_w, new_h);
                            if old_h != new_h {
                                maybe_adjust_scroll_for_thumbnail(
                                    pic.upcast_ref(),
                                    old_h,
                                    new_h,
                                );
                            }
                            pic.set_paintable(Some(&texture));
                            store_thumbnail(&path_string, &texture, new_w, new_h);
                            log::debug!("image thumbnail decoded: {w}x{h} for {path_string}");
                        }
                        Err(e) => log::warn!("image thumbnail decode failed for {path_string}: {e}"),
                    }
                }
            }
        });
    }

    // Click to enlarge: find the lightbox host overlay and layer the full image.
    let gesture = gtk::GestureClick::new();
    let path_owned = path.to_string();
    let motion_path_owned = motion_path.map(str::to_owned);
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
        show_lightbox(&host, &path_owned, motion_path_owned.as_deref());
    });
    pic.add_controller(gesture);

    let Some(motion_path) = motion_path else {
        return pic.upcast();
    };

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&pic));

    let live_button = gtk::Button::from_icon_name("media-playback-start-symbolic");
    live_button.add_css_class("live-photo-button");
    live_button.set_tooltip_text(Some("Play Live Photo"));
    live_button.set_halign(gtk::Align::End);
    live_button.set_valign(gtk::Align::End);
    live_button.set_margin_end(8);
    live_button.set_margin_bottom(8);

    let motion_path_owned = motion_path.to_string();
    let inline_playback: Rc<RefCell<Option<InlineVideoPlayback>>> = Rc::new(RefCell::new(None));
    let inline_playback_for_cleanup = inline_playback.clone();
    overlay.connect_unrealize(move |_| {
        // Removing an attachment can leave the GTK object alive briefly via
        // signal closures.  Drop the pipeline explicitly rather than letting
        // that lifetime keep playback running after the thumbnail is gone.
        inline_playback_for_cleanup.borrow_mut().take();
    });

    let pic_for_motion = pic.downgrade();
    let inline_playback_for_click = inline_playback.clone();
    live_button.connect_clicked(move |_| {
        let Some(pic) = pic_for_motion.upgrade() else {
            return;
        };

        // Keep the motion preview in the existing Picture allocation.  In
        // particular, do not create a lightbox or change the still's size
        // request when its paired video starts.
        crate::video::ensure_gst_init();
        let Some(playback) = InlineVideoPlayback::new(&motion_path_owned) else {
            log::warn!("Live Photo play: could not create pipeline for {motion_path_owned}");
            return;
        };
        pic.set_paintable(Some(&playback.paintable));
        *inline_playback_for_click.borrow_mut() = Some(playback);
    });
    overlay.add_overlay(&live_button);

    overlay.upcast()
}

/// A playbin and its paintable for an inline Live Photo preview.
///
/// The pipeline is owned by the thumbnail's widget tree and is stopped as
/// soon as that tree is unrooted (or when a new playback replaces it).
struct InlineVideoPlayback {
    playbin: gstreamer::Element,
    paintable: gtk::gdk::Paintable,
}

impl InlineVideoPlayback {
    fn new(path: &str) -> Option<Self> {
        use gstreamer as gst;
        use gst::prelude::ElementExt;

        let playbin = gst::ElementFactory::make("playbin")
            .name("inline-playbin")
            .build()
            .ok()?;
        let uri = format!("file://{path}");
        playbin.set_property("uri", &uri);

        let video_sink = gst::ElementFactory::make("gtk4paintablesink")
            .name("inline-video-sink")
            .build()
            .ok()?;
        playbin.set_property("video-sink", &video_sink);
        let paintable: gtk::gdk::Paintable = video_sink.property("paintable");

        playbin.set_state(gst::State::Playing).ok()?;
        Some(Self { playbin, paintable })
    }
}

impl Drop for InlineVideoPlayback {
    fn drop(&mut self) {
        use gstreamer::prelude::ElementExt;
        let _ = self.playbin.set_state(gstreamer::State::Null);
    }
}

/// A video thumbnail widget that decodes a single frame on a background thread
/// and swaps in the finished texture when ready.  Returns a placeholder with a
/// centered play-button overlay immediately so the chat opens without blocking.
pub(super) fn video_widget(path: &str, dimensions: Option<(i32, i32)>) -> gtk::Widget {
    const CHAT_THUMBNAIL_MAX_EDGE: u32 = 1024;
    const MAX_W: f64 = 260.0;
    const MAX_H: f64 = 340.0;

    let pic = gtk::Picture::new();
    pic.set_size_request(MAX_W as i32, MAX_H as i32);
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

    // Owned for the 'static decode callback below.
    let path_string = path.to_string();

    if let Some((width, height)) = dimensions {
        let (thumb_w, thumb_h) = thumbnail_size(width, height, MAX_W, MAX_H);
        pic.set_size_request(thumb_w, thumb_h);
    }

    let has_cached_thumbnail = if let Some(cached) = cached_thumbnail(path) {
        pic.set_size_request(cached.width, cached.height);
        pic.set_paintable(Some(&cached.texture));
        true
    } else {
        false
    };

    // Schedule background decode via the video scheduler.
    if !has_cached_thumbnail {
        let weak = pic.downgrade();
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
                                let (new_w, new_h) = thumbnail_size(w, h, MAX_W, MAX_H);

                                let old_h = pic.height_request();
                                pic.set_size_request(new_w, new_h);
                                if old_h != new_h {
                                    maybe_adjust_scroll_for_thumbnail(
                                        pic.upcast_ref(),
                                        old_h,
                                        new_h,
                                    );
                                }
                                pic.set_paintable(Some(&texture));
                                store_thumbnail(&path_string, &texture, new_w, new_h);
                                log::debug!("video thumbnail decoded: {w}x{h} for {path_string}");
                            }
                            Err(e) => {
                                log::warn!("video thumbnail decode failed for {path_string}: {e}")
                            }
                        }
                    }
                }
            },
        );
    }

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
/// press Escape to dismiss. A paired motion file gets a corner play affordance
/// which opens the video lightbox without changing dismissal behavior.
fn show_lightbox(host: &gtk::Overlay, path: &str, motion_path: Option<&str>) {
    log::debug!("show_lightbox: opening {path}");
    let Some(texture) = load_texture(path) else {
        log::warn!("show_lightbox: load_texture returned None for {path}");
        return;
    };

    let dim = gtk::Box::new(gtk::Orientation::Vertical, 0);
    dim.add_css_class("lightbox-dim");
    dim.set_hexpand(true);
    dim.set_vexpand(true);

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
    let content = gtk::Overlay::new();
    content.set_hexpand(true);
    content.set_vexpand(true);
    content.set_child(Some(&pic));

    let live_button = if let Some(motion_path) = motion_path {
        let live_button = gtk::Button::from_icon_name("media-playback-start-symbolic");
        live_button.add_css_class("live-photo-button");
        live_button.set_tooltip_text(Some("Play Live Photo"));
        live_button.set_halign(gtk::Align::End);
        live_button.set_valign(gtk::Align::End);
        live_button.set_margin_end(16);
        live_button.set_margin_bottom(16);

        content.add_overlay(&live_button);
        Some((live_button, motion_path.to_string()))
    } else {
        None
    };
    dim.append(&content);

    // Toplevel window for reliable key event capture.
    let toplevel = host.root().and_then(|r| r.downcast::<gtk::Window>().ok());

    // Escape handler on the toplevel window in Capture phase.
    // Capture fires before focus routing, so focus location doesn't matter.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let keys_for_click = keys.clone();
    let host_k = host.clone();
    let dim_k = dim.clone();
    let tl_esc = toplevel.clone();
    keys.connect_key_pressed(move |ctrl, key, _, _| {
        if key == gtk::gdk::Key::Escape && dim_k.parent().is_some() {
            if let Some(ref win) = tl_esc {
                win.remove_controller(ctrl);
            }
            host_k.remove_overlay(&dim_k);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    let keys_for_motion = keys.clone();
    if let Some(ref win) = toplevel {
        win.add_controller(keys);
    }

    // Move from the still preview to the video viewer rather than stacking two
    // lightboxes. This keeps Escape and outside-click dismissal unambiguous.
    if let Some((live_button, motion_path)) = live_button {
        let host_for_motion = host.clone();
        let dim_for_motion = dim.clone();
        let tl_motion = toplevel.clone();
        live_button.connect_clicked(move |_| {
            if let Some(ref win) = tl_motion {
                win.remove_controller(&keys_for_motion);
            }
            host_for_motion.remove_overlay(&dim_for_motion);
            show_video_lightbox(&host_for_motion, &motion_path);
        });
    }

    // Click on the dim layer dismisses (also removes the key controller).
    let click = gtk::GestureClick::new();
    let host_c = host.clone();
    let dim_c = dim.clone();
    let tl_click = toplevel.clone();
    click.connect_released(move |_, _, _, _| {
        if let Some(ref win) = tl_click {
            win.remove_controller(&keys_for_click);
        }
        host_c.remove_overlay(&dim_c);
    });
    dim.add_controller(click);

    host.add_overlay(&dim);
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

    // Toplevel window for reliable key event capture.
    let toplevel = host.root().and_then(|r| r.downcast::<gtk::Window>().ok());

    // Escape handler on the toplevel window in Capture phase.
    // Capture fires before focus routing, so focus location doesn't matter.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let keys_for_dismiss = keys.clone();
    let host_k = host.clone();
    let dim_k = dim.clone();
    let pb_esc = playbin.clone();
    let tl_esc = toplevel.clone();
    keys.connect_key_pressed(move |ctrl, key, _, _| {
        if key == gtk::gdk::Key::Escape && dim_k.parent().is_some() {
            let _ = pb_esc.set_state(gst::State::Null);
            if let Some(ref win) = tl_esc {
                win.remove_controller(ctrl);
            }
            host_k.remove_overlay(&dim_k);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    if let Some(ref win) = toplevel {
        win.add_controller(keys);
    }

    // Click on the dim background (outside the picture) dismisses, also
    // removes the key controller. Default Bubble phase fires for clicks on
    // the dim's empty area; clicks on the picture are caught in Capture
    // phase by toggle_gesture above.
    let dismiss_gesture = gtk::GestureClick::new();
    let host_c = host.clone();
    let dim_c = dim.clone();
    let pb_dismiss = playbin.clone();
    let tl_click = toplevel.clone();
    dismiss_gesture.connect_released(move |_, _, _, _| {
        let _ = pb_dismiss.set_state(gst::State::Null);
        if let Some(ref win) = tl_click {
            win.remove_controller(&keys_for_dismiss);
        }
        host_c.remove_overlay(&dim_c);
    });
    dim.add_controller(dismiss_gesture);

    host.add_overlay(&dim);
}
