//! Themes: skins (Aurora, Onyx), each with a dark and a light variant, fed to gpui-component so
//! its widgets match. Switch skin and mode at runtime with [`set_appearance`] / [`toggle_mode`].
//!
//! Two layers per skin:
//! - `<skin>.json` maps the skin onto gpui-component's `ThemeConfig` keys (surfaces, text,
//!   primary = accent, semantic colours). Keys it leaves out fall back to the library defaults.
//! - [`Palette`] holds the tokens gpui-component has no slot for (panel2, emphasis text, graph
//!   lanes, signature gradient, `now`), read with [`Palette::of`].

use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode, ThemeRegistry};
use gpui_kit::{App, Global, Hsla, rgb, rgba};

const AURORA_JSON: &str = include_str!("aurora.json");
const ONYX_JSON: &str = include_str!("onyx.json");

/// A colour family. Each has a dark and a light variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Skin {
    /// Violet accent + aurora gradient (shared with AgentSummary).
    #[default]
    Aurora,
    /// Charcoal greys + ember orange, from the onyx dragon logo.
    Onyx,
}

impl Skin {
    pub const ALL: [Skin; 2] = [Skin::Aurora, Skin::Onyx];

    pub fn label(self) -> &'static str {
        match self {
            Skin::Aurora => "Aurora",
            Skin::Onyx => "Onyx",
        }
    }

    pub fn next(self) -> Self {
        let ix = Self::ALL.iter().position(|s| *s == self).unwrap_or(0);
        Self::ALL[(ix + 1) % Self::ALL.len()]
    }

    fn theme_name(self, mode: Mode) -> String {
        let mode = match mode {
            Mode::Dark => "Dark",
            Mode::Light => "Light",
        };
        format!("{} {mode}", self.label())
    }
}

/// Dark or light variant of the active skin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Dark,
    Light,
}

impl Mode {
    pub fn toggled(self) -> Self {
        match self {
            Mode::Dark => Mode::Light,
            Mode::Light => Mode::Dark,
        }
    }
}

impl From<Mode> for ThemeMode {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Dark => ThemeMode::Dark,
            Mode::Light => ThemeMode::Light,
        }
    }
}

/// Skin + mode: everything needed to pick a theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Appearance {
    pub skin: Skin,
    pub mode: Mode,
}

impl Global for Appearance {}

impl Appearance {
    /// `aurora-dark`, `onyx-light`, ...: the inverse of [`Appearance::parse`].
    pub fn key(self) -> String {
        let mode = match self.mode {
            Mode::Dark => "dark",
            Mode::Light => "light",
        };
        format!("{}-{mode}", self.skin.label().to_ascii_lowercase())
    }

    /// Parses `"aurora"`, `"onyx-light"`, `"Onyx Dark"` and the like; unknown parts fall back
    /// to the defaults (Aurora, dark).
    pub fn parse(text: &str) -> Self {
        let text = text.to_ascii_lowercase();
        Self {
            skin: if text.contains("onyx") {
                Skin::Onyx
            } else {
                Skin::Aurora
            },
            mode: if text.contains("light") {
                Mode::Light
            } else {
                Mode::Dark
            },
        }
    }
}

/// Tokens for GitDragon's own drawing (graph, rail, badges, brand gradient).
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub bg: Hsla,
    pub panel: Hsla,
    pub panel2: Hsla,
    pub hover: Hsla,
    pub border: Hsla,
    pub border_strong: Hsla,
    pub fg: Hsla,
    pub emph: Hsla,
    pub muted: Hsla,
    /// Lightest text on a coloured fill; the "no pure white" white.
    pub white: Hsla,
    pub accent: Hsla,
    pub accent_soft: Hsla,
    /// Text / icon on an accent fill.
    pub on_accent: Hsla,
    /// Graph lane colours; lanes cycle through these.
    pub lanes: [Hsla; 5],
    /// Signature gradient stops (Aurora: blue, violet, pink, orange; Onyx: gold to deep ember).
    pub gradient: [Hsla; 4],
    pub ok: Hsla,
    pub warn: Hsla,
    pub error: Hsla,
    pub now: Hsla,
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

fn ca(hex: u32) -> Hsla {
    rgba(hex).into()
}

impl Palette {
    pub fn aurora_dark() -> Self {
        let lanes = [
            c(0x5b8cff),
            c(0xaa71ff),
            c(0xff6bb5),
            c(0xffa25b),
            c(0x3ccfb4),
        ];
        Self {
            bg: c(0x111118),
            panel: c(0x18181f),
            panel2: c(0x1f1f28),
            hover: c(0x282833),
            border: ca(0xececf212),
            border_strong: ca(0xececf221),
            fg: c(0xb8b8c4),
            emph: c(0xcfcfd6),
            muted: c(0x9090a4),
            white: c(0xececf2),
            accent: c(0x8d7eff),
            accent_soft: ca(0x8b7bff26),
            on_accent: c(0x17142e),
            lanes,
            gradient: [lanes[0], lanes[1], lanes[2], lanes[3]],
            ok: c(0x4cc38a),
            warn: c(0xf0a43a),
            error: c(0xff6b6b),
            now: c(0x4aa3ff),
        }
    }

    pub fn aurora_light() -> Self {
        let lanes = [
            c(0x3b6cf0),
            c(0x7f45dd),
            c(0xe0458f),
            c(0xce722c),
            c(0x149b86),
        ];
        Self {
            bg: c(0xefeff4),
            panel: c(0xfafafc),
            panel2: c(0xf4f4f8),
            hover: c(0xe8e8f0),
            border: ca(0x14142d14),
            border_strong: ca(0x14142d26),
            fg: c(0x40404f),
            emph: c(0x25252f),
            muted: c(0x67677c),
            white: c(0xfafafc),
            accent: c(0x594afa),
            accent_soft: ca(0x5b4bff1a),
            on_accent: c(0xfafafc),
            lanes,
            gradient: [lanes[0], lanes[1], lanes[2], lanes[3]],
            ok: c(0x18774a),
            warn: c(0x975a00),
            error: c(0xc13434),
            now: c(0x0966c9),
        }
    }

    pub fn onyx_dark() -> Self {
        Self {
            bg: c(0x0f0f13),
            panel: c(0x16161b),
            panel2: c(0x1e1e25),
            hover: c(0x26262e),
            border: ca(0xf3e6d412),
            border_strong: ca(0xf3e6d421),
            fg: c(0xc2bbb0),
            emph: c(0xddd2c2),
            muted: c(0x938d86),
            white: c(0xf3e6d4),
            accent: c(0xff7a2e),
            accent_soft: ca(0xff7a2e26),
            on_accent: c(0x1a0b04),
            // ember, steel blue, gold, amethyst, teal
            lanes: [
                c(0xff8a2e),
                c(0x5aa9e6),
                c(0xffd27a),
                c(0xb78cdb),
                c(0x4fc1b0),
            ],
            gradient: [c(0xffd27a), c(0xffb347), c(0xff7a2e), c(0xc93a12)],
            ok: c(0x5cc28a),
            warn: c(0xf2c14e),
            error: c(0xf0506e),
            now: c(0x5aa9e6),
        }
    }

    pub fn onyx_light() -> Self {
        Self {
            bg: c(0xeeece9),
            panel: c(0xfaf9f7),
            panel2: c(0xf4f2ef),
            hover: c(0xe6e2dc),
            border: ca(0x1a0b0414),
            border_strong: ca(0x1a0b0426),
            fg: c(0x3f3b37),
            emph: c(0x24211e),
            muted: c(0x6b655e),
            white: c(0xfaf9f7),
            accent: c(0xc2410c),
            accent_soft: ca(0xc2410c1a),
            on_accent: c(0xfaf9f7),
            lanes: [
                c(0xd0541a),
                c(0x2f78b0),
                c(0xa8740a),
                c(0x8a4fb0),
                c(0x18877a),
            ],
            gradient: [c(0xd99a1e), c(0xe07a1a), c(0xc2410c), c(0x8f2a08)],
            ok: c(0x1f7a4a),
            warn: c(0x8f5d00),
            error: c(0xb8283f),
            now: c(0x1f6fb8),
        }
    }

    pub fn for_appearance(appearance: Appearance) -> Self {
        match (appearance.skin, appearance.mode) {
            (Skin::Aurora, Mode::Dark) => Self::aurora_dark(),
            (Skin::Aurora, Mode::Light) => Self::aurora_light(),
            (Skin::Onyx, Mode::Dark) => Self::onyx_dark(),
            (Skin::Onyx, Mode::Light) => Self::onyx_light(),
        }
    }

    /// The palette matching the active theme.
    pub fn of(cx: &App) -> Self {
        Self::for_appearance(appearance(cx))
    }

    /// Lane colour for graph column `ix`.
    pub fn lane(&self, ix: usize) -> Hsla {
        self.lanes[ix % self.lanes.len()]
    }
}

/// Registers every skin and applies `appearance`. Call after `gpui_kit::init`.
pub fn init(appearance: Appearance, cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    for json in [AURORA_JSON, ONYX_JSON] {
        registry
            .load_themes_from_str(json)
            .expect("bundled theme JSON is valid");
    }
    set_appearance(appearance, cx);
}

/// The active skin + mode.
pub fn appearance(cx: &App) -> Appearance {
    cx.try_global::<Appearance>().copied().unwrap_or_default()
}

/// Switches skin and/or mode; every window repaints.
pub fn set_appearance(appearance: Appearance, cx: &mut App) {
    let themes = ThemeRegistry::global(cx).themes();
    let dark = themes[appearance.skin.theme_name(Mode::Dark).as_str()].clone();
    let light = themes[appearance.skin.theme_name(Mode::Light).as_str()].clone();
    cx.set_global(appearance);
    Theme::update(cx, |theme| {
        theme.dark_theme = dark;
        theme.light_theme = light;
    });
    // `change` reloads the mode's config even when the mode is unchanged, so a skin swap applies.
    Theme::change(appearance.mode, None, cx);
}

/// Flips dark <-> light, keeping the skin.
pub fn toggle_mode(cx: &mut App) -> Appearance {
    let mut next = appearance(cx);
    next.mode = next.mode.toggled();
    set_appearance(next, cx);
    next
}

/// Moves to the next skin, keeping the mode.
pub fn cycle_skin(cx: &mut App) -> Appearance {
    let mut next = appearance(cx);
    next.skin = next.skin.next();
    set_appearance(next, cx);
    next
}

/// Whether the gpui-component theme is dark (follows [`Appearance::mode`]).
pub fn is_dark(cx: &App) -> bool {
    cx.theme().is_dark()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(json: &str) -> Vec<(String, String)> {
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        value["themes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                (
                    t["name"].as_str().unwrap().into(),
                    t["mode"].as_str().unwrap().into(),
                )
            })
            .collect()
    }

    #[test]
    fn every_skin_has_dark_and_light() {
        for (skin, json) in [(Skin::Aurora, AURORA_JSON), (Skin::Onyx, ONYX_JSON)] {
            let expected = vec![
                (skin.theme_name(Mode::Dark), "dark".to_string()),
                (skin.theme_name(Mode::Light), "light".to_string()),
            ];
            assert_eq!(names(json), expected);
        }
    }

    #[test]
    fn no_pure_black_or_white() {
        for json in [AURORA_JSON, ONYX_JSON] {
            let lower = json.to_lowercase();
            for banned in [
                "#ffffff",
                "#000000",
                "\"#fff\"",
                "\"#000\"",
                "\"white\"",
                "\"black\"",
            ] {
                assert!(!lower.contains(banned), "theme uses {banned}");
            }
        }
    }

    #[test]
    fn semantic_colours_never_follow_the_accent() {
        for skin in Skin::ALL {
            for mode in [Mode::Dark, Mode::Light] {
                let p = Palette::for_appearance(Appearance { skin, mode });
                for semantic in [p.ok, p.warn, p.error, p.now] {
                    assert_ne!(semantic, p.accent, "{skin:?} {mode:?}");
                }
            }
        }
    }

    #[test]
    fn parses_appearance() {
        let onyx_light = Appearance {
            skin: Skin::Onyx,
            mode: Mode::Light,
        };
        assert_eq!(Appearance::parse("onyx-light"), onyx_light);
        assert_eq!(
            Appearance::parse("Onyx"),
            Appearance {
                skin: Skin::Onyx,
                mode: Mode::Dark
            }
        );
        assert_eq!(Appearance::parse(""), Appearance::default());
        assert_eq!(Appearance::parse(&onyx_light.key()), onyx_light);
    }

    #[test]
    fn skins_cycle_and_lanes_wrap() {
        assert_eq!(Skin::Aurora.next(), Skin::Onyx);
        assert_eq!(Skin::Onyx.next(), Skin::Aurora);
        let p = Palette::onyx_dark();
        assert_eq!(p.lane(0), p.lane(5));
    }
}
