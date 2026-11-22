//! Capture orchestration across swm IPC, Wayland selector, PNG encoder, clipboard, and toast.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::clipboard::spawn_clipboard_owner;
use crate::encoder::{EncodedCapture, encode_and_save};
use crate::ipc::{
    CaptureRect, CaptureSource, CapturedPixels, Connection, NodeInfo, OutputInfo, verify_compositor,
};
use crate::picker::Rect;
use crate::sync::{SyncedSettings, load_synced};
use crate::wayland::{
    self, InitialMode, SelectionResult, WindowCandidate, desktop_bounds_for_outputs,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureCommand {
    Menu,
    Region { rect: Option<CaptureRect> },
    Screen,
    Desktop,
    Window { node_id: Option<u64> },
}

#[derive(Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }
}

pub struct CaptureOptions {
    pub command: CaptureCommand,
    pub output: Option<String>,
    pub include_cursor: bool,
    pub explicit_path: Option<PathBuf>,
    pub clipboard: bool,
    pub cancel: Option<CancelToken>,
}

pub struct SavedCapture {
    pub settings: SyncedSettings,
    pub encoded: EncodedCapture,
    pub toast_output: Option<String>,
    pub outputs: Vec<OutputInfo>,
}

pub enum CaptureOutcome {
    Saved(Box<SavedCapture>),
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct CapturedOutput {
    pub name: String,
    pub geometry: Rect,
    pub scale: f64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub fn execute_capture(opts: CaptureOptions) -> Result<CaptureOutcome, String> {
    if opts.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
        return Ok(CaptureOutcome::Cancelled);
    }

    let mut conn = Connection::connect().map_err(|e| format!("cannot connect to swm IPC: {e}"))?;
    verify_compositor(&mut conn).map_err(|e| e.to_string())?;

    let outputs = conn
        .outputs()
        .map_err(|e| format!("failed to list swm outputs: {e}"))?;
    if outputs.is_empty() {
        return Err("swm reported no active outputs".into());
    }
    let nodes = conn.nodes(None).unwrap_or_default();
    let settings = load_synced(Some(&mut conn));

    let focused_output = nodes
        .iter()
        .find(|n| n.focused)
        .and_then(|n| n.output.clone());
    let preferred_output = opts
        .output
        .clone()
        .or(focused_output)
        .or_else(|| outputs.first().map(|o| o.name.clone()));

    let mut toast_output = preferred_output.clone();

    let pixels: Option<CapturedPixels> = match opts.command {
        CaptureCommand::Desktop => {
            let captured = capture_outputs(&mut conn, &outputs, None, opts.include_cursor)?;
            let bounds = desktop_bounds_for_outputs(&outputs)
                .ok_or_else(|| "could not resolve desktop output layout".to_string())?;
            Some(composite_and_crop_region(bounds, true, &captured)?)
        }
        CaptureCommand::Region { rect: Some(rect) } => {
            let captured = capture_outputs(
                &mut conn,
                &outputs,
                opts.output.as_deref(),
                opts.include_cursor,
            )?;
            let region = match opts.output.as_deref() {
                Some(name) => {
                    let out = captured
                        .iter()
                        .find(|o| o.name == name)
                        .ok_or_else(|| format!("unknown output {name:?}"))?;
                    Rect::from_xywh(
                        out.geometry.loc.x + rect.x,
                        out.geometry.loc.y + rect.y,
                        rect.width,
                        rect.height,
                    )
                }
                None => Rect::from_xywh(rect.x, rect.y, rect.width, rect.height),
            };
            Some(composite_and_crop_region(
                region,
                opts.output.is_none(),
                &captured,
            )?)
        }
        CaptureCommand::Window { node_id: Some(id) } => {
            let node = nodes
                .iter()
                .find(|n| n.id == id)
                .ok_or_else(|| format!("window {id} does not exist"))?;
            let screen = node
                .screen
                .ok_or_else(|| format!("window {id} is not visible on screen or blocks capture"))?;
            if let Some(ref win_out) = node.output {
                toast_output = Some(win_out.clone());
            }
            Some(capture_window_with_retry(
                &mut conn,
                id,
                screen.buffer_width,
                screen.buffer_height,
                opts.include_cursor,
            )?)
        }
        CaptureCommand::Screen if opts.output.is_some() || outputs.len() == 1 => {
            let name = preferred_output
                .clone()
                .ok_or_else(|| "no active output to capture".to_string())?;
            let info = outputs
                .iter()
                .find(|o| o.name == name)
                .ok_or_else(|| format!("unknown output {name:?}"))?;
            toast_output = Some(name);
            let out = capture_output_frame(&mut conn, info, opts.include_cursor)?;
            Some(CapturedPixels {
                width: out.width,
                height: out.height,
                rgba: out.rgba,
            })
        }
        CaptureCommand::Menu
        | CaptureCommand::Region { rect: None }
        | CaptureCommand::Screen
        | CaptureCommand::Window { node_id: None } => {
            let initial_mode = match opts.command {
                CaptureCommand::Menu => InitialMode::Menu,
                CaptureCommand::Region { .. } => InitialMode::Region,
                CaptureCommand::Screen => InitialMode::Screen,
                CaptureCommand::Window { .. } => InitialMode::Window,
                _ => unreachable!(),
            };

            let captured = capture_outputs(
                &mut conn,
                &outputs,
                opts.output.as_deref(),
                opts.include_cursor,
            )?;
            let backdrop_bounds = match opts.output.as_deref() {
                Some(name) => {
                    let info = outputs
                        .iter()
                        .find(|o| o.name == name)
                        .ok_or_else(|| format!("unknown output {name:?}"))?;
                    output_bounds(info)?
                }
                None => desktop_bounds_for_outputs(&outputs)
                    .ok_or_else(|| "could not resolve desktop output layout".to_string())?,
            };
            let backdrop = composite_and_crop_region(backdrop_bounds, false, &captured)?;

            let windows = collect_window_candidates(&nodes, opts.output.as_deref());
            let cancel_flag = opts.cancel.as_ref().map(CancelToken::flag);

            let selection = wayland::run_selector(
                initial_mode,
                &outputs,
                opts.output.as_deref(),
                preferred_output.as_deref(),
                &backdrop,
                backdrop_bounds,
                &settings,
                None,
                &windows,
                cancel_flag,
            )?;

            match selection {
                SelectionResult::Cancelled => None,
                SelectionResult::Screen { output_name, .. } => {
                    toast_output = Some(output_name.clone());
                    if let Some(frozen) = captured.into_iter().find(|o| o.name == output_name) {
                        Some(CapturedPixels {
                            width: frozen.width,
                            height: frozen.height,
                            rgba: frozen.rgba,
                        })
                    } else {
                        let info = outputs
                            .iter()
                            .find(|o| o.name == output_name)
                            .ok_or_else(|| format!("unknown output {output_name:?}"))?;
                        let out = capture_output_frame(&mut conn, info, opts.include_cursor)?;
                        Some(CapturedPixels {
                            width: out.width,
                            height: out.height,
                            rgba: out.rgba,
                        })
                    }
                }
                SelectionResult::Region {
                    output,
                    rect,
                    trim_outer_gaps,
                } => {
                    let region = match output.as_deref() {
                        Some(name) => {
                            toast_output = Some(name.to_owned());
                            let out = captured
                                .iter()
                                .find(|o| o.name == name)
                                .ok_or_else(|| format!("unknown output {name:?}"))?;
                            Rect::from_xywh(
                                out.geometry.loc.x + rect.loc.x,
                                out.geometry.loc.y + rect.loc.y,
                                rect.size.w,
                                rect.size.h,
                            )
                        }
                        None => rect,
                    };
                    Some(composite_and_crop_region(
                        region,
                        trim_outer_gaps,
                        &captured,
                    )?)
                }
                SelectionResult::Window {
                    node_id,
                    buffer_width,
                    buffer_height,
                    output_name,
                } => {
                    if output_name.is_some() {
                        toast_output = output_name;
                    }
                    Some(capture_window_with_retry(
                        &mut conn,
                        node_id,
                        buffer_width,
                        buffer_height,
                        opts.include_cursor,
                    )?)
                }
            }
        }
    };

    let Some(pixels) = pixels else {
        return Ok(CaptureOutcome::Cancelled);
    };

    let encoded = encode_and_save(
        &settings.screenshot_dir,
        opts.explicit_path.as_deref(),
        pixels.width,
        pixels.height,
        &pixels.rgba,
    )
    .map_err(|e| format!("failed to save screenshot PNG: {e}"))?;

    if opts.clipboard {
        spawn_clipboard_owner(Arc::clone(&encoded.png))?;
    }

    Ok(CaptureOutcome::Saved(Box::new(SavedCapture {
        settings,
        encoded,
        toast_output,
        outputs,
    })))
}

fn capture_window_with_retry(
    conn: &mut Connection,
    window_id: u64,
    width: i32,
    height: i32,
    include_cursor: bool,
) -> Result<CapturedPixels, String> {
    let first = conn.capture_frame(
        CaptureSource::Window {
            window_id,
            width,
            height,
        },
        include_cursor,
    );
    match first {
        Ok(px) => Ok(px),
        Err(first_err) => {
            if let Ok(nodes) = conn.nodes(None)
                && let Some(screen) = nodes
                    .into_iter()
                    .find(|n| n.id == window_id)
                    .and_then(|n| n.screen)
                && (screen.buffer_width != width || screen.buffer_height != height)
            {
                return conn
                    .capture_frame(
                        CaptureSource::Window {
                            window_id,
                            width: screen.buffer_width,
                            height: screen.buffer_height,
                        },
                        include_cursor,
                    )
                    .map_err(|e| e.to_string());
            }
            Err(first_err.to_string())
        }
    }
}

fn capture_output_frame(
    conn: &mut Connection,
    info: &OutputInfo,
    include_cursor: bool,
) -> Result<CapturedOutput, String> {
    let mode = info
        .current_mode
        .and_then(|idx| info.modes.get(idx))
        .ok_or_else(|| format!("output {:?} has no active mode", info.name))?;
    let geometry = output_bounds(info)?;
    let scale = if info.scale > 0.0 { info.scale } else { 1.0 };
    let px = conn
        .capture_frame(
            CaptureSource::Monitor {
                name: info.name.clone(),
                x: geometry.loc.x,
                y: geometry.loc.y,
                width: mode.width,
                height: mode.height,
            },
            include_cursor,
        )
        .map_err(|e| e.to_string())?;
    Ok(CapturedOutput {
        name: info.name.clone(),
        geometry,
        scale,
        width: px.width,
        height: px.height,
        rgba: px.rgba,
    })
}

fn capture_outputs(
    conn: &mut Connection,
    outputs: &[OutputInfo],
    scoped_output: Option<&str>,
    include_cursor: bool,
) -> Result<Vec<CapturedOutput>, String> {
    if let Some(name) = scoped_output {
        let info = outputs
            .iter()
            .find(|o| o.name == name)
            .ok_or_else(|| format!("unknown output {name:?}"))?;
        return Ok(vec![capture_output_frame(conn, info, include_cursor)?]);
    }
    let mut captured = Vec::with_capacity(outputs.len());
    for info in outputs {
        if info.current_mode.is_none() {
            continue;
        }
        captured.push(capture_output_frame(conn, info, include_cursor)?);
    }
    if captured.is_empty() {
        return Err("no active outputs available for capture".into());
    }
    Ok(captured)
}

pub fn collect_window_candidates(
    nodes: &[NodeInfo],
    scoped_output: Option<&str>,
) -> Vec<WindowCandidate> {
    nodes
        .iter()
        .filter(|node| scoped_output.is_none_or(|target| node.output.as_deref() == Some(target)))
        .filter_map(|node| {
            let screen = node.screen?;
            if screen.width <= 0
                || screen.height <= 0
                || screen.buffer_width <= 0
                || screen.buffer_height <= 0
            {
                return None;
            }
            Some(WindowCandidate {
                node_id: node.id,
                output: node.output.clone(),
                rect: Rect::from_xywh(screen.x, screen.y, screen.width, screen.height),
                buffer_width: screen.buffer_width,
                buffer_height: screen.buffer_height,
                stack_order: screen.stack_order,
                focused: node.focused,
            })
        })
        .collect()
}

pub fn composite_and_crop_region(
    region: Rect,
    trim_outer_gaps: bool,
    outputs: &[CapturedOutput],
) -> Result<CapturedPixels, String> {
    if region.size.w <= 0 || region.size.h <= 0 {
        return Err("capture region must have positive width and height".into());
    }
    let overlapping: Vec<&CapturedOutput> = outputs
        .iter()
        .filter(|o| o.geometry.overlaps(region))
        .collect();
    if overlapping.is_empty() {
        return Err("capture region is outside every active output".into());
    }

    let region = if trim_outer_gaps {
        visible_region_bounds(region, &overlapping)?
    } else {
        region
    };
    let overlapping: Vec<&CapturedOutput> = outputs
        .iter()
        .filter(|o| o.geometry.overlaps(region))
        .collect();
    if overlapping.is_empty() {
        return Err("capture region is outside every active output".into());
    }

    if let [single] = overlapping.as_slice()
        && single.geometry == region
    {
        return Ok(CapturedPixels {
            width: single.width,
            height: single.height,
            rgba: single.rgba.clone(),
        });
    }

    let max_scale = overlapping
        .iter()
        .map(|o| if o.scale > 0.0 { o.scale } else { 1.0 })
        .fold(1.0_f64, f64::max);
    let target_width = ((f64::from(region.size.w) * max_scale).round() as u32).max(1);
    let target_height = ((f64::from(region.size.h) * max_scale).round() as u32).max(1);
    let len = (target_width as usize)
        .checked_mul(target_height as usize)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| "capture dimensions overflow".to_string())?;
    let mut canvas = vec![0u8; len];

    for output in overlapping {
        let scaled_w = ((f64::from(output.geometry.size.w) * max_scale).round() as u32).max(1);
        let scaled_h = ((f64::from(output.geometry.size.h) * max_scale).round() as u32).max(1);
        let pixels = resize_rgba_nearest(
            &output.rgba,
            output.width,
            output.height,
            scaled_w,
            scaled_h,
        );
        let out_x = ((f64::from(output.geometry.loc.x - region.loc.x)) * max_scale).round() as i32;
        let out_y = ((f64::from(output.geometry.loc.y - region.loc.y)) * max_scale).round() as i32;

        let out_rect = Rect::from_xywh(out_x, out_y, scaled_w as i32, scaled_h as i32);
        let target_rect = Rect::from_xywh(0, 0, target_width as i32, target_height as i32);
        let Some(intersection) = out_rect.intersection(target_rect) else {
            continue;
        };

        for row in 0..intersection.size.h {
            let src_y = (intersection.loc.y - out_y + row) as usize;
            let dst_y = (intersection.loc.y + row) as usize;
            let src_x = (intersection.loc.x - out_x) as usize;
            let dst_x = intersection.loc.x as usize;
            let width = intersection.size.w as usize;
            let src_offset = (src_y * (scaled_w as usize) + src_x) * 4;
            let dst_offset = (dst_y * (target_width as usize) + dst_x) * 4;
            let byte_len = width * 4;
            canvas[dst_offset..dst_offset + byte_len]
                .copy_from_slice(&pixels[src_offset..src_offset + byte_len]);
        }
    }

    Ok(CapturedPixels {
        width: target_width,
        height: target_height,
        rgba: canvas,
    })
}

fn visible_region_bounds(region: Rect, outputs: &[&CapturedOutput]) -> Result<Rect, String> {
    outputs
        .iter()
        .filter_map(|o| region.intersection(o.geometry))
        .reduce(Rect::merge)
        .ok_or_else(|| "capture region is outside every active output".into())
}

fn resize_rgba_nearest(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    if src_w == dst_w && src_h == dst_h {
        return src.to_vec();
    }
    let mut dst = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    for y in 0..dst_h {
        let src_y = (((u64::from(y) * 2 + 1) * u64::from(src_h)) / (u64::from(dst_h) * 2))
            .min(u64::from(src_h.saturating_sub(1))) as usize;
        for x in 0..dst_w {
            let src_x = (((u64::from(x) * 2 + 1) * u64::from(src_w)) / (u64::from(dst_w) * 2))
                .min(u64::from(src_w.saturating_sub(1))) as usize;
            let src_offset = (src_y * (src_w as usize) + src_x) * 4;
            let dst_offset = ((y as usize) * (dst_w as usize) + (x as usize)) * 4;
            dst[dst_offset..dst_offset + 4].copy_from_slice(&src[src_offset..src_offset + 4]);
        }
    }
    dst
}

fn output_bounds(info: &OutputInfo) -> Result<Rect, String> {
    let mode = info
        .current_mode
        .and_then(|idx| info.modes.get(idx))
        .ok_or_else(|| format!("output {:?} has no active mode", info.name))?;
    let scale = if info.scale > 0.0 { info.scale } else { 1.0 };
    let w = (f64::from(mode.width) / scale).round().max(1.0) as i32;
    let h = (f64::from(mode.height) / scale).round().max(1.0) as i32;
    Ok(Rect::from_xywh(info.offset_x, info.offset_y, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{NodeKind, NodeProtocolFamily, NodeRole, NodeScreenInfo, NodeState};

    fn solid_output(
        name: &str,
        geometry: Rect,
        width: u32,
        height: u32,
        scale: f64,
        rgba: [u8; 4],
    ) -> CapturedOutput {
        CapturedOutput {
            name: name.into(),
            geometry,
            scale,
            width,
            height,
            rgba: rgba.repeat((width as usize) * (height as usize)),
        }
    }

    #[test]
    fn composite_stitches_multiple_outputs_and_upscales_to_max_scale() {
        let outputs = vec![
            solid_output(
                "DP-1",
                Rect::from_xywh(0, 0, 2, 2),
                2,
                2,
                1.0,
                [255, 0, 0, 255],
            ),
            solid_output(
                "DP-2",
                Rect::from_xywh(2, 0, 2, 2),
                4,
                4,
                2.0,
                [0, 255, 0, 255],
            ),
        ];
        let captured =
            composite_and_crop_region(Rect::from_xywh(1, 0, 2, 2), true, &outputs).unwrap();
        assert_eq!((captured.width, captured.height), (4, 4));
        assert_eq!(&captured.rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(&captured.rgba[8..12], &[0, 255, 0, 255]);
    }

    #[test]
    fn composite_trims_outer_void_on_uneven_outputs() {
        let outputs = vec![
            solid_output(
                "DP-1",
                Rect::from_xywh(0, 0, 4, 4),
                4,
                4,
                1.0,
                [10, 20, 30, 255],
            ),
            solid_output(
                "DP-2",
                Rect::from_xywh(4, 1, 4, 2),
                4,
                2,
                1.0,
                [40, 50, 60, 255],
            ),
        ];
        let captured =
            composite_and_crop_region(Rect::from_xywh(4, 0, 4, 4), true, &outputs).unwrap();
        assert_eq!((captured.width, captured.height), (4, 2));
        for px in captured.rgba.as_chunks::<4>().0 {
            assert_eq!(px, &[40, 50, 60, 255]);
        }
    }

    #[test]
    fn collect_window_candidates_filters_nodes_without_screen_info() {
        let nodes = vec![
            NodeInfo {
                id: 1,
                title: "Visible".into(),
                app_id: Some("foot".into()),
                output: Some("DP-1".into()),
                kind: NodeKind::Surface,
                state: NodeState::Active,
                visible: true,
                focused: true,
                latest: true,
                pinned: false,
                role: NodeRole::NormalToplevel,
                protocol_family: NodeProtocolFamily::XdgToplevel,
                modal: false,
                parent: None,
                transient_for: None,
                child_popup_count: 0,
                pos_x: 10.0,
                pos_y: 20.0,
                width: 800.0,
                height: 600.0,
                screen: Some(NodeScreenInfo {
                    x: 10,
                    y: 20,
                    width: 806,
                    height: 630,
                    buffer_width: 1612,
                    buffer_height: 1260,
                    stack_order: 3,
                }),
            },
            NodeInfo {
                id: 2,
                title: "Blocked or collapsed".into(),
                app_id: None,
                output: Some("DP-1".into()),
                kind: NodeKind::Surface,
                state: NodeState::Node,
                visible: false,
                focused: false,
                latest: false,
                pinned: false,
                role: NodeRole::NormalToplevel,
                protocol_family: NodeProtocolFamily::XdgToplevel,
                modal: false,
                parent: None,
                transient_for: None,
                child_popup_count: 0,
                pos_x: 0.0,
                pos_y: 0.0,
                width: 400.0,
                height: 300.0,
                screen: None,
            },
        ];
        let candidates = collect_window_candidates(&nodes, None);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].node_id, 1);
        assert_eq!(candidates[0].rect, Rect::from_xywh(10, 20, 806, 630));
        assert_eq!(candidates[0].buffer_width, 1612);
        assert_eq!(candidates[0].buffer_height, 1260);
    }
}
