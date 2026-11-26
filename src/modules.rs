//! Built-in modules: parsing, formatting and evaluation.
//!
//! Every parser takes synthetic text so it can be unit tested without a real
//! `/proc`, `/sys`, niri instance or audio server. [`evaluate`] wires the parsers
//! to an [`Env`].

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::action::ActionSpec;
use crate::niri::{self, NiriAction, Window, Workspace};
use crate::system::Env;
pub use crate::tray::Pixmap;

/// Maximum characters kept from a module value.
pub const DEFAULT_MAX_CHARS: usize = 64;

/// A built-in module kind, plus [`ModuleKind::Custom`] for shell commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ModuleKind {
    Clock,
    Workspaces,
    WindowTitle,
    Media,
    Battery,
    Cpu,
    Memory,
    Network,
    Volume,
    /// StatusNotifierItem tray icons.
    Tray,
    /// Open windows, focused one highlighted; click focuses the window.
    Taskbar,
    /// Compact visual map of workspaces and windows.
    Minimap,
    /// A shell command whose first output line is shown.
    #[default]
    Custom,
}

impl ModuleKind {
    pub fn all() -> &'static [ModuleKind] {
        &[
            Self::Clock,
            Self::Workspaces,
            Self::WindowTitle,
            Self::Media,
            Self::Battery,
            Self::Cpu,
            Self::Memory,
            Self::Network,
            Self::Volume,
            Self::Tray,
            Self::Taskbar,
            Self::Minimap,
            Self::Custom,
        ]
    }

    pub fn needs_command(self) -> bool {
        matches!(self, Self::Custom)
    }

    /// Stable name used in configs, selectors and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clock => "clock",
            Self::Workspaces => "workspaces",
            Self::WindowTitle => "window-title",
            Self::Media => "media",
            Self::Battery => "battery",
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Network => "network",
            Self::Volume => "volume",
            Self::Tray => "tray",
            Self::Taskbar => "taskbar",
            Self::Minimap => "minimap",
            Self::Custom => "custom",
        }
    }
}

/// Module configuration. `kind` defaults to `custom` so v1 command modules keep
/// working unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleSpec {
    #[serde(default)]
    pub kind: ModuleKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    #[serde(default = "crate::default_interval")]
    pub interval_secs: u64,
    /// Clock/strftime format, window-title template, or custom formatting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Network interface name; empty means pick the first non-`lo` interface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    /// Maximum characters kept before an ellipsis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chars: Option<u32>,
    /// Row height in pixels for visual modules such as `minimap`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

impl ModuleSpec {
    pub fn custom(command: impl Into<String>, interval_secs: u64) -> Self {
        Self {
            kind: ModuleKind::Custom,
            command: command.into(),
            interval_secs,
            format: None,
            interface: None,
            max_chars: None,
            height: None,
        }
    }

    pub fn of(kind: ModuleKind, interval_secs: u64) -> Self {
        Self {
            kind,
            command: String::new(),
            interval_secs,
            format: None,
            interface: None,
            max_chars: None,
            height: None,
        }
    }

    pub fn with_format(mut self, format: impl Into<String>) -> Self {
        self.format = Some(format.into());
        self
    }

    pub fn with_max_chars(mut self, max_chars: u32) -> Self {
        self.max_chars = Some(max_chars);
        self
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs)
    }

    pub fn max_chars(&self) -> usize {
        self.max_chars
            .map(|value| value as usize)
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_MAX_CHARS)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.kind.needs_command() && self.command.trim().is_empty() {
            return Err(format!(
                "module kind `{:?}` needs a non-empty `command`",
                self.kind
            ));
        }
        if let Some(max) = self.max_chars
            && max == 0
        {
            return Err("module `max_chars` must be greater than zero".to_owned());
        }
        if let Some(height) = self.height
            && !(24..=1024).contains(&height)
        {
            return Err("module `height` must be between 24 and 1024".to_owned());
        }
        if self.kind == ModuleKind::Clock {
            let format = self.format.as_deref().unwrap_or("%H:%M");
            if format.is_empty() {
                return Err("clock module `format` must not be empty".to_owned());
            }
        }
        Ok(())
    }
}

/// One clickable piece of a module value, e.g. a workspace chip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputItem {
    pub label: String,
    pub action: Option<ActionSpec>,
    pub active: bool,
    pub occupied: bool,
    /// Tray icon; when set it is drawn instead of `label`.
    pub pixmap: Option<Pixmap>,
}

impl OutputItem {
    pub fn text(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            action: None,
            active: false,
            occupied: false,
            pixmap: None,
        }
    }
}

/// One window rectangle in a minimap, normalized to per-mille coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinimapWindow {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub window_id: u64,
    pub active: bool,
}

/// A compact map of the workspaces on one output.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MinimapOutput {
    pub windows: Vec<MinimapWindow>,
}

/// A module's rendered value: a flat string, optional clickable items, or a
/// minimap.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModuleOutput {
    pub text: String,
    pub items: Vec<OutputItem>,
    pub minimap: Option<MinimapOutput>,
}

impl ModuleOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            items: Vec::new(),
            minimap: None,
        }
    }

    pub fn minimap(minimap: MinimapOutput) -> Self {
        Self {
            text: String::new(),
            items: Vec::new(),
            minimap: Some(minimap),
        }
    }
}

/// Per-row sampling state for modules that need deltas.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ModuleState {
    #[default]
    Idle,
    Cpu(Option<CpuSample>),
    Network(Option<NetSample>),
}

/// One `/proc/stat` CPU sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuSample {
    pub busy: u64,
    pub total: u64,
}

impl CpuSample {
    pub fn busy_percent_since(&self, previous: &Self) -> Option<f64> {
        let total = self.total.checked_sub(previous.total)?;
        if total == 0 {
            return None;
        }
        let busy = self.busy.saturating_sub(previous.busy);
        Some((busy as f64 / total as f64) * 100.)
    }
}

/// One `/sys/class/net/<if>/statistics` sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NetSample {
    pub rx: u64,
    pub tx: u64,
    pub unix: i64,
}

/// Evaluates one module row without a shared compositor snapshot.
pub fn evaluate(
    spec: &ModuleSpec,
    env: &mut dyn Env,
    state: &mut ModuleState,
    output: Option<&str>,
) -> ModuleOutput {
    evaluate_with_state(spec, env, state, output, None)
}

/// Evaluates one module row. `snapshot` feeds the visual modules that need
/// windows and workspaces without opening another IPC connection.
pub fn evaluate_with_state(
    spec: &ModuleSpec,
    env: &mut dyn Env,
    state: &mut ModuleState,
    output: Option<&str>,
    snapshot: Option<&loom_state::State>,
) -> ModuleOutput {
    let max = spec.max_chars();
    match spec.kind {
        ModuleKind::Clock => {
            let now = env.unix_time();
            let offset = env.utc_offset_secs(now);
            let format = spec.format.as_deref().unwrap_or("%H:%M");
            ModuleOutput::text(clip(&format_clock(now, offset, format), max))
        }
        ModuleKind::Workspaces => {
            let reply = env.niri(&Value::String("Workspaces".to_owned()));
            let workspaces = reply
                .as_ref()
                .map(niri::parse_workspaces)
                .unwrap_or_default();
            workspaces_output(&workspaces, output)
        }
        ModuleKind::WindowTitle => {
            let reply = env.niri(&Value::String("FocusedWindow".to_owned()));
            let window = reply.as_ref().and_then(niri::parse_focused_window);
            let format = spec.format.as_deref().unwrap_or("{app} - {title}");
            ModuleOutput::text(clip(&format_window_title(window.as_ref(), format), max))
        }
        ModuleKind::Media => {
            let command = "playerctl metadata --format '{{status}}|{{artist}}|{{title}}'";
            let reply = env.command_line(command);
            let media = reply.as_deref().and_then(parse_playerctl);
            match media {
                Some(media) => ModuleOutput::text(clip(&media.format(), max)),
                None => ModuleOutput::default(),
            }
        }
        ModuleKind::Battery => match read_battery(env) {
            Some(battery) => {
                let format = spec.format.as_deref().unwrap_or("{percent}% {status}");
                let text = format
                    .replace("{percent}", &battery.capacity.to_string())
                    .replace("{status}", &battery.status);
                ModuleOutput::text(clip(text.trim(), max))
            }
            None => ModuleOutput::default(),
        },
        ModuleKind::Cpu => {
            let current = env
                .read_file("/proc/stat")
                .as_deref()
                .and_then(parse_cpu_sample);
            let previous = match state {
                ModuleState::Cpu(previous) => *previous,
                _ => None,
            };
            *state = ModuleState::Cpu(current);
            match (previous, current) {
                (Some(previous), Some(current)) => current
                    .busy_percent_since(&previous)
                    .map(|percent| ModuleOutput::text(format!("{percent:.0}%")))
                    .unwrap_or_default(),
                _ => ModuleOutput::default(),
            }
        }
        ModuleKind::Memory => match env
            .read_file("/proc/meminfo")
            .as_deref()
            .and_then(parse_meminfo)
        {
            Some(memory) => ModuleOutput::text(clip(&memory.format(), max)),
            None => ModuleOutput::default(),
        },
        ModuleKind::Network => {
            let previous = match state {
                ModuleState::Network(previous) => *previous,
                _ => None,
            };
            let now = env.unix_time();
            match read_network(env, spec.interface.as_deref(), now) {
                Some((interface, up, sample)) => {
                    *state = ModuleState::Network(Some(sample));
                    let text = format_network(&interface, up, previous, sample);
                    ModuleOutput::text(clip(&text, max))
                }
                None => ModuleOutput::default(),
            }
        }
        ModuleKind::Volume => match read_volume(env) {
            Some(volume) => ModuleOutput::text(clip(&volume.format(), max)),
            None => ModuleOutput::default(),
        },
        ModuleKind::Tray => tray_output(),
        ModuleKind::Taskbar => taskbar_output(env, spec),
        ModuleKind::Minimap => minimap_output(snapshot, output),
        ModuleKind::Custom => match env.command_line(&spec.command) {
            Some(line) if !line.is_empty() => ModuleOutput::text(clip(&line, max)),
            _ => ModuleOutput::default(),
        },
    }
}

fn minimap_output(snapshot: Option<&loom_state::State>, output: Option<&str>) -> ModuleOutput {
    let Some(state) = snapshot else {
        return ModuleOutput::default();
    };
    let output = output.or_else(|| {
        state
            .focused_workspace()
            .and_then(|workspace| workspace.output.as_deref())
    });
    let Some(output) = output else {
        return ModuleOutput::default();
    };

    let workspaces = state.workspaces_on(output);
    if workspaces.is_empty() {
        return ModuleOutput::default();
    }

    let rows = workspaces.len() as u32;
    let row_height = 1000 / rows;
    let mut windows = Vec::new();
    for (row, workspace) in workspaces.iter().enumerate() {
        let windows_on_workspace = state.windows_on(workspace.id);
        if windows_on_workspace.is_empty() {
            continue;
        }
        let max_column = windows_on_workspace
            .iter()
            .filter_map(|window| window.layout.pos_in_scrolling_layout)
            .map(|(column, _)| column)
            .max()
            .unwrap_or(1) as u32;
        let max_tile = windows_on_workspace
            .iter()
            .filter_map(|window| window.layout.pos_in_scrolling_layout)
            .map(|(_, tile)| tile)
            .max()
            .unwrap_or(1) as u32;
        let column_width = 1000 / max_column.max(1);
        let tile_height = row_height / max_tile.max(1);
        for window in windows_on_workspace {
            let (column, tile) = window.layout.pos_in_scrolling_layout.unwrap_or((1, 1));
            let x = (column.saturating_sub(1) as u32 * column_width).min(999) as u16;
            let y = (row as u32 * row_height + tile.saturating_sub(1) as u32 * tile_height).min(999)
                as u16;
            windows.push(MinimapWindow {
                x,
                y,
                width: column_width.max(1).min(1000 - u32::from(x)) as u16,
                height: tile_height.max(1).min(1000 - u32::from(y)) as u16,
                window_id: window.id,
                active: window.is_focused,
            });
        }
    }

    if windows.is_empty() {
        ModuleOutput::default()
    } else {
        ModuleOutput::minimap(MinimapOutput { windows })
    }
}

fn taskbar_output(env: &mut dyn Env, spec: &ModuleSpec) -> ModuleOutput {
    let reply = env.niri(&Value::String("Windows".to_owned()));
    let Some(reply) = reply else {
        return ModuleOutput::default();
    };
    let max = spec.max_chars();
    let mut items = Vec::new();
    for window in niri::parse_windows(&reply) {
        let app = window.app_id.as_deref().unwrap_or("");
        let title = window.title.as_deref().unwrap_or("untitled");
        let label = if app.is_empty() {
            title.to_owned()
        } else {
            format!("{app}: {title}")
        };
        items.push(OutputItem {
            label: clip(&label, max),
            action: Some(ActionSpec::Niri(NiriAction::focus_window(window.id))),
            active: window.is_focused,
            occupied: false,
            pixmap: None,
        });
    }
    ModuleOutput {
        text: String::new(),
        items,
        minimap: None,
    }
}

fn tray_output() -> ModuleOutput {
    let exe = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "loom-sidepanels".to_owned());
    let exe = shell_quote(&exe);
    let items = crate::tray::snapshot()
        .into_iter()
        .map(|item| {
            let label = if item.title.trim().is_empty() {
                item.service.clone()
            } else {
                item.title.clone()
            };
            let action = ActionSpec::command(format!(
                "{exe} tray-click {} {} left",
                shell_quote(&item.service),
                shell_quote(&item.path)
            ));
            let active = matches!(item.status.as_str(), "Active" | "NeedsAttention");
            OutputItem {
                label: if item.pixmap.is_some() {
                    String::new()
                } else {
                    label
                },
                action: Some(action),
                active,
                occupied: false,
                pixmap: item.pixmap,
            }
        })
        .collect();
    ModuleOutput {
        text: String::new(),
        items,
        minimap: None,
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn clip(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    if max <= 3 {
        return text.chars().take(max).collect();
    }
    let mut out: String = text.chars().take(max - 3).collect();
    out.push_str("...");
    out
}

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

/// Formats `unix` (plus a UTC offset) with a small strftime subset.
///
/// Supported: `%H %M %S %I %p %d %e %m %Y %y %a %A %b %B %j %s %F %T %%`.
/// Unknown specifiers are copied verbatim after the `%`.
pub fn format_clock(unix: i64, offset_secs: i64, format: &str) -> String {
    let local = unix + offset_secs;
    let days = local.div_euclid(86_400);
    let seconds = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds / 3_600;
    let minute = (seconds % 3_600) / 60;
    let second = seconds % 60;
    let weekday = (days + 4).rem_euclid(7) as usize;
    let day_of_year = days - days_from_civil(year, 1, 1) + 1;

    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const WEEKDAYS_LONG: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const MONTHS_LONG: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];

    let mut out = String::with_capacity(format.len() + 16);
    let mut chars = format.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let Some(spec) = chars.next() else {
            out.push('%');
            break;
        };
        let value = match spec {
            'H' => format!("{hour:02}"),
            'M' => format!("{minute:02}"),
            'S' => format!("{second:02}"),
            'I' => format!("{:02}", hour % 12).replace("00", "12"),
            'p' => {
                if hour < 12 {
                    "AM".to_owned()
                } else {
                    "PM".to_owned()
                }
            }
            'd' => format!("{day:02}"),
            'e' => format!("{day:2}"),
            'm' => format!("{month:02}"),
            'Y' => format!("{year}"),
            'y' => format!("{:02}", year.rem_euclid(100)),
            'a' => WEEKDAYS[weekday].to_owned(),
            'A' => WEEKDAYS_LONG[weekday].to_owned(),
            'b' => MONTHS[(month as usize - 1) % 12].to_owned(),
            'B' => MONTHS_LONG[(month as usize - 1) % 12].to_owned(),
            'j' => format!("{day_of_year:03}"),
            's' => unix.to_string(),
            'F' => format!("{year:04}-{month:02}-{day:02}"),
            'T' => format!("{hour:02}:{minute:02}:{second:02}"),
            '%' => "%".to_owned(),
            other => format!("%{other}"),
        };
        out.push_str(&value);
    }
    out
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = (year - era * 400) as u64;
    let month = month as u64;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

// ---------------------------------------------------------------------------
// Workspaces and window title
// ---------------------------------------------------------------------------

/// Builds workspace chips: active is highlighted, occupied is brighter, empty
/// is dim. Every chip focuses its workspace by niri's stable id.
pub fn workspaces_output(workspaces: &[Workspace], output: Option<&str>) -> ModuleOutput {
    let mut workspaces: Vec<&Workspace> = workspaces
        .iter()
        .filter(|workspace| match output {
            Some(name) => workspace.output.as_deref() == Some(name),
            None => true,
        })
        .collect();
    workspaces.sort_by_key(|workspace| (workspace.output.clone(), workspace.idx));

    let items: Vec<OutputItem> = workspaces
        .iter()
        .map(|workspace| OutputItem {
            label: workspace.label(),
            action: Some(ActionSpec::Niri(NiriAction::focus_id(workspace.id))),
            active: workspace.is_active || workspace.is_focused,
            occupied: workspace.is_occupied(),
            pixmap: None,
        })
        .collect();
    ModuleOutput {
        text: items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        items,
        minimap: None,
    }
}

/// Formats a focused window with `{app}` and `{title}` placeholders.
pub fn format_window_title(window: Option<&Window>, format: &str) -> String {
    let Some(window) = window else {
        return String::new();
    };
    let app = window.app_id.as_deref().unwrap_or("").trim();
    let title = window.title.as_deref().unwrap_or("").trim();
    if app.is_empty() && title.is_empty() {
        return String::new();
    }
    if app.is_empty() {
        return title.to_owned();
    }
    if title.is_empty() {
        return app.to_owned();
    }
    let text = format.replace("{app}", app).replace("{title}", title);
    tidy_separators(&text)
}

fn tidy_separators(text: &str) -> String {
    let mut text = text.trim();
    for separator in ["-", "·", "|", ":"] {
        text = text
            .trim_start_matches(separator)
            .trim_end_matches(separator)
            .trim();
    }
    text.to_owned()
}

// ---------------------------------------------------------------------------
// Media
// ---------------------------------------------------------------------------

/// Parsed `playerctl metadata` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Media {
    pub status: String,
    pub artist: String,
    pub title: String,
}

impl Media {
    pub fn format(&self) -> String {
        let body = match (self.artist.is_empty(), self.title.is_empty()) {
            (true, true) => return String::new(),
            (true, false) => self.title.clone(),
            (false, true) => self.artist.clone(),
            (false, false) => format!("{} - {}", self.artist, self.title),
        };
        match self.status.to_ascii_lowercase().as_str() {
            "paused" => format!("[paused] {body}"),
            "stopped" => format!("[stopped] {body}"),
            _ => body,
        }
    }
}

/// Parses `Playing|Artist|Title` (playerctl's `--format` output).
pub fn parse_playerctl(text: &str) -> Option<Media> {
    let mut parts = text.trim().splitn(3, '|');
    let status = parts.next()?.trim();
    let artist = parts.next().unwrap_or_default().trim();
    let title = parts.next().unwrap_or_default().trim();
    if status.is_empty() && artist.is_empty() && title.is_empty() {
        return None;
    }
    Some(Media {
        status: status.to_owned(),
        artist: artist.to_owned(),
        title: title.to_owned(),
    })
}

// ---------------------------------------------------------------------------
// Battery
// ---------------------------------------------------------------------------

/// Parsed `/sys/class/power_supply/BAT*` state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Battery {
    pub capacity: u8,
    pub status: String,
}

pub fn parse_battery(capacity: &str, status: &str) -> Option<Battery> {
    let capacity: u8 = capacity.trim().parse().ok()?;
    let status = match status.trim() {
        "Charging" => "charging",
        "Discharging" => "discharging",
        "Full" => "full",
        "Not charging" => "idle",
        "" => "unknown",
        other => other,
    };
    Some(Battery {
        capacity,
        status: status.to_owned(),
    })
}

fn read_battery(env: &mut dyn Env) -> Option<Battery> {
    for entry in env.list_dir("/sys/class/power_supply") {
        let base = format!("/sys/class/power_supply/{entry}");
        let kind = env.read_file(&format!("{base}/type")).unwrap_or_default();
        if kind.trim() != "Battery" {
            continue;
        }
        let capacity = env.read_file(&format!("{base}/capacity"))?;
        let status = env.read_file(&format!("{base}/status")).unwrap_or_default();
        if let Some(battery) = parse_battery(&capacity, &status) {
            return Some(battery);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// CPU and memory
// ---------------------------------------------------------------------------

/// Parses the aggregate `cpu` line of `/proc/stat`.
pub fn parse_cpu_sample(stat: &str) -> Option<CpuSample> {
    let line = stat.lines().find(|line| line.starts_with("cpu "))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|value| value.parse().ok())
        .collect();
    if values.len() < 4 {
        return None;
    }
    let total: u64 = values.iter().sum();
    let idle = values[3] + values.get(4).copied().unwrap_or(0);
    Some(CpuSample {
        busy: total.saturating_sub(idle),
        total,
    })
}

/// Parsed `/proc/meminfo` totals, in KiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    pub total_kib: u64,
    pub available_kib: u64,
}

impl Memory {
    pub fn used_kib(&self) -> u64 {
        self.total_kib.saturating_sub(self.available_kib)
    }

    pub fn percent(&self) -> f64 {
        if self.total_kib == 0 {
            return 0.;
        }
        (self.used_kib() as f64 / self.total_kib as f64) * 100.
    }

    pub fn format(&self) -> String {
        format!(
            "{:.1}G/{:.1}G {:.0}%",
            self.used_kib() as f64 / 1_048_576.,
            self.total_kib as f64 / 1_048_576.,
            self.percent()
        )
    }
}

pub fn parse_meminfo(text: &str) -> Option<Memory> {
    let mut total = None;
    let mut available = None;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else {
            continue;
        };
        match name {
            "MemTotal:" => total = parts.next().and_then(|value| value.parse().ok()),
            "MemAvailable:" => available = parts.next().and_then(|value| value.parse().ok()),
            _ => {}
        }
        if total.is_some() && available.is_some() {
            break;
        }
    }
    let total_kib = total?;
    Some(Memory {
        total_kib,
        available_kib: available.unwrap_or(0),
    })
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

/// Formats a per-refresh network sample.
pub fn format_network(
    interface: &str,
    up: bool,
    previous: Option<NetSample>,
    current: NetSample,
) -> String {
    if !up {
        return format!("{interface} down");
    }
    let Some(previous) = previous else {
        return format!("{interface} up");
    };
    let seconds = (current.unix - previous.unix).max(1) as f64;
    let rx = current.rx.saturating_sub(previous.rx) as f64 / seconds;
    let tx = current.tx.saturating_sub(previous.tx) as f64 / seconds;
    format!(
        "{interface} up, {} rx {} tx",
        human_rate(rx),
        human_rate(tx)
    )
}

pub fn human_rate(bytes_per_second: f64) -> String {
    if bytes_per_second >= 1_000_000. {
        format!("{:.1}M/s", bytes_per_second / 1_000_000.)
    } else if bytes_per_second >= 1_000. {
        format!("{:.0}K/s", bytes_per_second / 1_000.)
    } else {
        format!("{bytes_per_second:.0}B/s")
    }
}

fn read_network(
    env: &mut dyn Env,
    configured: Option<&str>,
    now: i64,
) -> Option<(String, bool, NetSample)> {
    let interface = match configured.filter(|name| !name.is_empty()) {
        Some(name) => name.to_owned(),
        None => {
            let entries = env.list_dir("/sys/class/net");
            entries
                .iter()
                .find(|name| {
                    name.as_str() != "lo"
                        && env
                            .read_file(&format!("/sys/class/net/{name}/operstate"))
                            .is_some_and(|state| state.trim() == "up")
                })
                .or_else(|| entries.iter().find(|name| name.as_str() != "lo"))
                .cloned()?
        }
    };
    let base = format!("/sys/class/net/{interface}");
    let operstate = env.read_file(&format!("{base}/operstate"))?;
    let up = operstate.trim() == "up";
    let rx = env
        .read_file(&format!("{base}/statistics/rx_bytes"))
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0);
    let tx = env
        .read_file(&format!("{base}/statistics/tx_bytes"))
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0);
    Some((interface, up, NetSample { rx, tx, unix: now }))
}

// ---------------------------------------------------------------------------
// Volume
// ---------------------------------------------------------------------------

/// Volume level and mute state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Volume {
    pub percent: u8,
    pub muted: bool,
}

impl Volume {
    pub fn format(&self) -> String {
        if self.muted {
            format!("{}% muted", self.percent)
        } else {
            format!("{}%", self.percent)
        }
    }
}

/// Command used by the volume module: volume and mute in one shell invocation.
pub const PACTL_COMMAND: &str =
    "pactl get-sink-volume @DEFAULT_SINK@; pactl get-sink-mute @DEFAULT_SINK@";
pub const WPCTL_COMMAND: &str = "wpctl get-volume @DEFAULT_AUDIO_SINK@";

/// Parses `pactl get-sink-volume` (and an optional `get-sink-mute` line).
pub fn parse_pactl_volume(text: &str) -> Option<Volume> {
    let percent = text
        .split_whitespace()
        .find_map(|word| word.strip_suffix('%')?.parse::<u8>().ok())?;
    let muted = text
        .lines()
        .find(|line| line.contains("Mute:"))
        .map(|line| line.to_ascii_lowercase().contains("yes"))
        .unwrap_or(false);
    Some(Volume { percent, muted })
}

/// Parses `wpctl get-volume` (`Volume: 0.50 [MUTED]`).
pub fn parse_wpctl_volume(text: &str) -> Option<Volume> {
    let percent = text
        .split_whitespace()
        .find_map(|word| word.parse::<f64>().ok())
        .map(|value| (value * 100.).round() as u8)?;
    Some(Volume {
        percent,
        muted: text.contains("[MUTED]"),
    })
}

fn read_volume(env: &mut dyn Env) -> Option<Volume> {
    if let Some(output) = env.command_line(PACTL_COMMAND)
        && let Some(volume) = parse_pactl_volume(&output)
    {
        return Some(volume);
    }
    let output = env.command_line(WPCTL_COMMAND)?;
    parse_wpctl_volume(&output)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::system::mock::MockEnv;

    fn mock() -> MockEnv {
        MockEnv::new(1_709_251_200)
    }

    #[test]
    fn formats_clock_specifiers() {
        assert_eq!(
            format_clock(0, 0, "%Y-%m-%d %H:%M:%S %a %b %j"),
            "1970-01-01 00:00:00 Thu Jan 001"
        );
        // 2024-02-29 12:34:56 UTC.
        assert_eq!(
            format_clock(1_709_210_096, 0, "%F %T %A %B %y %I%p"),
            "2024-02-29 12:34:56 Thursday February 24 12PM"
        );
        assert_eq!(format_clock(0, 3_600, "%H:%M"), "01:00");
        assert_eq!(format_clock(0, 0, "100%% %q"), "100% %q");
    }

    #[test]
    fn clock_module_uses_format_and_local_offset() {
        let mut env = mock();
        env.offset = 3_600;
        let spec = ModuleSpec::of(ModuleKind::Clock, 1).with_format("%H:%M");
        let output = evaluate(&spec, &mut env, &mut ModuleState::Idle, None);
        assert_eq!(output.text, "01:00");
    }

    #[test]
    fn parses_cpu_deltas() {
        let first = "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 1 2 3 4";
        let second = "cpu  200 0 200 1600 0 0 0 0 0 0\n";
        let previous = parse_cpu_sample(first).unwrap();
        assert_eq!(previous.total, 1000);
        assert_eq!(previous.busy, 200);
        let current = parse_cpu_sample(second).unwrap();
        assert_eq!(current.busy_percent_since(&previous), Some(20.0));

        let mut env = mock();
        let spec = ModuleSpec::of(ModuleKind::Cpu, 1);
        let mut state = ModuleState::Idle;
        env.files.insert("/proc/stat".into(), first.into());
        assert_eq!(evaluate(&spec, &mut env, &mut state, None).text, "");
        env.files.insert("/proc/stat".into(), second.into());
        assert_eq!(evaluate(&spec, &mut env, &mut state, None).text, "20%");
        assert_eq!(parse_cpu_sample("nothing"), None);
    }

    #[test]
    fn formats_memory() {
        let memory = parse_meminfo(
            "MemTotal:       16000000 kB\nMemFree:  1000 kB\nMemAvailable:  8000000 kB\n",
        )
        .unwrap();
        assert_eq!(memory.used_kib(), 8_000_000);
        assert_eq!(memory.percent(), 50.0);
        assert_eq!(memory.format(), "7.6G/15.3G 50%");
        assert_eq!(parse_meminfo("MemTotal: 0 kB").unwrap().percent(), 0.0);
        assert_eq!(parse_meminfo(""), None);
    }

    #[test]
    fn parses_battery_and_picks_the_battery_entry() {
        assert_eq!(
            parse_battery("87\n", "Discharging\n"),
            Some(Battery {
                capacity: 87,
                status: "discharging".into()
            })
        );
        assert_eq!(parse_battery("nope", "Full"), None);

        let mut env = mock()
            .dir("/sys/class/power_supply", &["AC", "BAT0"])
            .file("/sys/class/power_supply/AC/type", "Mains\n")
            .file("/sys/class/power_supply/BAT0/type", "Battery\n")
            .file("/sys/class/power_supply/BAT0/capacity", "87\n")
            .file("/sys/class/power_supply/BAT0/status", "Charging\n");
        let spec = ModuleSpec::of(ModuleKind::Battery, 30);
        let output = evaluate(&spec, &mut env, &mut ModuleState::Idle, None);
        assert_eq!(output.text, "87% charging");
    }

    #[test]
    fn formats_network_rates_and_states() {
        let interface = "wlan0";
        let previous = NetSample {
            rx: 0,
            tx: 0,
            unix: 100,
        };
        let current = NetSample {
            rx: 2_400_000,
            tx: 120_000,
            unix: 102,
        };
        assert_eq!(
            format_network(interface, true, Some(previous), current),
            "wlan0 up, 1.2M/s rx 60K/s tx"
        );
        assert_eq!(format_network(interface, true, None, current), "wlan0 up");
        assert_eq!(
            format_network(interface, false, None, current),
            "wlan0 down"
        );
        assert_eq!(human_rate(512.), "512B/s");
    }

    #[test]
    fn network_module_picks_and_samples_the_interface() {
        let mut env = mock()
            .dir("/sys/class/net", &["lo", "wlan0"])
            .file("/sys/class/net/lo/operstate", "unknown\n")
            .file("/sys/class/net/wlan0/operstate", "up\n")
            .file("/sys/class/net/wlan0/statistics/rx_bytes", "1000\n")
            .file("/sys/class/net/wlan0/statistics/tx_bytes", "500\n");
        let spec = ModuleSpec::of(ModuleKind::Network, 1);
        let mut state = ModuleState::Idle;
        assert_eq!(evaluate(&spec, &mut env, &mut state, None).text, "wlan0 up");
        env.unix += 1;
        env.files.insert(
            "/sys/class/net/wlan0/statistics/rx_bytes".into(),
            "2000".into(),
        );
        assert_eq!(
            evaluate(&spec, &mut env, &mut state, None).text,
            "wlan0 up, 1K/s rx 0B/s tx"
        );
    }

    #[test]
    fn parses_volume_from_pactl_and_wpctl() {
        let pactl = "Volume: front-left: 32768 /  50% / -18.06 dB,   front-right: 32768 /  50% / -18.06 dB\nMute: no\n";
        assert_eq!(
            parse_pactl_volume(pactl),
            Some(Volume {
                percent: 50,
                muted: false
            })
        );
        assert_eq!(
            parse_pactl_volume(&pactl.replace("Mute: no", "Mute: yes")),
            Some(Volume {
                percent: 50,
                muted: true
            })
        );
        assert_eq!(
            parse_wpctl_volume("Volume: 0.35 [MUTED]"),
            Some(Volume {
                percent: 35,
                muted: true
            })
        );
        assert_eq!(parse_pactl_volume("Volume: nope"), None);
        assert_eq!(
            Volume {
                percent: 35,
                muted: true
            }
            .format(),
            "35% muted"
        );
    }

    #[test]
    fn volume_module_falls_back_to_wpctl() {
        let mut env = mock();
        {
            let mut queue = env.command_queue.borrow_mut();
            queue.push_back(None);
            queue.push_back(Some("Volume: 0.35 [MUTED]".into()));
        }
        let spec = ModuleSpec::of(ModuleKind::Volume, 5);
        let output = evaluate(&spec, &mut env, &mut ModuleState::Idle, None);
        assert_eq!(output.text, "35% muted");
    }

    #[test]
    fn parses_and_formats_media() {
        assert_eq!(
            parse_playerctl("Playing|Boards of Canada|Roygbiv"),
            Some(Media {
                status: "Playing".into(),
                artist: "Boards of Canada".into(),
                title: "Roygbiv".into()
            })
        );
        assert_eq!(
            parse_playerctl("Playing|Boards of Canada|Roygbiv")
                .unwrap()
                .format(),
            "Boards of Canada - Roygbiv"
        );
        assert_eq!(
            parse_playerctl("Paused||Interlude").unwrap().format(),
            "[paused] Interlude"
        );
        assert_eq!(parse_playerctl("  "), None);
    }

    #[test]
    fn media_module_degrades_when_playerctl_is_missing() {
        let mut env = mock();
        let spec = ModuleSpec::of(ModuleKind::Media, 2);
        assert_eq!(
            evaluate(&spec, &mut env, &mut ModuleState::Idle, None),
            ModuleOutput::default()
        );
        env.commands.insert(
            "playerctl metadata --format '{{status}}|{{artist}}|{{title}}'".into(),
            "Playing|A|B".into(),
        );
        assert_eq!(
            evaluate(&spec, &mut env, &mut ModuleState::Idle, None).text,
            "A - B"
        );
    }

    #[test]
    fn taskbar_lists_windows_and_focuses_them() {
        let mut env = MockEnv::new(0).reply(
            "Windows",
            serde_json::json!({
                "Ok": { "Windows": [
                    { "id": 1, "title": "Terminal", "app_id": "Alacritty", "is_focused": true },
                    { "id": 2, "title": "Browser", "app_id": "firefox", "is_focused": false }
                ]}
            }),
        );
        let spec = ModuleSpec::of(ModuleKind::Taskbar, 1);
        let out = evaluate(&spec, &mut env, &mut ModuleState::Idle, None);
        assert_eq!(out.items.len(), 2);
        assert_eq!(out.items[0].label, "Alacritty: Terminal");
        assert!(out.items[0].active);
        assert_eq!(
            out.items[1].action,
            Some(ActionSpec::Niri(NiriAction::focus_window(2)))
        );
        assert!(!out.items[1].active);
    }

    #[test]
    fn formats_and_truncates_window_titles() {
        let window = Window {
            id: 1,
            title: Some("A very long document title that keeps going".into()),
            app_id: Some("org.example.Notes".into()),
            is_focused: false,
        };
        assert_eq!(
            format_window_title(Some(&window), "{app}: {title}"),
            "org.example.Notes: A very long document title that keeps going"
        );
        assert_eq!(
            format_window_title(
                Some(&Window {
                    id: 2,
                    title: Some("Title".into()),
                    app_id: None,
                    is_focused: false,
                }),
                "{app} - {title}"
            ),
            "Title"
        );
        assert_eq!(format_window_title(None, "{title}"), "");
        let clipped = clip("A very long document title that keeps going", 20);
        assert_eq!(clipped, "A very long docum...");
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("abcdef", 3), "abc");
    }

    #[test]
    fn workspace_chips_show_active_occupied_and_labels() {
        let workspaces = vec![
            Workspace {
                id: 1,
                idx: 1,
                name: None,
                output: Some("DP-1".into()),
                is_active: true,
                is_focused: true,
                is_urgent: false,
                active_window_id: Some(4),
            },
            Workspace {
                id: 2,
                idx: 2,
                name: Some("web".into()),
                output: Some("DP-1".into()),
                is_active: false,
                is_focused: false,
                is_urgent: true,
                active_window_id: None,
            },
            Workspace {
                id: 3,
                idx: 1,
                name: None,
                output: Some("HDMI-A-1".into()),
                is_active: true,
                is_focused: false,
                is_urgent: false,
                active_window_id: None,
            },
        ];
        let output = workspaces_output(&workspaces, Some("DP-1"));
        assert_eq!(output.text, "1 web!");
        assert_eq!(output.items.len(), 2);
        assert!(output.items[0].active);
        assert!(output.items[0].occupied);
        assert!(!output.items[1].active);
        assert_eq!(
            output.items[1].action,
            Some(ActionSpec::Niri(NiriAction::focus_id(2)))
        );

        let all = workspaces_output(&workspaces, None);
        assert_eq!(all.items.len(), 3);
    }

    #[test]
    fn workspace_module_reads_niri_reply_and_requests() {
        let mut env = mock().reply(
            "Workspaces",
            json!({
                "Ok": {
                    "Workspaces": [
                        { "id": 1, "idx": 1, "output": "DP-1", "is_active": true, "active_window_id": 9 },
                        { "id": 2, "idx": 2, "output": "DP-1" }
                    ]
                }
            }),
        );
        let spec = ModuleSpec::of(ModuleKind::Workspaces, 1);
        let output = evaluate(&spec, &mut env, &mut ModuleState::Idle, Some("DP-1"));
        assert_eq!(output.text, "1 2");
        assert_eq!(env.niri_requests(), vec![json!("Workspaces")]);

        env.replies.insert(
            "FocusedWindow".into(),
            json!({ "Ok": { "FocusedWindow": { "id": 2, "title": "Build", "app_id": "Term" } } }),
        );
        let spec = ModuleSpec::of(ModuleKind::WindowTitle, 1);
        let output = evaluate(&spec, &mut env, &mut ModuleState::Idle, None);
        assert_eq!(output.text, "Term - Build");
    }

    #[test]
    fn minimap_height_is_validated() {
        let mut spec = ModuleSpec::of(ModuleKind::Minimap, 1);
        assert!(spec.validate().is_ok());
        spec.height = Some(8);
        assert!(spec.validate().is_err());
        spec.height = Some(96);
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn minimap_uses_the_shared_snapshot() {
        use loom_state::State;
        use niri_ipc::{Window, WindowLayout, Workspace};

        let mut state = State::default();
        state.workspaces.push(Workspace {
            id: 1,
            idx: 1,
            name: Some("dev".into()),
            output: Some("eDP-1".into()),
            is_urgent: false,
            is_active: true,
            is_focused: true,
            active_window_id: Some(7),
            is_hidden: false,
        });
        state.windows.push(Window {
            id: 7,
            title: Some("Editor".into()),
            app_id: Some("code".into()),
            pid: None,
            workspace_id: Some(1),
            is_focused: true,
            is_floating: false,
            is_sticky: false,
            is_urgent: false,
            is_minimized: false,
            is_agent: false,
            content_type: None,
            tearing_allowed: false,
            layout: WindowLayout {
                pos_in_scrolling_layout: Some((1, 1)),
                tile_size: (800., 600.),
                window_size: (800, 600),
                tile_pos_in_workspace_view: None,
                window_offset_in_tile: (0., 0.),
            },
            focus_timestamp: None,
        });

        let mut env = mock();
        let spec = ModuleSpec::of(ModuleKind::Minimap, 1);
        let output = evaluate_with_state(
            &spec,
            &mut env,
            &mut ModuleState::Idle,
            Some("eDP-1"),
            Some(&state),
        );
        let minimap = output.minimap.expect("minimap output");
        assert_eq!(minimap.windows.len(), 1);
        assert_eq!(minimap.windows[0].window_id, 7);
        assert!(minimap.windows[0].width > 0);
    }
    #[test]
    fn custom_module_runs_its_command() {
        let mut env = mock().command("printf hi", "hi");
        let spec = ModuleSpec::custom("printf hi", 60);
        assert_eq!(
            evaluate(&spec, &mut env, &mut ModuleState::Idle, None).text,
            "hi"
        );
        assert!(ModuleSpec::custom("", 1).validate().is_err());
        assert!(ModuleSpec::of(ModuleKind::Clock, 1).validate().is_ok());
    }
}
