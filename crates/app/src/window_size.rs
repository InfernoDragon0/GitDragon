//! The main window's remembered size and place, and the preset sizes the Settings page offers.
//! The pure part (what to restore, given the displays that exist now) lives here so it can be
//! tested; `main.rs` opens the window with it, the workspace records changes, and the Settings
//! page applies presets.

use gpui_kit::{Bounds, Pixels, Window, WindowBounds, point, px, size};
use serde::{Deserialize, Serialize};

/// First-launch size, and the fallback when nothing usable is saved.
pub const DEFAULT_SIZE: (f32, f32) = (1280., 800.);
/// The window cannot be dragged smaller than this.
pub const MIN_SIZE: (f32, f32) = (800., 500.);
/// Sizes the Settings page offers.
pub const PRESETS: [(f32, f32); 4] = [(1280., 800.), (1440., 900.), (1600., 1000.), (1920., 1080.)];
/// At least this much of the window must lie on a display for its saved place to be kept.
const MIN_OVERLAP: (f32, f32) = (96., 48.);

/// A plain rectangle in logical pixels (GPUI's `Bounds` is awkward to build in tests).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn overlap(&self, other: &Rect) -> (f32, f32) {
        let w = (self.x + self.width).min(other.x + other.width) - self.x.max(other.x);
        let h = (self.y + self.height).min(other.y + other.height) - self.y.max(other.y);
        (w.max(0.), h.max(0.))
    }

    fn centered_in(display: &Rect, width: f32, height: f32) -> Rect {
        Rect::new(
            display.x + (display.width - width) / 2.,
            display.y + (display.height - height) / 2.,
            width,
            height,
        )
    }
}

impl From<Bounds<Pixels>> for Rect {
    fn from(b: Bounds<Pixels>) -> Self {
        Rect::new(
            b.origin.x.into(),
            b.origin.y.into(),
            b.size.width.into(),
            b.size.height.into(),
        )
    }
}

impl From<Rect> for Bounds<Pixels> {
    fn from(r: Rect) -> Self {
        Bounds::new(point(px(r.x), px(r.y)), size(px(r.width), px(r.height)))
    }
}

/// Where the window was last, saved in `state.json` (field `window`). When maximized, the rect
/// is the size to come back to on un-maximizing.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    #[serde(flatten)]
    pub rect: Rect,
    #[serde(default)]
    pub maximized: bool,
}

impl SavedWindow {
    /// What the window is now. Fullscreen is remembered as maximized.
    pub fn capture(window: &Window) -> Self {
        let (bounds, maximized) = match window.window_bounds() {
            WindowBounds::Windowed(b) => (b, false),
            WindowBounds::Maximized(b) | WindowBounds::Fullscreen(b) => (b, true),
        };
        Self {
            rect: bounds.into(),
            maximized,
        }
    }

    /// Whether the numbers are sane enough to keep (a minimized window can report junk).
    pub fn is_usable(&self) -> bool {
        let r = &self.rect;
        [r.x, r.y, r.width, r.height].iter().all(|v| v.is_finite())
            && r.width >= MIN_SIZE.0
            && r.height >= MIN_SIZE.1
    }
}

/// Where to open the window: the saved rect kept on the display it mostly sits on (size capped
/// to that display and the window pulled back inside it), or, when it overlaps no display (a
/// monitor was unplugged), centred on `primary` with the saved size capped to it. Nothing saved
/// (or junk) gives the default size centred on `primary`.
pub fn restore(saved: Option<&SavedWindow>, displays: &[Rect], primary: Rect) -> Rect {
    let default = || {
        let (w, h) = DEFAULT_SIZE;
        Rect::centered_in(&primary, w.min(primary.width), h.min(primary.height))
    };
    let Some(saved) = saved.filter(|s| s.is_usable()) else {
        return default();
    };
    let fit = |display: &Rect| {
        let width = saved.rect.width.min(display.width);
        let height = saved.rect.height.min(display.height);
        (width, height)
    };
    let best = displays
        .iter()
        .map(|d| {
            let (w, h) = saved.rect.overlap(d);
            (d, w * h, (w, h))
        })
        .filter(|(_, _, (w, h))| *w >= MIN_OVERLAP.0 && *h >= MIN_OVERLAP.1)
        .max_by(|a, b| a.1.total_cmp(&b.1));
    match best {
        Some((display, _, _)) => {
            let (width, height) = fit(display);
            let x = saved
                .rect
                .x
                .clamp(display.x, display.x + display.width - width);
            let y = saved
                .rect
                .y
                .clamp(display.y, display.y + display.height - height);
            Rect::new(x, y, width, height)
        }
        None => {
            let (width, height) = fit(&primary);
            Rect::centered_in(&primary, width, height)
        }
    }
}

/// `WindowBounds` for `WindowOptions`, from what is saved and the displays that exist now.
pub fn initial_bounds(saved: Option<&SavedWindow>, cx: &gpui_kit::App) -> WindowBounds {
    let displays: Vec<Rect> = cx
        .displays()
        .iter()
        .map(|d| d.visible_bounds().into())
        .collect();
    let Some(primary) = cx.primary_display().map(|d| Rect::from(d.visible_bounds())) else {
        // No display information: let GPUI centre the default size.
        let (w, h) = DEFAULT_SIZE;
        return WindowBounds::Windowed(Bounds::centered(None, size(px(w), px(h)), cx));
    };
    let rect = restore(saved, &displays, primary);
    if saved.is_some_and(|s| s.maximized && s.is_usable()) {
        WindowBounds::Maximized(rect.into())
    } else {
        WindowBounds::Windowed(rect.into())
    }
}

/// A preset fits when it is no bigger than the usable area of the display.
pub fn preset_fits(preset: (f32, f32), display: Option<Rect>) -> bool {
    display.is_none_or(|d| preset.0 <= d.width && preset.1 <= d.height)
}

/// "1280 \u{d7} 800"
pub fn preset_label((w, h): (f32, f32)) -> String {
    format!("{w:.0} \u{d7} {h:.0}")
}

/// Resizes the window to a preset: leaves maximized / fullscreen first. The bounds observer in
/// the workspace then records it as the saved size.
pub fn apply_preset(preset: (f32, f32), window: &mut Window) {
    if window.is_fullscreen() {
        window.toggle_fullscreen();
    } else if window.is_maximized() {
        window.zoom_window();
    }
    window.resize(size(px(preset.0), px(preset.1)));
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: Rect = Rect::new(0., 0., 1920., 1040.);
    const SECOND: Rect = Rect::new(1920., 0., 2560., 1400.);

    fn saved(x: f32, y: f32, w: f32, h: f32) -> SavedWindow {
        SavedWindow {
            rect: Rect::new(x, y, w, h),
            maximized: false,
        }
    }

    #[test]
    fn nothing_saved_gives_the_centred_default() {
        let r = restore(None, &[MAIN], MAIN);
        assert_eq!(r, Rect::new(320., 120., 1280., 800.));
        // A small display caps the default.
        let small = Rect::new(0., 0., 1024., 700.);
        assert_eq!(restore(None, &[small], small), small);
    }

    #[test]
    fn a_visible_place_is_kept() {
        let s = saved(200., 100., 1000., 700.);
        assert_eq!(
            restore(Some(&s), &[MAIN, SECOND], MAIN),
            Rect::new(200., 100., 1000., 700.)
        );
        // On the second display.
        let s = saved(2200., 50., 1200., 800.);
        assert_eq!(
            restore(Some(&s), &[MAIN, SECOND], MAIN),
            Rect::new(2200., 50., 1200., 800.)
        );
    }

    #[test]
    fn an_unplugged_monitor_falls_back_to_the_primary_centre() {
        let s = saved(2200., 50., 1200., 800.);
        assert_eq!(
            restore(Some(&s), &[MAIN], MAIN),
            Rect::new(360., 120., 1200., 800.)
        );
        // Bigger than the primary: size is capped too.
        let s = saved(2200., 50., 2400., 1300.);
        assert_eq!(restore(Some(&s), &[MAIN], MAIN), MAIN);
    }

    #[test]
    fn a_barely_visible_window_counts_as_off_screen() {
        // Only a 20px sliver on the display.
        let s = saved(1900., 300., 1000., 700.);
        assert_eq!(
            restore(Some(&s), &[MAIN], MAIN),
            Rect::new(460., 170., 1000., 700.)
        );
    }

    #[test]
    fn a_partly_off_window_is_pulled_back_inside() {
        let s = saved(1500., 800., 1000., 700.);
        assert_eq!(
            restore(Some(&s), &[MAIN], MAIN),
            Rect::new(920., 340., 1000., 700.)
        );
        let s = saved(-300., -40., 1000., 700.);
        assert_eq!(
            restore(Some(&s), &[MAIN], MAIN),
            Rect::new(0., 0., 1000., 700.)
        );
    }

    #[test]
    fn junk_is_ignored() {
        let nan = saved(f32::NAN, 0., 1000., 700.);
        let tiny = saved(0., 0., 10., 10.);
        // A minimized window on Windows reports far-off tiny bounds.
        for s in [nan, tiny] {
            assert_eq!(
                restore(Some(&s), &[MAIN], MAIN),
                Rect::new(320., 120., 1280., 800.)
            );
        }
    }

    #[test]
    fn presets_fit_the_display() {
        let hd = Rect::new(0., 0., 1920., 1040.);
        assert!(preset_fits((1920., 1040.), Some(hd)));
        assert!(!preset_fits((1920., 1080.), Some(hd)));
        assert!(preset_fits((1920., 1080.), None));
        assert_eq!(preset_label(PRESETS[1]), "1440 \u{d7} 900");
    }

    #[test]
    fn old_state_without_a_window_loads_and_the_field_round_trips() {
        let none: crate::state::SavedState = serde_json::from_str(r#"{"open_tabs": []}"#).unwrap();
        assert_eq!(none.window, None);

        let state = crate::state::SavedState {
            window: Some(SavedWindow {
                rect: Rect::new(10., 20., 1300., 820.),
                maximized: true,
            }),
            ..Default::default()
        };
        let text = serde_json::to_string(&state).unwrap();
        assert!(text.contains(r#""maximized":true"#));
        let back: crate::state::SavedState = serde_json::from_str(&text).unwrap();
        assert_eq!(back, state);

        // `maximized` may be missing.
        let partial: crate::state::SavedState = serde_json::from_str(
            r#"{"window": {"x": 1.0, "y": 2.0, "width": 900.0, "height": 600.0}}"#,
        )
        .unwrap();
        assert_eq!(
            partial.window,
            Some(SavedWindow {
                rect: Rect::new(1., 2., 900., 600.),
                maximized: false
            })
        );
    }
}
