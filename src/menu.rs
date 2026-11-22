//! Interactive capture mode bar layout and state machine.

use crate::picker::{PointF, Rect};

const BAR_WIDTH: i32 = 420;
const BAR_HEIGHT: i32 = 80;
const BAR_BOTTOM_MARGIN: i32 = 24;
const ITEM_PADDING: i32 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenshotMode {
    Region,
    Screen,
    Window,
}

impl ScreenshotMode {
    pub const ALL: [Self; 3] = [Self::Region, Self::Screen, Self::Window];
}

#[derive(Clone, Debug)]
pub struct ScreenshotMenu {
    output_name: String,
    output_geometry: Rect,
    selected: usize,
    hovered: Option<usize>,
    window_available: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuLayout {
    pub bar: Rect,
    pub items: [Rect; 3],
}

impl ScreenshotMenu {
    pub fn new(output_name: String, output_geometry: Rect, window_available: bool) -> Self {
        Self {
            output_name,
            output_geometry,
            selected: 0,
            hovered: None,
            window_available,
        }
    }

    pub fn output_name(&self) -> &str {
        &self.output_name
    }

    pub fn output_geometry(&self) -> Rect {
        self.output_geometry
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn hovered(&self) -> Option<usize> {
        self.hovered
    }

    pub fn window_available(&self) -> bool {
        self.window_available
    }

    pub fn selected_mode(&self) -> ScreenshotMode {
        ScreenshotMode::ALL[self.selected]
    }

    pub fn hit_test(&self, position: PointF) -> Option<usize> {
        let p = position.to_i32_round();
        layout(self.output_geometry)
            .items
            .iter()
            .position(|item| item.contains_point(p))
            .filter(|index| self.is_enabled(*index))
    }

    pub fn hover(&mut self, position: PointF) -> bool {
        let hovered = self.hit_test(position);
        let changed = self.hovered != hovered;
        self.hovered = hovered;
        if let Some(index) = hovered {
            self.selected = index;
        }
        changed
    }

    pub fn move_selection(&mut self, delta: i32) -> bool {
        let direction = if delta < 0 { -1 } else { 1 };
        let count = ScreenshotMode::ALL.len() as i32;
        let mut candidate = self.selected as i32;
        for _ in 0..count {
            candidate = (candidate + direction).rem_euclid(count);
            if self.is_enabled(candidate as usize) {
                self.selected = candidate as usize;
                self.hovered = Some(self.selected);
                return true;
            }
        }
        false
    }

    fn is_enabled(&self, index: usize) -> bool {
        ScreenshotMode::ALL
            .get(index)
            .is_some_and(|mode| *mode != ScreenshotMode::Window || self.window_available)
    }
}

pub fn layout(output: Rect) -> MenuLayout {
    let width = BAR_WIDTH.min(output.size.w.max(1));
    let height = BAR_HEIGHT.min(output.size.h.max(1));
    let x = output.loc.x + (output.size.w - width) / 2;
    let y = output.loc.y + (output.size.h - height - BAR_BOTTOM_MARGIN).max(0);
    let bar = Rect::from_xywh(x, y, width, height);
    let slot_width = width / 3;
    let item = |index: i32| {
        let left = x + index * slot_width;
        let right = if index == 2 {
            x + width
        } else {
            left + slot_width
        };
        let horizontal_padding = ITEM_PADDING.min((right - left).max(0) / 2);
        let vertical_padding = ITEM_PADDING.min(height.max(0) / 2);
        Rect::from_xywh(
            left + horizontal_padding,
            y + vertical_padding,
            right - left - horizontal_padding * 2,
            height - vertical_padding * 2,
        )
    };
    MenuLayout {
        bar,
        items: [item(0), item(1), item(2)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output() -> Rect {
        Rect::from_xywh(1920, 0, 1920, 1080)
    }

    fn center(r: Rect) -> PointF {
        PointF::new(
            f64::from(r.loc.x) + f64::from(r.size.w) / 2.0,
            f64::from(r.loc.y) + f64::from(r.size.h) / 2.0,
        )
    }

    #[test]
    fn layout_is_centered_near_bottom() {
        let layout = layout(output());
        assert_eq!(layout.bar, Rect::from_xywh(2670, 976, 420, 80));
        let _ = crate::picker::Size::new(0, 0);
        assert!(
            layout
                .items
                .iter()
                .all(|item| layout.bar.contains_rect(*item))
        );
    }

    #[test]
    fn hit_testing_ignores_disabled_window_button() {
        let mut menu = ScreenshotMenu::new("DP-2".into(), output(), false);
        let layout = layout(output());
        assert!(!menu.hover(center(layout.items[2])));
        assert_eq!(menu.hovered(), None);
        assert!(menu.move_selection(1));
        assert_eq!(menu.selected_mode(), ScreenshotMode::Screen);
        assert!(menu.move_selection(1));
        assert_eq!(menu.selected_mode(), ScreenshotMode::Region);
        assert!(menu.move_selection(-1));
        assert_eq!(menu.selected_mode(), ScreenshotMode::Screen);
    }
}
