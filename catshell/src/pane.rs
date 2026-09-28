//! The split layout of a tab.
//!
//! A tab's panes form a binary tree: every split divides a rectangle in two, and each
//! leaf is one terminal. That shape is what makes splitting, closing and dragging a
//! divider local operations — each touches one node — rather than a re-flow of a list.
//!
//! Nothing here talks to a terminal or to the GPU, so the layout can be tested on its own.

use egui::{Rect, Vec2};

/// Identifies a terminal pane. Allocated by the app and never reused.
pub type PaneId = u64;

/// Which way a split divides its rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Children side by side, divided by a vertical line.
    Horizontal,
    /// Children stacked, divided by a horizontal line.
    Vertical,
}

/// The path from the root to a split node: `false` descends into the first child,
/// `true` into the second.
pub type SplitPath = Vec<bool>;

/// A tab's pane tree.
#[derive(Debug, Clone, PartialEq)]
pub enum Layout {
    Leaf(PaneId),
    Split {
        direction: Direction,
        /// Fraction of the space given to the first child, in `0.05..=0.95`.
        ratio: f32,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

/// Keeps a divider from being dragged until a pane has no room left.
const MIN_RATIO: f32 = 0.05;

impl Layout {
    pub fn new(pane: PaneId) -> Self {
        Layout::Leaf(pane)
    }

    /// Every pane in the tree, left to right and top to bottom.
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_panes(&mut out);
        out
    }

    fn collect_panes(&self, out: &mut Vec<PaneId>) {
        match self {
            Layout::Leaf(pane) => out.push(*pane),
            Layout::Split { first, second, .. } => {
                first.collect_panes(out);
                second.collect_panes(out);
            }
        }
    }

    /// Split the rectangle occupied by `target`, putting `new_pane` in the second half.
    ///
    /// Returns `false` if `target` is not in this tree.
    pub fn split(&mut self, target: PaneId, direction: Direction, new_pane: PaneId) -> bool {
        match self {
            Layout::Leaf(pane) if *pane == target => {
                *self = Layout::Split {
                    direction,
                    ratio: 0.5,
                    first: Box::new(Layout::Leaf(target)),
                    second: Box::new(Layout::Leaf(new_pane)),
                };
                true
            }
            Layout::Leaf(_) => false,
            Layout::Split { first, second, .. } => {
                first.split(target, direction, new_pane)
                    || second.split(target, direction, new_pane)
            }
        }
    }

    /// Remove `pane`, collapsing the split that held it so its sibling takes the space.
    ///
    /// Returns `false` when the tree is now empty — the pane was the last one — in which
    /// case the caller should close the tab.
    pub fn close(&mut self, pane: PaneId) -> bool {
        match self {
            Layout::Leaf(id) => *id != pane,
            Layout::Split { first, second, .. } => {
                // A child that is the doomed leaf is replaced by its sibling; this is the
                // collapse that keeps the tree free of one-child splits.
                if matches!(**first, Layout::Leaf(id) if id == pane) {
                    *self = (**second).clone();
                    return true;
                }
                if matches!(**second, Layout::Leaf(id) if id == pane) {
                    *self = (**first).clone();
                    return true;
                }
                first.close(pane);
                second.close(pane);
                true
            }
        }
    }

    /// Assign a rectangle to every pane.
    ///
    /// `divider` is the gap left between panes, in points; it is taken out of the
    /// available space rather than overlapping the panes, so no terminal is ever drawn
    /// underneath a divider.
    pub fn layout(&self, rect: Rect, divider: f32) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        self.layout_into(rect, divider, &mut out);
        out
    }

    fn layout_into(&self, rect: Rect, divider: f32, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Layout::Leaf(pane) => out.push((*pane, rect)),
            Layout::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let (a, b) = split_rect(rect, *direction, *ratio, divider);
                first.layout_into(a, divider, out);
                second.layout_into(b, divider, out);
            }
        }
    }

    /// The draggable divider of every split, with the path needed to adjust it.
    pub fn dividers(&self, rect: Rect, divider: f32) -> Vec<(SplitPath, Rect, Direction)> {
        let mut out = Vec::new();
        self.dividers_into(rect, divider, &mut Vec::new(), &mut out);
        out
    }

    fn dividers_into(
        &self,
        rect: Rect,
        divider: f32,
        path: &mut SplitPath,
        out: &mut Vec<(SplitPath, Rect, Direction)>,
    ) {
        let Layout::Split {
            direction,
            ratio,
            first,
            second,
        } = self
        else {
            return;
        };

        let (a, b) = split_rect(rect, *direction, *ratio, divider);
        let handle = match direction {
            Direction::Horizontal => Rect::from_min_max(
                egui::pos2(a.max.x, rect.min.y),
                egui::pos2(b.min.x, rect.max.y),
            ),
            Direction::Vertical => Rect::from_min_max(
                egui::pos2(rect.min.x, a.max.y),
                egui::pos2(rect.max.x, b.min.y),
            ),
        };
        out.push((path.clone(), handle, *direction));

        path.push(false);
        first.dividers_into(a, divider, path, out);
        path.pop();

        path.push(true);
        second.dividers_into(b, divider, path, out);
        path.pop();
    }

    /// Set the ratio of the split at `path`, clamped so neither side collapses.
    pub fn set_ratio(&mut self, path: &[bool], ratio: f32) {
        let mut node = self;
        for step in path {
            let Layout::Split { first, second, .. } = node else {
                return;
            };
            node = if *step { second } else { first };
        }
        if let Layout::Split { ratio: current, .. } = node {
            *current = ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
        }
    }

    /// The pane after `pane` in layout order, wrapping around.
    ///
    /// Returns `None` only when the tree is empty, which cannot happen while a tab exists.
    pub fn next_pane(&self, pane: PaneId) -> Option<PaneId> {
        let panes = self.panes();
        let index = panes.iter().position(|id| *id == pane)?;
        panes.get((index + 1) % panes.len()).copied()
    }
}

/// Divide `rect` in two, reserving `divider` points between the halves.
fn split_rect(rect: Rect, direction: Direction, ratio: f32, divider: f32) -> (Rect, Rect) {
    let ratio = ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
    match direction {
        Direction::Horizontal => {
            // Never let the divider consume more than the rectangle has.
            let divider = divider.min(rect.width());
            let usable = rect.width() - divider;
            let first = (usable * ratio).max(0.0);
            let a = Rect::from_min_size(rect.min, Vec2::new(first, rect.height()));
            let b = Rect::from_min_max(egui::pos2(a.max.x + divider, rect.min.y), rect.max);
            (a, b)
        }
        Direction::Vertical => {
            let divider = divider.min(rect.height());
            let usable = rect.height() - divider;
            let first = (usable * ratio).max(0.0);
            let a = Rect::from_min_size(rect.min, Vec2::new(rect.width(), first));
            let b = Rect::from_min_max(egui::pos2(rect.min.x, a.max.y + divider), rect.max);
            (a, b)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: f32, h: f32) -> Rect {
        Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(w, h))
    }

    #[test]
    fn a_new_tab_has_one_pane_filling_the_area() {
        let layout = Layout::new(1);
        assert_eq!(layout.panes(), vec![1]);
        let placed = layout.layout(rect(800.0, 600.0), 4.0);
        assert_eq!(placed, vec![(1, rect(800.0, 600.0))]);
    }

    #[test]
    fn splitting_divides_the_space() {
        let mut layout = Layout::new(1);
        assert!(layout.split(1, Direction::Horizontal, 2));
        assert_eq!(layout.panes(), vec![1, 2]);

        let placed = layout.layout(rect(804.0, 600.0), 4.0);
        let (_, left) = placed[0];
        let (_, right) = placed[1];
        // 804 wide less a 4pt divider leaves 800, split evenly.
        assert_eq!(left.width(), 400.0);
        assert_eq!(right.width(), 400.0);
        // The divider gap is real space, not an overlap.
        assert_eq!(right.min.x - left.max.x, 4.0);
        assert_eq!(left.height(), 600.0);
    }

    #[test]
    fn splitting_an_unknown_pane_does_nothing() {
        let mut layout = Layout::new(1);
        assert!(!layout.split(99, Direction::Horizontal, 2));
        assert_eq!(layout.panes(), vec![1]);
    }

    #[test]
    fn splitting_a_nested_pane_works() {
        let mut layout = Layout::new(1);
        layout.split(1, Direction::Horizontal, 2);
        assert!(layout.split(2, Direction::Vertical, 3));
        assert_eq!(layout.panes(), vec![1, 2, 3]);
    }

    #[test]
    fn closing_a_pane_gives_its_space_to_its_sibling() {
        let mut layout = Layout::new(1);
        layout.split(1, Direction::Horizontal, 2);

        assert!(layout.close(2));
        assert_eq!(layout.panes(), vec![1]);
        // The survivor takes the whole area, with no leftover divider gap.
        assert_eq!(
            layout.layout(rect(800.0, 600.0), 4.0),
            vec![(1, rect(800.0, 600.0))]
        );
    }

    #[test]
    fn closing_collapses_only_the_split_that_held_the_pane() {
        let mut layout = Layout::new(1);
        layout.split(1, Direction::Horizontal, 2);
        layout.split(2, Direction::Vertical, 3);

        assert!(layout.close(3));
        assert_eq!(layout.panes(), vec![1, 2]);
        // Pane 1 and 2 are still side by side, so the outer split survived.
        let placed = layout.layout(rect(804.0, 600.0), 4.0);
        assert_eq!(placed[0].1.width(), 400.0);
        assert_eq!(placed[1].1.width(), 400.0);
    }

    #[test]
    fn closing_the_last_pane_reports_the_tab_is_empty() {
        let mut layout = Layout::new(1);
        assert!(
            !layout.close(1),
            "closing the only pane should report an empty tab"
        );
    }

    #[test]
    fn closing_an_unknown_pane_leaves_the_tree_alone() {
        let mut layout = Layout::new(1);
        layout.split(1, Direction::Horizontal, 2);
        let before = layout.clone();
        assert!(layout.close(99));
        assert_eq!(layout, before);
    }

    #[test]
    fn ratios_are_respected_and_clamped() {
        let mut layout = Layout::new(1);
        layout.split(1, Direction::Horizontal, 2);

        layout.set_ratio(&[], 0.25);
        let placed = layout.layout(rect(804.0, 600.0), 4.0);
        assert_eq!(placed[0].1.width(), 200.0);
        assert_eq!(placed[1].1.width(), 600.0);

        // A drag past the edge must leave the pane usable rather than zero-width.
        layout.set_ratio(&[], -5.0);
        let placed = layout.layout(rect(804.0, 600.0), 4.0);
        assert!(placed[0].1.width() > 0.0, "pane collapsed to nothing");
        assert!(placed[1].1.width() < 800.0);
    }

    #[test]
    fn dividers_sit_between_the_panes_they_separate() {
        let mut layout = Layout::new(1);
        layout.split(1, Direction::Horizontal, 2);
        layout.split(2, Direction::Vertical, 3);

        let area = rect(804.0, 604.0);
        let dividers = layout.dividers(area, 4.0);
        assert_eq!(dividers.len(), 2, "one divider per split");

        let placed: std::collections::HashMap<_, _> =
            layout.layout(area, 4.0).into_iter().collect();
        for (_, handle, direction) in &dividers {
            match direction {
                Direction::Horizontal => assert_eq!(handle.width(), 4.0),
                Direction::Vertical => assert_eq!(handle.height(), 4.0),
            }
            // A divider must not overlap any pane, or dragging it would steal clicks.
            for pane_rect in placed.values() {
                assert!(
                    !handle.intersects(*pane_rect) || handle.intersect(*pane_rect).area() < 0.01,
                    "divider {handle:?} overlaps pane {pane_rect:?}"
                );
            }
        }
    }

    #[test]
    fn dividers_address_the_split_they_belong_to() {
        let mut layout = Layout::new(1);
        layout.split(1, Direction::Horizontal, 2);
        layout.split(2, Direction::Vertical, 3);

        let dividers = layout.dividers(rect(804.0, 604.0), 4.0);
        // Adjusting via the reported path must move that split and no other.
        let nested = dividers.iter().find(|(path, ..)| !path.is_empty()).unwrap();
        layout.set_ratio(&nested.0, 0.25);

        let placed: std::collections::HashMap<_, _> =
            layout.layout(rect(804.0, 604.0), 4.0).into_iter().collect();
        assert_eq!(placed[&1].width(), 400.0, "outer split moved");
        assert_eq!(placed[&2].height(), 150.0, "inner split did not move");
    }

    #[test]
    fn focus_cycles_through_every_pane() {
        let mut layout = Layout::new(1);
        layout.split(1, Direction::Horizontal, 2);
        layout.split(2, Direction::Vertical, 3);

        assert_eq!(layout.next_pane(1), Some(2));
        assert_eq!(layout.next_pane(2), Some(3));
        // And wraps back to the start.
        assert_eq!(layout.next_pane(3), Some(1));
    }

    #[test]
    fn panes_never_overlap_or_leave_gaps_beyond_the_dividers() {
        let mut layout = Layout::new(1);
        layout.split(1, Direction::Horizontal, 2);
        layout.split(2, Direction::Vertical, 3);
        layout.split(1, Direction::Vertical, 4);

        let area = rect(1000.0, 800.0);
        let placed = layout.layout(area, 4.0);
        assert_eq!(placed.len(), 4);
        for (i, (_, a)) in placed.iter().enumerate() {
            assert!(area.contains_rect(*a), "pane escaped the tab area: {a:?}");
            for (_, b) in placed.iter().skip(i + 1) {
                assert!(
                    a.intersect(*b).area() < 0.01,
                    "panes overlap: {a:?} and {b:?}"
                );
            }
        }
    }

    #[test]
    fn a_tiny_area_does_not_produce_negative_sizes() {
        // Windows get dragged to nothing; the layout must stay well-formed.
        let mut layout = Layout::new(1);
        layout.split(1, Direction::Horizontal, 2);
        for (_, r) in layout.layout(rect(2.0, 2.0), 4.0) {
            assert!(
                r.width() >= 0.0 && r.height() >= 0.0,
                "negative size: {r:?}"
            );
        }
    }
}
