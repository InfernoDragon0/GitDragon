//! Paints one row of the lane graph: lanes crossing the row, the half-row edges into and out of
//! the node, then the node on top. Geometry follows the Aurora mockup (screen A): lanes 24 px
//! apart, 2 px strokes, 5 px dots ringed in the panel colour; merges are hollow.

use git_core::graph::{Edge, EdgeKind, History, Node, RowKind};
use gpui_kit::{
    Bounds, ContentMask, Hsla, PathBuilder, Pixels, Window, canvas, fill, point, prelude::*, size,
};
use ui::Palette;

/// Unzoomed sizes; scaled by the zoom factor at paint time.
pub const LANE: f32 = 24.;
const PAD: f32 = 6.;
const STROKE: f32 = 2.;
const NODE: f32 = 5.;
/// Opacity of the other lanes while one is hovered.
const DIMMED: f32 = 0.25;
/// How far (px) from a line or dot the pointer still hovers it.
const HIT_SLOP: f32 = 5.;
/// Most columns the graph column holds; fewer when the view is narrow (see [`max_columns`]).
/// When a history is wider, the last one becomes a gutter: lanes past it are not drawn, edges
/// into them fade out, and their commits are pinned there.
pub const MAX_COLUMNS: u32 = 12;
/// Fewest columns, however narrow the view.
const MIN_COLUMNS: u32 = 4;
/// Share of the view's width the graph column may take.
const GRAPH_SHARE: f32 = 0.3;
/// Opacity of edges leading into hidden lanes.
const FADED: f32 = 0.35;

/// Everything needed to paint one row, copied out of the [`History`] so the canvas owns it.
#[derive(Debug, Clone)]
pub struct RowLanes {
    node: Node,
    edges: Vec<Edge>,
    /// Lanes crossing the whole row: (column, colour).
    through: Vec<(u32, u32)>,
    merge: bool,
    head: bool,
    kind: RowKind,
}

impl RowLanes {
    /// The commit has two or more parents.
    pub fn is_merge(&self) -> bool {
        self.merge
    }
}

/// Lanes for each row in `rows`, from one viewport query.
pub fn rows_lanes(history: &History, rows: std::ops::Range<usize>) -> Vec<RowLanes> {
    let rows = rows.start..rows.end.min(history.len());
    let layout = history.layout();
    let mut out: Vec<RowLanes> = rows
        .clone()
        .filter_map(|r| {
            let node = layout.node(r)?;
            Some(RowLanes {
                node,
                edges: layout.edges(r).to_vec(),
                through: Vec::new(),
                merge: false,
                head: history.head() == Some(r),
                kind: history.kind(r),
            })
        })
        .collect();
    for span in layout.spans(rows.clone()) {
        let first = (span.start as usize + 1).max(rows.start);
        let last = (span.end as usize).min(rows.end);
        for r in first..last {
            out[r - rows.start].through.push((span.column, span.color));
        }
    }
    for lanes in &mut out {
        let straight = lanes.through.iter().any(|&(c, _)| c == lanes.node.column);
        let outs = lanes
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Out)
            .count();
        lanes.merge = outs + usize::from(straight) > 1;
    }
    out
}

/// Width of the graph column for a layout `width` columns wide.
pub fn column_width(width: u32, cap: u32, unit: Pixels) -> Pixels {
    unit * (PAD * 2. + LANE * width.clamp(1, cap) as f32)
}

/// Columns that fit in [`GRAPH_SHARE`] of a view `available` wide (all of them before the
/// first layout, when the width is still unknown).
pub fn max_columns(available: Pixels, unit: Pixels) -> u32 {
    let budget = f32::from(available) * GRAPH_SHARE / f32::from(unit);
    if budget <= 0. {
        return MAX_COLUMNS;
    }
    (((budget - PAD * 2.) / LANE).floor() as u32).clamp(MIN_COLUMNS, MAX_COLUMNS)
}

/// The gutter column when a layout `width` columns wide does not fit in `cap` columns.
pub fn gutter(width: u32, cap: u32) -> Option<u32> {
    (width > cap).then(|| cap - 1)
}

/// A canvas filling its parent that paints `lanes`. `unit` is one zoomed pixel. With a
/// `hovered` lane colour, that branch is drawn thicker on top and every other one dimmed.
/// `gutter`: the column hidden lanes collapse into, when the history is too wide (see [`gutter`]).
pub fn lanes_canvas(
    lanes: RowLanes,
    palette: Palette,
    unit: Pixels,
    hovered: Option<u32>,
    gutter: Option<u32>,
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, (), window, _| {
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                // Dimmed lanes first so the hovered branch stays on top where lines cross.
                let pass = Pass {
                    unit,
                    bounds,
                    hovered,
                    gutter,
                };
                if hovered.is_some() {
                    paint(&lanes, &palette, &pass, false, window);
                }
                paint(&lanes, &palette, &pass, true, window);
            });
        },
    )
    .size_full()
}

/// Centre x of `column`, relative to the graph cell's left edge.
fn lane_x(column: u32, unit: Pixels) -> Pixels {
    unit * (PAD + LANE * (column as f32 + 0.5))
}

/// Is `column` past the gutter's start (not drawn as its own lane)?
fn hidden(column: u32, gutter: Option<u32>) -> bool {
    gutter.is_some_and(|g| column >= g)
}

/// Where `column` is drawn: its own lane, or the gutter when hidden.
fn shown_x(column: u32, gutter: Option<u32>, unit: Pixels) -> Pixels {
    lane_x(gutter.filter(|&g| column >= g).unwrap_or(column), unit)
}

#[derive(Clone, Copy)]
struct Pass {
    unit: Pixels,
    bounds: Bounds<Pixels>,
    hovered: Option<u32>,
    gutter: Option<u32>,
}

/// Paints the hovered branch (`front`) or everything else (`!front`); with nothing hovered,
/// `front` paints it all at full strength.
fn paint(lanes: &RowLanes, palette: &Palette, pass: &Pass, front: bool, window: &mut Window) {
    let Pass {
        unit,
        bounds,
        hovered,
        gutter,
    } = *pass;
    let x = |column: u32| bounds.left() + shown_x(column, gutter, unit);
    let hidden = |column: u32| hidden(column, gutter);
    let top = bounds.top();
    let bottom = bounds.bottom();
    let mid = top + bounds.size.height / 2.;
    // In this pass? Then its colour and stroke.
    let style = |c: u32| -> Option<(Hsla, Pixels)> {
        let lane = palette.lane(c as usize);
        match hovered {
            None => front.then_some((lane, unit * STROKE)),
            Some(h) if h == c => front.then_some((lane, unit * (STROKE + 1.))),
            Some(_) => (!front).then_some((lane.opacity(DIMMED), unit * STROKE)),
        }
    };

    let vline =
        |window: &mut Window, column: u32, from: Pixels, to: Pixels, (c, w): (Hsla, Pixels)| {
            window.paint_quad(fill(
                Bounds::new(point(x(column) - w / 2., from), size(w, to - from)),
                c,
            ));
        };

    // The gutter's edge, so the cut-off reads as a boundary.
    if let (Some(g), true) = (gutter, front) {
        let edge = bounds.left() + lane_x(g, unit) - unit * (LANE / 2.);
        window.paint_quad(fill(
            Bounds::new(point(edge, top), size(unit, bottom - top)),
            palette.border,
        ));
    }
    for &(column, c) in &lanes.through {
        if hidden(column) {
            continue;
        }
        if let Some(style) = style(c) {
            vline(window, column, top, bottom, style);
        }
    }
    let node_x = x(lanes.node.column);
    for edge in &lanes.edges {
        let Some((mut c, w)) = style(edge.color) else {
            continue;
        };
        match (hidden(edge.column), hidden(lanes.node.column)) {
            // Both in the gutter: nothing to show.
            (true, true) => continue,
            // Leading off into a hidden lane: fade out.
            (true, false) => c = c.opacity(c.a * FADED),
            _ => {}
        }
        let lane_x = x(edge.column);
        let (from, to) = match edge.kind {
            EdgeKind::Into => (point(lane_x, top), point(node_x, mid)),
            EdgeKind::Out => (point(node_x, mid), point(lane_x, bottom)),
        };
        if edge.column == lanes.node.column {
            vline(window, edge.column, from.y, to.y, (c, w));
            continue;
        }
        // Elbow: leaves the lane vertically, meets the node horizontally.
        let mut path = PathBuilder::stroke(w);
        path.move_to(from);
        path.curve_to(to, point(lane_x, mid));
        if let Ok(path) = path.build() {
            window.paint_path(path, c);
        }
    }

    // Dot inside a panel-coloured ring that separates it from the lines; merges are hollow.
    let Some((c, w)) = style(lanes.node.color) else {
        return;
    };
    let grow = if hovered == Some(lanes.node.color) {
        1.
    } else {
        0.
    };
    let r = unit * (if lanes.head { NODE + 1. } else { NODE } + grow);
    let disc = |r: Pixels| Bounds::new(point(node_x - r, mid - r), size(r * 2., r * 2.));
    window.paint_quad(fill(disc(r + unit * STROKE), palette.panel).corner_radii(r + unit * STROKE));
    // WIP: a hollow ring in the panel colour's place; a stash: a hollow rounded square.
    if lanes.kind != RowKind::Commit {
        let square = matches!(lanes.kind, RowKind::Stash(_));
        let ring = fill(disc(r), palette.panel)
            .border_widths(w)
            .border_color(c.opacity(c.a * 0.85));
        window.paint_quad(ring.corner_radii(if square { unit * 2. } else { r }));
        return;
    }
    let dot = if lanes.merge {
        fill(disc(r), palette.panel)
            .border_widths(w)
            .border_color(c)
    } else {
        fill(disc(r), c)
    };
    window.paint_quad(dot.corner_radii(r));
    // A commit of a hidden lane, pinned in the gutter: an arrow points at it.
    if hidden(lanes.node.column) {
        let tip = node_x - r - unit * (STROKE + 2.);
        let back = tip - unit * 4.;
        let mut arrow = PathBuilder::fill();
        arrow.move_to(point(back, mid - unit * 3.5));
        arrow.line_to(point(tip, mid));
        arrow.line_to(point(back, mid + unit * 3.5));
        arrow.close();
        if let Ok(arrow) = arrow.build() {
            window.paint_path(arrow, c);
        }
    }
}

/// The lane colour under a point of a row's graph cell (`x`, `y` from the cell's top-left), if
/// it is within a few pixels of a line or on the dot. Hidden lanes only hit through their
/// pinned commit.
pub fn hit(
    lanes: &RowLanes,
    x: Pixels,
    y: Pixels,
    height: Pixels,
    unit: Pixels,
    gutter: Option<u32>,
) -> Option<u32> {
    let near = |column: u32| {
        !hidden(column, gutter) && (x - lane_x(column, unit)).abs() <= unit * HIT_SLOP
    };
    let mid = height / 2.;
    let node_x = shown_x(lanes.node.column, gutter, unit);
    let on_node =
        (x - node_x).abs() <= unit * HIT_SLOP && (y - mid).abs() <= unit * (NODE + HIT_SLOP);
    if on_node {
        return Some(lanes.node.color);
    }
    let through = lanes.through.iter().find(|&&(column, _)| near(column));
    if let Some(&(_, color)) = through {
        return Some(color);
    }
    // Half-row edges, by the lane end they attach to (the curve itself is not tested).
    lanes
        .edges
        .iter()
        .find(|e| {
            let half = match e.kind {
                EdgeKind::Into => y < mid,
                EdgeKind::Out => y >= mid,
            };
            half && near(e.column)
        })
        .map(|e| e.color)
}

#[cfg(test)]
mod tests {
    use git_core::parse::log::{Commit, Signature};

    use super::*;

    fn commit(id: &str, parents: &[&str]) -> Commit {
        let sig = Signature {
            name: "t".into(),
            email: "t@e".into(),
            time: 0,
            tz_offset_minutes: 0,
        };
        Commit {
            id: format!("{id:0>40}"),
            parents: parents.iter().map(|p| format!("{p:0>40}")).collect(),
            author: sig.clone(),
            committer: sig,
            subject: "s".into(),
            body: Default::default(),
            refs: Vec::new(),
            head_target: None,
        }
    }

    /// m merges f; f runs beside main's c down to their base b.
    fn history() -> History {
        let mut h = History::new();
        h.extend([
            commit("m", &["c", "f"]),
            commit("f", &["b"]),
            commit("c", &["b"]),
            commit("b", &[]),
        ]);
        h
    }

    #[test]
    fn crossing_lanes_per_row() {
        let rows = rows_lanes(&history(), 0..4);
        assert_eq!(rows.len(), 4);
        // Row 1 (f, column 1): main's lane crosses in column 0, f's own lane runs through.
        assert_eq!(rows[1].node.column, 1);
        let mut through: Vec<u32> = rows[1].through.iter().map(|&(c, _)| c).collect();
        through.sort_unstable();
        assert_eq!(through, [0, 1]);
        // Row 2 (c): only f's lane crosses beside main's straight lane.
        assert_eq!(rows[2].through.len(), 2);
        assert!(rows[3].through.is_empty());
    }

    #[test]
    fn merge_flag_and_clamped_range() {
        let rows = rows_lanes(&history(), 0..10);
        assert_eq!(rows.len(), 4);
        assert!(rows[0].merge);
        assert!(!rows[1].merge && !rows[2].merge && !rows[3].merge);
        // A viewport starting mid-history sees the same lanes.
        let tail = rows_lanes(&history(), 2..4);
        assert_eq!(tail[0].through.len(), 2);
    }

    /// 14 branch tips off one root: wider than the column, so tips past the gutter are pinned.
    #[test]
    fn hidden_lanes_hit_only_their_pinned_commit() {
        let mut h = History::new();
        let tips: Vec<String> = (0..14).map(|i| format!("t{i}")).collect();
        h.extend(tips.iter().map(|t| commit(t, &["r"])));
        h.extend([commit("r", &[])]);
        let g = gutter(h.layout().width(), MAX_COLUMNS);
        assert_eq!(g, Some(MAX_COLUMNS - 1));
        let rows = rows_lanes(&h, 0..15);
        let last = &rows[13];
        assert!(hidden(last.node.column, g));
        let unit = gpui_kit::px(1.);
        let height = gpui_kit::px(30.);
        let at_gutter = lane_x(MAX_COLUMNS - 1, unit);
        let px = gpui_kit::px;
        assert_eq!(
            hit(last, at_gutter, px(15.), height, unit, g),
            Some(last.node.color)
        );
        // Its real lane position is off the column: nothing there.
        let real = lane_x(last.node.column, unit);
        assert_eq!(hit(last, real, px(15.), height, unit, g), None);
        // Lanes crossing the gutter are not hoverable on their own.
        assert_eq!(hit(last, at_gutter, px(2.), height, unit, g), None);
    }

    #[test]
    fn column_cap_follows_the_view_width() {
        let unit = gpui_kit::px(1.);
        assert_eq!(max_columns(gpui_kit::px(0.), unit), MAX_COLUMNS);
        assert_eq!(max_columns(gpui_kit::px(400.), unit), MIN_COLUMNS);
        // 30 % of 1000 px = 300 px: (300 - 12) / 24 = 12 columns; zoom 1.5 fits 7.
        assert_eq!(max_columns(gpui_kit::px(1000.), unit), 12);
        assert_eq!(max_columns(gpui_kit::px(1000.), gpui_kit::px(1.5)), 7);
        assert_eq!(gutter(7, 7), None);
        assert_eq!(gutter(8, 7), Some(6));
    }

    #[test]
    fn hit_finds_lines_and_dots() {
        let h = history();
        let rows = rows_lanes(&h, 0..4);
        let unit = gpui_kit::px(1.);
        let height = gpui_kit::px(30.);
        let at = |column: f32| unit * (PAD + LANE * (column + 0.5));
        let px = gpui_kit::px;
        // Row 1 (f in column 1): its dot, main's lane crossing column 0, nothing between.
        assert_eq!(
            hit(&rows[1], at(1.), px(15.), height, unit, None),
            Some(rows[1].node.color)
        );
        assert_eq!(
            hit(&rows[1], at(0.) + px(3.), px(2.), height, unit, None),
            Some(rows[0].node.color)
        );
        assert_eq!(hit(&rows[1], at(0.5), px(15.), height, unit, None), None);
        // Row 0 (merge m): the Out edge to column 1 is hit near its bottom end only.
        let side = rows[0].edges.iter().find(|e| e.column == 1).unwrap().color;
        assert_eq!(
            hit(&rows[0], at(1.), px(28.), height, unit, None),
            Some(side)
        );
        assert_eq!(hit(&rows[0], at(1.), px(5.), height, unit, None), None);
    }
}
