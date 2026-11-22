//! `swm-screenshoter` — companion screenshot utility and XDG portal backend for swm.

mod capture;
mod clipboard;
mod encoder;
mod ipc;
mod menu;
mod picker;
mod portal;
mod render;
mod sync;
mod wayland;

use std::path::PathBuf;
use std::process::ExitCode;

use capture::{CaptureCommand, CaptureOptions, CaptureOutcome, execute_capture};
use ipc::CaptureRect;

const HELP: &str = "\
swm-screenshoter — Companion screenshot utility for swm

Usage:
  swm-screenshoter [COMMAND] [OPTIONS]
  swm-screenshoter --portal
  swm-screenshoter --serve-clipboard

Commands:
  menu                  Open interactive capture menu (default)
  region                Select and capture a screen region (or pass --rect)
  screen                Capture a monitor output
  desktop               Capture the entire multi-monitor desktop
  window [NODE_ID]      Pick a window (or capture NODE_ID directly)

Options:
  -o, --output <NAME>   Scope capture or selector to a named output
  --rect <X Y W H>      Capture explicit region geometry (region only)
  --cursor              Include cursor in captured frame
  --clipboard           Copy PNG image to clipboard
  --silent              Suppress interactive preview toast
  -p, --path <FILE>     Save PNG to a specific output file path
  -h, --help            Print help
  -V, --version         Print version";

#[derive(Clone, Debug, PartialEq, Eq)]
enum CliAction {
    Help,
    Version,
    Portal,
    ServeClipboard,
    Capture {
        command: CaptureCommand,
        output: Option<String>,
        include_cursor: bool,
        clipboard: bool,
        silent: bool,
        path: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        Ok(CliAction::Help) => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Ok(CliAction::Version) => {
            println!("swm-screenshoter {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(CliAction::Portal) => match portal::run_portal_daemon() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("swm-screenshoter --portal: {err}");
                ExitCode::from(1)
            }
        },
        Ok(CliAction::ServeClipboard) => match clipboard::serve_clipboard_from_stdin() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("swm-screenshoter --serve-clipboard: {err}");
                ExitCode::from(1)
            }
        },
        Ok(CliAction::Capture {
            command,
            output,
            include_cursor,
            clipboard,
            silent,
            path,
        }) => {
            let outcome = execute_capture(CaptureOptions {
                command,
                output,
                include_cursor,
                explicit_path: path,
                clipboard,
                cancel: None,
            });
            match outcome {
                Ok(CaptureOutcome::Cancelled) => ExitCode::from(1),
                Ok(CaptureOutcome::Saved(saved)) => {
                    println!("{}", saved.encoded.path.display());
                    if !silent
                        && let Err(err) = wayland::run_toast(
                            &saved.settings,
                            &saved.encoded,
                            saved.toast_output.as_deref(),
                            &saved.outputs,
                        )
                    {
                        eprintln!("swm-screenshoter toast warning: {err}");
                    }
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("swm-screenshoter: {err}");
                    ExitCode::from(1)
                }
            }
        }
        Err(err) => {
            eprintln!("swm-screenshoter: {err}");
            eprintln!("Run `swm-screenshoter --help` for usage.");
            ExitCode::from(2)
        }
    }
}

fn parse_args(args: &[String]) -> Result<CliAction, String> {
    if args.is_empty() {
        return Ok(CliAction::Capture {
            command: CaptureCommand::Menu,
            output: None,
            include_cursor: false,
            clipboard: false,
            silent: false,
            path: None,
        });
    }

    if args.len() == 1 {
        match args[0].as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            "--portal" => return Ok(CliAction::Portal),
            "--serve-clipboard" => return Ok(CliAction::ServeClipboard),
            _ => {}
        }
    }

    let mut idx = 0usize;
    let mut subcommand: Option<&str> = None;
    if !args[0].starts_with('-') {
        subcommand = Some(args[0].as_str());
        idx = 1;
    }

    let mut output: Option<String> = None;
    let mut include_cursor = false;
    let mut clipboard = false;
    let mut silent = false;
    let mut path: Option<PathBuf> = None;
    let mut rect: Option<CaptureRect> = None;
    let mut window_id: Option<u64> = None;

    while idx < args.len() {
        match args[idx].as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            "--portal" | "--serve-clipboard" => {
                return Err(format!(
                    "`{}` cannot be combined with other arguments",
                    args[idx]
                ));
            }
            "-o" | "--output" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "missing output name after `-o/--output`".to_string())?;
                output = Some(value.clone());
            }
            "--cursor" => include_cursor = true,
            "--clipboard" => clipboard = true,
            "--silent" => silent = true,
            "-p" | "--path" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "missing file path after `-p/--path`".to_string())?;
                path = Some(PathBuf::from(value));
            }
            "--rect" => {
                if !matches!(subcommand, Some("region")) {
                    return Err("`--rect` is only valid with the `region` subcommand".into());
                }
                let coords = args
                    .get(idx + 1..idx + 5)
                    .ok_or_else(|| "`--rect` requires 4 integer arguments: X Y W H".to_string())?;
                let x = parse_i32(&coords[0], "X")?;
                let y = parse_i32(&coords[1], "Y")?;
                let width = parse_i32(&coords[2], "W")?;
                let height = parse_i32(&coords[3], "H")?;
                if width <= 0 || height <= 0 {
                    return Err("`--rect` width and height must be positive".into());
                }
                rect = Some(CaptureRect {
                    x,
                    y,
                    width,
                    height,
                });
                idx += 4;
            }
            arg if !arg.starts_with('-')
                && matches!(subcommand, Some("window"))
                && window_id.is_none() =>
            {
                let parsed = arg
                    .parse::<u64>()
                    .map_err(|_| format!("invalid window node id {arg:?}"))?;
                window_id = Some(parsed);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
        idx += 1;
    }

    let command = match subcommand {
        None | Some("menu") => CaptureCommand::Menu,
        Some("region") => CaptureCommand::Region { rect },
        Some("screen") => CaptureCommand::Screen,
        Some("desktop") => CaptureCommand::Desktop,
        Some("window") => CaptureCommand::Window { node_id: window_id },
        Some(other) => return Err(format!("unknown subcommand {other:?}")),
    };

    Ok(CliAction::Capture {
        command,
        output,
        include_cursor,
        clipboard,
        silent,
        path,
    })
}

fn parse_i32(raw: &str, label: &str) -> Result<i32, String> {
    raw.parse::<i32>()
        .map_err(|_| format!("invalid integer for `{label}`: {raw:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec_args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn defaults_to_menu_capture_with_no_args() {
        let parsed = parse_args(&vec_args(&[])).unwrap();
        assert_eq!(
            parsed,
            CliAction::Capture {
                command: CaptureCommand::Menu,
                output: None,
                include_cursor: false,
                clipboard: false,
                silent: false,
                path: None,
            }
        );
    }

    #[test]
    fn parses_region_rect_and_flags() {
        let parsed = parse_args(&vec_args(&[
            "region",
            "--rect",
            "10",
            "20",
            "300",
            "200",
            "-o",
            "DP-1",
            "--silent",
            "--clipboard",
            "--cursor",
            "-p",
            "/tmp/shot.png",
        ]))
        .unwrap();
        assert_eq!(
            parsed,
            CliAction::Capture {
                command: CaptureCommand::Region {
                    rect: Some(CaptureRect {
                        x: 10,
                        y: 20,
                        width: 300,
                        height: 200,
                    }),
                },
                output: Some("DP-1".into()),
                include_cursor: true,
                clipboard: true,
                silent: true,
                path: Some(PathBuf::from("/tmp/shot.png")),
            }
        );
    }

    #[test]
    fn parses_window_id_and_portal_modes() {
        assert_eq!(
            parse_args(&vec_args(&["--portal"])).unwrap(),
            CliAction::Portal
        );
        assert_eq!(
            parse_args(&vec_args(&["--serve-clipboard"])).unwrap(),
            CliAction::ServeClipboard
        );
        assert_eq!(
            parse_args(&vec_args(&["window", "42"])).unwrap(),
            CliAction::Capture {
                command: CaptureCommand::Window { node_id: Some(42) },
                output: None,
                include_cursor: false,
                clipboard: false,
                silent: false,
                path: None,
            }
        );
    }

    #[test]
    fn rejects_invalid_flags_and_rect_dimensions() {
        assert!(parse_args(&vec_args(&["screen", "--rect", "0", "0", "10", "10"])).is_err());
        assert!(parse_args(&vec_args(&["region", "--rect", "0", "0", "0", "10"])).is_err());
        assert!(parse_args(&vec_args(&["--portal", "--silent"])).is_err());
        assert!(parse_args(&vec_args(&["bogus"])).is_err());
    }
}
