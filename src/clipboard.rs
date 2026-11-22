//! Wayland `image/png` clipboard provider (`ext-data-control-v1` + detached fallback).

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;

use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

struct ClipboardState {
    manager: Option<ExtDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    png: Arc<[u8]>,
    active: bool,
    exit_when_replaced: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "ext_data_control_manager_v1" && state.manager.is_none() {
                state.manager = Some(registry.bind::<ExtDataControlManagerV1, _, _>(
                    name,
                    version.min(1),
                    qh,
                    (),
                ));
            } else if interface == "wl_seat" && state.seat.is_none() {
                state.seat =
                    Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(9), qh, ()));
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for ClipboardState {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for ClipboardState {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for ClipboardState {
    fn event(
        _: &mut Self,
        _: &ExtDataControlDeviceV1,
        _: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                if mime_type == "image/png" {
                    let png = Arc::clone(&state.png);
                    std::thread::spawn(move || {
                        let mut file = std::fs::File::from(fd);
                        let _ = file.write_all(&png);
                    });
                }
            }
            ext_data_control_source_v1::Event::Cancelled => {
                state.active = false;
                state.exit_when_replaced = true;
            }
            _ => {}
        }
    }
}

pub fn spawn_clipboard_owner(png: Arc<[u8]>) -> Result<(), String> {
    if has_ext_data_control()? {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let mut child = Command::new(exe)
            .arg("--serve-clipboard")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn clipboard daemon: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(&png)
                .map_err(|e| format!("send PNG to clipboard daemon: {e}"))?;
        }
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        return Ok(());
    }

    // Fallback when SWM runs w/o `SWM_ALLOW_CLIPBOARD_CONTROL`.
    serve_via_wl_copy(&png)
}

pub fn serve_clipboard_from_stdin() -> Result<(), String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read PNG from stdin: {e}"))?;
    if bytes.is_empty() {
        return Ok(());
    }
    let png: Arc<[u8]> = Arc::from(bytes);

    let conn = Connection::connect_to_env().map_err(|e| format!("Wayland connect: {e}"))?;
    let mut queue = conn.new_event_queue::<ClipboardState>();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = ClipboardState {
        manager: None,
        seat: None,
        png,
        active: true,
        exit_when_replaced: false,
    };
    queue
        .roundtrip(&mut state)
        .map_err(|e| format!("Wayland roundtrip: {e}"))?;

    if let (Some(manager), Some(seat)) = (state.manager.clone(), state.seat.clone()) {
        let device = manager.get_data_device(&seat, &qh, ());
        let source = manager.create_data_source(&qh, ());
        source.offer("image/png".into());
        device.set_selection(Some(&source));
        queue.flush().map_err(|e| format!("Wayland flush: {e}"))?;
        while state.active && !state.exit_when_replaced {
            if queue.blocking_dispatch(&mut state).is_err() {
                break;
            }
        }
        return Ok(());
    }

    serve_via_wl_copy(&state.png)
}

fn has_ext_data_control() -> Result<bool, String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("Wayland connect: {e}"))?;
    let mut queue = conn.new_event_queue::<ClipboardState>();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = ClipboardState {
        manager: None,
        seat: None,
        png: Arc::from([]),
        active: false,
        exit_when_replaced: false,
    };
    queue
        .roundtrip(&mut state)
        .map_err(|e| format!("Wayland roundtrip: {e}"))?;
    Ok(state.manager.is_some() && state.seat.is_some())
}

fn serve_via_wl_copy(png: &[u8]) -> Result<(), String> {
    let mut child = Command::new("wl-copy")
        .args(["--type", "image/png"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            format!(
                "ext-data-control is disabled and wl-copy failed ({e}); enable SWM_ALLOW_CLIPBOARD_CONTROL or install wl-clipboard"
            )
        })?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(png);
    }
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
