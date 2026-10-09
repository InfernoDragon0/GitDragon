//! Syntax colours for diff lines. Each hunk's old side (context + deleted) and new side
//! (context + added) is highlighted as one snippet, then split back into lines. Snippets are
//! partial code; both engines recover well enough for colouring.
//!
//! Two engines, picked by cargo feature (colours always come from the app's highlight theme):
//! - `syntax-tree-sitter`: gpui-component's tree-sitter highlighter, a small grammar set.
//! - `syntax-syntect`: syntect with two-face's extended Sublime grammars (~200 languages);
//!   scopes are mapped onto the theme's tree-sitter style names.

use std::ops::Range;

use git_core::parse::diff::LineKind;
use gpui_kit::component::highlighter::HighlightTheme;
use gpui_kit::{HighlightStyle, Hsla};

use super::model::DiffModel;

/// Styled byte ranges inside one line's display text.
pub type Spans = Vec<(Range<usize>, HighlightStyle)>;

/// Lower-case extension of the last path component (`None` for dotfiles / no dot).
fn extension(path: &[u8]) -> Option<String> {
    let name = path.rsplit(|&b| b == b'/').next()?;
    let ext = name
        .rsplit(|&b| b == b'.')
        .next()
        .filter(|e| e.len() < name.len() && !e.is_empty())?;
    if ext.len() + 1 == name.len() {
        return None; // `.gitignore`: a dotfile, not an extension
    }
    Some(std::str::from_utf8(ext).ok()?.to_ascii_lowercase())
}

/// Fills `LineView::syntax` for every line of `model` (`path` picks the language). Skips large
/// diffs and unknown languages.
pub fn highlight(model: &mut DiffModel, path: &[u8], theme: &HighlightTheme) {
    if model.is_large() {
        return;
    }
    let Some(engine) = engine::Engine::for_path(path) else {
        return;
    };
    for (h, hunk) in model.file.hunks.iter().enumerate() {
        for old_side in [true, false] {
            // Lines of this side, in order, with their offsets in the snippet.
            let mut text = String::new();
            let mut placed: Vec<(usize, Range<usize>)> = Vec::new();
            for (i, line) in hunk.lines.iter().enumerate() {
                let on_side = match line.kind {
                    LineKind::Context => true,
                    LineKind::Deleted => old_side,
                    LineKind::Added => !old_side,
                    LineKind::NoNewlineAtEof => false,
                };
                if !on_side {
                    continue;
                }
                let start = text.len();
                text.push_str(&model.lines[h][i].text);
                placed.push((i, start..text.len()));
                text.push('\n');
            }
            if placed.is_empty() {
                continue;
            }
            let styles = engine.styles(&text, theme);
            for (i, range) in placed {
                // Context lines sit on both sides: take the new side's colours.
                let kind = hunk.lines[i].kind;
                if kind == LineKind::Context && old_side {
                    continue;
                }
                model.lines[h][i].syntax = clip(&styles, &range);
            }
        }
    }
}

#[cfg(feature = "syntax-tree-sitter")]
mod engine {
    use gpui_kit::component::Rope;
    use gpui_kit::component::highlighter::{HighlightTheme, SyntaxHighlighter};

    use super::Spans;

    pub struct Engine(&'static str);

    impl Engine {
        /// The grammars compiled into `ui` (see its Cargo features).
        pub fn for_path(path: &[u8]) -> Option<Self> {
            let lang = match super::extension(path)?.as_str() {
                "rs" => "rust",
                "js" | "mjs" | "cjs" | "jsx" => "javascript",
                "ts" | "mts" | "cts" => "typescript",
                "tsx" => "tsx",
                "py" | "pyi" => "python",
                "toml" => "toml",
                "md" | "markdown" => "markdown",
                "cs" => "csharp",
                "go" => "go",
                "json" | "jsonc" => "json",
                _ => return None,
            };
            Some(Self(lang))
        }

        pub fn styles(&self, text: &str, theme: &HighlightTheme) -> Spans {
            let mut highlighter = SyntaxHighlighter::new(self.0);
            highlighter.update(None, &Rope::from(text), None);
            highlighter.styles(&(0..text.len()), theme)
        }
    }
}

#[cfg(all(feature = "syntax-syntect", not(feature = "syntax-tree-sitter")))]
mod engine {
    use std::ops::Range;
    use std::sync::LazyLock;

    use gpui_kit::component::highlighter::HighlightTheme;
    use gpui_kit::component::input::HighlightStyleResolver as _;
    use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

    use super::Spans;

    /// Loaded on first use (a few ms), off the UI thread like the rest of diff building.
    static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

    pub struct Engine(&'static SyntaxReference);

    impl Engine {
        pub fn for_path(path: &[u8]) -> Option<Self> {
            let ext = super::extension(path)?;
            SYNTAXES.find_syntax_by_extension(&ext).map(Self)
        }

        pub fn styles(&self, text: &str, theme: &HighlightTheme) -> Spans {
            let mut state = ParseState::new(self.0);
            let mut stack = ScopeStack::new();
            let mut out = Spans::new();
            let mut offset = 0;
            for line in text.split_inclusive('\n') {
                let Ok(ops) = state.parse_line(line, &SYNTAXES) else {
                    break;
                };
                let mut pos = 0;
                for (at, op) in ops {
                    push(&mut out, offset + pos..offset + at, &stack, theme);
                    if stack.apply(&op).is_err() {
                        return out;
                    }
                    pos = at;
                }
                push(&mut out, offset + pos..offset + line.len(), &stack, theme);
                offset += line.len();
            }
            out
        }
    }

    fn push(out: &mut Spans, range: Range<usize>, stack: &ScopeStack, theme: &HighlightTheme) {
        if range.is_empty() {
            return;
        }
        if let Some(style) = category(stack).and_then(|name| theme.style(name)) {
            out.push((range, style));
        }
    }

    /// Theme style name for the innermost scope we know (TextMate scope -> tree-sitter name).
    fn category(stack: &ScopeStack) -> Option<&'static str> {
        const MAP: &[(&str, &str)] = &[
            ("comment", "comment"),
            ("string.regexp", "string.regex"),
            ("constant.character.escape", "string.escape"),
            ("string", "string"),
            ("constant.numeric", "number"),
            ("constant.language", "boolean"),
            ("constant", "constant"),
            ("entity.name.function", "function"),
            ("support.function", "function"),
            ("meta.function-call", "function"),
            ("entity.name.type", "type"),
            ("entity.name.class", "type"),
            ("support.type", "type"),
            ("support.class", "type"),
            ("storage.type", "type"),
            ("keyword.operator", "operator"),
            ("keyword", "keyword"),
            ("storage", "keyword"),
            ("entity.name.tag", "tag"),
            ("entity.other.attribute-name", "attribute"),
            ("variable.parameter", "variable.special"),
            ("variable.language", "variable.special"),
            ("variable.other.member", "property"),
            ("variable.other.property", "property"),
            ("markup.heading", "title"),
            ("punctuation", "punctuation"),
        ];
        stack.as_slice().iter().rev().find_map(|scope| {
            let s = scope.build_string();
            MAP.iter()
                .find(|(prefix, _)| s.starts_with(prefix))
                .map(|(_, name)| *name)
        })
    }
}

#[cfg(not(any(feature = "syntax-tree-sitter", feature = "syntax-syntect")))]
mod engine {
    use gpui_kit::component::highlighter::HighlightTheme;

    use super::Spans;

    /// No engine compiled in: no colours.
    pub struct Engine;

    impl Engine {
        pub fn for_path(_: &[u8]) -> Option<Self> {
            None
        }

        pub fn styles(&self, _: &str, _: &HighlightTheme) -> Spans {
            Spans::new()
        }
    }
}

/// The styles overlapping `line`, shifted to start at 0.
fn clip(styles: &Spans, line: &Range<usize>) -> Spans {
    styles
        .iter()
        .filter(|(r, s)| {
            r.start < line.end && r.end > line.start && *s != HighlightStyle::default()
        })
        .map(|(r, s)| {
            let start = r.start.max(line.start) - line.start;
            let end = r.end.min(line.end) - line.start;
            (start..end, *s)
        })
        .filter(|(r, _)| !r.is_empty())
        .collect()
}

/// Syntax colours plus word-level emphasis backgrounds as one sorted, non-overlapping list
/// (what `StyledText::with_highlights` wants). Overlapping syntax spans: the later one wins.
pub fn merge(syntax: &Spans, emphasis: &[Range<usize>], strong: Hsla, len: usize) -> Spans {
    let mut cuts: Vec<usize> = vec![0, len];
    for (r, _) in syntax {
        cuts.extend([r.start, r.end]);
    }
    for r in emphasis {
        cuts.extend([r.start, r.end]);
    }
    cuts.retain(|&c| c <= len);
    cuts.sort_unstable();
    cuts.dedup();
    let mut out: Spans = Vec::new();
    for pair in cuts.windows(2) {
        let seg = pair[0]..pair[1];
        let mut style = syntax
            .iter()
            .rev()
            .find(|(r, _)| r.start <= seg.start && r.end >= seg.end)
            .map(|(_, s)| *s)
            .unwrap_or_default();
        if emphasis
            .iter()
            .any(|r| r.start <= seg.start && r.end >= seg.end)
        {
            style.background_color = Some(strong);
        }
        if style == HighlightStyle::default() {
            continue;
        }
        match out.last_mut() {
            Some((last, s)) if last.end == seg.start && *s == style => last.end = seg.end,
            _ => out.push((seg, style)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_core::parse::diff;
    use gpui_kit::hsla;

    #[test]
    fn extensions() {
        assert_eq!(extension(b"src/main.rs").as_deref(), Some("rs"));
        assert_eq!(extension(b"a/b/App.TSX").as_deref(), Some("tsx"));
        assert_eq!(extension(b"Makefile"), None);
        assert_eq!(extension(b".gitignore"), None);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)] // one emphasis range, not a range of indices
    fn merge_splits_overlaps() {
        let red = hsla(0., 1., 0.5, 1.);
        let blue = hsla(0.6, 1., 0.5, 1.);
        let kw = HighlightStyle {
            color: Some(red),
            ..Default::default()
        };
        let out = merge(&vec![(0..3, kw)], &[2..5], blue, 6);
        let with_bg = HighlightStyle {
            background_color: Some(blue),
            ..kw
        };
        let bg_only = HighlightStyle {
            background_color: Some(blue),
            ..Default::default()
        };
        assert_eq!(out, vec![(0..2, kw), (2..3, with_bg), (3..5, bg_only)]);
    }

    #[cfg(any(feature = "syntax-tree-sitter", feature = "syntax-syntect"))]
    #[test]
    fn rust_hunk_gets_colours_on_both_sides() {
        let patch = "diff --git a/f.rs b/f.rs\n--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,2 @@\n fn main() {\n-    let a = 1;\n+    let a = 2;\n";
        let mut model = DiffModel::new(diff::parse(patch.as_bytes()).unwrap().remove(0));
        highlight(&mut model, b"f.rs", &HighlightTheme::default_dark());
        let lines = &model.lines[0];
        assert!(!lines[0].syntax.is_empty(), "context `fn` is a keyword");
        assert!(!lines[1].syntax.is_empty() && !lines[2].syntax.is_empty());
        // Ranges stay inside their line.
        for line in lines {
            assert!(line.syntax.iter().all(|(r, _)| r.end <= line.text.len()));
        }
    }
}
