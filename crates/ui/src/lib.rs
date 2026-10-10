//! Aurora theme + shared components.

pub mod assets;
pub mod diff;
mod display;
pub mod motion;
pub mod theme;
pub mod typography;

pub use display::{sanitize_display, sanitize_line};
pub use theme::{Appearance, Fonts, Mode, Palette, Skin, Zoom, fonts, zoomed};
