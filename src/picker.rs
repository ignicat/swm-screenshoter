//! Region geometry, multi-output void constraints, and picker layout math.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointF {
    pub x: f64,
    pub y: f64,
}

impl PointF {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn to_i32_round(self) -> Point {
        Point::new(self.x.round() as i32, self.y.round() as i32)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub w: i32,
    pub h: i32,
}

impl Size {
    pub const fn new(w: i32, h: i32) -> Self {
        Self { w, h }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub loc: Point,
    pub size: Size,
}

impl Rect {
    pub const fn from_xywh(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self {
            loc: Point::new(x, y),
            size: Size::new(w, h),
        }
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        let left = self.loc.x.max(other.loc.x);
        let top = self.loc.y.max(other.loc.y);
        let right = (self.loc.x + self.size.w).min(other.loc.x + other.size.w);
        let bottom = (self.loc.y + self.size.h).min(other.loc.y + other.size.h);
        let w = right - left;
        let h = bottom - top;
        (w > 0 && h > 0).then(|| Self::from_xywh(left, top, w, h))
    }

    pub fn merge(self, other: Self) -> Self {
        let left = self.loc.x.min(other.loc.x);
        let top = self.loc.y.min(other.loc.y);
        let right = (self.loc.x + self.size.w).max(other.loc.x + other.size.w);
        let bottom = (self.loc.y + self.size.h).max(other.loc.y + other.size.h);
        Self::from_xywh(left, top, right - left, bottom - top)
    }

    pub fn overlaps(self, other: Self) -> bool {
        self.intersection(other).is_some()
    }

    pub fn contains_point(self, p: Point) -> bool {
        p.x >= self.loc.x
            && p.y >= self.loc.y
            && p.x < self.loc.x + self.size.w
            && p.y < self.loc.y + self.size.h
    }

    pub fn contains_point_f(self, p: PointF) -> bool {
        p.x >= f64::from(self.loc.x)
            && p.y >= f64::from(self.loc.y)
            && p.x < f64::from(self.loc.x + self.size.w)
            && p.y < f64::from(self.loc.y + self.size.h)
    }

    #[cfg(test)]
    pub fn contains_rect(self, inner: Self) -> bool {
        inner.loc.x >= self.loc.x
            && inner.loc.y >= self.loc.y
            && inner.loc.x + inner.size.w <= self.loc.x + self.size.w
            && inner.loc.y + inner.size.h <= self.loc.y + self.size.h
    }
}

pub const PICKER_HANDLE_SIZE: i32 = 12;
pub const PICKER_BORDER_PX: f32 = 2.0;
pub const PICKER_DASH_PERIOD: f32 = 16.0;
pub const PICKER_DASH_LENGTH: f32 = 10.0;

pub struct PickerLayout {
    pub dim: Vec<Rect>,
    pub outline: Rect,
    pub handles: [Rect; 4],
}

pub fn picker_layout(output: Rect, selection: Rect) -> Option<PickerLayout> {
    let output_local = Rect::from_xywh(0, 0, output.size.w, output.size.h);
    let clipped = output.intersection(selection).map(|i| {
        Rect::from_xywh(
            i.loc.x - output.loc.x,
            i.loc.y - output.loc.y,
            i.size.w,
            i.size.h,
        )
    })?;

    let right = clipped.loc.x + clipped.size.w;
    let bottom = clipped.loc.y + clipped.size.h;
    let dim = [
        Rect::from_xywh(0, 0, output_local.size.w, clipped.loc.y.max(0)),
        Rect::from_xywh(
            0,
            bottom,
            output_local.size.w,
            (output_local.size.h - bottom).max(0),
        ),
        Rect::from_xywh(0, clipped.loc.y, clipped.loc.x.max(0), clipped.size.h),
        Rect::from_xywh(
            right,
            clipped.loc.y,
            (output_local.size.w - right).max(0),
            clipped.size.h,
        ),
    ]
    .into_iter()
    .filter(|rect| rect.size.w > 0 && rect.size.h > 0)
    .collect();

    let outline = Rect::from_xywh(
        selection.loc.x - output.loc.x,
        selection.loc.y - output.loc.y,
        selection.size.w,
        selection.size.h,
    );
    let outline_right = outline.loc.x + outline.size.w;
    let outline_bottom = outline.loc.y + outline.size.h;
    let handles = [
        outline.loc,
        Point::new(outline_right, outline.loc.y),
        Point::new(outline.loc.x, outline_bottom),
        Point::new(outline_right, outline_bottom),
    ]
    .map(|pt| {
        Rect::from_xywh(
            pt.x - PICKER_HANDLE_SIZE / 2,
            pt.y - PICKER_HANDLE_SIZE / 2,
            PICKER_HANDLE_SIZE,
            PICKER_HANDLE_SIZE,
        )
    });

    Some(PickerLayout {
        dim,
        outline,
        handles,
    })
}

const EDGE_GRAB_DISTANCE: f64 = 7.0;
const CORNER_GRAB_DISTANCE: f64 = 12.0;
const MOVE_THRESHOLD: f64 = 4.0;
const MIN_REGION_SIZE: i32 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorHint {
    Crosshair,
    Grabbing,
    ResizeTop,
    ResizeBottom,
    ResizeLeft,
    ResizeRight,
    ResizeTopLeft,
    ResizeTopRight,
    ResizeBottomLeft,
    ResizeBottomRight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Handle {
    Move,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Clone, Copy, Debug)]
struct Interaction {
    handle: Handle,
    start_pointer: PointF,
    start_region: Rect,
    moving: bool,
}

#[derive(Debug, Default)]
pub struct RegionPicker {
    bounds: Option<Rect>,
    outputs: Vec<Rect>,
    region: Option<Rect>,
    remembered: Option<Rect>,
    interaction: Option<Interaction>,
}

impl RegionPicker {
    pub fn set_remembered(&mut self, region: Option<Rect>) {
        self.remembered = region;
    }

    pub fn is_active(&self) -> bool {
        self.region.is_some()
    }

    pub fn region(&self) -> Option<Rect> {
        self.region
    }

    pub fn cursor_at(&self, pointer: PointF) -> CursorHint {
        if let Some(interaction) = self.interaction {
            return match interaction.handle {
                Handle::Move => CursorHint::Grabbing,
                Handle::Top => CursorHint::ResizeTop,
                Handle::Bottom => CursorHint::ResizeBottom,
                Handle::Left => CursorHint::ResizeLeft,
                Handle::Right => CursorHint::ResizeRight,
                Handle::TopLeft => CursorHint::ResizeTopLeft,
                Handle::TopRight => CursorHint::ResizeTopRight,
                Handle::BottomLeft => CursorHint::ResizeBottomLeft,
                Handle::BottomRight => CursorHint::ResizeBottomRight,
            };
        }
        let Some(region) = self.region else {
            return CursorHint::Crosshair;
        };
        match hit_test(region, pointer) {
            Some(Handle::Move) => CursorHint::Grabbing,
            Some(Handle::Top) => CursorHint::ResizeTop,
            Some(Handle::Bottom) => CursorHint::ResizeBottom,
            Some(Handle::Left) => CursorHint::ResizeLeft,
            Some(Handle::Right) => CursorHint::ResizeRight,
            Some(Handle::TopLeft) => CursorHint::ResizeTopLeft,
            Some(Handle::TopRight) => CursorHint::ResizeTopRight,
            Some(Handle::BottomLeft) => CursorHint::ResizeBottomLeft,
            Some(Handle::BottomRight) => CursorHint::ResizeBottomRight,
            None => CursorHint::Crosshair,
        }
    }

    pub fn adjust(&mut self, dx: i32, dy: i32, resize: bool) -> bool {
        let (Some(region), Some(bounds)) = (self.region, self.bounds) else {
            return false;
        };
        let before = region;
        let interaction = Interaction {
            handle: if resize {
                Handle::BottomRight
            } else {
                Handle::Move
            },
            start_pointer: PointF::new(0.0, 0.0),
            start_region: region,
            moving: true,
        };
        let region = transform_region(interaction, PointF::new(dx as f64, dy as f64), bounds);
        self.region = Some(constrain_region(region, bounds, &self.outputs));
        self.interaction = None;
        self.region != Some(before)
    }

    pub fn begin(&mut self, outputs: Vec<Rect>, active_output: Rect) -> Rect {
        let desktop_bounds = outputs
            .iter()
            .copied()
            .reduce(Rect::merge)
            .unwrap_or(active_output);
        self.bounds = Some(desktop_bounds);
        self.outputs = outputs;
        self.interaction = None;
        let initial = self
            .remembered
            .map(|region| constrain_region(region, desktop_bounds, &self.outputs))
            .unwrap_or_else(|| centered_half(active_output));
        let initial = constrain_region(initial, desktop_bounds, &self.outputs);
        self.region = Some(initial);
        initial
    }

    pub fn press(&mut self, pointer: PointF) -> bool {
        let Some(region) = self.region else {
            return false;
        };
        self.interaction = hit_test(region, pointer).map(|handle| Interaction {
            handle,
            start_pointer: pointer,
            start_region: region,
            moving: handle != Handle::Move,
        });
        true
    }

    pub fn motion(&mut self, pointer: PointF) -> bool {
        let (Some(bounds), Some(mut interaction)) = (self.bounds, self.interaction) else {
            return self.is_active();
        };
        let delta = PointF::new(
            pointer.x - interaction.start_pointer.x,
            pointer.y - interaction.start_pointer.y,
        );
        if interaction.handle == Handle::Move && !interaction.moving {
            interaction.moving = delta.x.hypot(delta.y) >= MOVE_THRESHOLD;
            self.interaction = Some(interaction);
            if !interaction.moving {
                return true;
            }
        }
        self.region = Some(constrain_region(
            transform_region(interaction, delta, bounds),
            bounds,
            &self.outputs,
        ));
        true
    }

    pub fn release(&mut self) -> bool {
        if !self.is_active() {
            return false;
        }
        self.interaction = None;
        true
    }

    pub fn accept(&mut self) -> Option<Rect> {
        self.finish()
    }

    fn finish(&mut self) -> Option<Rect> {
        self.interaction = None;
        let region = self.region.take()?;
        self.remembered = Some(match self.bounds {
            Some(bounds) => constrain_region(region, bounds, &self.outputs),
            None => region,
        });
        Some(region)
    }
}

fn centered_half(output: Rect) -> Rect {
    let w = (output.size.w / 2).max(MIN_REGION_SIZE).min(output.size.w);
    let h = (output.size.h / 2).max(MIN_REGION_SIZE).min(output.size.h);
    Rect::from_xywh(
        output.loc.x + (output.size.w - w) / 2,
        output.loc.y + (output.size.h - h) / 2,
        w,
        h,
    )
}

fn clamp_region(region: Rect, bounds: Rect) -> Rect {
    let w = region.size.w.max(1).min(bounds.size.w.max(1));
    let h = region.size.h.max(1).min(bounds.size.h.max(1));
    let x = region
        .loc
        .x
        .clamp(bounds.loc.x, bounds.loc.x + bounds.size.w - w);
    let y = region
        .loc
        .y
        .clamp(bounds.loc.y, bounds.loc.y + bounds.size.h - h);
    Rect::from_xywh(x, y, w, h)
}

pub fn constrain_region(region: Rect, desktop_bounds: Rect, outputs: &[Rect]) -> Rect {
    let mut region = clamp_region(region, desktop_bounds);
    if !outputs.iter().any(|output| region.overlaps(*output)) {
        return outputs
            .iter()
            .copied()
            .min_by_key(|output| distance_to_rect_squared(region.loc, *output))
            .map(|output| clamp_region(region, output))
            .unwrap_or(region);
    }
    if let Some((top, bottom)) = covered_vertical_span(region, outputs) {
        region = clamp_vertical(region, top, bottom);
    }
    if let Some((left, right)) = covered_horizontal_span(region, outputs) {
        region = clamp_horizontal(region, left, right);
    }
    if let Some((top, bottom)) = covered_vertical_span(region, outputs) {
        region = clamp_vertical(region, top, bottom);
    }
    region
}

fn merged_runs(mut intervals: Vec<(i32, i32)>) -> Vec<(i32, i32)> {
    intervals.retain(|(s, e)| s < e);
    intervals.sort_unstable();
    let mut runs: Vec<(i32, i32)> = Vec::new();
    for (s, e) in intervals {
        match runs.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => runs.push((s, e)),
        }
    }
    runs
}

fn covered_vertical_span(region: Rect, outputs: &[Rect]) -> Option<(i32, i32)> {
    let left = region.loc.x;
    let right = left + region.size.w;
    let probe = region.loc.y + region.size.h / 2;
    let mut edges = vec![left, right];
    for output in outputs {
        for edge in [output.loc.x, output.loc.x + output.size.w] {
            if edge > left && edge < right {
                edges.push(edge);
            }
        }
    }
    edges.sort_unstable();
    edges.dedup();
    let mut span: Option<(i32, i32)> = None;
    for slab in edges.windows(2) {
        let (start, end) = (slab[0], slab[1]);
        let covering = outputs
            .iter()
            .filter(|o| o.loc.x <= start && o.loc.x + o.size.w >= end)
            .map(|o| (o.loc.y, o.loc.y + o.size.h))
            .collect::<Vec<_>>();
        let run = merged_runs(covering)
            .into_iter()
            .find(|(t, b)| (*t..*b).contains(&probe))?;
        span = Some(match span {
            Some((t, b)) => (t.max(run.0), b.min(run.1)),
            None => run,
        });
    }
    span.filter(|(t, b)| t < b)
}

fn covered_horizontal_span(region: Rect, outputs: &[Rect]) -> Option<(i32, i32)> {
    let top = region.loc.y;
    let bottom = top + region.size.h;
    let probe = region.loc.x + region.size.w / 2;
    let mut edges = vec![top, bottom];
    for output in outputs {
        for edge in [output.loc.y, output.loc.y + output.size.h] {
            if edge > top && edge < bottom {
                edges.push(edge);
            }
        }
    }
    edges.sort_unstable();
    edges.dedup();
    let mut span: Option<(i32, i32)> = None;
    for slab in edges.windows(2) {
        let (start, end) = (slab[0], slab[1]);
        let covering = outputs
            .iter()
            .filter(|o| o.loc.y <= start && o.loc.y + o.size.h >= end)
            .map(|o| (o.loc.x, o.loc.x + o.size.w))
            .collect::<Vec<_>>();
        let run = merged_runs(covering)
            .into_iter()
            .find(|(l, r)| (*l..*r).contains(&probe))?;
        span = Some(match span {
            Some((l, r)) => (l.max(run.0), r.min(run.1)),
            None => run,
        });
    }
    span.filter(|(l, r)| l < r)
}

fn clamp_vertical(region: Rect, top: i32, bottom: i32) -> Rect {
    let height = region.size.h.min((bottom - top).max(1));
    Rect::from_xywh(
        region.loc.x,
        region.loc.y.clamp(top, bottom - height),
        region.size.w,
        height,
    )
}

fn clamp_horizontal(region: Rect, left: i32, right: i32) -> Rect {
    let width = region.size.w.min((right - left).max(1));
    Rect::from_xywh(
        region.loc.x.clamp(left, right - width),
        region.loc.y,
        width,
        region.size.h,
    )
}

fn distance_to_rect_squared(point: Point, rect: Rect) -> i64 {
    let right = rect.loc.x + rect.size.w - 1;
    let bottom = rect.loc.y + rect.size.h - 1;
    let dx = i64::from(point.x - point.x.clamp(rect.loc.x, right));
    let dy = i64::from(point.y - point.y.clamp(rect.loc.y, bottom));
    dx * dx + dy * dy
}

fn hit_test(region: Rect, pointer: PointF) -> Option<Handle> {
    let left = f64::from(region.loc.x);
    let top = f64::from(region.loc.y);
    let right = f64::from(region.loc.x + region.size.w);
    let bottom = f64::from(region.loc.y + region.size.h);
    for (x, y, handle) in [
        (left, top, Handle::TopLeft),
        (right, top, Handle::TopRight),
        (left, bottom, Handle::BottomLeft),
        (right, bottom, Handle::BottomRight),
    ] {
        if (pointer.x - x).hypot(pointer.y - y) <= CORNER_GRAB_DISTANCE {
            return Some(handle);
        }
    }
    let within_x = pointer.x >= left && pointer.x <= right;
    let within_y = pointer.y >= top && pointer.y <= bottom;
    if within_x && (pointer.y - top).abs() <= EDGE_GRAB_DISTANCE {
        return Some(Handle::Top);
    }
    if within_x && (pointer.y - bottom).abs() <= EDGE_GRAB_DISTANCE {
        return Some(Handle::Bottom);
    }
    if within_y && (pointer.x - left).abs() <= EDGE_GRAB_DISTANCE {
        return Some(Handle::Left);
    }
    if within_y && (pointer.x - right).abs() <= EDGE_GRAB_DISTANCE {
        return Some(Handle::Right);
    }
    (within_x && within_y).then_some(Handle::Move)
}

fn transform_region(interaction: Interaction, delta: PointF, bounds: Rect) -> Rect {
    let dx = delta.x.round() as i32;
    let dy = delta.y.round() as i32;
    let start = interaction.start_region;
    if interaction.handle == Handle::Move {
        return clamp_region(
            Rect::from_xywh(
                start.loc.x + dx,
                start.loc.y + dy,
                start.size.w,
                start.size.h,
            ),
            bounds,
        );
    }
    let mut left = start.loc.x;
    let mut top = start.loc.y;
    let mut right = start.loc.x + start.size.w;
    let mut bottom = start.loc.y + start.size.h;
    let bounds_right = bounds.loc.x + bounds.size.w;
    let bounds_bottom = bounds.loc.y + bounds.size.h;
    if matches!(
        interaction.handle,
        Handle::Left | Handle::TopLeft | Handle::BottomLeft
    ) {
        left = (left + dx).clamp(bounds.loc.x, right - MIN_REGION_SIZE);
    }
    if matches!(
        interaction.handle,
        Handle::Right | Handle::TopRight | Handle::BottomRight
    ) {
        right = (right + dx).clamp(left + MIN_REGION_SIZE, bounds_right);
    }
    if matches!(
        interaction.handle,
        Handle::Top | Handle::TopLeft | Handle::TopRight
    ) {
        top = (top + dy).clamp(bounds.loc.y, bottom - MIN_REGION_SIZE);
    }
    if matches!(
        interaction.handle,
        Handle::Bottom | Handle::BottomLeft | Handle::BottomRight
    ) {
        bottom = (bottom + dy).clamp(top + MIN_REGION_SIZE, bounds_bottom);
    }
    Rect::from_xywh(left, top, right - left, bottom - top)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_outline_and_grips_match_selection() {
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let selection = Rect::from_xywh(320, 180, 1280, 720);
        let layout = picker_layout(output, selection).unwrap();
        assert_eq!(layout.outline, selection);
        assert_eq!(layout.handles[0], Rect::from_xywh(314, 174, 12, 12));
        assert_eq!(layout.handles[3], Rect::from_xywh(1594, 894, 12, 12));
    }

    #[test]
    fn selection_spanning_outputs_keeps_one_grip_set() {
        let main = Rect::from_xywh(0, 0, 2560, 1440);
        let secondary = Rect::from_xywh(2560, 0, 1920, 1200);
        let selection = Rect::from_xywh(2000, 100, 1500, 800);
        let on_main = picker_layout(main, selection).unwrap();
        let on_secondary = picker_layout(secondary, selection).unwrap();
        assert_eq!(on_main.outline.loc.x - 2560, on_secondary.outline.loc.x);
        let visible = |layout: &PickerLayout, bounds: Rect| {
            let local = Rect::from_xywh(0, 0, bounds.size.w, bounds.size.h);
            layout
                .handles
                .iter()
                .filter(|h| local.overlaps(**h))
                .count()
        };
        assert_eq!(visible(&on_main, main), 2);
        assert_eq!(visible(&on_secondary, secondary), 2);
    }

    #[test]
    fn uneven_outputs_cap_spanning_selection() {
        let outputs = [
            Rect::from_xywh(0, 0, 2560, 1440),
            Rect::from_xywh(2560, 0, 1920, 1200),
        ];
        let selected = Rect::from_xywh(2000, 0, 1000, 1440);
        let desktop = Rect::from_xywh(0, 0, 4480, 1440);
        assert_eq!(
            constrain_region(selected, desktop, &outputs),
            Rect::from_xywh(2000, 0, 1000, 1200)
        );
    }

    #[test]
    fn keyboard_adjustment_moves_and_resizes_safely() {
        let output = Rect::from_xywh(0, 0, 800, 600);
        let mut picker = RegionPicker::default();
        let initial = picker.begin(vec![output], output);
        assert!(picker.adjust(10, 0, false));
        let moved = picker.region().unwrap();
        assert_eq!(moved.loc.x, initial.loc.x + 10);
        assert!(picker.adjust(10, 10, true));
        let resized = picker.region().unwrap();
        assert_eq!(resized.size.w, moved.size.w + 10);
    }
}
