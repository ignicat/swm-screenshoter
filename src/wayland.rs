//! Layer-shell Wayland freeze-frame selector and interactive toast overlay.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_cursor::CursorTheme;
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{self, Shape as CursorShape, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};
use xkbcommon::xkb::{self, keysyms};

use crate::clipboard::spawn_clipboard_owner;
use crate::encoder::EncodedCapture;
use crate::ipc::{CapturedPixels, OutputInfo};
use crate::menu::{ScreenshotMenu, ScreenshotMode};
use crate::picker::{CursorHint, PointF, Rect, RegionPicker, Size};
use crate::render::{
    OverlayMode, TextRenderer, ToastAction, ToastLayout, prepare_output_backdrop, toast_layout,
};
use crate::sync::{
    SyncedSettings, load_remembered_region, region_state_path, save_remembered_region,
};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const TOAST_FADE_MS: u64 = 180;
const DOUBLE_CLICK_MS: u64 = 350;
const DOUBLE_CLICK_DIST_PX: f64 = 5.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitialMode {
    Menu,
    Region,
    Screen,
    Window,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowCandidate {
    pub node_id: u64,
    pub output: Option<String>,
    pub rect: Rect,
    pub buffer_width: i32,
    pub buffer_height: i32,
    pub stack_order: u32,
    pub focused: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectionResult {
    Region {
        output: Option<String>,
        rect: Rect,
        trim_outer_gaps: bool,
    },
    Screen {
        output_name: String,
        rect: Rect,
    },
    Window {
        node_id: u64,
        buffer_width: i32,
        buffer_height: i32,
        output_name: Option<String>,
    },
    Cancelled,
}

struct ShmSlot {
    width: u32,
    height: u32,
    size: usize,
    fd: OwnedFd,
    ptr: NonNull<u8>,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    busy: bool,
}

impl Drop for ShmSlot {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.size);
        }
        let _ = &self.fd;
    }
}

impl ShmSlot {
    fn create(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<App>,
        slot_id: (u32, usize),
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let stride = width.checked_mul(4).ok_or("shm stride overflow")?;
        let size = (stride as usize)
            .checked_mul(height as usize)
            .ok_or("shm buffer overflow")?;
        if size == 0 {
            return Err("zero-sized shm buffer".into());
        }
        let raw_fd = unsafe { libc::memfd_create(c"swm-screenshoter".as_ptr(), libc::MFD_CLOEXEC) };
        if raw_fd < 0 {
            return Err(format!(
                "memfd_create failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let len = libc::off_t::try_from(size).map_err(|_| "shm size exceeds off_t".to_string())?;
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len) } < 0 {
            return Err(format!(
                "ftruncate failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(format!("mmap failed: {}", std::io::Error::last_os_error()));
        }
        let ptr = NonNull::new(mapped.cast::<u8>()).ok_or("mmap returned null")?;
        let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            wl_shm::Format::Argb8888,
            qh,
            slot_id,
        );
        Ok(Self {
            width,
            height,
            size,
            fd,
            ptr,
            pool,
            buffer,
            busy: false,
        })
    }

    fn as_bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.size) }
    }
}

struct TrackedOutput {
    global_name: u32,
    wl: wl_output::WlOutput,
    xdg: Option<ZxdgOutputV1>,
    name: Option<String>,
    logical_pos: (i32, i32),
    logical_size: (i32, i32),
    physical_size: (i32, i32),
    scale_int: i32,
    done: bool,
}

impl TrackedOutput {
    fn new(global_name: u32, wl: wl_output::WlOutput) -> Self {
        Self {
            global_name,
            wl,
            xdg: None,
            name: None,
            logical_pos: (0, 0),
            logical_size: (0, 0),
            physical_size: (0, 0),
            scale_int: 1,
            done: false,
        }
    }

    fn resolved_geometry(&self, infos: &[OutputInfo]) -> Option<Rect> {
        if let Some(name) = self.name.as_deref()
            && let Some(info) = infos.iter().find(|o| o.name == name)
            && let Some(rect) = output_info_rect(info)
        {
            return Some(rect);
        }
        if self.logical_size.0 > 0 && self.logical_size.1 > 0 {
            return Some(Rect::from_xywh(
                self.logical_pos.0,
                self.logical_pos.1,
                self.logical_size.0,
                self.logical_size.1,
            ));
        }
        None
    }

    fn resolved_scale_and_pixels(&self, infos: &[OutputInfo], logical: Size) -> (f64, u32, u32) {
        if let Some(name) = self.name.as_deref()
            && let Some(info) = infos.iter().find(|o| o.name == name)
            && let Some(mode) = info.current_mode.and_then(|idx| info.modes.get(idx))
            && mode.width > 0
            && mode.height > 0
        {
            return (info.scale.max(0.1), mode.width as u32, mode.height as u32);
        }
        let scale = f64::from(self.scale_int.max(1));
        if self.physical_size.0 > 0 && self.physical_size.1 > 0 && self.scale_int > 1 {
            return (
                scale,
                self.physical_size.0 as u32,
                self.physical_size.1 as u32,
            );
        }
        let w = ((f64::from(logical.w.max(1)) * scale).round() as u32).max(1);
        let h = ((f64::from(logical.h.max(1)) * scale).round() as u32).max(1);
        (scale, w, h)
    }
}

struct SurfaceSlot {
    output_name: String,
    geometry: Rect,
    scale: f64,
    buf_w: u32,
    buf_h: u32,
    surface: wl_surface::WlSurface,
    viewport: Option<WpViewport>,
    layer_surface: ZwlrLayerSurfaceV1,
    configured: bool,
    configured_size: (u32, u32),
    needs_redraw: bool,
    backdrop_premul: Vec<u8>,
    slots: [Option<ShmSlot>; 2],
}

impl SurfaceSlot {
    fn acquire_slot(
        &mut self,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<App>,
        surface_id: u32,
    ) -> Result<usize, String> {
        let w = self.buf_w.max(1);
        let h = self.buf_h.max(1);
        let idx = if self.slots[0].as_ref().is_some_and(|s| !s.busy) {
            0
        } else if self.slots[1].as_ref().is_some_and(|s| !s.busy) {
            1
        } else {
            0
        };
        let needs_alloc = self.slots[idx]
            .as_ref()
            .is_none_or(|s| s.width != w || s.height != h);
        if needs_alloc {
            self.slots[idx] = None;
            self.slots[idx] = Some(ShmSlot::create(shm, qh, (surface_id, idx), w, h)?);
        }
        Ok(idx)
    }
}

impl Drop for SurfaceSlot {
    fn drop(&mut self) {
        self.slots = [None, None];
        if let Some(vp) = self.viewport.take() {
            vp.destroy();
        }
        self.layer_surface.destroy();
        self.surface.destroy();
    }
}

enum AppMode {
    Selector(SelectorState),
    Toast(ToastState),
}

struct SelectorState {
    initial_mode: InitialMode,
    menu: Option<ScreenshotMenu>,
    picker: RegionPicker,
    picking_region: bool,
    picking_screen: bool,
    picking_window: bool,
    hovered_output: Option<String>,
    screen_index: usize,
    windows: Vec<WindowCandidate>,
    window_index: usize,
    hovered_window: Option<usize>,
    active_output_name: String,
    active_output_geom: Rect,
    backdrop: Arc<CapturedPixels>,
    backdrop_bounds: Rect,
    visuals: crate::sync::OverlayVisuals,
    scoped_output: Option<String>,
    last_click: Option<(Instant, PointF)>,
    result: Option<SelectionResult>,
}

struct ToastState {
    settings: SyncedSettings,
    encoded: EncodedCapture,
    message: String,
    hovered: Option<ToastAction>,
    pointer_inside: bool,
    layout: ToastLayout,
    opened_at: Instant,
    dismiss_at: Instant,
    remaining_before_pause: Duration,
    closing_at: Option<Instant>,
    finished: bool,
}

struct KeyboardState {
    xkb_context: xkb::Context,
    keymap: Option<xkb::Keymap>,
    xkb_state: Option<xkb::State>,
    seat_kbd: Option<wl_keyboard::WlKeyboard>,
}

struct CursorState {
    manager: Option<WpCursorShapeManagerV1>,
    shape_device: Option<WpCursorShapeDeviceV1>,
    theme: Option<CursorTheme>,
    cursor_surface: Option<wl_surface::WlSurface>,
    last_enter_serial: u32,
    last_name: &'static str,
}

impl CursorState {
    fn new() -> Self {
        Self {
            manager: None,
            shape_device: None,
            theme: None,
            cursor_surface: None,
            last_enter_serial: 0,
            last_name: "",
        }
    }

    fn set_cursor(
        &mut self,
        conn: &Connection,
        pointer: Option<&wl_pointer::WlPointer>,
        compositor: Option<&wl_compositor::WlCompositor>,
        shm: Option<&wl_shm::WlShm>,
        qh: &QueueHandle<App>,
        shape_name: &'static str,
    ) {
        let Some(pointer) = pointer else {
            return;
        };
        if self.last_enter_serial == 0 || self.last_name == shape_name {
            return;
        }
        self.last_name = shape_name;

        if let Some(device) = &self.shape_device {
            let shape = match shape_name {
                "crosshair" => CursorShape::Crosshair,
                "grabbing" => CursorShape::Grabbing,
                "n-resize" => CursorShape::NResize,
                "s-resize" => CursorShape::SResize,
                "w-resize" => CursorShape::WResize,
                "e-resize" => CursorShape::EResize,
                "nw-resize" => CursorShape::NwResize,
                "ne-resize" => CursorShape::NeResize,
                "sw-resize" => CursorShape::SwResize,
                "se-resize" => CursorShape::SeResize,
                "pointer" => CursorShape::Pointer,
                _ => CursorShape::Default,
            };
            device.set_shape(self.last_enter_serial, shape);
            return;
        }

        if self.theme.is_none()
            && let Some(shm) = shm
            && let Some(compositor) = compositor
        {
            if let Ok(theme) = CursorTheme::load(conn, shm.clone(), 24) {
                self.theme = Some(theme);
            }
            if self.cursor_surface.is_none() {
                self.cursor_surface = Some(compositor.create_surface(qh, ()));
            }
        }
        let Some(theme) = &mut self.theme else {
            return;
        };
        let Some(cursor_surface) = &self.cursor_surface else {
            return;
        };
        let candidates: &[&str] = match shape_name {
            "pointer" => &["pointer", "hand2", "left_ptr"],
            "crosshair" => &["crosshair", "cross", "left_ptr"],
            "grabbing" => &["grabbing", "closedhand", "fleur", "left_ptr"],
            "n-resize" => &["n-resize", "top_side", "ns-resize", "left_ptr"],
            "s-resize" => &["s-resize", "bottom_side", "ns-resize", "left_ptr"],
            "w-resize" => &["w-resize", "left_side", "ew-resize", "left_ptr"],
            "e-resize" => &["e-resize", "right_side", "ew-resize", "left_ptr"],
            "nw-resize" => &["nw-resize", "top_left_corner", "nwse-resize", "left_ptr"],
            "ne-resize" => &["ne-resize", "top_right_corner", "nesw-resize", "left_ptr"],
            "sw-resize" => &["sw-resize", "bottom_left_corner", "nesw-resize", "left_ptr"],
            "se-resize" => &[
                "se-resize",
                "bottom_right_corner",
                "nwse-resize",
                "left_ptr",
            ],
            _ => &["left_ptr", "default"],
        };
        for name in candidates {
            if let Some(cursor) = theme.get_cursor(name) {
                let image = &cursor[0];
                let (hx, hy) = image.hotspot();
                cursor_surface.attach(Some(image), 0, 0);
                let (w, h) = image.dimensions();
                cursor_surface.damage_buffer(0, 0, w as i32, h as i32);
                cursor_surface.commit();
                pointer.set_cursor(
                    self.last_enter_serial,
                    Some(cursor_surface),
                    hx as i32,
                    hy as i32,
                );
                return;
            }
        }
    }
}

struct App {
    conn: Connection,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    viewporter: Option<WpViewporter>,
    xdg_output_mgr: Option<ZxdgOutputManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: KeyboardState,
    cursor: CursorState,
    outputs: Vec<TrackedOutput>,
    output_infos: Vec<OutputInfo>,
    surfaces: Vec<SurfaceSlot>,
    pointer_surface_id: Option<u32>,
    pointer_pos_local: PointF,
    renderer: TextRenderer,
    mode: AppMode,
    qh: Option<QueueHandle<App>>,
}

impl App {
    fn surface_index_for_wl(&self, wl: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces.iter().position(|s| &s.surface == wl)
    }

    fn pointer_desktop(&self) -> PointF {
        if let Some(id) = self.pointer_surface_id
            && let Some(slot) = self.surfaces.get(id as usize)
        {
            return PointF::new(
                f64::from(slot.geometry.loc.x) + self.pointer_pos_local.x,
                f64::from(slot.geometry.loc.y) + self.pointer_pos_local.y,
            );
        }
        self.pointer_pos_local
    }

    fn pointer_output_name(&self) -> Option<&str> {
        self.pointer_surface_id
            .and_then(|id| self.surfaces.get(id as usize))
            .map(|slot| slot.output_name.as_str())
    }

    fn desired_cursor_shape(&self) -> &'static str {
        match &self.mode {
            AppMode::Selector(sel) => {
                if sel.picking_region {
                    cursor_hint_to_shape(sel.picker.cursor_at(self.pointer_desktop()))
                } else if sel.picking_screen || sel.picking_window {
                    "pointer"
                } else if let Some(menu) = &sel.menu {
                    if menu.hit_test(self.pointer_desktop()).is_some() {
                        "pointer"
                    } else {
                        "default"
                    }
                } else {
                    "default"
                }
            }
            AppMode::Toast(toast) => {
                if toast.hovered.is_some() {
                    "pointer"
                } else {
                    "default"
                }
            }
        }
    }

    fn refresh_cursor(&mut self) {
        let shape = self.desired_cursor_shape();
        let Some(qh) = self.qh.clone() else {
            return;
        };
        let pointer = self.pointer.as_ref();
        let compositor = self.compositor.as_ref();
        let shm = self.shm.as_ref();
        self.cursor
            .set_cursor(&self.conn, pointer, compositor, shm, &qh, shape);
    }

    fn mark_all_dirty(&mut self) {
        for s in &mut self.surfaces {
            s.needs_redraw = true;
        }
    }

    fn handle_pointer_motion(&mut self, sx: f64, sy: f64) {
        self.pointer_pos_local = PointF::new(sx, sy);
        let desktop = self.pointer_desktop();
        match &mut self.mode {
            AppMode::Selector(sel) => {
                let mut changed = false;
                if sel.picking_region {
                    changed = sel.picker.motion(desktop);
                } else if sel.picking_screen {
                    let hovered = self
                        .pointer_surface_id
                        .and_then(|id| self.surfaces.get(id as usize))
                        .map(|s| s.output_name.clone());
                    if sel.hovered_output != hovered {
                        sel.hovered_output = hovered;
                        if let Some(name) = &sel.hovered_output
                            && let Some(pos) =
                                self.surfaces.iter().position(|s| &s.output_name == name)
                        {
                            sel.screen_index = pos;
                        }
                        changed = true;
                    }
                } else if sel.picking_window {
                    let next = topmost_window_at(&sel.windows, desktop);
                    if sel.hovered_window != next {
                        sel.hovered_window = next;
                        if let Some(idx) = next {
                            sel.window_index = idx;
                        }
                        changed = true;
                    }
                } else if let Some(menu) = &mut sel.menu {
                    changed = menu.hover(desktop);
                }
                if changed {
                    self.mark_all_dirty();
                }
            }
            AppMode::Toast(toast) => {
                if self.pointer_surface_id.is_some() {
                    let card_point = PointF::new(
                        f64::from(toast.layout.card.loc.x) + sx,
                        f64::from(toast.layout.card.loc.y) + sy,
                    );
                    let next = toast.layout.action_at(card_point);
                    if toast.hovered != next {
                        toast.hovered = next;
                        let target_idx = slot_index(self.surfaces.len(), self.pointer_surface_id);
                        if let Some(s) = self.surfaces.get_mut(target_idx) {
                            s.needs_redraw = true;
                        }
                    }
                }
            }
        }
        self.refresh_cursor();
    }

    fn handle_pointer_button(&mut self, button: u32, pressed: bool) {
        let desktop = self.pointer_desktop();
        let active_name = self.pointer_output_name().map(str::to_owned);
        match &mut self.mode {
            AppMode::Selector(sel) => {
                if !pressed {
                    if button == BTN_LEFT && sel.picking_region && sel.picker.release() {
                        self.mark_all_dirty();
                    }
                    self.refresh_cursor();
                    return;
                }

                if button == BTN_RIGHT {
                    sel.result = Some(SelectionResult::Cancelled);
                    return;
                }
                if button != BTN_LEFT {
                    return;
                }

                if sel.picking_region {
                    let now = Instant::now();
                    let is_double_click = sel.last_click.is_some_and(|(t, pos)| {
                        now.duration_since(t) <= Duration::from_millis(DOUBLE_CLICK_MS)
                            && (pos.x - desktop.x).hypot(pos.y - desktop.y) <= DOUBLE_CLICK_DIST_PX
                    });
                    if is_double_click
                        && sel
                            .picker
                            .region()
                            .is_some_and(|r| r.contains_point_f(desktop))
                    {
                        accept_picker(sel, &self.surfaces);
                        return;
                    }
                    sel.last_click = Some((now, desktop));
                    if sel.picker.press(desktop) {
                        self.mark_all_dirty();
                    }
                } else if sel.picking_screen {
                    let chosen = active_name
                        .as_deref()
                        .and_then(|n| self.surfaces.iter().find(|s| s.output_name == n))
                        .or_else(|| self.surfaces.get(sel.screen_index))
                        .or_else(|| self.surfaces.first());
                    if let Some(target) = chosen {
                        sel.result = Some(SelectionResult::Screen {
                            output_name: target.output_name.clone(),
                            rect: target.geometry,
                        });
                    }
                } else if sel.picking_window {
                    let chosen = topmost_window_at(&sel.windows, desktop)
                        .or(sel.hovered_window)
                        .and_then(|idx| sel.windows.get(idx));
                    if let Some(win) = chosen {
                        sel.result = Some(SelectionResult::Window {
                            node_id: win.node_id,
                            buffer_width: win.buffer_width,
                            buffer_height: win.buffer_height,
                            output_name: win.output.clone().or(active_name),
                        });
                    }
                } else if let Some(menu) = &mut sel.menu {
                    match menu.hit_test(desktop) {
                        Some(idx) => {
                            let chosen_mode = ScreenshotMode::ALL[idx];
                            activate_menu_mode(sel, chosen_mode, &self.surfaces, active_name);
                            self.mark_all_dirty();
                        }
                        None => {
                            let layout = crate::menu::layout(menu.output_geometry());
                            if !layout.bar.contains_point_f(desktop) {
                                sel.result = Some(SelectionResult::Cancelled);
                            }
                        }
                    }
                }
            }
            AppMode::Toast(toast) => {
                if !pressed {
                    return;
                }
                if button == BTN_RIGHT {
                    toast.finished = true;
                    return;
                }
                if button != BTN_LEFT {
                    return;
                }
                let card_point = PointF::new(
                    f64::from(toast.layout.card.loc.x) + self.pointer_pos_local.x,
                    f64::from(toast.layout.card.loc.y) + self.pointer_pos_local.y,
                );
                match toast.layout.action_at(card_point) {
                    Some(ToastAction::Copy) => {
                        match spawn_clipboard_owner(Arc::clone(&toast.encoded.png)) {
                            Ok(()) => toast.message = "Copied to clipboard".into(),
                            Err(err) => toast.message = err,
                        }
                        let dur =
                            Duration::from_millis(toast.settings.success_duration_ms.max(1000));
                        toast.remaining_before_pause = dur;
                        toast.dismiss_at = Instant::now() + dur;
                        toast.closing_at = None;
                        self.mark_all_dirty();
                    }
                    Some(ToastAction::Open) => {
                        let _ = std::process::Command::new("xdg-open")
                            .arg(&toast.encoded.path)
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn();
                        toast.finished = true;
                    }
                    None => {
                        toast.finished = true;
                    }
                }
            }
        }
        self.refresh_cursor();
    }

    fn handle_key(&mut self, sym: xkb::Keysym) {
        let shift = self.keyboard.xkb_state.as_ref().is_some_and(|st| {
            st.mod_name_is_active(xkb::MOD_NAME_SHIFT, xkb::STATE_MODS_EFFECTIVE)
        });
        let ctrl =
            self.keyboard.xkb_state.as_ref().is_some_and(|st| {
                st.mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE)
            });

        let AppMode::Selector(sel) = &mut self.mode else {
            return;
        };
        let sym_raw = sym.raw();

        if sym_raw == keysyms::KEY_Escape {
            if sel.initial_mode == InitialMode::Menu
                && (sel.picking_region || sel.picking_screen || sel.picking_window)
            {
                sel.picking_region = false;
                sel.picking_screen = false;
                sel.picking_window = false;
                sel.menu = Some(ScreenshotMenu::new(
                    sel.active_output_name.clone(),
                    sel.active_output_geom,
                    !sel.windows.is_empty(),
                ));
                self.mark_all_dirty();
                self.refresh_cursor();
                return;
            }
            sel.result = Some(SelectionResult::Cancelled);
            return;
        }

        if sel.picking_region {
            let step = if ctrl { 1 } else { 10 };
            let delta = match sym_raw {
                keysyms::KEY_Left | keysyms::KEY_KP_Left => Some((-step, 0)),
                keysyms::KEY_Right | keysyms::KEY_KP_Right => Some((step, 0)),
                keysyms::KEY_Up | keysyms::KEY_KP_Up => Some((0, -step)),
                keysyms::KEY_Down | keysyms::KEY_KP_Down => Some((0, step)),
                _ => None,
            };
            if let Some((dx, dy)) = delta {
                if sel.picker.adjust(dx, dy, shift) {
                    self.mark_all_dirty();
                }
                return;
            }
            if matches!(
                sym_raw,
                keysyms::KEY_Return | keysyms::KEY_KP_Enter | keysyms::KEY_space
            ) {
                accept_picker(sel, &self.surfaces);
            }
            return;
        }

        if sel.picking_screen {
            let count = self.surfaces.len();
            if count > 0 {
                match sym_raw {
                    keysyms::KEY_Left
                    | keysyms::KEY_Up
                    | keysyms::KEY_KP_Left
                    | keysyms::KEY_KP_Up => {
                        sel.screen_index = (sel.screen_index + count - 1) % count;
                        sel.hovered_output =
                            Some(self.surfaces[sel.screen_index].output_name.clone());
                        self.mark_all_dirty();
                    }
                    keysyms::KEY_Right
                    | keysyms::KEY_Down
                    | keysyms::KEY_KP_Right
                    | keysyms::KEY_KP_Down
                    | keysyms::KEY_Tab => {
                        sel.screen_index = (sel.screen_index + 1) % count;
                        sel.hovered_output =
                            Some(self.surfaces[sel.screen_index].output_name.clone());
                        self.mark_all_dirty();
                    }
                    keysyms::KEY_Return | keysyms::KEY_KP_Enter | keysyms::KEY_space => {
                        let chosen = sel
                            .hovered_output
                            .as_deref()
                            .and_then(|n| self.surfaces.iter().find(|s| s.output_name == n))
                            .or_else(|| self.surfaces.get(sel.screen_index))
                            .or_else(|| self.surfaces.first());
                        if let Some(target) = chosen {
                            sel.result = Some(SelectionResult::Screen {
                                output_name: target.output_name.clone(),
                                rect: target.geometry,
                            });
                        }
                    }
                    _ => {}
                }
            }
            return;
        }

        if sel.picking_window {
            let count = sel.windows.len();
            if count > 0 {
                match sym_raw {
                    keysyms::KEY_Left
                    | keysyms::KEY_Up
                    | keysyms::KEY_KP_Left
                    | keysyms::KEY_KP_Up
                    | keysyms::KEY_ISO_Left_Tab => {
                        sel.window_index = (sel.window_index + count - 1) % count;
                        sel.hovered_window = Some(sel.window_index);
                        self.mark_all_dirty();
                    }
                    keysyms::KEY_Right
                    | keysyms::KEY_Down
                    | keysyms::KEY_KP_Right
                    | keysyms::KEY_KP_Down
                    | keysyms::KEY_Tab => {
                        sel.window_index = (sel.window_index + 1) % count;
                        sel.hovered_window = Some(sel.window_index);
                        self.mark_all_dirty();
                    }
                    keysyms::KEY_Return | keysyms::KEY_KP_Enter | keysyms::KEY_space => {
                        let chosen = sel
                            .hovered_window
                            .and_then(|idx| sel.windows.get(idx))
                            .or_else(|| sel.windows.get(sel.window_index))
                            .or_else(|| sel.windows.first());
                        if let Some(win) = chosen {
                            sel.result = Some(SelectionResult::Window {
                                node_id: win.node_id,
                                buffer_width: win.buffer_width,
                                buffer_height: win.buffer_height,
                                output_name: win.output.clone(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            return;
        }

        if let Some(menu) = &mut sel.menu {
            match sym_raw {
                keysyms::KEY_Left | keysyms::KEY_KP_Left => {
                    if menu.move_selection(-1) {
                        self.mark_all_dirty();
                    }
                }
                keysyms::KEY_Right | keysyms::KEY_KP_Right | keysyms::KEY_Tab => {
                    if menu.move_selection(1) {
                        self.mark_all_dirty();
                    }
                }
                keysyms::KEY_ISO_Left_Tab => {
                    if menu.move_selection(-1) {
                        self.mark_all_dirty();
                    }
                }
                keysyms::KEY_Return | keysyms::KEY_KP_Enter | keysyms::KEY_space => {
                    let chosen = menu.selected_mode();
                    let active_name = Some(menu.output_name().to_owned());
                    activate_menu_mode(sel, chosen, &self.surfaces, active_name);
                    self.mark_all_dirty();
                }
                _ => {}
            }
        }
    }

    fn redraw_dirty(&mut self) -> Result<(), String> {
        let (Some(shm), Some(qh)) = (self.shm.clone(), self.qh.clone()) else {
            return Ok(());
        };
        let now = Instant::now();
        let renderer = &mut self.renderer;
        let mode = &mut self.mode;

        for (id, slot) in self.surfaces.iter_mut().enumerate() {
            if !slot.configured || !slot.needs_redraw {
                continue;
            }
            slot.needs_redraw = false;

            if let Some(vp) = &slot.viewport {
                vp.set_destination(slot.geometry.size.w.max(1), slot.geometry.size.h.max(1));
            } else {
                slot.surface
                    .set_buffer_scale(slot.scale.round().max(1.0) as i32);
            }

            let buf_w = slot.buf_w;
            let buf_h = slot.buf_h;
            let out_rect = slot.geometry;
            let out_name = slot.output_name.clone();
            let scale = slot.scale;
            let slot_idx = slot.acquire_slot(&shm, &qh, id as u32)?;
            let backdrop_premul = &slot.backdrop_premul;
            let buffer_slot = slot.slots[slot_idx]
                .as_mut()
                .expect("buffer slot initialized");
            let out_bytes = buffer_slot.as_bytes_mut();

            match mode {
                AppMode::Selector(sel) => {
                    let overlay_mode = if sel.picking_region {
                        if let Some(r) = sel.picker.region() {
                            OverlayMode::Region(r)
                        } else {
                            OverlayMode::Highlight(None)
                        }
                    } else if sel.picking_screen {
                        let selected = sel
                            .hovered_output
                            .as_deref()
                            .filter(|name| *name == out_name.as_str())
                            .map(|_| out_rect);
                        OverlayMode::Highlight(selected)
                    } else if sel.picking_window {
                        let selected = sel
                            .hovered_window
                            .and_then(|idx| sel.windows.get(idx))
                            .map(|w| w.rect);
                        OverlayMode::Highlight(selected)
                    } else if let Some(menu) = &sel.menu {
                        OverlayMode::Menu(menu)
                    } else {
                        OverlayMode::Highlight(None)
                    };
                    renderer.draw_selector(
                        out_rect,
                        backdrop_premul,
                        buf_w,
                        buf_h,
                        scale,
                        overlay_mode,
                        &sel.visuals,
                        out_bytes,
                    );
                }
                AppMode::Toast(toast) => {
                    let mix = compute_toast_mix(toast, now);
                    renderer.draw_toast(
                        buf_w,
                        buf_h,
                        scale,
                        &toast.layout,
                        &toast.settings,
                        &toast.message,
                        &toast.encoded.path,
                        toast.encoded.thumb_width,
                        toast.encoded.thumb_height,
                        &toast.encoded.thumb_premultiplied,
                        toast.hovered,
                        mix,
                        out_bytes,
                    );
                }
            }

            buffer_slot.busy = true;
            let wl_buf = buffer_slot.buffer.clone();
            slot.surface.attach(Some(&wl_buf), 0, 0);
            slot.surface.damage_buffer(0, 0, buf_w as i32, buf_h as i32);
            slot.surface.commit();
        }
        Ok(())
    }
}

fn slot_index(len: usize, surface_id: Option<u32>) -> usize {
    surface_id
        .map(|id| id as usize)
        .filter(|id| *id < len)
        .unwrap_or(0)
}

fn topmost_window_at(windows: &[WindowCandidate], desktop: PointF) -> Option<usize> {
    windows
        .iter()
        .enumerate()
        .filter(|(_, w)| w.rect.contains_point_f(desktop))
        .max_by_key(|(_, w)| w.stack_order)
        .map(|(idx, _)| idx)
}

fn initial_window_index(windows: &[WindowCandidate], active_output: Option<&str>) -> Option<usize> {
    if windows.is_empty() {
        return None;
    }
    windows
        .iter()
        .position(|w| w.focused)
        .or_else(|| {
            active_output.and_then(|out| {
                windows
                    .iter()
                    .enumerate()
                    .filter(|(_, w)| w.output.as_deref() == Some(out))
                    .max_by_key(|(_, w)| w.stack_order)
                    .map(|(idx, _)| idx)
            })
        })
        .or_else(|| {
            windows
                .iter()
                .enumerate()
                .max_by_key(|(_, w)| w.stack_order)
                .map(|(idx, _)| idx)
        })
}

fn activate_menu_mode(
    sel: &mut SelectorState,
    chosen: ScreenshotMode,
    surfaces: &[SurfaceSlot],
    active_output_name: Option<String>,
) {
    match chosen {
        ScreenshotMode::Region => {
            sel.menu = None;
            sel.picking_region = true;
            sel.picking_screen = false;
            sel.picking_window = false;
            let active_geom = active_output_name
                .as_deref()
                .and_then(|name| surfaces.iter().find(|s| s.output_name == name))
                .or_else(|| surfaces.first())
                .map(|s| s.geometry)
                .unwrap_or(sel.backdrop_bounds);
            let output_rects: Vec<Rect> = surfaces.iter().map(|s| s.geometry).collect();
            sel.picker.begin(output_rects, active_geom);
        }
        ScreenshotMode::Screen => {
            if surfaces.len() == 1 {
                sel.result = Some(SelectionResult::Screen {
                    output_name: surfaces[0].output_name.clone(),
                    rect: surfaces[0].geometry,
                });
                return;
            }
            sel.menu = None;
            sel.picking_region = false;
            sel.picking_screen = true;
            sel.picking_window = false;
            let initial_name =
                active_output_name.or_else(|| surfaces.first().map(|s| s.output_name.clone()));
            sel.screen_index = initial_name
                .as_deref()
                .and_then(|n| surfaces.iter().position(|s| s.output_name == n))
                .unwrap_or(0);
            sel.hovered_output = initial_name;
        }
        ScreenshotMode::Window => {
            if sel.windows.is_empty() {
                return;
            }
            sel.menu = None;
            sel.picking_region = false;
            sel.picking_screen = false;
            sel.picking_window = true;
            let idx =
                initial_window_index(&sel.windows, active_output_name.as_deref()).unwrap_or(0);
            sel.window_index = idx;
            sel.hovered_window = Some(idx);
        }
    }
}

fn accept_picker(sel: &mut SelectorState, surfaces: &[SurfaceSlot]) {
    let Some(rect) = sel.picker.accept() else {
        sel.result = Some(SelectionResult::Cancelled);
        return;
    };
    save_remembered_region(region_state_path().as_deref(), rect);

    if let Some(scoped) = sel.scoped_output.clone()
        && let Some(slot) = surfaces.iter().find(|s| s.output_name == scoped)
    {
        let rel = Rect::from_xywh(
            (rect.loc.x - slot.geometry.loc.x).max(0),
            (rect.loc.y - slot.geometry.loc.y).max(0),
            rect.size.w.min(slot.geometry.size.w),
            rect.size.h.min(slot.geometry.size.h),
        );
        sel.result = Some(SelectionResult::Region {
            output: Some(scoped),
            rect: rel,
            trim_outer_gaps: false,
        });
        return;
    }

    sel.result = Some(SelectionResult::Region {
        output: None,
        rect,
        trim_outer_gaps: true,
    });
}

fn cursor_hint_to_shape(hint: CursorHint) -> &'static str {
    match hint {
        CursorHint::Crosshair => "crosshair",
        CursorHint::Grabbing => "grabbing",
        CursorHint::ResizeTop => "n-resize",
        CursorHint::ResizeBottom => "s-resize",
        CursorHint::ResizeLeft => "w-resize",
        CursorHint::ResizeRight => "e-resize",
        CursorHint::ResizeTopLeft => "nw-resize",
        CursorHint::ResizeTopRight => "ne-resize",
        CursorHint::ResizeBottomLeft => "sw-resize",
        CursorHint::ResizeBottomRight => "se-resize",
    }
}

fn output_info_rect(info: &OutputInfo) -> Option<Rect> {
    let mode = info.current_mode.and_then(|idx| info.modes.get(idx))?;
    let scale = if info.scale > 0.0 { info.scale } else { 1.0 };
    let w = (f64::from(mode.width) / scale).round().max(1.0) as i32;
    let h = (f64::from(mode.height) / scale).round().max(1.0) as i32;
    Some(Rect::from_xywh(info.offset_x, info.offset_y, w, h))
}

#[allow(clippy::too_many_arguments)]
pub fn run_selector(
    initial_mode: InitialMode,
    outputs: &[OutputInfo],
    scoped_output: Option<&str>,
    preferred_output: Option<&str>,
    backdrop: &CapturedPixels,
    backdrop_bounds: Rect,
    settings: &SyncedSettings,
    remembered_region: Option<Rect>,
    windows: &[WindowCandidate],
    cancel_flag: Option<Arc<AtomicBool>>,
) -> Result<SelectionResult, String> {
    let window_available = !windows.is_empty();
    let conn = Connection::connect_to_env().map_err(|e| format!("Wayland connect: {e}"))?;
    let mut queue: EventQueue<App> = conn.new_event_queue();
    let qh = queue.handle();

    let mut picker = RegionPicker::default();
    picker.set_remembered(
        remembered_region.or_else(|| load_remembered_region(region_state_path().as_deref())),
    );

    let mut sorted_windows = windows.to_vec();
    sorted_windows.sort_by_key(|a| std::cmp::Reverse(a.stack_order));

    let mut app = App {
        conn: conn.clone(),
        compositor: None,
        shm: None,
        layer_shell: None,
        viewporter: None,
        xdg_output_mgr: None,
        seat: None,
        pointer: None,
        keyboard: KeyboardState {
            xkb_context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            keymap: None,
            xkb_state: None,
            seat_kbd: None,
        },
        cursor: CursorState::new(),
        outputs: Vec::new(),
        output_infos: outputs.to_vec(),
        surfaces: Vec::new(),
        pointer_surface_id: None,
        pointer_pos_local: PointF::new(0.0, 0.0),
        renderer: TextRenderer::new(),
        mode: AppMode::Selector(SelectorState {
            initial_mode,
            menu: None,
            picker,
            picking_region: false,
            picking_screen: false,
            picking_window: false,
            hovered_output: None,
            screen_index: 0,
            windows: sorted_windows,
            window_index: 0,
            hovered_window: None,
            active_output_name: String::new(),
            active_output_geom: backdrop_bounds,
            backdrop: Arc::new(backdrop.clone()),
            backdrop_bounds,
            visuals: settings.visuals,
            scoped_output: scoped_output.map(str::to_owned),
            last_click: None,
            result: None,
        }),
        qh: Some(qh.clone()),
    };

    let _registry = conn.display().get_registry(&qh, ());
    queue
        .roundtrip(&mut app)
        .map_err(|e| format!("Wayland roundtrip: {e}"))?;

    if let Some(mgr) = app.xdg_output_mgr.clone() {
        for out in &mut app.outputs {
            if out.xdg.is_none() {
                out.xdg = Some(mgr.get_xdg_output(&out.wl, &qh, out.global_name));
            }
        }
        queue
            .roundtrip(&mut app)
            .map_err(|e| format!("xdg-output roundtrip: {e}"))?;
    } else {
        queue
            .roundtrip(&mut app)
            .map_err(|e| format!("wl-output roundtrip: {e}"))?;
    }

    if app.layer_shell.is_none() {
        return Err("compositor does not support zwlr_layer_shell_v1".into());
    }
    if app.compositor.is_none() || app.shm.is_none() {
        return Err("missing required Wayland globals (wl_compositor or wl_shm)".into());
    }

    let mut targets: Vec<(String, wl_output::WlOutput, Rect, f64, u32, u32)> = Vec::new();
    for out in &app.outputs {
        let Some(name) = out.name.clone() else {
            continue;
        };
        if let Some(filter) = scoped_output
            && name != filter
        {
            continue;
        }
        let Some(geom) = out.resolved_geometry(&app.output_infos) else {
            continue;
        };
        let (scale, buf_w, buf_h) = out.resolved_scale_and_pixels(&app.output_infos, geom.size);
        targets.push((name, out.wl.clone(), geom, scale, buf_w, buf_h));
    }

    if targets.is_empty() {
        return Err(match scoped_output {
            Some(name) => format!("Wayland output {name:?} not found"),
            None => "no active Wayland outputs found".into(),
        });
    }

    let compositor = app.compositor.clone().unwrap();
    let layer_shell = app.layer_shell.clone().unwrap();
    let viewporter = app.viewporter.clone();

    for (idx, (name, wl_out, geom, scale, buf_w, buf_h)) in targets.into_iter().enumerate() {
        let surface = compositor.create_surface(&qh, ());
        let viewport = viewporter
            .as_ref()
            .map(|vp| vp.get_viewport(&surface, &qh, ()));
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            Some(&wl_out),
            Layer::Overlay,
            "swm-screenshoter".into(),
            &qh,
            idx as u32,
        );
        layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer_surface.set_size(0, 0);
        surface.commit();

        let backdrop_premul = prepare_output_backdrop(
            backdrop_bounds,
            &backdrop.rgba,
            backdrop.width,
            backdrop.height,
            geom,
            buf_w,
            buf_h,
        );

        app.surfaces.push(SurfaceSlot {
            output_name: name,
            geometry: geom,
            scale,
            buf_w,
            buf_h,
            surface,
            viewport,
            layer_surface,
            configured: false,
            configured_size: (0, 0),
            needs_redraw: true,
            backdrop_premul,
            slots: [None, None],
        });
    }

    let active_idx = preferred_output
        .and_then(|pref| app.surfaces.iter().position(|s| s.output_name == pref))
        .unwrap_or(0);
    let active_name = app.surfaces[active_idx].output_name.clone();
    let active_geom = app.surfaces[active_idx].geometry;
    let output_rects: Vec<Rect> = app.surfaces.iter().map(|s| s.geometry).collect();

    if let AppMode::Selector(sel) = &mut app.mode {
        sel.active_output_name = active_name.clone();
        sel.active_output_geom = active_geom;
        match sel.initial_mode {
            InitialMode::Menu => {
                sel.menu = Some(ScreenshotMenu::new(
                    active_name,
                    active_geom,
                    window_available,
                ));
            }
            InitialMode::Region => {
                sel.picking_region = true;
                sel.picker.begin(output_rects, active_geom);
            }
            InitialMode::Screen => {
                sel.picking_screen = true;
                sel.screen_index = active_idx;
                sel.hovered_output = Some(active_name);
            }
            InitialMode::Window => {
                if sel.windows.is_empty() {
                    return Ok(SelectionResult::Cancelled);
                }
                sel.picking_window = true;
                let idx = initial_window_index(&sel.windows, Some(&active_name)).unwrap_or(0);
                sel.window_index = idx;
                sel.hovered_window = Some(idx);
            }
        }
    }

    queue
        .roundtrip(&mut app)
        .map_err(|e| format!("configure roundtrip: {e}"))?;
    app.redraw_dirty()?;
    conn.flush().map_err(|e| format!("Wayland flush: {e}"))?;

    let poll_ms = if cancel_flag.is_some() { 25 } else { -1 };
    loop {
        if let Some(flag) = &cancel_flag
            && flag.load(Ordering::Relaxed)
        {
            return Ok(SelectionResult::Cancelled);
        }
        if let AppMode::Selector(sel) = &app.mode
            && let Some(result) = sel.result.clone()
        {
            return Ok(result);
        }

        dispatch_step(&conn, &mut queue, &mut app, poll_ms)?;
        app.redraw_dirty()?;
        conn.flush().map_err(|e| format!("Wayland flush: {e}"))?;
    }
}

pub fn run_toast(
    settings: &SyncedSettings,
    encoded: &EncodedCapture,
    target_output: Option<&str>,
    outputs: &[OutputInfo],
) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("Wayland connect: {e}"))?;
    let mut queue: EventQueue<App> = conn.new_event_queue();
    let qh = queue.handle();

    let dummy_layout = toast_layout(Rect::from_xywh(0, 0, 1920, 1080), settings, 1.0);
    let now = Instant::now();
    let duration = Duration::from_millis(settings.success_duration_ms.max(800));

    let mut app = App {
        conn: conn.clone(),
        compositor: None,
        shm: None,
        layer_shell: None,
        viewporter: None,
        xdg_output_mgr: None,
        seat: None,
        pointer: None,
        keyboard: KeyboardState {
            xkb_context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            keymap: None,
            xkb_state: None,
            seat_kbd: None,
        },
        cursor: CursorState::new(),
        outputs: Vec::new(),
        output_infos: outputs.to_vec(),
        surfaces: Vec::new(),
        pointer_surface_id: None,
        pointer_pos_local: PointF::new(0.0, 0.0),
        renderer: TextRenderer::new(),
        mode: AppMode::Toast(ToastState {
            settings: settings.clone(),
            encoded: EncodedCapture {
                path: encoded.path.clone(),
                png: Arc::clone(&encoded.png),
                thumb_width: encoded.thumb_width,
                thumb_height: encoded.thumb_height,
                thumb_premultiplied: encoded.thumb_premultiplied.clone(),
            },
            message: "Screenshot saved".into(),
            hovered: None,
            pointer_inside: false,
            layout: dummy_layout,
            opened_at: now,
            dismiss_at: now + duration,
            remaining_before_pause: duration,
            closing_at: None,
            finished: false,
        }),
        qh: Some(qh.clone()),
    };

    let _registry = conn.display().get_registry(&qh, ());
    queue
        .roundtrip(&mut app)
        .map_err(|e| format!("Wayland roundtrip: {e}"))?;
    if let Some(mgr) = app.xdg_output_mgr.clone() {
        for out in &mut app.outputs {
            if out.xdg.is_none() {
                out.xdg = Some(mgr.get_xdg_output(&out.wl, &qh, out.global_name));
            }
        }
        queue
            .roundtrip(&mut app)
            .map_err(|e| format!("xdg-output roundtrip: {e}"))?;
    }

    if app.layer_shell.is_none() || app.compositor.is_none() || app.shm.is_none() {
        return Err("compositor lacks required globals for toast".into());
    }

    let chosen = target_output
        .and_then(|name| app.outputs.iter().find(|o| o.name.as_deref() == Some(name)))
        .or_else(|| app.outputs.first())
        .ok_or_else(|| "no Wayland output available for toast".to_string())?;
    let name = chosen.name.clone().unwrap_or_else(|| "default".into());
    let out_geom = chosen
        .resolved_geometry(&app.output_infos)
        .unwrap_or(Rect::from_xywh(0, 0, 1920, 1080));
    let (scale, _, _) = chosen.resolved_scale_and_pixels(&app.output_infos, out_geom.size);
    let layout = toast_layout(out_geom, settings, 1.0);
    let card_geom = layout.card;
    if let AppMode::Toast(toast) = &mut app.mode {
        toast.layout = layout;
    }

    let compositor = app.compositor.clone().unwrap();
    let layer_shell = app.layer_shell.clone().unwrap();
    let surface = compositor.create_surface(&qh, ());
    let viewport = app
        .viewporter
        .as_ref()
        .map(|vp| vp.get_viewport(&surface, &qh, ()));
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        Some(&chosen.wl),
        Layer::Overlay,
        "swm-screenshoter-toast".into(),
        &qh,
        0,
    );
    let margin_left = (card_geom.loc.x - out_geom.loc.x).max(0);
    let margin_top = (card_geom.loc.y - out_geom.loc.y).max(0);
    layer_surface.set_anchor(Anchor::Top | Anchor::Left);
    layer_surface.set_margin(margin_top, 0, 0, margin_left);
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer_surface.set_size(
        card_geom.size.w.max(1) as u32,
        card_geom.size.h.max(1) as u32,
    );
    surface.commit();

    let buf_w = ((f64::from(card_geom.size.w.max(1)) * scale).round() as u32).max(1);
    let buf_h = ((f64::from(card_geom.size.h.max(1)) * scale).round() as u32).max(1);
    app.surfaces.push(SurfaceSlot {
        output_name: name,
        geometry: card_geom,
        scale,
        buf_w,
        buf_h,
        surface,
        viewport,
        layer_surface,
        configured: false,
        configured_size: (0, 0),
        needs_redraw: true,
        backdrop_premul: Vec::new(),
        slots: [None, None],
    });

    queue
        .roundtrip(&mut app)
        .map_err(|e| format!("toast configure: {e}"))?;
    app.redraw_dirty()?;
    conn.flush().map_err(|e| format!("Wayland flush: {e}"))?;

    loop {
        let now = Instant::now();
        let finished = if let AppMode::Toast(toast) = &mut app.mode {
            if toast.finished {
                true
            } else {
                if !toast.pointer_inside && toast.closing_at.is_none() && now >= toast.dismiss_at {
                    toast.closing_at = Some(now);
                }
                if let Some(close_start) = toast.closing_at
                    && now.duration_since(close_start) >= Duration::from_millis(TOAST_FADE_MS)
                {
                    toast.finished = true;
                }
                let opening = now.duration_since(toast.opened_at)
                    <= Duration::from_millis(TOAST_FADE_MS + 16);
                let closing = toast.closing_at.is_some();
                if opening || closing {
                    for s in &mut app.surfaces {
                        s.needs_redraw = true;
                    }
                }
                toast.finished
            }
        } else {
            true
        };
        if finished {
            return Ok(());
        }

        let timeout_ms = if let AppMode::Toast(toast) = &app.mode {
            let now = Instant::now();
            if now.duration_since(toast.opened_at) < Duration::from_millis(TOAST_FADE_MS + 16)
                || toast.closing_at.is_some()
            {
                16
            } else if toast.pointer_inside {
                100
            } else {
                toast
                    .dismiss_at
                    .saturating_duration_since(now)
                    .as_millis()
                    .clamp(16, 100) as i32
            }
        } else {
            16
        };

        dispatch_step(&conn, &mut queue, &mut app, timeout_ms)?;
        app.redraw_dirty()?;
        conn.flush().map_err(|e| format!("Wayland flush: {e}"))?;
    }
}

fn compute_toast_mix(toast: &ToastState, now: Instant) -> f32 {
    if let Some(closing) = toast.closing_at {
        let t = now.duration_since(closing).as_secs_f32() / (TOAST_FADE_MS as f32 / 1000.0);
        return (1.0 - t).clamp(0.0, 1.0);
    }
    let t = now.duration_since(toast.opened_at).as_secs_f32() / (TOAST_FADE_MS as f32 / 1000.0);
    t.clamp(0.0, 1.0)
}

fn dispatch_step(
    conn: &Connection,
    queue: &mut EventQueue<App>,
    app: &mut App,
    timeout_ms: i32,
) -> Result<(), String> {
    queue
        .dispatch_pending(app)
        .map_err(|e| format!("Wayland dispatch: {e}"))?;
    conn.flush().map_err(|e| format!("Wayland flush: {e}"))?;

    if let Some(guard) = queue.prepare_read() {
        let fd = guard.connection_fd().as_raw_fd();
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = loop {
            let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if r < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break r;
        };
        if rc < 0 {
            return Err(format!(
                "Wayland poll failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        if rc > 0 && (pfd.revents & libc::POLLIN) != 0 {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(format!("Wayland read: {e}")),
            }
            queue
                .dispatch_pending(app)
                .map_err(|e| format!("Wayland dispatch: {e}"))?;
        } else {
            drop(guard);
        }
    }
    Ok(())
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" if state.compositor.is_none() => {
                state.compositor = Some(registry.bind::<wl_compositor::WlCompositor, _, _>(
                    name,
                    version.min(6),
                    qh,
                    (),
                ));
            }
            "wl_shm" if state.shm.is_none() => {
                state.shm =
                    Some(registry.bind::<wl_shm::WlShm, _, _>(name, version.min(2), qh, ()));
            }
            "zwlr_layer_shell_v1" if state.layer_shell.is_none() => {
                state.layer_shell =
                    Some(registry.bind::<ZwlrLayerShellV1, _, _>(name, version.min(4), qh, ()));
            }
            "wp_viewporter" if state.viewporter.is_none() => {
                state.viewporter =
                    Some(registry.bind::<WpViewporter, _, _>(name, version.min(1), qh, ()));
            }
            "zxdg_output_manager_v1" if state.xdg_output_mgr.is_none() => {
                state.xdg_output_mgr =
                    Some(registry.bind::<ZxdgOutputManagerV1, _, _>(name, version.min(3), qh, ()));
            }
            "wp_cursor_shape_manager_v1" if state.cursor.manager.is_none() => {
                let mgr =
                    registry.bind::<WpCursorShapeManagerV1, _, _>(name, version.min(1), qh, ());
                if let Some(pointer) = &state.pointer {
                    state.cursor.shape_device = Some(mgr.get_pointer(pointer, qh, ()));
                }
                state.cursor.manager = Some(mgr);
            }
            "wl_seat" if state.seat.is_none() => {
                let seat = registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(9), qh, ());
                state.seat = Some(seat);
            }
            "wl_output" => {
                let wl = registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, name);
                state.outputs.push(TrackedOutput::new(name, wl));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, (u32, usize)> for App {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        &(surface_id, slot_idx): &(u32, usize),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && let Some(surface) = state.surfaces.get_mut(surface_id as usize)
            && let Some(Some(buf)) = surface.slots.get_mut(slot_idx)
        {
            buf.busy = false;
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwlrLayerShellV1,
        _: zwlr_layer_shell_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, u32> for App {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        &surface_id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer_surface.ack_configure(serial);
                if let Some(slot) = state.surfaces.get_mut(surface_id as usize) {
                    slot.configured = true;
                    if (width > 0 && height > 0) && slot.configured_size != (width, height) {
                        slot.configured_size = (width, height);
                        let new_w = ((f64::from(width) * slot.scale).round() as u32).max(1);
                        let new_h = ((f64::from(height) * slot.scale).round() as u32).max(1);
                        if matches!(state.mode, AppMode::Selector(_))
                            && (width as i32 != slot.geometry.size.w
                                || height as i32 != slot.geometry.size.h)
                        {
                            slot.geometry.size = Size::new(width as i32, height as i32);
                            slot.buf_w = new_w;
                            slot.buf_h = new_h;
                            if let Some(sel) = sel_read_only(&state.mode) {
                                slot.backdrop_premul = prepare_output_backdrop(
                                    sel.backdrop_bounds,
                                    &sel.backdrop.rgba,
                                    sel.backdrop.width,
                                    sel.backdrop.height,
                                    slot.geometry,
                                    slot.buf_w,
                                    slot.buf_h,
                                );
                            }
                        }
                    }
                    slot.needs_redraw = true;
                }
            }
            zwlr_layer_surface_v1::Event::Closed => match &mut state.mode {
                AppMode::Selector(sel) => {
                    sel.result = Some(SelectionResult::Cancelled);
                }
                AppMode::Toast(toast) => {
                    toast.finished = true;
                }
            },
            _ => {}
        }
    }
}

fn sel_read_only(mode: &AppMode) -> Option<&SelectorState> {
    match mode {
        AppMode::Selector(sel) => Some(sel),
        AppMode::Toast(_) => None,
    }
}

impl Dispatch<WpViewporter, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpViewporter,
        _: wayland_protocols::wp::viewporter::client::wp_viewporter::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewport, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpViewport,
        _: wayland_protocols::wp::viewporter::client::wp_viewport::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZxdgOutputManagerV1,
        _: wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputV1, u32> for App {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        &global_name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(out) = state
            .outputs
            .iter_mut()
            .find(|o| o.global_name == global_name)
        else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                out.logical_pos = (x, y);
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                out.logical_size = (width, height);
            }
            zxdg_output_v1::Event::Name { name } => {
                out.name = Some(name);
            }
            zxdg_output_v1::Event::Done => {
                out.done = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for App {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        &global_name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(out) = state
            .outputs
            .iter_mut()
            .find(|o| o.global_name == global_name)
        else {
            return;
        };
        match event {
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                if let WEnum::Value(f) = flags
                    && f.contains(wl_output::Mode::Current)
                {
                    out.physical_size = (width, height);
                }
            }
            wl_output::Event::Scale { factor } => {
                out.scale_int = factor.max(1);
            }
            wl_output::Event::Name { name } if out.name.is_none() => {
                out.name = Some(name);
            }
            wl_output::Event::Done => {
                out.done = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                let ptr = seat.get_pointer(qh, ());
                if let Some(mgr) = &state.cursor.manager {
                    state.cursor.shape_device = Some(mgr.get_pointer(&ptr, qh, ()));
                }
                state.pointer = Some(ptr);
            }
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.seat_kbd.is_none() {
                state.keyboard.seat_kbd = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for App {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                state.cursor.last_enter_serial = serial;
                state.cursor.last_name = "";
                state.pointer_surface_id = state.surface_index_for_wl(&surface).map(|i| i as u32);
                if let AppMode::Toast(toast) = &mut state.mode {
                    toast.pointer_inside = true;
                    toast.closing_at = None;
                    toast.remaining_before_pause = toast
                        .dismiss_at
                        .saturating_duration_since(Instant::now())
                        .max(Duration::from_millis(500));
                }
                state.handle_pointer_motion(surface_x, surface_y);
            }
            wl_pointer::Event::Leave { .. } => {
                state.pointer_surface_id = None;
                if let AppMode::Toast(toast) = &mut state.mode {
                    toast.pointer_inside = false;
                    toast.hovered = None;
                    toast.dismiss_at = Instant::now() + toast.remaining_before_pause;
                    state.mark_all_dirty();
                } else if let AppMode::Selector(sel) = &mut state.mode
                    && sel.picking_screen
                {
                    sel.hovered_output = None;
                    state.mark_all_dirty();
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.handle_pointer_motion(surface_x, surface_y);
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(st),
                ..
            } => {
                let pressed = st == wl_pointer::ButtonState::Pressed;
                state.handle_pointer_button(button, pressed);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                let keymap = unsafe {
                    xkb::Keymap::new_from_fd(
                        &state.keyboard.xkb_context,
                        fd,
                        size as usize,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::KEYMAP_COMPILE_NO_FLAGS,
                    )
                }
                .ok()
                .flatten();
                state.keyboard.xkb_state = keymap.as_ref().map(xkb::State::new);
                state.keyboard.keymap = keymap;
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xkb_state) = &mut state.keyboard.xkb_state {
                    xkb_state.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            wl_keyboard::Event::Key {
                key,
                state: key_state,
                ..
            } => {
                if let WEnum::Value(wl_keyboard::KeyState::Pressed) = key_state
                    && let Some(xkb_state) = &state.keyboard.xkb_state
                {
                    let keycode = xkb::Keycode::new(key + 8);
                    let sym = xkb_state.key_get_one_sym(keycode);
                    state.handle_key(sym);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WpCursorShapeManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpCursorShapeManagerV1,
        _: wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpCursorShapeDeviceV1,
        _: wp_cursor_shape_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

pub fn desktop_bounds_for_outputs(outputs: &[OutputInfo]) -> Option<Rect> {
    outputs
        .iter()
        .filter_map(output_info_rect)
        .reduce(Rect::merge)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::ModeInfo;

    #[test]
    fn output_info_rect_accounts_for_fractional_scale() {
        let info = OutputInfo {
            name: "DP-1".into(),
            modes: vec![ModeInfo {
                width: 2560,
                height: 1440,
                refresh_millihz: 144_000,
                preferred: true,
            }],
            current_mode: Some(0),
            offset_x: 1920,
            offset_y: 0,
            scale: 1.5,
            vrr: "off".into(),
            vrr_supported: false,
            vrr_active: false,
        };
        let rect = output_info_rect(&info).unwrap();
        assert_eq!(rect, Rect::from_xywh(1920, 0, 1707, 960));
    }

    #[test]
    fn selector_menu_has_all_three_items_enabled_when_window_exists() {
        let out = Rect::from_xywh(0, 0, 1920, 1080);
        let menu = ScreenshotMenu::new("DP-1".into(), out, true);
        assert!(menu.window_available());
    }

    #[test]
    fn topmost_window_at_picks_highest_stack_order_on_overlap() {
        let windows = vec![
            WindowCandidate {
                node_id: 1,
                output: Some("DP-1".into()),
                rect: Rect::from_xywh(100, 100, 500, 400),
                buffer_width: 500,
                buffer_height: 400,
                stack_order: 2,
                focused: false,
            },
            WindowCandidate {
                node_id: 2,
                output: Some("DP-1".into()),
                rect: Rect::from_xywh(200, 150, 500, 400),
                buffer_width: 500,
                buffer_height: 400,
                stack_order: 5,
                focused: true,
            },
        ];
        assert_eq!(
            topmost_window_at(&windows, PointF::new(250.0, 200.0)),
            Some(1)
        );
        assert_eq!(
            topmost_window_at(&windows, PointF::new(150.0, 200.0)),
            Some(0)
        );
        assert_eq!(topmost_window_at(&windows, PointF::new(50.0, 50.0)), None);
    }
}
