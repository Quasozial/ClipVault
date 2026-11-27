//! Game window capture (Windows.Graphics.Capture): pure geometry and picture
//! decision helpers, platform independent so they can be unit tested.

/// Pixel rectangle inside a frame or output picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Rounds to the nearest even number, at least 2 and at most `cap`.
fn even(v: f64, cap: u32) -> u32 {
    let e = ((2.0 * (v / 2.0).round()) as u32).max(2);
    e.min(cap & !1).max(2)
}

/// Fits a `src_w` x `src_h` picture into `out_w` x `out_h`, preserving the
/// aspect ratio and centring it. Width, height and position are even (NV12).
pub fn letterbox(src_w: u32, src_h: u32, out_w: u32, out_h: u32) -> Rect {
    let scale = (out_w as f64 / src_w.max(1) as f64).min(out_h as f64 / src_h.max(1) as f64);
    let w = even(src_w as f64 * scale, out_w);
    let h = even(src_h as f64 * scale, out_h);
    Rect { x: (out_w.saturating_sub(w) / 2) & !1, y: (out_h.saturating_sub(h) / 2) & !1, w, h }
}

/// Client area inside a captured window frame. `bounds` is the DWM extended
/// frame bounds (left, top, right, bottom) and `client` the client rect, both
/// in screen pixels. The result is the client relative to `bounds`, clamped to
/// the frame; an empty or invalid client yields the whole frame.
pub fn client_crop(
    frame_w: u32,
    frame_h: u32,
    bounds: (i32, i32, i32, i32),
    client: (i32, i32, i32, i32),
) -> Rect {
    let full = Rect { x: 0, y: 0, w: frame_w, h: frame_h };
    let left = (client.0 - bounds.0).clamp(0, frame_w as i32);
    let top = (client.1 - bounds.1).clamp(0, frame_h as i32);
    let right = (client.2 - bounds.0).clamp(0, frame_w as i32);
    let bottom = (client.3 - bounds.1).clamp(0, frame_h as i32);
    if right <= left || bottom <= top {
        return full;
    }
    Rect { x: left as u32, y: top as u32, w: (right - left) as u32, h: (bottom - top) as u32 }
}

/// What a tick sends to the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowPicture {
    /// The newest captured frame.
    New,
    /// The previous picture again (static window).
    Same,
    /// A black picture.
    Black,
}

/// Inputs of [`decide`].
#[derive(Clone, Copy)]
pub struct PictureState {
    pub found: bool,
    pub minimized: bool,
    pub capturing: bool,
    pub new_frame: bool,
    pub have_last: bool,
}

/// Black while the window is missing, minimized or not captured; otherwise
/// the new frame, else the last picture, else black. The caller must clear
/// `have_last` whenever Black is decided, so a restored window stays black
/// until a fresh frame arrives instead of showing a stale picture.
pub fn decide(s: &PictureState) -> WindowPicture {
    if !s.found || s.minimized || !s.capturing {
        WindowPicture::Black
    } else if s.new_frame {
        WindowPicture::New
    } else if s.have_last {
        WindowPicture::Same
    } else {
        WindowPicture::Black
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] fn letterbox_same_aspect() { assert_eq!(letterbox(3840, 2160, 1920, 1080), Rect { x: 0, y: 0, w: 1920, h: 1080 }); }
    #[test] fn letterbox_ultrawide() { assert_eq!(letterbox(3440, 1440, 1920, 1080), Rect { x: 0, y: 138, w: 1920, h: 804 }); }
    #[test] fn letterbox_4_3() { assert_eq!(letterbox(1600, 1200, 1920, 1080), Rect { x: 240, y: 0, w: 1440, h: 1080 }); }
    #[test] fn letterbox_portrait() { assert_eq!(letterbox(1080, 1920, 1920, 1080), Rect { x: 656, y: 0, w: 608, h: 1080 }); }
    #[test] fn letterbox_tiny_source() { let r = letterbox(1, 1, 1920, 1080); assert!(r.w >= 2 && r.h >= 2 && r.x % 2 == 0 && r.y % 2 == 0); }
    #[test] fn letterbox_never_exceeds_output() {
        for (w, h) in [(1281, 721), (1366, 768), (2559, 1079), (7, 3000)] {
            let r = letterbox(w, h, 1920, 1080);
            assert!(r.x + r.w <= 1920 && r.y + r.h <= 1080, "{w}x{h}: {r:?}");
            assert!(r.w % 2 == 0 && r.h % 2 == 0 && r.x % 2 == 0 && r.y % 2 == 0, "{w}x{h}: {r:?}");
        }
    }
    #[test] fn client_crop_windowed() {
        assert_eq!(client_crop(1280, 720, (100, 100, 1380, 820), (108, 131, 1372, 812)), Rect { x: 8, y: 31, w: 1264, h: 681 });
    }
    #[test] fn client_crop_borderless_is_full() {
        assert_eq!(client_crop(1920, 1080, (0, 0, 1920, 1080), (0, 0, 1920, 1080)), Rect { x: 0, y: 0, w: 1920, h: 1080 });
    }
    #[test] fn client_crop_clamped_and_empty() {
        assert_eq!(client_crop(800, 600, (0, 0, 800, 600), (-50, -50, 900, 700)), Rect { x: 0, y: 0, w: 800, h: 600 });
        assert_eq!(client_crop(800, 600, (0, 0, 800, 600), (10, 10, 10, 10)), Rect { x: 0, y: 0, w: 800, h: 600 });
    }
    #[test] fn decide_cases() {
        let base = PictureState { found: true, minimized: false, capturing: true, new_frame: false, have_last: true };
        assert_eq!(decide(&PictureState { found: false, ..base }), WindowPicture::Black);
        assert_eq!(decide(&PictureState { minimized: true, new_frame: true, ..base }), WindowPicture::Black);
        assert_eq!(decide(&PictureState { capturing: false, ..base }), WindowPicture::Black);
        assert_eq!(decide(&PictureState { new_frame: true, ..base }), WindowPicture::New);
        assert_eq!(decide(&base), WindowPicture::Same);
    }
    #[test] fn restore_waits_for_fresh_frame() {
        // After a Black (minimized), the caller cleared have_last: restored window with no new frame stays black.
        let s = PictureState { found: true, minimized: false, capturing: true, new_frame: false, have_last: false };
        assert_eq!(decide(&s), WindowPicture::Black);
    }
}
