//! `org.freedesktop.impl.portal.Screenshot` D-Bus portal backend (`--portal`).

use std::collections::HashMap;
use std::path::Path;

use zbus::blocking::Connection;
use zbus::fdo;
use zbus::interface;
use zbus::message::Header;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::capture::{
    CancelToken, CaptureCommand, CaptureOptions, CaptureOutcome, execute_capture,
};
use crate::wayland;

const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.swm-screenshoter";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_VERSION: u32 = 1;

type Vardict = HashMap<String, OwnedValue>;

pub fn run_portal_daemon() -> Result<(), String> {
    let connection = Connection::session().map_err(|e| format!("session D-Bus connect: {e}"))?;
    connection
        .object_server()
        .at(OBJECT_PATH, ScreenshotPortal::new(connection.clone()))
        .map_err(|e| format!("export portal object: {e}"))?;
    connection
        .request_name(BUS_NAME)
        .map_err(|e| format!("claim bus name {BUS_NAME}: {e}"))?;

    loop {
        std::thread::park();
    }
}

#[derive(Clone)]
struct ScreenshotPortal {
    connection: Connection,
}

impl ScreenshotPortal {
    fn new(connection: Connection) -> Self {
        Self { connection }
    }
}

#[interface(name = "org.freedesktop.impl.portal.Screenshot")]
impl ScreenshotPortal {
    #[zbus(property)]
    async fn version(&self) -> u32 {
        PORTAL_VERSION
    }

    async fn screenshot(
        &self,
        handle: OwnedObjectPath,
        _app_id: &str,
        _parent_window: &str,
        options: Vardict,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<(u32, Vardict)> {
        let this = self.clone();
        let sender = header.sender().map(|name| name.to_owned());
        let interactive = extract_bool(&options, "interactive").unwrap_or(false);
        let include_cursor = extract_bool(&options, "cursor").unwrap_or(false);

        blocking::unblock(move || {
            let owner = sender
                .as_ref()
                .ok_or_else(|| fdo::Error::AccessDenied("missing D-Bus sender".into()))?
                .to_string();
            let cancel = CancelToken::default();
            let _request =
                export_request(&this.connection, &handle, owner.as_str(), cancel.clone())?;

            let outcome = execute_capture(CaptureOptions {
                command: if interactive {
                    CaptureCommand::Menu
                } else {
                    CaptureCommand::Desktop
                },
                output: None,
                include_cursor,
                explicit_path: None,
                clipboard: false,
                cancel: Some(cancel),
            })
            .map_err(fdo::Error::Failed)?;

            match outcome {
                CaptureOutcome::Cancelled => Ok((1, Vardict::new())),
                CaptureOutcome::Saved(saved) => {
                    let uri = path_to_file_uri(&saved.encoded.path).map_err(fdo::Error::Failed)?;
                    if interactive {
                        std::thread::spawn(move || {
                            if let Err(err) = wayland::run_toast(
                                &saved.settings,
                                &saved.encoded,
                                saved.toast_output.as_deref(),
                                &saved.outputs,
                            ) {
                                eprintln!("swm-screenshoter portal toast error: {err}");
                            }
                        });
                    }
                    let mut results = Vardict::new();
                    results.insert("uri".to_string(), owned(Value::from(uri))?);
                    Ok((0, results))
                }
            }
        })
        .await
    }
}

#[derive(Clone)]
struct RequestInterface {
    owner: String,
    cancel: CancelToken,
}

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl RequestInterface {
    async fn close(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        let sender = header
            .sender()
            .map(|name| name.as_str().to_owned())
            .unwrap_or_default();
        same_owner(&sender, &self.owner)?;
        self.cancel.cancel();
        Ok(())
    }
}

struct RequestGuard {
    connection: Connection,
    handle: OwnedObjectPath,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        let _ = self
            .connection
            .object_server()
            .remove::<RequestInterface, _>(self.handle.clone());
    }
}

fn export_request(
    connection: &Connection,
    handle: &OwnedObjectPath,
    owner: &str,
    cancel: CancelToken,
) -> fdo::Result<RequestGuard> {
    if !connection.object_server().at(
        handle.clone(),
        RequestInterface {
            owner: owner.to_owned(),
            cancel,
        },
    )? {
        return Err(fdo::Error::InvalidArgs("request already exists".into()));
    }
    Ok(RequestGuard {
        connection: connection.clone(),
        handle: handle.clone(),
    })
}

fn same_owner(sender: &str, owner: &str) -> fdo::Result<()> {
    if sender.starts_with(':') && sender == owner {
        Ok(())
    } else {
        Err(fdo::Error::AccessDenied(
            "portal object belongs to another D-Bus connection".into(),
        ))
    }
}

fn extract_bool(dict: &Vardict, key: &str) -> Option<bool> {
    match &**dict.get(key)? {
        Value::Bool(v) => Some(*v),
        _ => None,
    }
}

fn owned(value: Value<'_>) -> fdo::Result<OwnedValue> {
    OwnedValue::try_from(value).map_err(|err| fdo::Error::Failed(err.to_string()))
}

fn path_to_file_uri(path: &Path) -> Result<String, String> {
    use std::os::unix::ffi::OsStrExt;
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    let mut uri = String::from("file://");
    for &byte in abs.as_os_str().as_bytes() {
        let unreserved = matches!(
            byte,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/'
        );
        if unreserved {
            uri.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(&mut uri, "%{byte:02X}");
        }
    }
    Ok(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encodes_spaces_and_special_chars_in_file_uri() {
        let uri = path_to_file_uri(Path::new("/home/user/My Shots/shot #1.png")).unwrap();
        assert_eq!(uri, "file:///home/user/My%20Shots/shot%20%231.png");
    }

    #[test]
    fn portal_request_owner_validation_requires_matching_unique_name() {
        assert!(same_owner(":1.42", ":1.42").is_ok());
        assert!(same_owner(":1.99", ":1.42").is_err());
        assert!(same_owner("org.example.App", "org.example.App").is_err());
    }
}
