//! Whole-row selection of a diff view and the text it copies. The selection is a range of row
//! indices (click, shift-click, drag); the copy builders are pure so both layouts and both copy
//! modes are testable.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use git_core::parse::diff::{Hunk, LineKind};

use super::model::{DiffModel, DiffRow, Layout};

/// A click / drag selection over a list's rows. `anchor` is where it started, `head` where the
/// pointer is now; either may be the lower one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RowSelection {
    anchor: Option<usize>,
    head: usize,
    dragging: bool,
}

impl RowSelection {
    /// The selected rows, ascending.
    pub fn range(&self) -> Option<Range<usize>> {
        let anchor = self.anchor?;
        Some(anchor.min(self.head)..anchor.max(self.head) + 1)
    }

    pub fn contains(&self, ix: usize) -> bool {
        self.range().is_some_and(|r| r.contains(&ix))
    }

    pub fn is_empty(&self) -> bool {
        self.anchor.is_none()
    }

    /// Mouse down on row `ix`: starts a selection there, or with `extend` grows the existing
    /// one up to `ix`. Starts a drag either way.
    pub fn press(&mut self, ix: usize, extend: bool) {
        if !(extend && self.anchor.is_some()) {
            self.anchor = Some(ix);
        }
        self.head = ix;
        self.dragging = true;
    }

    /// The pointer moved onto row `ix` with the button down; true when the range changed.
    pub fn drag_to(&mut self, ix: usize) -> bool {
        if !self.dragging || self.head == ix {
            return false;
        }
        self.head = ix;
        true
    }

    pub fn release(&mut self) {
        self.dragging = false;
    }

    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Selects just `ix` unless it is already inside the selection (right click).
    pub fn ensure(&mut self, ix: usize) {
        if !self.contains(ix) {
            self.anchor = Some(ix);
            self.head = ix;
        }
    }
}

/// `(hunk, line)` slots a row shows, in display order. A split pair shows its old side then its
/// new side (a context line sits on both sides but is one line).
fn slots(row: DiffRow) -> Vec<(usize, usize)> {
    match row {
        DiffRow::Hunk { .. } => Vec::new(),
        DiffRow::Line { hunk, line } => vec![(hunk, line)],
        DiffRow::Pair { hunk, left, right } => match (left, right) {
            (Some(l), Some(r)) if l == r => vec![(hunk, l)],
            _ => [left, right]
                .into_iter()
                .flatten()
                .map(|line| (hunk, line))
                .collect(),
        },
    }
}

fn text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.strip_suffix('\r').unwrap_or(&text).to_string()
}

/// The changed (added / deleted) lines in `rows`, as hunk -> line indices: what a dragged range
/// picks for line staging.
pub fn changed_lines(
    model: &DiffModel,
    layout: Layout,
    rows: Range<usize>,
) -> BTreeMap<usize, BTreeSet<usize>> {
    let all = model.rows(layout);
    let rows = rows.start.min(all.len())..rows.end.min(all.len());
    let mut out: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for &row in &all[rows] {
        for (hunk, line) in slots(row) {
            let kind = model.file.hunks.get(hunk).and_then(|h| h.lines.get(line));
            if kind.is_some_and(|l| matches!(l.kind, LineKind::Added | LineKind::Deleted)) {
                out.entry(hunk).or_default().insert(line);
            }
        }
    }
    out
}

/// "Copy lines": the code of the selected rows without `+` / `-` / space markers, line numbers
/// or hunk headers, joined by `\n`. A split pair copies its new (right) side when it has one,
/// else its old side, so a changed line copies as it reads after the change.
pub fn copy_lines(model: &DiffModel, layout: Layout, rows: Range<usize>) -> String {
    let all = model.rows(layout);
    let rows = rows.start.min(all.len())..rows.end.min(all.len());
    let mut out: Vec<String> = Vec::new();
    for &row in &all[rows] {
        let line = match row {
            DiffRow::Hunk { .. } => None,
            DiffRow::Line { hunk, line } => Some((hunk, line)),
            DiffRow::Pair { hunk, left, right } => right.or(left).map(|line| (hunk, line)),
        };
        if let Some(l) = line.and_then(|(h, l)| model.file.hunks.get(h)?.lines.get(l)) {
            out.push(text(&l.content));
        }
    }
    out.join("\n")
}

/// "Copy as patch": the selected rows as unified-diff lines with their `+` / `-` / space
/// prefixes, each hunk they touch preceded by its `@@` header (a selected hunk row alone copies
/// just the header). A split pair gives its old line then its new line.
pub fn copy_patch(model: &DiffModel, layout: Layout, rows: Range<usize>) -> String {
    let all = model.rows(layout);
    let rows = rows.start.min(all.len())..rows.end.min(all.len());
    let mut out: Vec<String> = Vec::new();
    let mut header_of: Option<usize> = None;
    let mut push_header = |out: &mut Vec<String>, hunk: usize, h: &Hunk| {
        if header_of != Some(hunk) {
            header_of = Some(hunk);
            out.push(text(&h.header));
        }
    };
    for &row in &all[rows] {
        let hunk = match row {
            DiffRow::Hunk { hunk } | DiffRow::Line { hunk, .. } | DiffRow::Pair { hunk, .. } => {
                hunk
            }
        };
        let Some(h) = model.file.hunks.get(hunk) else {
            continue;
        };
        push_header(&mut out, hunk, h);
        for (_, line) in slots(row) {
            let Some(l) = h.lines.get(line) else { continue };
            let sign = match l.kind {
                LineKind::Added => '+',
                LineKind::Deleted => '-',
                _ => ' ',
            };
            out.push(format!("{sign}{}", text(&l.content)));
            if (h.lines.get(line + 1)).is_some_and(|m| m.kind == LineKind::NoNewlineAtEof) {
                out.push(r"\ No newline at end of file".to_string());
            }
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_core::parse::diff;

    const PATCH: &str = "\
diff --git a/f b/f
--- a/f
+++ b/f
@@ -1,2 +1,3 @@
 a
-old
+new
+more
@@ -10,2 +10,1 @@ fn x()
 z
-q
\\ No newline at end of file
";

    fn model() -> DiffModel {
        DiffModel::new(diff::parse(PATCH.as_bytes()).unwrap().remove(0))
    }

    #[test]
    fn changed_lines_skip_context_and_headers() {
        let m = model();
        let set = |v: &[usize]| v.iter().copied().collect::<BTreeSet<_>>();
        // Unified rows: 0 hunk, 1 " a", 2 "-old", 3 "+new", 4 "+more", 5 hunk, 6 " z", 7 "-q".
        let got = changed_lines(&m, Layout::Unified, 0..8);
        assert_eq!(got.get(&0), Some(&set(&[1, 2, 3])));
        assert_eq!(got.get(&1), Some(&set(&[1])));
        assert_eq!(changed_lines(&m, Layout::Unified, 0..2), BTreeMap::new());
        // A split pair contributes both of its sides.
        let got = changed_lines(&m, Layout::Split, 2..3);
        assert_eq!(got.get(&0), Some(&set(&[1, 2])));
    }

    #[test]
    fn selection_ranges() {
        let mut s = RowSelection::default();
        assert!(s.range().is_none());
        s.press(5, false);
        assert_eq!(s.range(), Some(5..6));
        assert!(s.drag_to(2));
        assert_eq!(s.range(), Some(2..6));
        s.release();
        assert!(!s.drag_to(4));
        s.press(8, true);
        assert_eq!(s.range(), Some(5..9));
        s.ensure(6);
        assert_eq!(s.range(), Some(5..9));
        s.ensure(20);
        assert_eq!(s.range(), Some(20..21));
        s.clear();
        assert!(s.is_empty());
    }

    #[test]
    fn unified_lines_have_no_markers() {
        let m = model();
        // Rows: 0 hunk, 1 " a", 2 "-old", 3 "+new", 4 "+more", 5 hunk, 6 " z", 7 "-q".
        assert_eq!(copy_lines(&m, Layout::Unified, 1..5), "a\nold\nnew\nmore");
        // A range over a hunk header copies no header text.
        assert_eq!(copy_lines(&m, Layout::Unified, 4..8), "more\nz\nq");
    }

    #[test]
    fn split_pairs_copy_the_new_side() {
        let m = model();
        // Rows: 0 hunk, 1 a|a, 2 old|new, 3 -|more, 4 hunk, 5 z|z, 6 q|-.
        assert_eq!(copy_lines(&m, Layout::Split, 1..4), "a\nnew\nmore");
        assert_eq!(copy_lines(&m, Layout::Split, 6..7), "q");
        assert_eq!(copy_lines(&m, Layout::Split, 0..100), "a\nnew\nmore\nz\nq");
    }

    #[test]
    fn patch_adds_prefixes_and_hunk_headers() {
        let m = model();
        assert_eq!(
            copy_patch(&m, Layout::Unified, 2..4),
            "@@ -1,2 +1,3 @@\n-old\n+new"
        );
        assert_eq!(
            copy_patch(&m, Layout::Unified, 4..8),
            "@@ -1,2 +1,3 @@\n+more\n@@ -10,2 +10,1 @@ fn x()\n z\n-q\n\\ No newline at end of file"
        );
        // A hunk row alone is just its header.
        assert_eq!(
            copy_patch(&m, Layout::Unified, 5..6),
            "@@ -10,2 +10,1 @@ fn x()"
        );
    }

    #[test]
    fn split_patch_gives_old_then_new() {
        let m = model();
        assert_eq!(
            copy_patch(&m, Layout::Split, 1..3),
            "@@ -1,2 +1,3 @@\n a\n-old\n+new"
        );
        assert_eq!(
            copy_patch(&m, Layout::Split, 3..5),
            "@@ -1,2 +1,3 @@\n+more\n@@ -10,2 +10,1 @@ fn x()"
        );
    }
}
