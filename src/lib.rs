//! `loom-sidepanels`: persistent layer-shell side panels for niri.
//!
//! The library holds everything that can be tested without a compositor:
//! configuration, module refresh, layout/hit testing and the cairo software
//! renderer. `main.rs` is the thin Wayland layer-shell daemon on top.
//!
//! Panels are a list of rows. Vertical panels stack rows top to bottom;
//! horizontal panels (top/bottom bars) lay rows out left to right. A row has a
//! label, an optional icon, an optional module (a built-in value or a shell
//! command), and optional click/scroll actions.

pub mod action;
pub mod behavior;
pub mod modules;
pub mod niri;
pub mod session;
pub mod system;
pub mod tray;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

pub use action::ActionSpec;
use behavior::{AppRule, Context as BehaviorContext};
pub use behavior::{PeekState, ReloadSignal};
use cairo::{Context, FontSlant, FontWeight, Format, ImageSurface};
pub use modules::{
    MinimapOutput, ModuleKind, ModuleOutput, ModuleSpec, ModuleState, OutputItem, Pixmap,
};
pub use niri::NiriAction;
use serde::{Deserialize, Serialize};
pub use session::SessionState;
pub use system::{Env, RealEnv};

pub const HEADER_HEIGHT: u32 = 46;
pub const ROW_GAP: u32 = 4;
pub const CONFIG_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    /// True for left/right panels, where rows stack vertically.
    pub fn is_vertical(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }

    pub fn is_horizontal(self) -> bool {
        !self.is_vertical()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Top => "top",
            Self::Bottom => "bottom",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Parses `#rgb`, `#rrggbb` or `#rrggbbaa`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let hex = text.trim().trim_start_matches('#');
        let digits: Vec<u8> = hex
            .chars()
            .map(|ch| ch.to_digit(16).map(|digit| digit as u8))
            .collect::<Option<Vec<u8>>>()
            .ok_or_else(|| format!("`{text}` is not a hex colour"))?;
        match digits.len() {
            3 => Ok(Self {
                r: digits[0] * 17,
                g: digits[1] * 17,
                b: digits[2] * 17,
                a: 255,
            }),
            6 | 8 => {
                let byte = |index: usize| digits[index] * 16 + digits[index + 1];
                Ok(Self {
                    r: byte(0),
                    g: byte(2),
                    b: byte(4),
                    a: if digits.len() == 8 { byte(6) } else { 255 },
                })
            }
            _ => Err(format!("`{text}` must be #rgb, #rrggbb or #rrggbbaa")),
        }
    }

    fn cairo(self) -> (f64, f64, f64, f64) {
        (
            self.r as f64 / 255.,
            self.g as f64 / 255.,
            self.b as f64 / 255.,
            self.a as f64 / 255.,
        )
    }

    fn with_alpha(self, alpha: f64) -> (f64, f64, f64, f64) {
        let (r, g, b, a) = self.cairo();
        (r, g, b, a * alpha)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Theme {
    #[serde(default = "default_bg")]
    pub bg: String,
    #[serde(default = "default_fg")]
    pub fg: String,
    #[serde(default = "default_dim")]
    pub dim: String,
    #[serde(default = "default_accent")]
    pub accent: String,
    #[serde(default = "default_row")]
    pub row: String,
    /// Font family used for labels and module values.
    #[serde(default = "default_font")]
    pub font: String,
    /// Multiplier applied to the theme alpha, 0.0 (invisible) to 1.0 (opaque).
    #[serde(default = "default_opacity")]
    pub opacity: f64,
}

fn default_bg() -> String {
    "#14161b".to_owned()
}
fn default_fg() -> String {
    "#e6e6e6".to_owned()
}
fn default_dim() -> String {
    "#9aa0a6".to_owned()
}
fn default_accent() -> String {
    "#4f9cf9".to_owned()
}
fn default_row() -> String {
    "#1d2027".to_owned()
}
fn default_font() -> String {
    "Sans".to_owned()
}
fn default_opacity() -> f64 {
    1.0
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            bg: default_bg(),
            fg: default_fg(),
            dim: default_dim(),
            accent: default_accent(),
            row: default_row(),
            font: default_font(),
            opacity: default_opacity(),
        }
    }
}

impl Theme {
    pub fn bg(&self) -> Result<Rgba, String> {
        Rgba::parse(&self.bg)
    }
    pub fn fg(&self) -> Result<Rgba, String> {
        Rgba::parse(&self.fg)
    }
    pub fn dim(&self) -> Result<Rgba, String> {
        Rgba::parse(&self.dim)
    }
    pub fn accent(&self) -> Result<Rgba, String> {
        Rgba::parse(&self.accent)
    }
    pub fn row(&self) -> Result<Rgba, String> {
        Rgba::parse(&self.row)
    }

    fn colors(&self) -> Result<Colors, String> {
        Ok(Colors {
            bg: self.bg()?,
            fg: self.fg()?,
            dim: self.dim()?,
            accent: self.accent()?,
            row: self.row()?,
            opacity: self.opacity,
        })
    }

    fn validate(&self, errors: &mut Vec<String>) {
        for (name, value) in [
            ("bg", &self.bg),
            ("fg", &self.fg),
            ("dim", &self.dim),
            ("accent", &self.accent),
            ("row", &self.row),
        ] {
            if let Err(err) = Rgba::parse(value) {
                errors.push(format!("theme `{name}`: {err}"));
            }
        }
        if self.font.trim().is_empty() {
            errors.push("theme `font` must not be empty".to_owned());
        }
        if !(0.0..=1.0).contains(&self.opacity) || !self.opacity.is_finite() {
            errors.push("theme `opacity` must be between 0.0 and 1.0".to_owned());
        }
    }
}

struct Colors {
    bg: Rgba,
    fg: Rgba,
    dim: Rgba,
    accent: Rgba,
    row: Rgba,
    opacity: f64,
}

/// A row of a panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    #[serde(default)]
    pub label: String,
    /// Optional prefix drawn before the label, e.g. a Nerd Font glyph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// v1 click command, kept for backwards compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<ModuleSpec>,
    /// Click action; overrides `command` when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub click: Option<ActionSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll_up: Option<ActionSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll_down: Option<ActionSpec>,
    /// Draws a divider instead of a label, and is not clickable.
    #[serde(default, skip_serializing_if = "is_false")]
    pub separator: bool,
    /// Shows the row only on these workspace names (globs allowed). Omitted
    /// means every workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<Vec<String>>,
    /// Shows the row only for these focused app ids (globs allowed). Omitted
    /// means every application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<Vec<String>>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Row {
    pub fn label(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            icon: None,
            command: None,
            module: None,
            click: None,
            scroll_up: None,
            scroll_down: None,
            separator: false,
            workspaces: None,
            apps: None,
        }
    }

    pub fn separator() -> Self {
        Self {
            separator: true,
            ..Self::label("")
        }
    }

    pub fn with_icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.command = Some(command.into());
        self
    }

    /// v1 custom command module.
    pub fn with_module(mut self, command: impl Into<String>, interval_secs: u64) -> Self {
        self.module = Some(ModuleSpec::custom(command, interval_secs));
        self
    }

    /// Built-in module.
    pub fn with_kind(mut self, kind: ModuleKind, interval_secs: u64) -> Self {
        self.module = Some(ModuleSpec::of(kind, interval_secs));
        self
    }

    pub fn with_click(mut self, action: ActionSpec) -> Self {
        self.click = Some(action);
        self
    }

    pub fn on_scroll(mut self, up: ActionSpec, down: ActionSpec) -> Self {
        self.scroll_up = Some(up);
        self.scroll_down = Some(down);
        self
    }

    /// Shows this row only on the listed workspaces.
    pub fn on_workspaces<I, S>(mut self, workspaces: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.workspaces = Some(workspaces.into_iter().map(Into::into).collect());
        self
    }

    /// Shows this row only when one of these app ids is focused.
    pub fn for_apps<I, S>(mut self, apps: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.apps = Some(apps.into_iter().map(Into::into).collect());
        self
    }

    fn click_action(&self) -> Option<ActionSpec> {
        self.click
            .clone()
            .or_else(|| self.command.as_ref().map(ActionSpec::command))
    }

    fn validate(&self, errors: &mut Vec<String>) {
        if self.separator {
            return;
        }
        if self.label.trim().is_empty() && self.module.is_none() {
            errors.push(format!(
                "row `{}` has neither a label nor a module",
                self.label
            ));
        }
        if let Some(module) = &self.module
            && let Err(err) = module.validate()
        {
            errors.push(format!("row `{}`: {err}", self.label));
        }
        for (name, action) in [
            ("click", &self.click),
            ("scroll_up", &self.scroll_up),
            ("scroll_down", &self.scroll_down),
        ] {
            if let Some(ActionSpec::Niri(action)) = action
                && let Err(err) = action.request()
            {
                errors.push(format!("row `{}` {name}: {err}", self.label));
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanelConfig {
    /// Output name this panel belongs to; `None` or `"*"` means every output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default)]
    pub side: Side,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_margin")]
    pub margin: u32,
    #[serde(default = "default_padding")]
    pub padding: u32,
    #[serde(default = "default_row_height")]
    pub row_height: u32,
    #[serde(default = "default_title")]
    pub title: String,
    /// Fixed panel height. Vertical panels otherwise span the output;
    /// top/bottom bars derive it from padding and row height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Hide behind the output edge, leaving a thin reveal strip.
    #[serde(default)]
    pub auto_hide: bool,
    /// Width (vertical) or height (horizontal) of the reveal strip.
    #[serde(default = "default_reveal")]
    pub reveal: u32,
    /// Do not reserve space (exclusive zone 0) for tiled windows.
    #[serde(default)]
    pub floating: bool,
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub rows: Vec<Row>,
}

fn default_width() -> u32 {
    260
}
fn default_margin() -> u32 {
    8
}
fn default_padding() -> u32 {
    12
}
fn default_row_height() -> u32 {
    34
}
fn default_title() -> String {
    "LOOM".to_owned()
}
fn default_reveal() -> u32 {
    behavior::DEFAULT_REVEAL
}
pub fn default_interval() -> u64 {
    30
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            output: None,
            side: Side::Left,
            width: default_width(),
            margin: default_margin(),
            padding: default_padding(),
            row_height: default_row_height(),
            title: default_title(),
            height: None,
            auto_hide: false,
            reveal: default_reveal(),
            floating: false,
            theme: Theme::default(),
            rows: vec![
                Row::label("Workspaces").with_command("niri msg workspaces"),
                Row::label("Clock").with_module("date +%H:%M", 10),
            ],
        }
    }
}

impl PanelConfig {
    /// Loads a single panel (version 1 file shape).
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|err| format!("cannot parse {}: {err}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(format!("cannot read {}: {err}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|err| format!("cannot serialize config: {err}"))?;
        write_config_file(path, &bytes)
    }

    /// Space reserved on the output edge; used as the exclusive zone.
    pub fn exclusive_zone(&self) -> u32 {
        self.exclusive_zone_with(false)
    }

    /// Exclusive zone for a panel that may be hidden or floating.
    pub fn exclusive_zone_with(&self, hidden: bool) -> u32 {
        if self.floating || (self.auto_hide && hidden) {
            return 0;
        }
        match self.side {
            Side::Left | Side::Right => self.width + self.margin,
            Side::Top | Side::Bottom => self.bar_height() + self.margin,
        }
    }

    /// Height of a horizontal (top/bottom) panel, or the fixed height of a
    /// vertical panel.
    pub fn bar_height(&self) -> u32 {
        let rows = self
            .rows
            .iter()
            .map(|row| {
                row.module
                    .as_ref()
                    .and_then(|module| module.height)
                    .unwrap_or(self.row_height)
            })
            .max()
            .unwrap_or(self.row_height);
        self.height.unwrap_or(self.padding * 2 + rows)
    }

    /// Full (revealed) size of the panel along its stacking axis.
    pub fn panel_extent(&self) -> u32 {
        match self.side {
            Side::Left | Side::Right => self.width,
            Side::Top | Side::Bottom => self.bar_height(),
        }
    }

    pub fn matches_output(&self, output: &str) -> bool {
        match self.output.as_deref() {
            None | Some("*") | Some("") => true,
            Some(name) => name == output,
        }
    }

    /// Validates colours, sizes, modules and actions. All problems are returned.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        if let Some(output) = &self.output
            && output.trim().is_empty()
            && output != "*"
        {
            errors.push(
                "panel `output` must not be blank; omit it or use `*` for every output".to_owned(),
            );
        }
        if self.width < 80 {
            errors.push("panel `width` must be at least 80".to_owned());
        }
        if self.padding.saturating_mul(2) >= self.width {
            errors.push("panel `padding` leaves no row width".to_owned());
        }
        if self.row_height < 16 {
            errors.push("panel `row_height` must be at least 16".to_owned());
        }
        if self.reveal == 0 {
            errors.push("panel `reveal` must be at least 1".to_owned());
        }
        if let Some(height) = self.height
            && height < 16
        {
            errors.push("panel `height` must be at least 16".to_owned());
        }
        if self.auto_hide && !self.floating && self.reveal >= self.panel_extent() {
            errors.push("panel `reveal` must be smaller than the panel".to_owned());
        }
        self.theme.validate(&mut errors);
        if self.rows.is_empty() {
            errors.push("panel has no rows".to_owned());
        }
        for row in &self.rows {
            row.validate(&mut errors);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

fn write_config_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    }
    std::fs::write(path, bytes).map_err(|err| format!("cannot write {}: {err}", path.display()))
}

/// Partial per-output theme overrides.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeOverrides {
    pub bg: Option<String>,
    pub fg: Option<String>,
    pub dim: Option<String>,
    pub accent: Option<String>,
    pub row: Option<String>,
    pub font: Option<String>,
    pub opacity: Option<f64>,
}

impl ThemeOverrides {
    fn apply(&self, theme: &mut Theme) {
        if let Some(bg) = &self.bg {
            theme.bg = bg.clone();
        }
        if let Some(fg) = &self.fg {
            theme.fg = fg.clone();
        }
        if let Some(dim) = &self.dim {
            theme.dim = dim.clone();
        }
        if let Some(accent) = &self.accent {
            theme.accent = accent.clone();
        }
        if let Some(row) = &self.row {
            theme.row = row.clone();
        }
        if let Some(font) = &self.font {
            theme.font = font.clone();
        }
        if let Some(opacity) = self.opacity {
            theme.opacity = opacity;
        }
    }
}

/// Per-output panel overrides, keyed by the output name from niri/Wayland.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PanelOverrides {
    pub side: Option<Side>,
    pub width: Option<u32>,
    pub margin: Option<u32>,
    pub padding: Option<u32>,
    pub row_height: Option<u32>,
    pub title: Option<String>,
    pub theme: Option<ThemeOverrides>,
}

impl PanelOverrides {
    pub fn apply(&self, panel: &mut PanelConfig) {
        if let Some(side) = self.side {
            panel.side = side;
        }
        if let Some(width) = self.width {
            panel.width = width;
        }
        if let Some(margin) = self.margin {
            panel.margin = margin;
        }
        if let Some(padding) = self.padding {
            panel.padding = padding;
        }
        if let Some(row_height) = self.row_height {
            panel.row_height = row_height;
        }
        if let Some(title) = &self.title {
            panel.title = title.clone();
        }
        if let Some(theme) = &self.theme {
            theme.apply(&mut panel.theme);
        }
    }
}

/// Versioned config file.
///
/// A v1 file (a bare panel object with `rows`) is still accepted and wrapped in
/// a single panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    /// Per-output overrides keyed by output connector name, e.g. `DP-1`.
    #[serde(default)]
    pub outputs: BTreeMap<String, PanelOverrides>,
    /// Panels of the unnamed/default layout.
    #[serde(default)]
    pub panels: Vec<PanelConfig>,
    /// Named panel layouts ("profiles"), switchable at runtime. `layouts` is
    /// accepted as an alias so either term can be used in a config file.
    #[serde(default, alias = "layouts")]
    pub profiles: BTreeMap<String, Vec<PanelConfig>>,
    /// Named profile used when the config does not say otherwise; session state
    /// (and `--layout`) can override it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<String>,
    /// Context-aware show/hide/reorder rules for rows.
    #[serde(default)]
    pub rules: Vec<AppRule>,
}

fn default_version() -> u32 {
    CONFIG_VERSION
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            outputs: BTreeMap::new(),
            panels: vec![PanelConfig {
                title: "LOOM".to_owned(),
                rows: vec![
                    Row::label("Workspaces").with_kind(ModuleKind::Workspaces, 1),
                    Row::label("Window").with_kind(ModuleKind::WindowTitle, 1),
                    Row::separator(),
                    Row::label("Media").with_kind(ModuleKind::Media, 2),
                    Row::separator(),
                    Row::label("CPU").with_kind(ModuleKind::Cpu, 2),
                    Row::label("Memory").with_kind(ModuleKind::Memory, 5),
                    Row::label("Battery").with_kind(ModuleKind::Battery, 30),
                    Row::label("Network").with_kind(ModuleKind::Network, 2),
                    Row::label("Volume").with_kind(ModuleKind::Volume, 5),
                    Row::separator(),
                    Row::label("Clock").with_kind(ModuleKind::Clock, 1),
                ],
                ..PanelConfig::default()
            }],
            profiles: BTreeMap::new(),
            active_profile: None,
            rules: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(format!("cannot read {}: {err}", path.display())),
        };
        Self::from_slice(&bytes).map_err(|err| format!("cannot parse {}: {err}", path.display()))
    }

    /// Parses v2 (`panels`) or v1 (bare panel) JSON.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|err| err.to_string())?;
        if value.get("panels").is_some()
            || value.get("profiles").is_some()
            || value.get("layouts").is_some()
            || value.get("version").is_some()
            || value.get("outputs").is_some()
            || value.get("rules").is_some()
            || value.get("active_profile").is_some()
        {
            return serde_json::from_value(value).map_err(|err| err.to_string());
        }
        let panel: PanelConfig = serde_json::from_value(value).map_err(|err| err.to_string())?;
        Ok(Self {
            version: 1,
            outputs: BTreeMap::new(),
            panels: vec![panel],
            profiles: BTreeMap::new(),
            active_profile: None,
            rules: Vec::new(),
        })
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|err| format!("cannot serialize config: {err}"))?;
        write_config_file(path, &bytes)
    }

    /// Panels of the selected profile, or the default `panels` list.
    pub fn base_panels(&self) -> &[PanelConfig] {
        self.active_profile
            .as_deref()
            .and_then(|name| self.profiles.get(name))
            .map(Vec::as_slice)
            .unwrap_or(&self.panels)
    }

    /// Panels for one output, with per-output overrides applied.
    pub fn panels_for(&self, output: &str) -> Vec<PanelConfig> {
        let overrides = self.outputs.get(output);
        self.base_panels()
            .iter()
            .filter(|panel| panel.matches_output(output))
            .map(|panel| {
                let mut panel = panel.clone();
                if let Some(overrides) = overrides {
                    overrides.apply(&mut panel);
                }
                panel
            })
            .collect()
    }

    /// Names of every named layout, sorted.
    pub fn profile_names(&self) -> Vec<String> {
        self.profiles.keys().cloned().collect()
    }

    /// Switches to a named profile. Returns false when the name is unknown.
    pub fn select_profile(&mut self, name: &str) -> bool {
        if self.profiles.contains_key(name) {
            self.active_profile = Some(name.to_owned());
            true
        } else {
            false
        }
    }

    /// Applies `--layout`/session profile selection, ignoring unknown names.
    pub fn with_profile(mut self, name: Option<&str>) -> Self {
        if let Some(name) = name
            && self.profiles.contains_key(name)
        {
            self.active_profile = Some(name.to_owned());
        }
        self
    }

    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        if self.version == 0 || self.version > CONFIG_VERSION {
            errors.push(format!(
                "config `version` must be between 1 and {CONFIG_VERSION}, got {}",
                self.version
            ));
        }
        if let Some(active) = self.active_profile.as_deref()
            && !active.trim().is_empty()
            && !self.profiles.contains_key(active)
        {
            errors.push(format!("config `active_profile` `{active}` is not defined"));
        }
        for (name, overrides) in &self.outputs {
            if name.trim().is_empty() {
                errors.push("output override name must not be empty".to_owned());
            }
            for (index, panel) in self.base_panels().iter().enumerate() {
                let mut panel = panel.clone();
                overrides.apply(&mut panel);
                if let Err(problems) = panel.validate() {
                    for problem in problems {
                        errors.push(format!("panel {index} on output `{name}`: {problem}"));
                    }
                }
            }
        }
        if self.base_panels().is_empty() {
            errors.push("config defines no panels".to_owned());
        }
        for (index, panel) in self.base_panels().iter().enumerate() {
            if let Err(problems) = panel.validate() {
                for problem in problems {
                    errors.push(format!("panel {index}: {problem}"));
                }
            }
        }
        for (name, panels) in &self.profiles {
            if name.trim().is_empty() {
                errors.push("profile name must not be empty".to_owned());
            }
            if panels.is_empty() {
                errors.push(format!("profile `{name}` defines no panels"));
            }
            for (index, panel) in panels.iter().enumerate() {
                if let Err(problems) = panel.validate() {
                    for problem in problems {
                        errors.push(format!("profile `{name}` panel {index}: {problem}"));
                    }
                }
            }
        }
        for rule in &self.rules {
            if rule.app.trim().is_empty()
                && rule
                    .workspace
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(str::is_empty)
                && rule.show.is_empty()
                && rule.hide.is_empty()
                && rule.order.is_empty()
            {
                errors.push("rule has no app, workspace, show, hide or order".to_owned());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_default();
    base.join("loom-sidepanels/config.json")
}

type FileStamp = (Option<SystemTime>, u64);

fn file_stamp(path: &Path) -> FileStamp {
    match std::fs::metadata(path) {
        Ok(metadata) => (metadata.modified().ok(), metadata.len()),
        Err(_) => (None, 0),
    }
}

/// Polls the config file's mtime and size; used to reload without restarting.
#[derive(Debug, Clone)]
pub struct ConfigWatcher {
    path: PathBuf,
    stamp: FileStamp,
}

impl ConfigWatcher {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let stamp = file_stamp(&path);
        Self { path, stamp }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns true once when the file changed since the last poll.
    pub fn poll(&mut self) -> bool {
        let stamp = file_stamp(&self.path);
        if stamp == self.stamp {
            return false;
        }
        self.stamp = stamp;
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64
            && x < (self.x + self.width) as f64
            && y >= self.y as f64
            && y < (self.y + self.height) as f64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
}

/// A clickable region: a whole row, or a module item inside one.
#[derive(Debug, Clone, PartialEq)]
pub struct HitRegion {
    pub row: usize,
    pub rect: Rect,
    pub action: Option<ActionSpec>,
}

/// Clickable geometry of a rendered panel.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InteractionMap {
    pub regions: Vec<HitRegion>,
}

impl InteractionMap {
    /// Topmost region at a point (module items win over their row).
    pub fn at(&self, x: f64, y: f64) -> Option<&HitRegion> {
        self.regions
            .iter()
            .rev()
            .find(|region| region.rect.contains(x, y))
    }

    pub fn row_at(&self, x: f64, y: f64) -> Option<usize> {
        self.at(x, y).map(|region| region.row)
    }
}

/// Reads the focused app id and workspace name from niri for context rules.
fn context_from_env(env: &mut dyn Env) -> BehaviorContext {
    let workspace = env
        .niri(&serde_json::Value::String("Workspaces".to_owned()))
        .as_ref()
        .map(niri::parse_workspaces)
        .and_then(|workspaces| {
            workspaces
                .into_iter()
                .find(|workspace| workspace.is_focused)
                .and_then(|workspace| workspace.name)
        });
    let app = env
        .niri(&serde_json::Value::String("FocusedWindow".to_owned()))
        .as_ref()
        .and_then(niri::parse_focused_window)
        .and_then(|window| window.app_id);
    BehaviorContext { app, workspace }
}

/// Runtime panel state: config plus refreshed module output.
#[derive(Debug, Clone)]
pub struct Panel {
    pub config: PanelConfig,
    output: Option<String>,
    outputs: Vec<String>,
    items: Vec<Vec<OutputItem>>,
    minimaps: Vec<Option<MinimapOutput>>,
    updated: Vec<Option<Instant>>,
    states: Vec<ModuleState>,
    /// Rows as authored; `config.rows` is the context-filtered view.
    base_rows: Vec<Row>,
    rules: Vec<AppRule>,
    context: BehaviorContext,
}

impl Panel {
    pub fn new(config: PanelConfig) -> Self {
        let len = config.rows.len();
        Self {
            base_rows: config.rows.clone(),
            config,
            output: None,
            outputs: vec![String::new(); len],
            items: vec![Vec::new(); len],
            minimaps: vec![None; len],
            updated: vec![None; len],
            states: vec![ModuleState::Idle; len],
            rules: Vec::new(),
            context: BehaviorContext::default(),
        }
    }

    /// Attaches context-aware show/hide/reorder rules.
    pub fn with_rules(mut self, rules: Vec<AppRule>) -> Self {
        self.rules = rules;
        self
    }

    pub fn rules(&self) -> &[AppRule] {
        &self.rules
    }

    /// Replaces the rules and filters the current rows. Returns true when the
    /// visible row set changed.
    pub fn set_rules(&mut self, rules: Vec<AppRule>) -> bool {
        self.rules = rules;
        let rows = self.rows_for_context(&self.context.clone());
        if rows == self.config.rows {
            return false;
        }
        self.reset_rows(rows);
        true
    }

    pub fn context(&self) -> &BehaviorContext {
        &self.context
    }

    /// Panel bound to the output it is displayed on (workspace filtering).
    pub fn with_output(mut self, output: impl Into<String>) -> Self {
        self.output = Some(output.into());
        self
    }

    pub fn output_name(&self) -> Option<&str> {
        self.output.as_deref()
    }

    /// Replaces the configuration, resetting module output and deltas.
    pub fn set_config(&mut self, config: PanelConfig) {
        self.base_rows = config.rows.clone();
        self.set_config_keep_base(config);
        let rows = self.rows_for_context(&self.context.clone());
        if rows != self.config.rows {
            self.reset_rows(rows);
        }
    }

    fn set_config_keep_base(&mut self, config: PanelConfig) {
        self.reset_rows(config.rows.clone());
        self.config = config;
    }

    fn reset_rows(&mut self, rows: Vec<Row>) {
        let len = rows.len();
        self.config.rows = rows;
        self.outputs = vec![String::new(); len];
        self.items = vec![Vec::new(); len];
        self.updated = vec![None; len];
        self.states = vec![ModuleState::Idle; len];
    }

    /// Rows for a context: authored rows filtered by `rules` and by each row's
    /// `workspaces`/`apps` visibility lists.
    pub fn rows_for_context(&self, context: &BehaviorContext) -> Vec<Row> {
        let filtered = behavior::filter_rows(&self.base_rows, context, &self.rules);
        filtered
            .into_iter()
            .filter(|row| behavior::row_visible_on_workspace(row, context.workspace.as_deref()))
            .filter(|row| behavior::row_visible_for_app(row, context.app.as_deref()))
            .collect()
    }

    /// Applies a focused app/workspace context. Returns true when the visible
    /// row set changed (so the caller redraws).
    pub fn apply_context(&mut self, context: BehaviorContext) -> bool {
        if self.context == context {
            return false;
        }
        self.context = context;
        let rows = self.rows_for_context(&self.context.clone());
        if rows == self.config.rows {
            return false;
        }
        self.reset_rows(rows);
        true
    }

    /// Reads the focused window/workspace from niri when rules or per-row
    /// visibility lists need it.
    fn refresh_context(&mut self, env: &mut dyn Env) -> bool {
        let needs_context = !self.rules.is_empty()
            || self
                .base_rows
                .iter()
                .any(|row| row.workspaces.is_some() || row.apps.is_some());
        if !needs_context {
            return false;
        }
        let context = context_from_env(env);
        self.apply_context(context)
    }

    pub fn output(&self, index: usize) -> Option<&str> {
        self.outputs.get(index).map(String::as_str)
    }

    pub fn output_items(&self, index: usize) -> &[OutputItem] {
        self.items.get(index).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Runs every module whose interval elapsed. Returns true if anything
    /// changed (so the caller can redraw).
    pub fn refresh(&mut self) -> bool {
        self.refresh_at(Instant::now())
    }

    pub fn refresh_at(&mut self, now: Instant) -> bool {
        self.refresh_with(&mut RealEnv::new(), now)
    }

    pub fn refresh_at_with_state(
        &mut self,
        now: Instant,
        snapshot: Option<&loom_state::State>,
    ) -> bool {
        self.refresh_with_state(&mut RealEnv::new(), now, snapshot)
    }

    pub fn refresh_with(&mut self, env: &mut dyn Env, now: Instant) -> bool {
        self.refresh_with_state(env, now, None)
    }

    pub fn refresh_with_state(
        &mut self,
        env: &mut dyn Env,
        now: Instant,
        snapshot: Option<&loom_state::State>,
    ) -> bool {
        let mut changed = self.refresh_context(env);
        for (index, row) in self.config.rows.iter().enumerate() {
            let Some(module) = &row.module else {
                continue;
            };
            let due = match self.updated[index] {
                None => true,
                Some(last) => now.duration_since(last) >= module.interval(),
            };
            if !due {
                continue;
            }
            let output = modules::evaluate_with_state(
                module,
                env,
                &mut self.states[index],
                self.output.as_deref(),
                snapshot,
            );
            self.updated[index] = Some(now);
            if self.outputs[index] != output.text {
                self.outputs[index] = output.text;
                changed = true;
            }
            if self.items[index] != output.items {
                self.items[index] = output.items;
                changed = true;
            }
            if self.minimaps[index] != output.minimap {
                self.minimaps[index] = output.minimap;
                changed = true;
            }
        }
        changed
    }

    /// Runs every module immediately (startup and config reload).
    pub fn refresh_all(&mut self) -> bool {
        self.refresh_all_with(&mut RealEnv::new())
    }

    pub fn refresh_all_with(&mut self, env: &mut dyn Env) -> bool {
        self.refresh_all_with_state(env, None)
    }

    pub fn refresh_all_with_state(
        &mut self,
        env: &mut dyn Env,
        snapshot: Option<&loom_state::State>,
    ) -> bool {
        self.updated.fill(None);
        self.refresh_with_state(env, Instant::now(), snapshot)
    }

    /// Row rectangles in panel-local coordinates.
    pub fn row_rects(&self) -> Vec<Rect> {
        self.row_rects_for(self.config.width)
    }

    /// Row rectangles for a concrete surface width.
    pub fn row_rects_for(&self, width: u32) -> Vec<Rect> {
        let count = self.config.rows.len() as u32;
        if count == 0 {
            return Vec::new();
        }
        let inset = self.config.padding;
        let heights: Vec<u32> = (0..count as usize)
            .map(|index| {
                self.config.rows[index]
                    .module
                    .as_ref()
                    .and_then(|module| module.height)
                    .unwrap_or(self.config.row_height)
            })
            .collect();
        if self.config.side.is_vertical() {
            let row_width = self.config.width.saturating_sub(inset * 2);
            let mut y = HEADER_HEIGHT;
            return heights
                .into_iter()
                .map(|height| {
                    let rect = Rect {
                        x: inset,
                        y,
                        width: row_width,
                        height,
                    };
                    y += height + ROW_GAP;
                    rect
                })
                .collect();
        }
        let available = width.saturating_sub(inset * 2);
        let cell = available / count;
        let height = heights.into_iter().max().unwrap_or(self.config.row_height);
        (0..count)
            .map(|index| {
                let x = inset + index * cell;
                let cell_width = if index + 1 == count {
                    available.saturating_sub(index * cell)
                } else {
                    cell
                };
                Rect {
                    x,
                    y: inset,
                    width: cell_width,
                    height,
                }
            })
            .collect()
    }

    pub fn hit(&self, x: f64, y: f64) -> Option<usize> {
        self.row_rects().iter().position(|rect| rect.contains(x, y))
    }

    /// Executes a row's click action, if it has one.
    pub fn activate(&self, index: usize) -> Result<Option<String>, String> {
        self.activate_with(index, &mut RealEnv::new())
    }

    pub fn activate_with(&self, index: usize, env: &mut dyn Env) -> Result<Option<String>, String> {
        let row = self
            .config
            .rows
            .get(index)
            .ok_or_else(|| format!("row {index} does not exist"))?;
        match row.click_action() {
            Some(action) => {
                action.run_with(env)?;
                Ok(Some(action.describe()))
            }
            None => Ok(None),
        }
    }

    /// Runs an arbitrary action (used for module item clicks).
    pub fn run_action(&self, action: &ActionSpec) -> Result<(), String> {
        action.run_with(&mut RealEnv::new())
    }

    /// Explicit scroll action, or workspace cycling for workspace modules.
    pub fn scroll_action(&self, index: usize, direction: ScrollDirection) -> Option<ActionSpec> {
        let row = self.config.rows.get(index)?;
        let explicit = match direction {
            ScrollDirection::Up => row.scroll_up.clone(),
            ScrollDirection::Down => row.scroll_down.clone(),
        };
        explicit.or_else(|| {
            let workspaces = row
                .module
                .as_ref()
                .is_some_and(|module| module.kind == ModuleKind::Workspaces);
            workspaces.then(|| {
                ActionSpec::Niri(match direction {
                    ScrollDirection::Up => NiriAction::up(),
                    ScrollDirection::Down => NiriAction::down(),
                })
            })
        })
    }

    /// Clickable geometry for the current output and config.
    pub fn interaction_map(&self, width: u32, height: u32) -> Result<InteractionMap, String> {
        if width == 0 || height == 0 {
            return Ok(InteractionMap::default());
        }
        let surface = ImageSurface::create(Format::ARgb32, 1, 1)
            .map_err(|err| format!("cannot create surface: {err}"))?;
        let ctx = Context::new(&surface).map_err(|err| format!("cairo: {err}"))?;
        let parts = self.compute_parts(&ctx, width)?;
        let mut regions = Vec::new();
        for part in parts {
            if part.separator {
                continue;
            }
            let row = &self.config.rows[part.row];
            if let Some(minimap) = &self.minimaps[part.row] {
                for window in &minimap.windows {
                    regions.push(HitRegion {
                        row: part.row,
                        rect: Self::minimap_rect(part.rect, window),
                        action: Some(ActionSpec::Niri(NiriAction::focus_window(window.window_id))),
                    });
                }
                continue;
            }
            regions.push(HitRegion {
                row: part.row,
                rect: part.rect,
                action: row.click_action(),
            });
            for item in part.items {
                let Some(action) = item.action else {
                    continue;
                };
                regions.push(HitRegion {
                    row: part.row,
                    rect: Rect {
                        x: item.x.max(0.) as u32,
                        y: part.rect.y,
                        width: item.width.ceil().max(1.) as u32,
                        height: part.rect.height,
                    },
                    action: Some(action),
                });
            }
        }
        Ok(InteractionMap { regions })
    }

    /// Renders a thin reveal strip shown while an auto-hidden panel is hidden.
    pub fn render_strip(&self, width: u32, height: u32) -> Result<PanelImage, String> {
        if width == 0 || height == 0 {
            return Err("cannot render an empty strip".to_owned());
        }
        let colors = self.config.theme.colors()?;
        let stride = width as i32 * 4;
        let surface = ImageSurface::create_for_data(
            vec![0u8; (width * height * 4) as usize],
            Format::ARgb32,
            width as i32,
            height as i32,
            stride,
        )
        .map_err(|err| format!("cannot create surface: {err}"))?;
        {
            let ctx = Context::new(&surface).map_err(|err| format!("cairo: {err}"))?;
            let (r, g, b, a) = colors.accent.with_alpha(colors.opacity.max(0.5));
            ctx.set_source_rgba(r, g, b, a);
            ctx.paint().map_err(|err| format!("cairo: {err}"))?;
        }
        let data = surface
            .take_data()
            .map_err(|_| "cannot take surface data".to_owned())?;
        Ok(PanelImage {
            width,
            height,
            stride: stride as usize,
            data: data.as_ref().to_vec(),
        })
    }

    /// Renders the panel into an ARGB32 buffer.
    pub fn render(&self, width: u32, height: u32) -> Result<PanelImage, String> {
        self.render_with(width, height, None)
    }

    pub fn render_with(
        &self,
        width: u32,
        height: u32,
        hovered: Option<usize>,
    ) -> Result<PanelImage, String> {
        if width == 0 || height == 0 {
            return Err("cannot render an empty panel".to_owned());
        }
        let colors = self.config.theme.colors()?;
        let stride = width as i32 * 4;
        let surface = ImageSurface::create_for_data(
            vec![0u8; (width * height * 4) as usize],
            Format::ARgb32,
            width as i32,
            height as i32,
            stride,
        )
        .map_err(|err| format!("cannot create surface: {err}"))?;

        {
            let ctx = Context::new(&surface).map_err(|err| format!("cairo: {err}"))?;
            let (r, g, b, a) = colors.bg.with_alpha(colors.opacity);
            ctx.set_source_rgba(r, g, b, a);
            ctx.paint().map_err(|err| format!("cairo: {err}"))?;

            let padding = self.config.padding as f64;
            if self.config.side.is_vertical() {
                // Header title.
                ctx.set_source_rgba_tuple(colors.fg.cairo());
                ctx.select_font_face(&self.config.theme.font, FontSlant::Normal, FontWeight::Bold);
                ctx.set_font_size(15.);
                ctx.move_to(padding, padding + 16.);
                ctx.show_text(&self.config.title)
                    .map_err(|err| format!("cairo: {err}"))?;

                // Accent line under the header.
                ctx.set_source_rgba_tuple(colors.accent.cairo());
                ctx.rectangle(0., HEADER_HEIGHT as f64 - 1., width as f64, 1.);
                ctx.fill().map_err(|err| format!("cairo: {err}"))?;
            }

            let parts = self.compute_parts(&ctx, width)?;
            for part in &parts {
                if part.separator {
                    let y = part.rect.y as f64 + part.rect.height as f64 / 2.;
                    ctx.set_source_rgba_tuple(colors.dim.with_alpha(0.4));
                    ctx.rectangle(part.rect.x as f64 + 8., y, part.rect.width as f64 - 16., 1.);
                    ctx.fill().map_err(|err| format!("cairo: {err}"))?;
                    continue;
                }
                self.paint_row_background(&ctx, &part.rect, hovered == Some(part.row), &colors)?;
                if let Some(minimap) = &self.minimaps[part.row] {
                    Self::paint_minimap(&ctx, part.rect, minimap, &colors)?;
                    continue;
                }
                if let Some(label) = &part.label {
                    ctx.set_source_rgba_tuple(colors.fg.cairo());
                    ctx.select_font_face(
                        &self.config.theme.font,
                        FontSlant::Normal,
                        FontWeight::Normal,
                    );
                    ctx.set_font_size(label.size);
                    ctx.move_to(label.x, label.baseline);
                    ctx.show_text(&label.text)
                        .map_err(|err| format!("cairo: {err}"))?;
                }
                for item in &part.items {
                    if let Some(pixmap) = &item.pixmap {
                        let size = item.width.max(1.);
                        let top = part.rect.y as f64 + (part.rect.height as f64 - size) / 2.;
                        let stride = pixmap.width as i32 * 4;
                        if let Ok(surface) = ImageSurface::create_for_data(
                            pixmap.data.clone(),
                            Format::ARgb32,
                            pixmap.width as i32,
                            pixmap.height as i32,
                            stride,
                        ) {
                            ctx.save().map_err(|err| format!("cairo: {err}"))?;
                            ctx.translate(item.x, top);
                            ctx.scale(size / pixmap.width as f64, size / pixmap.height as f64);
                            ctx.set_source_surface(&surface, 0., 0.)
                                .map_err(|err| format!("cairo: {err}"))?;
                            ctx.paint().map_err(|err| format!("cairo: {err}"))?;
                            ctx.restore().map_err(|err| format!("cairo: {err}"))?;
                        }
                        continue;
                    }
                    let color = if item.active {
                        colors.accent
                    } else if item.dim {
                        colors.dim
                    } else if item.occupied {
                        colors.fg
                    } else {
                        colors.dim
                    };
                    ctx.set_source_rgba_tuple(color.cairo());
                    ctx.select_font_face(
                        &self.config.theme.font,
                        FontSlant::Normal,
                        FontWeight::Normal,
                    );
                    ctx.set_font_size(item.size);
                    ctx.move_to(item.x, item.baseline);
                    ctx.show_text(&item.text)
                        .map_err(|err| format!("cairo: {err}"))?;
                }
            }

            if self.config.side.is_horizontal() {
                let y = match self.config.side {
                    Side::Top => 0.,
                    _ => height as f64 - 3.,
                };
                ctx.set_source_rgba_tuple(colors.accent.cairo());
                ctx.rectangle(0., y, width as f64, 3.);
                ctx.fill().map_err(|err| format!("cairo: {err}"))?;
            }
        }

        let data = surface
            .take_data()
            .map_err(|_| "cannot take surface data".to_owned())?;
        Ok(PanelImage {
            width,
            height,
            stride: stride as usize,
            data: data.as_ref().to_vec(),
        })
    }

    fn minimap_rect(part: Rect, window: &crate::modules::MinimapWindow) -> Rect {
        let inner = 3u32;
        let width = part.width.saturating_sub(inner * 2);
        let height = part.height.saturating_sub(inner * 2);
        Rect {
            x: part.x + inner + u32::from(window.x) * width / 1000,
            y: part.y + inner + u32::from(window.y) * height / 1000,
            width: (u32::from(window.width) * width / 1000).max(2),
            height: (u32::from(window.height) * height / 1000).max(2),
        }
    }

    fn paint_minimap(
        ctx: &Context,
        part: Rect,
        minimap: &crate::modules::MinimapOutput,
        colors: &Colors,
    ) -> Result<(), String> {
        for window in &minimap.windows {
            let rect = Self::minimap_rect(part, window);
            let color = if window.active {
                colors.accent
            } else {
                colors.fg
            };
            ctx.set_source_rgba_tuple(color.with_alpha(if window.active { 0.9 } else { 0.55 }));
            ctx.rectangle(
                rect.x as f64,
                rect.y as f64,
                rect.width as f64,
                rect.height as f64,
            );
            ctx.fill().map_err(|err| format!("cairo: {err}"))?;
        }
        Ok(())
    }

    fn paint_row_background(
        &self,
        ctx: &Context,
        rect: &Rect,
        hovered: bool,
        colors: &Colors,
    ) -> Result<(), String> {
        let x = rect.x as f64;
        let y = rect.y as f64;
        let w = rect.width as f64;
        let h = rect.height as f64;

        let row_color = if hovered { colors.accent } else { colors.row };
        let alpha = if hovered { 0.35 } else { 1.0 };
        ctx.set_source_rgba_tuple(row_color.with_alpha(alpha));
        ctx.rectangle(x, y, w, h);
        ctx.fill().map_err(|err| format!("cairo: {err}"))?;

        // Accent stripe on the panel's outer edge.
        if self.config.side.is_vertical() {
            let stripe = 3.;
            let stripe_x = match self.config.side {
                Side::Left => x,
                _ => x + w - stripe,
            };
            ctx.set_source_rgba_tuple(colors.accent.cairo());
            ctx.rectangle(stripe_x, y, stripe, h);
            ctx.fill().map_err(|err| format!("cairo: {err}"))?;
        }
        Ok(())
    }

    /// Measures labels and module items once for rendering and hit testing.
    fn compute_parts(&self, ctx: &Context, width: u32) -> Result<Vec<RowParts>, String> {
        let rects = self.row_rects_for(width);
        let horizontal = self.config.side.is_horizontal();
        let padding = if horizontal { 8. } else { 12. };
        let font_size = if horizontal { 12. } else { 13. };
        let gap = 8.0;
        let baseline_offset = if horizontal { 4. } else { 5. };
        let mut parts = Vec::with_capacity(rects.len());

        for (index, rect) in rects.into_iter().enumerate() {
            if index >= self.config.rows.len() || rect.height == 0 {
                continue;
            }
            let row = &self.config.rows[index];
            if row.separator {
                parts.push(RowParts {
                    row: index,
                    rect,
                    label: None,
                    items: Vec::new(),
                    separator: true,
                });
                continue;
            }

            ctx.select_font_face(
                &self.config.theme.font,
                FontSlant::Normal,
                FontWeight::Normal,
            );
            ctx.set_font_size(font_size);

            let mut runs: Vec<TextPart> = if self.items[index].is_empty() {
                match self.outputs.get(index).filter(|text| !text.is_empty()) {
                    Some(text) => vec![TextPart {
                        text: text.clone(),
                        x: 0.,
                        baseline: 0.,
                        width: 0.,
                        size: font_size,
                        active: false,
                        occupied: false,
                        dim: true,
                        action: None,
                        pixmap: None,
                    }],
                    None => Vec::new(),
                }
            } else {
                self.items[index]
                    .iter()
                    .map(|item| TextPart {
                        text: item.label.clone(),
                        x: 0.,
                        baseline: 0.,
                        width: 0.,
                        size: font_size,
                        active: item.active,
                        occupied: item.occupied,
                        dim: false,
                        action: item.action.clone(),
                        pixmap: item.pixmap.clone(),
                    })
                    .collect()
            };

            let row_width = rect.width as f64;
            let module_budget = (row_width * if horizontal { 0.6 } else { 0.55 })
                .max(20.)
                .min(row_width - padding * 2. - 4.);
            let icon_size = if horizontal { 14. } else { 16. };
            let mut widths = Vec::with_capacity(runs.len());
            for run in &runs {
                widths.push(match run.pixmap {
                    Some(_) => icon_size,
                    None => measure(ctx, &run.text)?,
                });
            }
            let total = widths.iter().sum::<f64>() + gap * runs.len().saturating_sub(1) as f64;
            if total > module_budget && !runs.is_empty() {
                let per = ((module_budget - gap * runs.len().saturating_sub(1) as f64)
                    / runs.len() as f64)
                    .max(16.);
                for (run, width) in runs.iter_mut().zip(widths.iter_mut()) {
                    if run.pixmap.is_some() {
                        continue;
                    }
                    run.text = truncate(ctx, &run.text, per)?;
                    *width = measure(ctx, &run.text)?;
                }
            }
            let module_total =
                widths.iter().sum::<f64>() + gap * runs.len().saturating_sub(1) as f64;
            let module_end = rect.x as f64 + row_width - padding;
            let baseline = rect.y as f64 + rect.height as f64 / 2. + baseline_offset;
            let mut x = (module_end - module_total).max(rect.x as f64 + padding);
            for (run, part_width) in runs.iter_mut().zip(widths) {
                run.x = x;
                run.baseline = baseline;
                run.width = part_width;
                x += part_width + gap;
            }

            let icon_prefix = row
                .icon
                .as_deref()
                .map(|icon| format!("{icon} "))
                .unwrap_or_default();
            let label_text = format!("{icon_prefix}{}", row.label);
            let label_x = rect.x as f64 + padding;
            let module_start = if runs.is_empty() {
                rect.x as f64 + row_width - padding
            } else {
                runs[0].x
            };
            let available = (module_start - gap - label_x).max(12.);
            let label_text = if label_text.is_empty() {
                String::new()
            } else {
                truncate(ctx, &label_text, available)?
            };
            let label = if label_text.is_empty() {
                None
            } else {
                Some(TextPart {
                    text: label_text,
                    x: label_x,
                    baseline,
                    width: 0.,
                    size: font_size,
                    active: false,
                    occupied: false,
                    dim: false,
                    action: None,
                    pixmap: None,
                })
            };

            parts.push(RowParts {
                row: index,
                rect,
                label,
                items: runs,
                separator: false,
            });
        }
        Ok(parts)
    }
}

struct RowParts {
    row: usize,
    rect: Rect,
    label: Option<TextPart>,
    items: Vec<TextPart>,
    separator: bool,
}

struct TextPart {
    text: String,
    x: f64,
    baseline: f64,
    width: f64,
    size: f64,
    active: bool,
    occupied: bool,
    dim: bool,
    action: Option<ActionSpec>,
    pixmap: Option<Pixmap>,
}

fn measure(ctx: &Context, text: &str) -> Result<f64, String> {
    Ok(ctx
        .text_extents(text)
        .map_err(|err| format!("cairo: {err}"))?
        .width())
}

trait CairoSource {
    fn set_source_rgba_tuple(&self, color: (f64, f64, f64, f64));
}

impl CairoSource for Context {
    fn set_source_rgba_tuple(&self, color: (f64, f64, f64, f64)) {
        self.set_source_rgba(color.0, color.1, color.2, color.3);
    }
}

fn truncate(ctx: &Context, text: &str, max_width: f64) -> Result<String, String> {
    let extents = ctx
        .text_extents(text)
        .map_err(|err| format!("cairo: {err}"))?;
    if extents.width() <= max_width {
        return Ok(text.to_owned());
    }
    let mut out = String::new();
    for ch in text.chars() {
        let mut candidate = out.clone();
        candidate.push(ch);
        candidate.push_str("...");
        let extents = ctx
            .text_extents(&candidate)
            .map_err(|err| format!("cairo: {err}"))?;
        if extents.width() > max_width {
            break;
        }
        out.push(ch);
    }
    out.push_str("...");
    Ok(out)
}

/// A rendered panel: ARGB32 bytes, row-major, `stride` bytes per row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelImage {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub data: Vec<u8>,
}

impl PanelImage {
    /// Pixel as `[r, g, b, a]` (the buffer is little-endian ARGB32).
    pub fn rgba(&self, x: u32, y: u32) -> [u8; 4] {
        let index = y as usize * self.stride + x as usize * 4;
        let pixel = &self.data[index..index + 4];
        [pixel[2], pixel[1], pixel[0], pixel[3]]
    }

    pub fn is_solid(&self, width: u32, height: u32) -> bool {
        let first = self.rgba(0, 0);
        (0..width).all(|x| (0..height).all(|y| self.rgba(x, y) == first))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::modules::ModuleOutput;
    use crate::system::mock::MockEnv;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("loom-sidepanels-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn panel() -> Panel {
        let config = PanelConfig {
            rows: vec![
                Row::label("Workspaces").with_command("true"),
                Row::label("Volume").with_module("echo 42%", 1),
                Row::label("Network"),
            ],
            ..PanelConfig::default()
        };
        Panel::new(config)
    }

    fn workspace_panel() -> Panel {
        let mut env = MockEnv::new(1_709_251_200).reply(
            "Workspaces",
            json!({
                "Ok": {
                    "Workspaces": [
                        { "id": 10, "idx": 1, "output": "DP-1", "is_active": true, "active_window_id": 3 },
                        { "id": 11, "idx": 2, "output": "DP-1" },
                        { "id": 12, "idx": 3, "output": "DP-1", "name": "web" }
                    ]
                }
            }),
        );
        let config = PanelConfig {
            rows: vec![Row::label("Workspaces").with_kind(ModuleKind::Workspaces, 1)],
            ..PanelConfig::default()
        };
        let mut panel = Panel::new(config).with_output("DP-1");
        panel.refresh_all_with(&mut env);
        panel
    }

    #[test]
    fn parses_hex_colours() {
        assert_eq!(Rgba::parse("#fff").unwrap(), Rgba::rgb(255, 255, 255));
        assert_eq!(Rgba::parse("1d2027").unwrap(), Rgba::rgb(0x1d, 0x20, 0x27));
        assert_eq!(
            Rgba::parse("#11223380").unwrap(),
            Rgba {
                r: 0x11,
                g: 0x22,
                b: 0x33,
                a: 0x80
            }
        );
        assert!(Rgba::parse("#12").is_err());
        assert!(Rgba::parse("#zzzzzz").is_err());
    }

    #[test]
    fn config_round_trips_and_uses_defaults() {
        let dir = temp_dir("config");
        let path = dir.join("config.json");
        let config = PanelConfig {
            side: Side::Right,
            width: 300,
            rows: vec![Row::label("Custom")],
            ..PanelConfig::default()
        };
        config.save(&path).unwrap();

        let loaded = PanelConfig::load(&path).unwrap();
        assert_eq!(loaded, config);

        std::fs::write(&path, r#"{"rows":[{"label":"Only"}]}"#).unwrap();
        let partial = PanelConfig::load(&path).unwrap();
        assert_eq!(partial.width, default_width());
        assert_eq!(partial.side, Side::Left);
        assert_eq!(partial.rows[0].label, "Only");

        let missing = PanelConfig::load(&dir.join("missing.json")).unwrap();
        assert_eq!(missing, PanelConfig::default());
    }

    #[test]
    fn config_v2_round_trips_and_wraps_v1() {
        let dir = temp_dir("config-v2");
        let path = dir.join("config.json");
        let config = Config {
            panels: vec![
                PanelConfig {
                    side: Side::Left,
                    rows: vec![Row::label("A").with_kind(ModuleKind::Clock, 1)],
                    ..PanelConfig::default()
                },
                PanelConfig {
                    side: Side::Bottom,
                    rows: vec![Row::label("B")],
                    ..PanelConfig::default()
                },
            ],
            ..Config::default()
        };
        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded, config);
        assert_eq!(loaded.version, CONFIG_VERSION);
        assert!(loaded.validate().is_ok());

        std::fs::write(
            &path,
            r#"{"side":"right","rows":[{"label":"Legacy","command":"true"}]}"#,
        )
        .unwrap();
        let legacy = Config::load(&path).unwrap();
        assert_eq!(legacy.version, 1);
        assert_eq!(legacy.panels.len(), 1);
        assert_eq!(legacy.panels[0].side, Side::Right);
        assert_eq!(legacy.panels[0].rows[0].label, "Legacy");
        assert!(legacy.validate().is_ok());
    }

    #[test]
    fn per_output_overrides_apply_to_matching_panels() {
        let config = Config {
            panels: vec![PanelConfig {
                rows: vec![Row::label("A")],
                ..PanelConfig::default()
            }],
            outputs: BTreeMap::from([(
                "DP-1".to_owned(),
                PanelOverrides {
                    width: Some(320),
                    theme: Some(ThemeOverrides {
                        bg: Some("#000000".to_owned()),
                        ..ThemeOverrides::default()
                    }),
                    ..PanelOverrides::default()
                },
            )]),
            ..Config::default()
        };
        let dp1 = config.panels_for("DP-1");
        assert_eq!(dp1[0].width, 320);
        assert_eq!(dp1[0].theme.bg, "#000000");
        assert_eq!(dp1[0].theme.fg, default_fg());

        let hdmi = config.panels_for("HDMI-A-1");
        assert_eq!(hdmi[0].width, default_width());

        let mut config = config;
        config.panels[0].output = Some("DP-1".to_owned());
        assert_eq!(config.panels_for("HDMI-A-1").len(), 0);
        assert_eq!(config.panels_for("DP-1").len(), 1);
    }

    #[test]
    fn multiple_panels_per_output_and_override_validation() {
        let config = Config {
            panels: vec![
                PanelConfig {
                    side: Side::Left,
                    rows: vec![Row::label("Left")],
                    ..PanelConfig::default()
                },
                PanelConfig {
                    side: Side::Top,
                    rows: vec![Row::label("Bar")],
                    ..PanelConfig::default()
                },
            ],
            ..Config::default()
        };
        assert_eq!(config.panels_for("DP-1").len(), 2);
        assert_eq!(config.panels_for("DP-1")[1].side, Side::Top);
        assert!(Config::default().validate().is_ok());

        let mut bad = config;
        bad.outputs.insert(
            "DP-1".to_owned(),
            PanelOverrides {
                theme: Some(ThemeOverrides {
                    accent: Some("#zzzzzz".to_owned()),
                    ..ThemeOverrides::default()
                }),
                ..PanelOverrides::default()
            },
        );
        let errors = bad.validate().unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| error.contains("panel 0 on output `DP-1`")),
            "{errors:?}"
        );
    }

    #[test]
    fn validation_reports_every_problem() {
        let config = Config {
            panels: vec![PanelConfig {
                width: 20,
                padding: 12,
                row_height: 4,
                theme: Theme {
                    bg: "#nope".to_owned(),
                    opacity: 2.0,
                    ..Theme::default()
                },
                rows: vec![
                    Row::label("Bad").with_kind(ModuleKind::Custom, 1),
                    Row::label("Focus").with_click(ActionSpec::Niri(NiriAction {
                        niri: niri::NiriVerb::FocusWorkspace,
                        index: None,
                        name: None,
                        id: None,
                    })),
                ],
                ..PanelConfig::default()
            }],
            ..Config::default()
        };
        let errors = config.validate().unwrap_err();
        let joined = errors.join("\n");
        for expected in [
            "width",
            "padding",
            "row_height",
            "theme `bg`",
            "opacity",
            "needs a non-empty `command`",
            "focus-workspace needs",
        ] {
            assert!(
                joined.contains(expected),
                "missing `{expected}` in:\n{joined}"
            );
        }
    }

    #[test]
    fn exclusive_zone_covers_panel_and_margin() {
        let config = PanelConfig {
            width: 240,
            margin: 10,
            ..PanelConfig::default()
        };
        assert_eq!(config.exclusive_zone(), 250);

        let bar = PanelConfig {
            side: Side::Top,
            padding: 6,
            row_height: 30,
            margin: 4,
            ..PanelConfig::default()
        };
        assert_eq!(bar.bar_height(), 42);
        assert_eq!(bar.exclusive_zone(), 46);
    }

    #[test]
    fn layout_stacks_rows_and_hit_tests_them() {
        let panel = panel();
        let rects = panel.row_rects();
        assert_eq!(rects.len(), 3);
        assert_eq!(rects[0].y, HEADER_HEIGHT);
        assert_eq!(rects[1].y, HEADER_HEIGHT + 34 + ROW_GAP);
        assert_eq!(rects[0].x, 12);
        assert_eq!(rects[0].width, 260 - 24);

        let rect = rects[1];
        let inside = panel.hit(rect.x as f64 + 1., rect.y as f64 + 1.);
        assert_eq!(inside, Some(1));
        assert_eq!(panel.hit(0., 0.), None);
        assert_eq!(panel.hit(rect.x as f64 + rect.width as f64 + 1., 60.), None);
    }

    #[test]
    fn horizontal_panels_lay_rows_side_by_side() {
        let config = PanelConfig {
            side: Side::Bottom,
            padding: 10,
            row_height: 30,
            rows: vec![Row::label("A"), Row::label("B"), Row::separator()],
            ..PanelConfig::default()
        };
        let panel = Panel::new(config);
        let rects = panel.row_rects_for(320);
        assert_eq!(rects.len(), 3);
        assert_eq!(
            rects[0],
            Rect {
                x: 10,
                y: 10,
                width: 100,
                height: 30
            }
        );
        assert_eq!(rects[1].x, 110);
        assert_eq!(rects[2].width, 100);
        assert_eq!(panel.config.bar_height(), 50);
    }

    #[test]
    fn module_output_is_refreshed_and_throttled() {
        let dir = temp_dir("module");
        let counter = dir.join("counter");
        let command = format!("echo run >> {}; echo hello", counter.display());
        let config = PanelConfig {
            rows: vec![Row::label("A").with_module(command, 60)],
            ..PanelConfig::default()
        };
        let mut panel = Panel::new(config);
        let start = Instant::now();
        assert!(panel.refresh_at(start));
        assert_eq!(panel.output(0), Some("hello"));
        assert_eq!(
            std::fs::read_to_string(&counter).unwrap().lines().count(),
            1
        );

        // Within the interval nothing runs again.
        assert!(!panel.refresh_at(start + Duration::from_secs(10)));
        assert_eq!(
            std::fs::read_to_string(&counter).unwrap().lines().count(),
            1
        );

        // Past the interval it runs again, even if the value is unchanged.
        panel.refresh_at(start + Duration::from_secs(61));
        assert_eq!(
            std::fs::read_to_string(&counter).unwrap().lines().count(),
            2
        );
    }

    #[test]
    fn failing_module_is_blank() {
        let config = PanelConfig {
            rows: vec![Row::label("A").with_module("exit 3", 60)],
            ..PanelConfig::default()
        };
        let mut panel = Panel::new(config);
        panel.refresh();
        assert_eq!(panel.output(0), Some(""));
    }

    #[test]
    fn activate_runs_commands_only_for_rows_with_one() {
        let dir = temp_dir("activate");
        let marker = dir.join("ran");
        let config = PanelConfig {
            rows: vec![
                Row::label("Run").with_command(format!("touch {}", marker.display())),
                Row::label("Inert"),
            ],
            ..PanelConfig::default()
        };
        let panel = Panel::new(config);
        assert_eq!(panel.activate(1).unwrap(), None);
        let command = panel.activate(0).unwrap().unwrap();
        assert!(command.contains("touch"));
        // Wait for the detached command.
        for _ in 0..50 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(marker.exists());
        assert!(panel.activate(9).is_err());
    }

    #[test]
    fn renders_background_header_and_rows() {
        let panel = panel();
        let image = panel.render(260, 200).unwrap();
        assert_eq!(image.width, 260);
        assert_eq!(image.height, 200);
        assert_eq!(image.data.len(), 260 * 200 * 4);
        // Background pixel matches the theme.
        assert_eq!(image.rgba(250, 190), [0x14, 0x16, 0x1b, 0xff]);
        // Row background matches the row colour.
        let rect = panel.row_rects()[0];
        assert_eq!(
            image.rgba(rect.x + 30, rect.y + 5),
            [0x1d, 0x20, 0x27, 0xff]
        );
        // Text and the accent stripe introduce other colours.
        assert!(!image.is_solid(260, 200));
        let accent = image.rgba(rect.x + 1, rect.y + 5);
        assert_eq!(accent, [0x4f, 0x9c, 0xf9, 0xff]);
    }

    #[test]
    fn hover_changes_the_row_and_right_side_flips_the_stripe() {
        let panel = panel();
        let plain = panel.render(260, 200).unwrap();
        let hovered = panel.render_with(260, 200, Some(1)).unwrap();
        let rect = panel.row_rects()[1];
        assert_ne!(
            plain.rgba(rect.x + 60, rect.y + 10),
            hovered.rgba(rect.x + 60, rect.y + 10)
        );

        let mut config = panel.config.clone();
        config.side = Side::Right;
        let right = Panel::new(config).render(260, 200).unwrap();
        let stripe = right.rgba(rect.x + rect.width - 1, rect.y + 5);
        assert_eq!(stripe, [0x4f, 0x9c, 0xf9, 0xff]);
        let left_of_stripe = right.rgba(rect.x, rect.y + 5);
        assert_eq!(left_of_stripe, [0x1d, 0x20, 0x27, 0xff]);
    }

    #[test]
    fn module_output_is_drawn() {
        let config = PanelConfig {
            rows: vec![Row::label("Vol").with_module("echo 42%", 60)],
            ..PanelConfig::default()
        };
        let mut panel = Panel::new(config);
        panel.refresh();
        let blank = {
            let mut copy = panel.clone();
            copy.outputs[0] = String::new();
            copy.render(260, 120).unwrap()
        };
        let filled = panel.render(260, 120).unwrap();
        assert_ne!(blank.data, filled.data);
    }

    #[test]
    fn tray_pixmap_is_drawn_and_clickable() {
        let mut panel = Panel::new(PanelConfig {
            rows: vec![Row::label("Tray").with_kind(ModuleKind::Tray, 1)],
            ..PanelConfig::default()
        });
        panel.items[0] = vec![OutputItem {
            label: String::new(),
            action: Some(ActionSpec::command("true")),
            active: false,
            occupied: false,
            pixmap: Some(Pixmap {
                width: 2,
                height: 2,
                data: vec![255; 16],
            }),
        }];

        let image = panel.render(260, 120).unwrap();
        assert!(!image.is_solid(260, 120));
        let map = panel.interaction_map(260, 120).unwrap();
        assert!(
            map.regions
                .iter()
                .any(|region| region.action == Some(ActionSpec::command("true"))),
            "tray icon is not clickable"
        );
    }

    #[test]
    fn rejects_empty_render_size() {
        assert!(panel().render(0, 10).is_err());
        assert!(panel().render(10, 0).is_err());
    }

    #[test]
    fn workspace_module_renders_and_clicks_focus_the_right_workspace() {
        let panel = workspace_panel();
        assert_eq!(panel.output(0), Some("1 2 web"));
        assert_eq!(panel.output_items(0).len(), 3);

        let image = panel.render(260, 120).unwrap();
        assert!(!image.is_solid(260, 120));

        let map = panel.interaction_map(260, 120).unwrap();
        let item_regions: Vec<_> = map
            .regions
            .iter()
            .filter(|region| region.action.is_some())
            .collect();
        assert_eq!(item_regions.len(), 3);
        let second = item_regions[1];
        let point = (
            second.rect.x as f64 + second.rect.width as f64 / 2.,
            second.rect.y as f64 + 4.,
        );
        let hit = map.at(point.0, point.1).unwrap();
        assert_eq!(hit.action, Some(ActionSpec::Niri(NiriAction::focus_id(11))));

        let mut env = MockEnv::new(0);
        hit.action.clone().unwrap().run_with(&mut env).unwrap();
        assert_eq!(
            env.niri_requests(),
            vec![json!({ "Action": { "FocusWorkspace": { "reference": { "Id": 11 } } } })]
        );
        assert!(env.spawned().is_empty());
    }

    #[test]
    fn scroll_uses_explicit_actions_then_workspace_cycling() {
        let panel = workspace_panel();
        assert_eq!(
            panel.scroll_action(0, ScrollDirection::Down),
            Some(ActionSpec::Niri(NiriAction::down()))
        );

        let config = PanelConfig {
            rows: vec![
                Row::label("Volume")
                    .with_kind(ModuleKind::Volume, 5)
                    .on_scroll(
                        ActionSpec::command("volume up"),
                        ActionSpec::command("volume down"),
                    ),
                Row::label("Inert"),
            ],
            ..PanelConfig::default()
        };
        let panel = Panel::new(config);
        assert_eq!(
            panel.scroll_action(0, ScrollDirection::Up),
            Some(ActionSpec::command("volume up"))
        );
        assert_eq!(panel.scroll_action(1, ScrollDirection::Up), None);
    }

    #[test]
    fn row_commands_are_used_as_click_actions_in_the_interaction_map() {
        let panel = panel();
        let map = panel.interaction_map(260, 200).unwrap();
        let row = map.regions.iter().find(|region| region.row == 0).unwrap();
        assert_eq!(row.action, Some(ActionSpec::command("true")));

        let mut env = MockEnv::new(0);
        row.action.clone().unwrap().run_with(&mut env).unwrap();
        assert_eq!(env.spawned(), vec!["true".to_owned()]);
    }

    #[test]
    fn horizontal_panels_render_all_four_sides() {
        for side in [Side::Top, Side::Bottom] {
            let config = PanelConfig {
                side,
                rows: vec![
                    Row::label("CPU").with_kind(ModuleKind::Cpu, 1),
                    Row::separator(),
                    Row::label("Clock").with_kind(ModuleKind::Clock, 1),
                ],
                ..PanelConfig::default()
            };
            let panel = Panel::new(config);
            let height = panel.config.bar_height();
            let image = panel.render(320, height).unwrap();
            assert!(!image.is_solid(320, height));
            let map = panel.interaction_map(320, height).unwrap();
            assert!(!map.regions.is_empty());
        }
    }

    #[test]
    fn icons_labels_and_opacity_are_applied() {
        let config = PanelConfig {
            rows: vec![Row::label("Clock").with_icon("")],
            theme: Theme {
                opacity: 0.5,
                font: "Sans".to_owned(),
                ..Theme::default()
            },
            ..PanelConfig::default()
        };
        let panel = Panel::new(config);
        let image = panel.render(260, 120).unwrap();
        // cairo's ARGB32 is premultiplied, so only the alpha is predictable.
        let bg = image.rgba(250, 110);
        assert!(bg[3] > 0);
        assert!(bg[3] < 0xff);
    }

    #[test]
    fn untrusted_window_titles_never_reach_a_command() {
        let mut env = MockEnv::new(0).reply(
            "FocusedWindow",
            json!({
                "Ok": {
                    "FocusedWindow": {
                        "id": 1,
                        "title": "$(touch /tmp/pwned); `id`",
                        "app_id": "evil"
                    }
                }
            }),
        );
        let config = PanelConfig {
            rows: vec![
                Row::label("Window")
                    .with_kind(ModuleKind::WindowTitle, 1)
                    .with_click(ActionSpec::command("notify-send focus")),
            ],
            ..PanelConfig::default()
        };
        let mut panel = Panel::new(config);
        panel.refresh_all_with(&mut env);
        assert_eq!(panel.output(0), Some("evil - $(touch /tmp/pwned); `id`"));
        panel.activate_with(0, &mut env).unwrap();
        assert_eq!(env.spawned(), vec!["notify-send focus".to_owned()]);
    }

    #[test]
    fn config_watcher_reports_changes_once() {
        let dir = temp_dir("watch");
        let path = dir.join("config.json");
        std::fs::write(&path, "{}").unwrap();
        let mut watcher = ConfigWatcher::new(&path);
        assert!(!watcher.poll());
        std::fs::write(&path, r#"{"panels":[]}"#).unwrap();
        assert!(watcher.poll());
        assert!(!watcher.poll());
        assert_eq!(watcher.path(), path.as_path());
    }

    #[test]
    fn reload_updates_panel_in_place() {
        let dir = temp_dir("reload");
        let path = dir.join("config.json");
        Config::default().save(&path).unwrap();
        let first = Config::load(&path).unwrap();
        let mut panel = Panel::new(first.panels[0].clone());
        let mut watcher = ConfigWatcher::new(&path);

        let mut second = first.clone();
        second.panels[0].rows = vec![Row::label("Replaced").with_module("echo ok", 1)];
        second.save(&path).unwrap();

        assert!(watcher.poll());
        let reloaded = Config::load(&path).unwrap();
        panel.set_config(reloaded.panels[0].clone());
        panel.refresh();
        assert_eq!(panel.config.rows[0].label, "Replaced");
        assert_eq!(panel.output(0), Some("ok"));
    }


    #[test]
    fn minimap_row_uses_its_height_and_click_regions() {
        let mut spec = ModuleSpec::of(ModuleKind::Minimap, 1);
        spec.height = Some(80);
        let mut panel = Panel::new(PanelConfig {
            rows: vec![Row { module: Some(spec), ..Row::label("Map") }],
            ..PanelConfig::default()
        });
        panel.minimaps[0] = Some(MinimapOutput {
            windows: vec![crate::modules::MinimapWindow {
                x: 0,
                y: 0,
                width: 500,
                height: 1000,
                window_id: 42,
                active: true,
            }],
        });
        assert_eq!(panel.row_rects_for(260)[0].height, 80);
        let map = panel.interaction_map(260, 200).unwrap();
        assert!(map.regions.iter().any(|region| {
            region.action == Some(ActionSpec::Niri(NiriAction::focus_window(42)))
        }));
        panel.render(260, 200).unwrap();
    }
    #[test]
    fn module_output_keeps_text_and_items_in_sync() {
        let output = ModuleOutput {
            text: "1 2".to_owned(),
            items: vec![OutputItem::text("1"), OutputItem::text("2")],
            minimap: None,
        };
        assert_eq!(output.text, "1 2");
    }

    #[test]
    fn reveal_strip_renders_in_the_accent_colour() {
        let panel = panel();
        let strip = panel.render_strip(6, 200).unwrap();
        assert_eq!((strip.width, strip.height), (6, 200));
        assert_eq!(strip.rgba(0, 0), [0x4f, 0x9c, 0xf9, 0xff]);
        assert!(panel.render_strip(0, 10).is_err());
    }

    #[test]
    fn named_profiles_are_switchable_layouts() {
        let mut config = Config {
            panels: vec![PanelConfig {
                rows: vec![Row::label("Main")],
                ..PanelConfig::default()
            }],
            profiles: BTreeMap::from([
                (
                    "media".to_owned(),
                    vec![PanelConfig {
                        side: Side::Top,
                        rows: vec![Row::label("Media").with_kind(ModuleKind::Media, 1)],
                        ..PanelConfig::default()
                    }],
                ),
                (
                    "work".to_owned(),
                    vec![PanelConfig {
                        side: Side::Right,
                        width: 320,
                        rows: vec![Row::label("Work")],
                        ..PanelConfig::default()
                    }],
                ),
            ]),
            ..Config::default()
        };
        assert_eq!(config.profile_names(), ["media", "work"]);
        assert_eq!(config.panels_for("DP-1")[0].rows[0].label, "Main");
        assert!(config.select_profile("work"));
        assert_eq!(config.panels_for("DP-1")[0].width, 320);
        assert!(!config.select_profile("missing"));
        assert_eq!(
            config
                .clone()
                .with_profile(Some("media"))
                .panels_for("DP-1")[0]
                .side,
            Side::Top
        );
        assert_eq!(
            config
                .clone()
                .with_profile(Some("missing"))
                .panels_for("DP-1")[0]
                .side,
            Side::Right
        );
        assert_eq!(
            config.clone().with_profile(None).panels_for("DP-1")[0].side,
            Side::Right
        );
        config.active_profile = Some("missing".to_owned());
        assert!(config.validate().is_err());
    }

    #[test]
    fn layouts_alias_and_active_profile_round_trip() {
        let dir = temp_dir("profiles-json");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{
                "version": 2,
                "layouts": { "media": [ { "side": "top", "rows": [ { "label": "M" } ] } ] },
                "active_profile": "media",
                "rules": [ { "app": "fire*", "hide": ["Media"] } ]
            }"#,
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.panels_for("DP-1").len(), 1);
        assert_eq!(config.panels_for("DP-1")[0].side, Side::Top);
        assert_eq!(config.rules[0].app, "fire*");
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn focused_context_applies_rules_and_refreshes_rows() {
        let env = MockEnv::new(0)
            .reply(
                "Workspaces",
                json!({ "Ok": { "Workspaces": [
                    { "id": 1, "idx": 1, "name": "web", "output": "DP-1", "is_active": true, "is_focused": true }
                ] } }),
            )
            .reply(
                "FocusedWindow",
                json!({ "Ok": { "FocusedWindow": { "id": 2, "title": "Docs", "app_id": "firefox" } } }),
            );
        let config = PanelConfig {
            rows: vec![
                Row::label("Workspaces").with_kind(ModuleKind::Workspaces, 1),
                Row::label("Media").with_kind(ModuleKind::Media, 1),
                Row::label("Scratch").on_workspaces(["scratch"]),
            ],
            ..PanelConfig::default()
        };
        let rules = vec![AppRule {
            app: "fire*".to_owned(),
            hide: vec!["Media".to_owned()],
            ..AppRule::default()
        }];
        let mut panel = Panel::new(config).with_rules(rules).with_output("DP-1");
        let mut env = env;
        panel.refresh_all_with(&mut env);
        assert_eq!(panel.context().app.as_deref(), Some("firefox"));
        assert_eq!(panel.context().workspace.as_deref(), Some("web"));
        // The hidden Media row and the scratch-only row are gone.
        assert_eq!(panel.config.rows.len(), 1);
        assert_eq!(panel.config.rows[0].label, "Workspaces");
        assert_eq!(panel.output(0), Some("web"));

        // A different workspace keeps the workspace-scoped row.
        env.replies.insert(
            "Workspaces".into(),
            json!({ "Ok": { "Workspaces": [
                { "id": 1, "idx": 1, "name": "scratch", "output": "DP-1", "is_active": true, "is_focused": true }
            ] } }),
        );
        env.replies.insert(
            "FocusedWindow".into(),
            json!({ "Ok": { "FocusedWindow": { "id": 3, "title": "Term", "app_id": "foot" } } }),
        );
        panel.refresh_all_with(&mut env);
        assert_eq!(panel.config.rows.len(), 3);
    }

    #[test]
    fn floating_height_and_auto_hide_change_geometry() {
        let config = PanelConfig {
            height: Some(120),
            floating: true,
            auto_hide: true,
            reveal: 6,
            ..PanelConfig::default()
        };
        assert_eq!(config.bar_height(), 120);
        assert_eq!(config.exclusive_zone(), 0);
        assert_eq!(config.exclusive_zone_with(false), 0);
        let normal = PanelConfig {
            height: Some(120),
            auto_hide: true,
            reveal: 6,
            ..PanelConfig::default()
        };
        assert_eq!(normal.exclusive_zone_with(true), 0);
        assert_eq!(
            normal.exclusive_zone_with(false),
            normal.width + normal.margin
        );
        assert_eq!(
            behavior::surface_size(
                normal.side,
                normal.width,
                normal.bar_height(),
                normal.reveal,
                true,
                Some((normal.width, 900))
            ),
            (6, 900)
        );

        let bar = PanelConfig {
            side: Side::Bottom,
            height: Some(40),
            ..PanelConfig::default()
        };
        assert_eq!(bar.bar_height(), 40);
        assert_eq!(bar.exclusive_zone(), 48);

        let mut invalid = PanelConfig {
            height: Some(1),
            reveal: 0,
            auto_hide: true,
            width: 6,
            margin: 0,
            ..PanelConfig::default()
        };
        invalid.rows = vec![Row::label("x")];
        let errors = invalid.validate().unwrap_err().join("\n");
        assert!(errors.contains("height"), "{errors}");
        assert!(errors.contains("reveal"), "{errors}");
        invalid.reveal = 6;
        let errors = invalid.validate().unwrap_err().join("\n");
        assert!(errors.contains("smaller than the panel"), "{errors}");
    }
}
