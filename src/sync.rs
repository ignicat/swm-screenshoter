//! Syncs screenshot destination, font, and overlay palette with SWM configuration.

use std::path::{Path, PathBuf};

use rune_cfg::RuneConfig;

use crate::ipc::Connection;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgb {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgb {
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub fn bytes(self) -> [u8; 3] {
        [
            (self.r.clamp(0.0, 1.0) * 255.0).round() as u8,
            (self.g.clamp(0.0, 1.0) * 255.0).round() as u8,
            (self.b.clamp(0.0, 1.0) * 255.0).round() as u8,
        ]
    }

    pub fn mix(self, other: Self, amount: f32) -> Self {
        let t = amount.clamp(0.0, 1.0);
        Self {
            r: self.r + (other.r - self.r) * t,
            g: self.g + (other.g - self.g) * t,
            b: self.b + (other.b - self.b) * t,
            a: self.a + (other.a - self.a) * t,
        }
    }

    fn luminance(self) -> f32 {
        self.r * 0.2126 + self.g * 0.7152 + self.b * 0.0722
    }

    pub fn to_tiny_color(self, alpha_mul: f32) -> tiny_skia::Color {
        tiny_skia::Color::from_rgba(
            self.r.clamp(0.0, 1.0),
            self.g.clamp(0.0, 1.0),
            self.b.clamp(0.0, 1.0),
            (self.a * alpha_mul).clamp(0.0, 1.0),
        )
        .unwrap_or(tiny_skia::Color::TRANSPARENT)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ColorMode {
    Auto,
    System,
    Light,
    Dark,
    Fixed(Rgb),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SystemColorScheme {
    #[default]
    NoPreference,
    PreferDark,
    PreferLight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationPosition {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayVisuals {
    pub fill: Rgb,
    pub text: Rgb,
    pub error: Rgb,
    pub subtext: Rgb,
    pub key_fill: Rgb,
    pub border: Rgb,
    pub border_px: f32,
    pub radius: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ThemeInputs {
    bg: ColorMode,
    fg: ColorMode,
    err: ColorMode,
    border: ColorMode,
    borders: bool,
    border_size: i32,
    radius: i32,
}

impl Default for ThemeInputs {
    fn default() -> Self {
        Self {
            bg: ColorMode::Auto,
            fg: ColorMode::Auto,
            err: ColorMode::Fixed(SWM_ERROR),
            border: ColorMode::Fixed(SWM_ACCENT),
            borders: true,
            border_size: 3,
            radius: 8,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SyncedSettings {
    pub screenshot_dir: PathBuf,
    pub font_family: String,
    pub font_size: u16,
    pub notification_position: NotificationPosition,
    pub notification_offset_x: i32,
    pub notification_offset_y: i32,
    pub success_duration_ms: u64,
    pub visuals: OverlayVisuals,
    pub swm_config_path: Option<PathBuf>,
    theme: ThemeInputs,
}

const LIGHT_FILL: Rgb = Rgb::new(0.92, 0.95, 0.98, 1.0);
const DARK_FILL: Rgb = Rgb::new(0.15, 0.18, 0.22, 1.0);
const LIGHT_TEXT: Rgb = Rgb::new(0.08, 0.10, 0.12, 1.0);
const DARK_TEXT: Rgb = Rgb::new(0.94, 0.96, 0.98, 1.0);
const SWM_ACCENT: Rgb = Rgb::new(
    0xd6 as f32 / 255.0,
    0x5d as f32 / 255.0,
    0x26 as f32 / 255.0,
    1.0,
);
const SWM_ERROR: Rgb = Rgb::new(
    0xfb as f32 / 255.0,
    0x49 as f32 / 255.0,
    0x34 as f32 / 255.0,
    1.0,
);
pub const BACKDROP_DIM: Rgb = Rgb::new(0.02, 0.03, 0.05, 0.62);

impl Default for SyncedSettings {
    fn default() -> Self {
        let mut settings = Self {
            screenshot_dir: default_screenshot_dir(),
            font_family: "monospace".into(),
            font_size: 11,
            notification_position: NotificationPosition::TopCenter,
            notification_offset_x: 0,
            notification_offset_y: 0,
            success_duration_ms: 4_000,
            visuals: resolve_visuals(
                ColorMode::Auto,
                ColorMode::Auto,
                ColorMode::Fixed(SWM_ERROR),
                ColorMode::Fixed(SWM_ACCENT),
                true,
                3,
                8,
                SystemColorScheme::NoPreference,
            ),
            swm_config_path: None,
            theme: ThemeInputs::default(),
        };
        settings.swm_config_path = default_swm_config_path();
        settings
    }
}

pub fn load_synced(conn: Option<&mut Connection>) -> SyncedSettings {
    let mut settings = SyncedSettings::default();
    let scheme = read_system_color_scheme();
    let ipc_path = conn.and_then(|c| c.config_path().ok().flatten());
    if let Some(path) = ipc_path.or_else(default_swm_config_path) {
        settings.swm_config_path = Some(path.clone());
        if let Ok(cfg) = RuneConfig::from_file(&path) {
            apply_rune(&mut settings, &cfg, scheme);
        }
    }
    if let Some(extra) = screenshoter_override_path().filter(|p| p.exists())
        && let Ok(cfg) = RuneConfig::from_file(&extra)
    {
        apply_rune(&mut settings, &cfg, scheme);
    }
    settings
}

pub fn default_swm_config_path() -> Option<PathBuf> {
    config_base_dir().map(|dir| dir.join("swm/swm.rune"))
}

pub fn screenshoter_override_path() -> Option<PathBuf> {
    config_base_dir().map(|dir| dir.join("swm/swm-screenshoter.rune"))
}

pub fn region_state_path() -> Option<PathBuf> {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(state).join("swm/screenshoter-region.state"));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".local/state/swm/screenshoter-region.state"))
}

fn config_base_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".config"))
}

fn default_screenshot_dir() -> PathBuf {
    if let Some(pics) = std::env::var_os("XDG_PICTURES_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(pics).join("Screenshots");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(home).join("Pictures/Screenshots");
    }
    PathBuf::from("./Pictures/Screenshots")
}

pub fn expand_directory(raw: &str) -> PathBuf {
    for prefix in ["$env.HOME/", "$HOME/", "~/"] {
        if let Some(rest) = raw.strip_prefix(prefix)
            && let Some(home) = std::env::var_os("HOME")
        {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(raw)
}

fn lookup_any_str(cfg: &RuneConfig, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| cfg.get_optional::<String>(key).ok().flatten())
}

fn lookup_any_i32(cfg: &RuneConfig, keys: &[&str]) -> Option<i32> {
    keys.iter()
        .find_map(|key| cfg.get_optional::<i32>(key).ok().flatten())
}

fn lookup_any_u64(cfg: &RuneConfig, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| cfg.get_optional::<u64>(key).ok().flatten())
}

fn lookup_any_bool(cfg: &RuneConfig, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| cfg.get_optional::<bool>(key).ok().flatten())
}

pub fn apply_rune(settings: &mut SyncedSettings, cfg: &RuneConfig, scheme: SystemColorScheme) {
    if let Some(dir) = lookup_any_str(cfg, &["screenshot.directory"]) {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            settings.screenshot_dir = expand_directory(trimmed);
        }
    }
    if let Some(family) = lookup_any_str(cfg, &["font.family"]) {
        let trimmed = family.trim();
        if !trimmed.is_empty() {
            settings.font_family = trimmed.to_string();
        }
    }
    if let Some(size) = lookup_any_u64(cfg, &["font.size"]) {
        settings.font_size = (size.clamp(6, 96)) as u16;
    }
    if let Some(pos) = lookup_any_str(
        cfg,
        &[
            "overlays.notifications.position",
            "overlay.notifications.position",
        ],
    ) && let Some(parsed) = parse_position(&pos)
    {
        settings.notification_position = parsed;
    }
    if let Some(ox) = lookup_any_i32(
        cfg,
        &[
            "overlays.notifications.offset-x",
            "overlay.notifications.offset-x",
        ],
    ) {
        settings.notification_offset_x = ox;
    }
    if let Some(oy) = lookup_any_i32(
        cfg,
        &[
            "overlays.notifications.offset-y",
            "overlay.notifications.offset-y",
        ],
    ) {
        settings.notification_offset_y = oy;
    }
    if let Some(ms) = lookup_any_u64(
        cfg,
        &[
            "overlays.notifications.success-duration-ms",
            "overlay.notifications.success-duration-ms",
        ],
    ) && ms > 0
    {
        settings.success_duration_ms = ms;
    }

    if let Some(bg) = lookup_any_str(
        cfg,
        &[
            "overlays.background-colour",
            "overlays.background-color",
            "overlay.background-colour",
            "overlay.background-color",
        ],
    )
    .and_then(|s| parse_color_mode(&s))
    {
        settings.theme.bg = bg;
    }
    if let Some(fg) = lookup_any_str(
        cfg,
        &[
            "overlays.text-colour",
            "overlays.text-color",
            "overlay.text-colour",
            "overlay.text-color",
        ],
    )
    .and_then(|s| parse_color_mode(&s))
    {
        settings.theme.fg = fg;
    }
    if let Some(err) = lookup_any_str(
        cfg,
        &[
            "overlays.error-colour",
            "overlays.error-color",
            "overlay.error-colour",
            "overlay.error-color",
        ],
    )
    .and_then(|s| parse_color_mode(&s))
    {
        settings.theme.err = err;
    }
    if let Some(border) = lookup_any_str(
        cfg,
        &[
            "overlays.border-colour",
            "overlays.border-color",
            "overlay.border-colour",
            "overlay.border-color",
        ],
    )
    .and_then(|s| parse_color_mode(&s))
    {
        settings.theme.border = border;
    }
    if let Some(borders) = lookup_any_bool(cfg, &["overlays.borders", "overlay.borders"]) {
        settings.theme.borders = borders;
    }
    if let Some(border_size) = lookup_any_i32(cfg, &["overlays.border-size", "overlay.border-size"])
    {
        settings.theme.border_size = border_size.clamp(0, 64);
    }
    if let Some(radius) = lookup_any_i32(cfg, &["overlays.radius", "overlay.radius"]) {
        settings.theme.radius = radius.clamp(0, 256);
    }

    settings.visuals = resolve_visuals(
        settings.theme.bg,
        settings.theme.fg,
        settings.theme.err,
        settings.theme.border,
        settings.theme.borders,
        settings.theme.border_size,
        settings.theme.radius,
        scheme,
    );
}

fn parse_position(raw: &str) -> Option<NotificationPosition> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "top-left" => Some(NotificationPosition::TopLeft),
        "top-center" => Some(NotificationPosition::TopCenter),
        "top-right" => Some(NotificationPosition::TopRight),
        "bottom-left" => Some(NotificationPosition::BottomLeft),
        "bottom-center" => Some(NotificationPosition::BottomCenter),
        "bottom-right" => Some(NotificationPosition::BottomRight),
        _ => None,
    }
}

fn parse_color_mode(raw: &str) -> Option<ColorMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(ColorMode::Auto),
        "system" => Some(ColorMode::System),
        "light" => Some(ColorMode::Light),
        "dark" => Some(ColorMode::Dark),
        other => parse_hex(other).map(ColorMode::Fixed),
    }
}

pub fn parse_hex(value: &str) -> Option<Rgb> {
    let hex = value
        .trim()
        .strip_prefix('#')
        .unwrap_or(value.trim())
        .as_bytes();
    if !hex.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let digit = |i: usize| (hex[i] as char).to_digit(16).map(|d| d as u8);
    let short = |i: usize| digit(i).map(|d| d * 17);
    let pair = |i: usize| Some(digit(i)? * 16 + digit(i + 1)?);
    let [r, g, b, a] = match hex.len() {
        3 => [short(0)?, short(1)?, short(2)?, 255],
        4 => [short(0)?, short(1)?, short(2)?, short(3)?],
        6 => [pair(0)?, pair(2)?, pair(4)?, 255],
        8 => [pair(0)?, pair(2)?, pair(4)?, pair(6)?],
        _ => return None,
    };
    Some(Rgb::new(
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
        a as f32 / 255.0,
    ))
}

fn resolve_system(mode: ColorMode, scheme: SystemColorScheme) -> ColorMode {
    if mode != ColorMode::System {
        return mode;
    }
    match scheme {
        SystemColorScheme::PreferDark => ColorMode::Dark,
        SystemColorScheme::PreferLight => ColorMode::Light,
        SystemColorScheme::NoPreference => ColorMode::Auto,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn resolve_visuals(
    bg: ColorMode,
    fg: ColorMode,
    err: ColorMode,
    border: ColorMode,
    borders: bool,
    border_size_px: i32,
    radius_px: i32,
    scheme: SystemColorScheme,
) -> OverlayVisuals {
    let fill = match resolve_system(bg, scheme) {
        ColorMode::Auto | ColorMode::System | ColorMode::Light => LIGHT_FILL,
        ColorMode::Dark => DARK_FILL,
        ColorMode::Fixed(c) => c,
    };
    let text = match resolve_system(fg, scheme) {
        ColorMode::Auto | ColorMode::System => {
            if fill.luminance() < 0.45 {
                DARK_TEXT
            } else {
                LIGHT_TEXT
            }
        }
        ColorMode::Light => LIGHT_TEXT,
        ColorMode::Dark => DARK_TEXT,
        ColorMode::Fixed(c) => c,
    };
    let error = match resolve_system(err, scheme) {
        ColorMode::Fixed(c) => c,
        _ => SWM_ERROR,
    };
    let border = match resolve_system(border, scheme) {
        ColorMode::Fixed(c) => c,
        _ => SWM_ACCENT,
    };
    OverlayVisuals {
        fill,
        text,
        error,
        subtext: text.mix(fill, 0.20),
        key_fill: fill.mix(text, 0.10),
        border,
        border_px: if borders {
            border_size_px.max(0) as f32
        } else {
            0.0
        },
        radius: radius_px.max(0) as f32,
    }
}

fn read_system_color_scheme() -> SystemColorScheme {
    let Ok(conn) = zbus::blocking::Connection::session() else {
        return SystemColorScheme::NoPreference;
    };
    let Ok(proxy) = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
    ) else {
        return SystemColorScheme::NoPreference;
    };
    let value: zbus::Result<zbus::zvariant::OwnedValue> =
        proxy.call("Read", &("org.freedesktop.appearance", "color-scheme"));
    match value.ok().and_then(|v| u32::try_from(v).ok()) {
        Some(1) => SystemColorScheme::PreferDark,
        Some(2) => SystemColorScheme::PreferLight,
        _ => SystemColorScheme::NoPreference,
    }
}

pub fn load_remembered_region(path: Option<&Path>) -> Option<crate::picker::Rect> {
    let text = std::fs::read_to_string(path?).ok()?;
    let nums: Vec<i32> = text
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    if nums.len() == 4 && nums[2] > 0 && nums[3] > 0 {
        Some(crate::picker::Rect::from_xywh(
            nums[0], nums[1], nums[2], nums[3],
        ))
    } else {
        None
    }
}

pub fn save_remembered_region(path: Option<&Path>, rect: crate::picker::Rect) {
    let Some(path) = path else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        path,
        format!(
            "{} {} {} {}\n",
            rect.loc.x, rect.loc.y, rect.size.w, rect.size.h
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_home_spellings() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        for raw in ["$env.HOME/Pictures", "$HOME/Pictures", "~/Pictures"] {
            assert_eq!(expand_directory(raw), PathBuf::from(&home).join("Pictures"));
        }
    }

    #[test]
    fn syncs_swm_overlay_palette_and_directory() {
        let cfg = RuneConfig::from_str(
            r##"
screenshot:
  directory "/tmp/shots"
end
font:
  family "JetBrains Mono"
  size 13
end
overlays:
  background-colour "system"
  border-colour "#ff8800"
  radius 12
  notifications:
    position "bottom-right"
    offset-x -10
    offset-y -14
    success-duration-ms 5000
  end
end
"##,
        )
        .unwrap();
        let mut settings = SyncedSettings::default();
        apply_rune(&mut settings, &cfg, SystemColorScheme::PreferDark);
        assert_eq!(settings.screenshot_dir, PathBuf::from("/tmp/shots"));
        assert_eq!(settings.font_family, "JetBrains Mono");
        assert_eq!(settings.font_size, 13);
        assert_eq!(
            settings.notification_position,
            NotificationPosition::BottomRight
        );
        assert_eq!(settings.notification_offset_x, -10);
        assert_eq!(settings.notification_offset_y, -14);
        assert_eq!(settings.success_duration_ms, 5000);
        assert_eq!(settings.visuals.fill, DARK_FILL);
        assert_eq!(settings.visuals.text, DARK_TEXT);
        assert_eq!(settings.visuals.radius, 12.0);
    }
}
