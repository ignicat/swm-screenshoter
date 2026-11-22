//! Software vector, SVG, and text renderer onto `wl_shm` ARGB8888 buffers.

use std::collections::HashMap;
use std::path::Path;

use cosmic_text::{
    Attrs, Buffer as TextBuffer, Color, Family, FontSystem, Hinting, Metrics, Shaping, Style,
    SwashCache, Weight,
};
use resvg::usvg;
use tiny_skia::{
    Color as SkiaColor, FillRule, LineCap, LineJoin, Paint, Path as SkiaPath, PathBuilder, Pixmap,
    PixmapMut, Rect as SkiaRect, Stroke, StrokeDash, Transform,
};

use crate::menu::{self, ScreenshotMenu};
use crate::picker::{
    PICKER_BORDER_PX, PICKER_DASH_LENGTH, PICKER_DASH_PERIOD, PointF, Rect, Size, picker_layout,
};
use crate::sync::{BACKDROP_DIM, NotificationPosition, OverlayVisuals, SyncedSettings};

const REGION_SVG: &[u8] = include_bytes!("../assets/region.svg");
const MONITOR_SVG: &[u8] = include_bytes!("../assets/screen.svg");
const WINDOW_SVG: &[u8] = include_bytes!("../assets/window.svg");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureIcon {
    Region,
    Monitor,
    Window,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastAction {
    Copy,
    Open,
}

#[derive(Clone, Debug)]
pub struct ToastLayout {
    pub card: Rect,
    pub preview: Rect,
    pub title: Rect,
    pub subtitle: Rect,
    pub copy: Rect,
    pub open: Rect,
}

impl ToastLayout {
    pub fn action_at(&self, point: PointF) -> Option<ToastAction> {
        if self.copy.contains_point_f(point) {
            Some(ToastAction::Copy)
        } else if self.open.contains_point_f(point) {
            Some(ToastAction::Open)
        } else {
            None
        }
    }
}

pub fn notification_rect(
    output: Rect,
    size: Size,
    position: NotificationPosition,
    offset_x: i32,
    offset_y: i32,
    mix: f32,
) -> Rect {
    use NotificationPosition::*;
    let margin = 24;
    let slide = ((1.0 - mix) * 8.0).round() as i32;
    let x = match position {
        TopLeft | BottomLeft => margin,
        TopCenter | BottomCenter => (output.size.w - size.w) / 2,
        TopRight | BottomRight => output.size.w - size.w - margin,
    };
    let y = match position {
        TopLeft | TopCenter | TopRight => margin - slide,
        BottomLeft | BottomCenter | BottomRight => output.size.h - size.h - margin + slide,
    };
    Rect::from_xywh(
        output.loc.x.saturating_add(x).saturating_add(offset_x),
        output.loc.y.saturating_add(y).saturating_add(offset_y),
        size.w,
        size.h,
    )
}

pub fn toast_layout(output: Rect, settings: &SyncedSettings, mix: f32) -> ToastLayout {
    let width = (output.size.w - 48).clamp(80, 360);
    let padding = 16;
    let inner = width - padding * 2;
    let line = (f32::from(settings.font_size.max(8)) * 1.25).ceil() as i32;
    let button_height = line + 16;
    let preview_height = (output.size.h - (line * 2 + button_height + 112)).clamp(24, 164);
    let height = preview_height + line * 2 + button_height + padding * 2 + 28;
    let card = notification_rect(
        output,
        Size::new(width, height),
        settings.notification_position,
        settings.notification_offset_x,
        settings.notification_offset_y,
        mix,
    );
    let r = |x, y, w, h| Rect::from_xywh(card.loc.x + x, card.loc.y + y, w, h);
    let title_y = padding + preview_height + 12;
    let buttons_y = title_y + line * 2 + 16;
    let button_width = (inner - 8) / 2;
    ToastLayout {
        card,
        preview: r(padding, padding, inner, preview_height),
        title: r(padding, title_y, inner, line),
        subtitle: r(padding, title_y + line + 4, inner, line),
        copy: r(padding, buttons_y, button_width, button_height),
        open: r(
            padding + button_width + 8,
            buttons_y,
            button_width,
            button_height,
        ),
    }
}

pub enum OverlayMode<'a> {
    Menu(&'a ScreenshotMenu),
    Region(Rect),
    Highlight(Option<Rect>),
}

pub struct TextRenderer {
    fonts: FontSystem,
    swash: SwashCache,
    icons: HashMap<(u8, u32, [u8; 3]), Pixmap>,
}

impl Default for TextRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl TextRenderer {
    pub fn new() -> Self {
        Self {
            fonts: FontSystem::new(),
            swash: SwashCache::new(),
            icons: HashMap::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw_selector(
        &mut self,
        output_rect: Rect,
        backdrop_premul_rgba: &[u8],
        buf_w: u32,
        buf_h: u32,
        scale: f64,
        mode: OverlayMode<'_>,
        visuals: &OverlayVisuals,
        out_argb8888: &mut [u8],
    ) {
        let mut pixmap = Pixmap::new(buf_w.max(1), buf_h.max(1)).expect("pixmap");
        let expected = (buf_w as usize) * (buf_h as usize) * 4;
        if backdrop_premul_rgba.len() == expected {
            pixmap.data_mut().copy_from_slice(backdrop_premul_rgba);
        }

        let s = scale as f32;
        let to_px = |v: i32| (v as f32 * s).round();

        match mode {
            OverlayMode::Menu(menu) => {
                let full = SkiaRect::from_xywh(0.0, 0.0, buf_w as f32, buf_h as f32);
                if let Some(rect) = full {
                    let mut paint = Paint::default();
                    paint.set_color(BACKDROP_DIM.to_tiny_color(1.0));
                    pixmap.fill_rect(rect, &paint, Transform::identity(), None);
                }

                if menu.output_geometry() == output_rect {
                    let layout = menu::layout(output_rect);
                    let localize = |r: Rect| {
                        (
                            to_px(r.loc.x - output_rect.loc.x),
                            to_px(r.loc.y - output_rect.loc.y),
                            to_px(r.size.w),
                            to_px(r.size.h),
                        )
                    };

                    let (bx, by, bw, bh) = localize(layout.bar);
                    draw_card(
                        &mut pixmap.as_mut(),
                        bx,
                        by,
                        bw,
                        bh,
                        visuals.radius * s,
                        visuals.border_px * s,
                        visuals.fill.to_tiny_color(0.96),
                        visuals.border.to_tiny_color(0.96),
                    );

                    let icons = [
                        CaptureIcon::Region,
                        CaptureIcon::Monitor,
                        CaptureIcon::Window,
                    ];
                    for (idx, item) in layout.items.into_iter().enumerate() {
                        let disabled = idx == 2 && !menu.window_available();
                        let active =
                            !disabled && (menu.selected() == idx || menu.hovered() == Some(idx));
                        let (ix, iy, iw, ih) = localize(item);
                        let fill = if disabled {
                            visuals.key_fill.mix(visuals.fill, 0.55)
                        } else if active {
                            visuals.fill.mix(visuals.border, 0.12)
                        } else {
                            visuals.key_fill
                        };
                        let accent = if disabled {
                            visuals.subtext.mix(visuals.fill, 0.45)
                        } else if active {
                            visuals.border
                        } else {
                            visuals.subtext
                        };
                        let border_w = if visuals.border_px > 0.0 {
                            2.0 * s
                        } else {
                            0.0
                        };
                        let alpha = if disabled {
                            0.50
                        } else if active {
                            0.98
                        } else {
                            0.94
                        };
                        draw_card(
                            &mut pixmap.as_mut(),
                            ix,
                            iy,
                            iw,
                            ih,
                            (visuals.radius * s - 2.0 * s).max(2.0),
                            border_w,
                            fill.to_tiny_color(alpha),
                            accent.to_tiny_color(alpha),
                        );

                        let icon_px = ((42.0 * s)
                            .round()
                            .min(iw - 12.0 * s)
                            .min(ih - 12.0 * s)
                            .max(12.0 * s)) as u32;
                        let icon_rgb = if active {
                            visuals.text.bytes()
                        } else {
                            visuals.subtext.bytes()
                        };
                        if let Some(icon_pm) = self.cached_icon(icons[idx], icon_px, icon_rgb) {
                            let dx = (ix + (iw - icon_px as f32) * 0.5).round() as i32;
                            let dy = (iy + (ih - icon_px as f32) * 0.5).round() as i32;
                            let icon_alpha = if disabled {
                                0.30
                            } else if active {
                                1.0
                            } else {
                                0.72
                            };
                            let paint = tiny_skia::PixmapPaint {
                                opacity: icon_alpha,
                                ..tiny_skia::PixmapPaint::default()
                            };
                            pixmap.draw_pixmap(
                                dx,
                                dy,
                                icon_pm.as_ref(),
                                &paint,
                                Transform::identity(),
                                None,
                            );
                        }
                    }
                }
            }
            OverlayMode::Region(selection) => {
                draw_selection_overlay(
                    &mut pixmap.as_mut(),
                    output_rect,
                    buf_w,
                    buf_h,
                    s,
                    Some(selection),
                    true,
                    visuals,
                );
            }
            OverlayMode::Highlight(selection) => {
                draw_selection_overlay(
                    &mut pixmap.as_mut(),
                    output_rect,
                    buf_w,
                    buf_h,
                    s,
                    selection,
                    false,
                    visuals,
                );
            }
        }

        rgba_premul_to_argb8888(pixmap.data(), out_argb8888);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw_toast(
        &mut self,
        card_w: u32,
        card_h: u32,
        scale: f64,
        layout: &ToastLayout,
        settings: &SyncedSettings,
        message: &str,
        saved_path: &Path,
        thumb_w: u32,
        thumb_h: u32,
        thumb_premul: &[u8],
        hovered: Option<ToastAction>,
        mix: f32,
        out_argb8888: &mut [u8],
    ) {
        let mut pixmap = Pixmap::new(card_w.max(1), card_h.max(1)).expect("toast pixmap");
        let s = scale as f32;
        let ox = layout.card.loc.x;
        let oy = layout.card.loc.y;
        let visuals = &settings.visuals;

        draw_card(
            &mut pixmap.as_mut(),
            0.0,
            0.0,
            layout.card.size.w as f32 * s,
            layout.card.size.h as f32 * s,
            visuals.radius * s,
            visuals.border_px * s,
            visuals.fill.to_tiny_color(0.97 * mix),
            visuals.border.to_tiny_color(mix),
        );

        let local = |r: Rect| {
            (
                (r.loc.x - ox) as f32 * s,
                (r.loc.y - oy) as f32 * s,
                r.size.w as f32 * s,
                r.size.h as f32 * s,
            )
        };

        let (px, py, pw, ph) = local(layout.preview);
        draw_card(
            &mut pixmap.as_mut(),
            px,
            py,
            pw,
            ph,
            (visuals.radius * 0.6).min(6.0) * s,
            0.0,
            visuals.key_fill.to_tiny_color(mix),
            SkiaColor::TRANSPARENT,
        );
        if thumb_w > 0
            && thumb_h > 0
            && thumb_premul.len() == (thumb_w * thumb_h * 4) as usize
            && let Some(src) = Pixmap::from_vec(
                thumb_premul.to_vec(),
                tiny_skia::IntSize::from_wh(thumb_w, thumb_h).expect("thumb size"),
            )
        {
            let ts = (pw / thumb_w as f32).min(ph / thumb_h as f32);
            let dx = px + (pw - thumb_w as f32 * ts) * 0.5;
            let dy = py + (ph - thumb_h as f32 * ts) * 0.5;
            let transform = Transform::from_scale(ts, ts).post_translate(dx, dy);
            let paint = tiny_skia::PixmapPaint {
                opacity: mix,
                quality: tiny_skia::FilterQuality::Bilinear,
                ..tiny_skia::PixmapPaint::default()
            };
            pixmap.draw_pixmap(0, 0, src.as_ref(), &paint, transform, None);
        }

        let font_px = ((settings.font_size as f64) * scale).round().max(1.0) as u16;

        let (tx, ty, tw, th) = local(layout.title);
        self.draw_text_line(
            &mut pixmap.as_mut(),
            message,
            &settings.font_family,
            font_px,
            visuals.text.bytes(),
            mix,
            tx,
            ty,
            tw,
            th,
            false,
        );

        let dir_str = saved_path
            .parent()
            .unwrap_or(saved_path)
            .display()
            .to_string();
        let (sx, sy, sw, sh) = local(layout.subtitle);
        self.draw_text_line(
            &mut pixmap.as_mut(),
            &dir_str,
            &settings.font_family,
            font_px,
            visuals.subtext.bytes(),
            mix,
            sx,
            sy,
            sw,
            sh,
            false,
        );

        for (rect, label, action) in [
            (layout.copy, "Copy", ToastAction::Copy),
            (layout.open, "Open", ToastAction::Open),
        ] {
            let (bx, by, bw, bh) = local(rect);
            let is_hover = hovered == Some(action);
            let fill = if is_hover {
                visuals.key_fill.mix(visuals.border, 0.18)
            } else {
                visuals.key_fill
            };
            draw_card(
                &mut pixmap.as_mut(),
                bx,
                by,
                bw,
                bh,
                (visuals.radius * 0.6).min(6.0) * s,
                if is_hover { 1.5 * s } else { 0.0 },
                fill.to_tiny_color(mix),
                visuals.border.to_tiny_color(mix),
            );
            self.draw_text_line(
                &mut pixmap.as_mut(),
                label,
                &settings.font_family,
                font_px,
                visuals.text.bytes(),
                mix,
                bx,
                by,
                bw,
                bh,
                true,
            );
        }

        rgba_premul_to_argb8888(pixmap.data(), out_argb8888);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text_line(
        &mut self,
        pixmap: &mut PixmapMut<'_>,
        text: &str,
        family: &str,
        size_px: u16,
        rgb: [u8; 3],
        alpha: f32,
        x: f32,
        y: f32,
        max_w: f32,
        box_h: f32,
        centered: bool,
    ) {
        let fitted = self.fit_middle(text, family, size_px, (max_w - 8.0).max(16.0));
        if fitted.is_empty() {
            return;
        }
        let metrics = Metrics::new(size_px as f32, (size_px as f32 * 1.25).ceil());
        let mut buffer = TextBuffer::new(&mut self.fonts, metrics);
        buffer.set_hinting(if size_px <= 16 {
            Hinting::Enabled
        } else {
            Hinting::Disabled
        });
        buffer.set_size(None, None);
        let parsed = parse_font_request(family);
        buffer.set_text(
            &fitted,
            &Attrs::new()
                .family(resolve_family(&self.fonts, parsed.0))
                .style(parsed.1)
                .weight(parsed.2),
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut self.fonts, true);
        let text_w = buffer.layout_runs().next().map(|r| r.line_w).unwrap_or(0.0);
        let start_x = x + if centered {
            ((max_w - text_w) * 0.5).max(0.0)
        } else {
            0.0
        };
        let start_y = y + ((box_h - metrics.line_height) * 0.5).max(0.0);

        let pw = pixmap.width() as i32;
        let ph = pixmap.height() as i32;
        let data = pixmap.data_mut();
        let a_scale = alpha.clamp(0.0, 1.0);
        let color = Color::rgba(rgb[0], rgb[1], rgb[2], 255);

        for run in buffer.layout_runs() {
            for glyph in run.glyphs {
                let physical = glyph.physical((start_x, start_y + run.line_y), 1.0);
                self.swash
                    .with_pixels(&mut self.fonts, physical.cache_key, color, |gx, gy, px| {
                        let px_x = physical.x + gx;
                        let px_y = physical.y + gy;
                        if px_x < 0 || px_y < 0 || px_x >= pw || px_y >= ph {
                            return;
                        }
                        let src_a = (px.a() as f32 / 255.0) * a_scale;
                        if src_a <= 0.0 {
                            return;
                        }
                        let idx = ((px_y as usize) * (pw as usize) + (px_x as usize)) * 4;
                        let inv = 1.0 - src_a;
                        let sr = px.r() as f32 * src_a;
                        let sg = px.g() as f32 * src_a;
                        let sb = px.b() as f32 * src_a;
                        let sa = 255.0 * src_a;
                        data[idx] = (sr + data[idx] as f32 * inv).round().clamp(0.0, 255.0) as u8;
                        data[idx + 1] =
                            (sg + data[idx + 1] as f32 * inv).round().clamp(0.0, 255.0) as u8;
                        data[idx + 2] =
                            (sb + data[idx + 2] as f32 * inv).round().clamp(0.0, 255.0) as u8;
                        data[idx + 3] =
                            (sa + data[idx + 3] as f32 * inv).round().clamp(0.0, 255.0) as u8;
                    });
            }
        }
    }

    fn fit_middle(&mut self, text: &str, family: &str, size_px: u16, max_w: f32) -> String {
        let measure = |fonts: &mut FontSystem, s: &str| -> f32 {
            if s.is_empty() {
                return 0.0;
            }
            let metrics = Metrics::new(size_px as f32, (size_px as f32 * 1.25).ceil());
            let mut buffer = TextBuffer::new(fonts, metrics);
            buffer.set_size(None, None);
            let parsed = parse_font_request(family);
            buffer.set_text(
                s,
                &Attrs::new()
                    .family(resolve_family(fonts, parsed.0))
                    .style(parsed.1)
                    .weight(parsed.2),
                Shaping::Advanced,
                None,
            );
            buffer.shape_until_scroll(fonts, true);
            buffer.layout_runs().next().map(|r| r.line_w).unwrap_or(0.0)
        };
        if measure(&mut self.fonts, text) <= max_w {
            return text.to_string();
        }
        let chars: Vec<char> = text.chars().collect();
        for keep in (1..chars.len()).rev() {
            let left = keep.div_ceil(2);
            let right = keep / 2;
            let mut candidate: String = chars[..left].iter().collect();
            candidate.push('…');
            candidate.extend(chars[chars.len() - right..].iter());
            if measure(&mut self.fonts, &candidate) <= max_w {
                return candidate;
            }
        }
        String::new()
    }

    fn cached_icon(&mut self, icon: CaptureIcon, size: u32, rgb: [u8; 3]) -> Option<&Pixmap> {
        let tag = match icon {
            CaptureIcon::Region => 0u8,
            CaptureIcon::Monitor => 1u8,
            CaptureIcon::Window => 2u8,
        };
        let key = (tag, size, rgb);
        if let std::collections::hash_map::Entry::Vacant(entry) = self.icons.entry(key) {
            let svg = match icon {
                CaptureIcon::Region => REGION_SVG,
                CaptureIcon::Monitor => MONITOR_SVG,
                CaptureIcon::Window => WINDOW_SVG,
            };
            let pm = rasterize_svg(svg, size, rgb)?;
            entry.insert(pm);
        }
        self.icons.get(&key)
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_selection_overlay(
    pixmap: &mut PixmapMut<'_>,
    output_rect: Rect,
    buf_w: u32,
    buf_h: u32,
    scale: f32,
    selection: Option<Rect>,
    region_style: bool,
    visuals: &OverlayVisuals,
) {
    let mut dim_paint = Paint::default();
    dim_paint.set_color(BACKDROP_DIM.to_tiny_color(1.0));

    let Some(selection) = selection else {
        if let Some(full) = SkiaRect::from_xywh(0.0, 0.0, buf_w as f32, buf_h as f32) {
            pixmap.fill_rect(full, &dim_paint, Transform::identity(), None);
        }
        return;
    };

    let Some(layout) = picker_layout(output_rect, selection) else {
        if let Some(full) = SkiaRect::from_xywh(0.0, 0.0, buf_w as f32, buf_h as f32) {
            pixmap.fill_rect(full, &dim_paint, Transform::identity(), None);
        }
        return;
    };

    for rect in &layout.dim {
        if let Some(r) = SkiaRect::from_xywh(
            rect.loc.x as f32 * scale,
            rect.loc.y as f32 * scale,
            rect.size.w as f32 * scale,
            rect.size.h as f32 * scale,
        ) {
            pixmap.fill_rect(r, &dim_paint, Transform::identity(), None);
        }
    }

    let ox = layout.outline.loc.x as f32 * scale;
    let oy = layout.outline.loc.y as f32 * scale;
    let ow = layout.outline.size.w as f32 * scale;
    let oh = layout.outline.size.h as f32 * scale;
    if let Some(rect) = SkiaRect::from_xywh(ox, oy, ow, oh) {
        let mut paint = Paint::default();
        paint.set_color(visuals.border.to_tiny_color(1.0));
        paint.anti_alias = true;
        let path = PathBuilder::from_rect(rect);
        let stroke = Stroke {
            width: PICKER_BORDER_PX * scale,
            line_cap: LineCap::Butt,
            line_join: LineJoin::Miter,
            dash: if region_style {
                StrokeDash::new(
                    vec![
                        PICKER_DASH_LENGTH * scale,
                        (PICKER_DASH_PERIOD - PICKER_DASH_LENGTH) * scale,
                    ],
                    0.0,
                )
            } else {
                None
            },
            ..Stroke::default()
        };
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    if region_style {
        let mut handle_paint = Paint::default();
        handle_paint.set_color(visuals.border.to_tiny_color(1.0));
        for h in &layout.handles {
            if let Some(r) = SkiaRect::from_xywh(
                h.loc.x as f32 * scale,
                h.loc.y as f32 * scale,
                h.size.w as f32 * scale,
                h.size.h as f32 * scale,
            ) {
                pixmap.fill_rect(r, &handle_paint, Transform::identity(), None);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_card(
    pixmap: &mut PixmapMut<'_>,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    border_px: f32,
    fill: SkiaColor,
    border: SkiaColor,
) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let rad = radius.max(0.0).min(w.min(h) * 0.5);
    if let Some(path) = rounded_rect_path(x, y, w, h, rad) {
        let mut paint = Paint::default();
        paint.set_color(fill);
        paint.anti_alias = true;
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    if border_px > 0.0 && border.alpha() > 0.0 {
        let half = border_px * 0.5;
        let bw = (w - border_px).max(1.0);
        let bh = (h - border_px).max(1.0);
        let brad = (rad - half).max(0.0);
        if let Some(path) = rounded_rect_path(x + half, y + half, bw, bh, brad) {
            let mut paint = Paint::default();
            paint.set_color(border);
            paint.anti_alias = true;
            let stroke = Stroke {
                width: border_px,
                ..Stroke::default()
            };
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }
}

fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<SkiaPath> {
    if r <= 0.0 {
        return Some(PathBuilder::from_rect(SkiaRect::from_xywh(x, y, w, h)?));
    }
    let k = 0.5522848 * r;
    let right = x + w;
    let bottom = y + h;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(right - r, y);
    pb.cubic_to(right - r + k, y, right, y + r - k, right, y + r);
    pb.line_to(right, bottom - r);
    pb.cubic_to(
        right,
        bottom - r + k,
        right - r + k,
        bottom,
        right - r,
        bottom,
    );
    pb.line_to(x + r, bottom);
    pb.cubic_to(x + r - k, bottom, x, bottom - r + k, x, bottom - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

fn rasterize_svg(svg: &[u8], size: u32, rgb: [u8; 3]) -> Option<Pixmap> {
    let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).ok()?;
    let svg_size = tree.size().to_int_size();
    let mut pixmap = Pixmap::new(size, size)?;
    let sx = size as f32 / svg_size.width() as f32;
    let sy = size as f32 / svg_size.height() as f32;
    let scale = sx.min(sy);
    let dx = (size as f32 - svg_size.width() as f32 * scale) * 0.5;
    let dy = (size as f32 - svg_size.height() as f32 * scale) * 0.5;
    let transform = Transform::from_scale(scale, scale).post_translate(dx, dy);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    for pixel in pixmap.data_mut().as_chunks_mut::<4>().0 {
        let alpha = u16::from(pixel[3]);
        pixel[0] = ((u16::from(rgb[0]) * alpha) / 255) as u8;
        pixel[1] = ((u16::from(rgb[1]) * alpha) / 255) as u8;
        pixel[2] = ((u16::from(rgb[2]) * alpha) / 255) as u8;
    }
    Some(pixmap)
}

pub fn prepare_output_backdrop(
    desktop_bounds: Rect,
    desktop_rgba: &[u8],
    desktop_w: u32,
    desktop_h: u32,
    output: Rect,
    out_w: u32,
    out_h: u32,
) -> Vec<u8> {
    let out_w = out_w.max(1);
    let out_h = out_h.max(1);
    let mut dst = vec![0u8; (out_w * out_h * 4) as usize];
    if desktop_w == 0
        || desktop_h == 0
        || desktop_rgba.len() != (desktop_w * desktop_h * 4) as usize
    {
        return dst;
    }
    let scale_x = desktop_w as f64 / desktop_bounds.size.w.max(1) as f64;
    let scale_y = desktop_h as f64 / desktop_bounds.size.h.max(1) as f64;
    let src_x0 = ((output.loc.x - desktop_bounds.loc.x) as f64 * scale_x).round();
    let src_y0 = ((output.loc.y - desktop_bounds.loc.y) as f64 * scale_y).round();
    let src_w = output.size.w as f64 * scale_x;
    let src_h = output.size.h as f64 * scale_y;
    for y in 0..out_h {
        let sy = (src_y0 + (y as f64 + 0.5) * src_h / out_h as f64)
            .floor()
            .clamp(0.0, (desktop_h - 1) as f64) as u32;
        for x in 0..out_w {
            let sx = (src_x0 + (x as f64 + 0.5) * src_w / out_w as f64)
                .floor()
                .clamp(0.0, (desktop_w - 1) as f64) as u32;
            let s_idx = ((sy * desktop_w + sx) * 4) as usize;
            let d_idx = ((y * out_w + x) * 4) as usize;
            let r = u16::from(desktop_rgba[s_idx]);
            let g = u16::from(desktop_rgba[s_idx + 1]);
            let b = u16::from(desktop_rgba[s_idx + 2]);
            let a = u16::from(desktop_rgba[s_idx + 3]);
            dst[d_idx] = ((r * a) / 255) as u8;
            dst[d_idx + 1] = ((g * a) / 255) as u8;
            dst[d_idx + 2] = ((b * a) / 255) as u8;
            dst[d_idx + 3] = a as u8;
        }
    }
    dst
}

fn rgba_premul_to_argb8888(src: &[u8], dst: &mut [u8]) {
    for (s, d) in src
        .as_chunks::<4>()
        .0
        .iter()
        .zip(dst.as_chunks_mut::<4>().0.iter_mut())
    {
        // LE ARGB8888 is [B, G, R, A].
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
        d[3] = s[3];
    }
}

fn parse_font_request(requested: &str) -> (&str, Style, Weight) {
    let trimmed = requested.trim();
    if trimmed.is_empty() {
        return ("monospace", Style::Normal, Weight::NORMAL);
    }
    let mut parts: Vec<&str> = trimmed.split_whitespace().collect();
    let mut style = Style::Normal;
    let mut weight = Weight::NORMAL;
    while parts.len() > 1 {
        let last = parts.last().unwrap().to_ascii_lowercase();
        match last.as_str() {
            "italic" | "oblique" => {
                style = Style::Italic;
                parts.pop();
            }
            "bold" => {
                weight = Weight::BOLD;
                parts.pop();
            }
            "semibold" | "semi-bold" | "demibold" => {
                weight = Weight::SEMIBOLD;
                parts.pop();
            }
            "medium" => {
                weight = Weight::MEDIUM;
                parts.pop();
            }
            "light" => {
                weight = Weight::LIGHT;
                parts.pop();
            }
            _ => break,
        }
    }
    let len = parts.iter().map(|p| p.len()).sum::<usize>() + parts.len().saturating_sub(1);
    let end = trimmed.len().min(len.max(1));
    (&trimmed[..end], style, weight)
}

fn resolve_family<'a>(fonts: &FontSystem, name: &'a str) -> Family<'a> {
    match name.to_ascii_lowercase().as_str() {
        "serif" => Family::Serif,
        "sans-serif" | "sans" => Family::SansSerif,
        "monospace" | "mono" => Family::Monospace,
        "cursive" => Family::Cursive,
        "fantasy" => Family::Fantasy,
        _ => {
            let needle = name.to_ascii_lowercase();
            let exists = fonts.db().faces().any(|f| {
                f.families
                    .iter()
                    .any(|(fam, _)| fam.to_ascii_lowercase() == needle)
            });
            if exists {
                Family::Name(name)
            } else {
                Family::Monospace
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svgs_rasterize_cleanly() {
        let mut renderer = TextRenderer::new();
        for icon in [
            CaptureIcon::Region,
            CaptureIcon::Monitor,
            CaptureIcon::Window,
        ] {
            let pm = renderer.cached_icon(icon, 42, [214, 93, 38]);
            assert!(pm.is_some());
        }
    }

    #[test]
    fn toast_layout_fits_inside_output() {
        let output = Rect::from_xywh(1920, 0, 1920, 1080);
        let settings = SyncedSettings::default();
        let layout = toast_layout(output, &settings, 1.0);
        assert!(output.contains_rect(layout.card));
        assert!(layout.card.contains_rect(layout.preview));
        assert!(layout.card.contains_rect(layout.copy));
        assert!(layout.card.contains_rect(layout.open));
    }
}
