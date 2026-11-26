//! Behaviour that is independent of the Wayland daemon: context-aware row
//! filtering, auto-hide ("peek") state and SIGHUP reload signalling.
//!
//! Keeping these here means the rules can be tested with synthetic focused
//! windows/workspaces and synthetic pointer transitions, without a compositor.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::modules::ModuleKind;
use crate::{Row, Side};

/// The focused application/workspace context used by [`AppRule`]s.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context {
    pub app: Option<String>,
    pub workspace: Option<String>,
}

impl Context {
    pub fn new(app: Option<impl Into<String>>, workspace: Option<impl Into<String>>) -> Self {
        Self {
            app: app.map(Into::into),
            workspace: workspace.map(Into::into),
        }
    }
}

/// A rule that shows, hides or reorders rows for a focused app/workspace.
///
/// `app` is a case-insensitive glob (`*`/`?`) or substring; `workspace` is an
/// optional case-insensitive glob matched against the focused workspace name.
/// Selectors match a row label, `kind:<module-kind>` or `#<row-index>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppRule {
    /// Focused application match; empty or `*` matches every app.
    pub app: String,
    /// Focused workspace match; omitted matches every workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// If non-empty, only these rows are kept (before `order`).
    pub show: Vec<String>,
    /// Rows removed after `show`.
    pub hide: Vec<String>,
    /// Rows moved to the front in this order; unlisted rows keep their order.
    pub order: Vec<String>,
}

/// Case-insensitive glob supporting `*` and `?`, or a substring match when the
/// pattern has no wildcard. Empty patterns match nothing.
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern = pattern.trim().to_lowercase();
    let text = text.to_lowercase();
    if pattern.is_empty() {
        return false;
    }
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') && !pattern.contains('?') {
        return text.contains(&pattern);
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let mut p = 0;
    let mut t = 0;
    let mut star = None;
    let mut resume = 0;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            resume = t;
            p += 1;
        } else if let Some(star_index) = star {
            p = star_index + 1;
            resume += 1;
            t = resume;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// Whether a rule applies to this focused context.
pub fn rule_matches_context(rule: &AppRule, context: &Context) -> bool {
    let app = rule.app.trim();
    if !app.is_empty() && app != "*" {
        let Some(focused) = context.app.as_deref() else {
            return false;
        };
        if !glob_matches(app, focused) {
            return false;
        }
    }
    match rule.workspace.as_deref().map(str::trim) {
        Some(workspace) if !workspace.is_empty() && workspace != "*" => context
            .workspace
            .as_deref()
            .is_some_and(|focused| glob_matches(workspace, focused)),
        _ => true,
    }
}

/// Whether a selector (`label`, `kind:<kind>` or `#<index>`) matches a row.
pub fn selector_matches(selector: &str, index: usize, row: &Row) -> bool {
    let selector = selector.trim();
    if selector.is_empty() {
        return false;
    }
    if let Some(kind) = selector.strip_prefix("kind:") {
        return row
            .module
            .as_ref()
            .is_some_and(|module| module.kind.as_str() == kind.trim().to_lowercase());
    }
    if let Some(number) = selector.strip_prefix('#')
        && let Ok(wanted) = number.trim().parse::<usize>()
    {
        return wanted == index;
    }
    glob_matches(selector, &row.label)
}

/// Applies every matching rule to `rows`.
///
/// Rules are applied in declaration order. `show` is a whitelist applied to the
/// current rows, `hide` removes rows, and `order` moves rows to the front. A
/// row hidden by one rule is not resurrected by a later one.
pub fn filter_rows(rows: &[Row], context: &Context, rules: &[AppRule]) -> Vec<Row> {
    let mut rows: Vec<Row> = rows.to_vec();
    for rule in rules
        .iter()
        .filter(|rule| rule_matches_context(rule, context))
    {
        if !rule.show.is_empty() {
            let mut index = 0;
            rows.retain(|row| {
                let keep = rule
                    .show
                    .iter()
                    .any(|selector| selector_matches(selector, index, row));
                index += 1;
                keep
            });
        }
        if !rule.hide.is_empty() {
            let mut index = 0;
            rows.retain(|row| {
                let drop = rule
                    .hide
                    .iter()
                    .any(|selector| selector_matches(selector, index, row));
                index += 1;
                !drop
            });
        }
        if !rule.order.is_empty() {
            let mut ordered = Vec::with_capacity(rows.len());
            let mut used = vec![false; rows.len()];
            for selector in &rule.order {
                for (index, row) in rows.iter().enumerate() {
                    if !used[index] && selector_matches(selector, index, row) {
                        used[index] = true;
                        ordered.push(row.clone());
                    }
                }
            }
            for (index, row) in rows.iter().enumerate() {
                if !used[index] {
                    ordered.push(row.clone());
                }
            }
            rows = ordered;
        }
    }
    rows
}

/// Whether a row is visible on the named workspace. A row without a
/// `workspaces` list is always visible.
pub fn row_visible_on_workspace(row: &Row, workspace: Option<&str>) -> bool {
    match row.workspaces.as_deref() {
        None => true,
        Some([]) => true,
        Some(names) => workspace.is_some_and(|workspace| {
            names
                .iter()
                .any(|name| glob_matches(name, workspace) || name == "*")
        }),
    }
}

/// Whether a row is visible for the focused application. A row without an
/// `apps` list is always visible.
pub fn row_visible_for_app(row: &Row, app: Option<&str>) -> bool {
    match row.apps.as_deref() {
        None => true,
        Some([]) => true,
        Some(apps) => app.is_some_and(|app| apps.iter().any(|pattern| glob_matches(pattern, app))),
    }
}

/// Auto-hide state: the panel is hidden behind the edge until the pointer
/// enters its reveal strip (or it is explicitly pinned/clicked).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeekState {
    hidden: bool,
}

impl PeekState {
    /// Starts hidden when the panel has `auto_hide`.
    pub fn new(auto_hide: bool) -> Self {
        Self { hidden: auto_hide }
    }

    pub fn hidden(&self) -> bool {
        self.hidden
    }

    /// Pointer entered the panel/reveal strip: reveal it.
    pub fn enter(&mut self) -> bool {
        let changed = self.hidden;
        self.hidden = false;
        changed
    }

    /// Pointer left: hide again.
    pub fn leave(&mut self) -> bool {
        let changed = !self.hidden;
        self.hidden = true;
        changed
    }

    /// A click keeps an auto-hidden panel visible for this interaction.
    pub fn click(&mut self) -> bool {
        self.enter()
    }

    /// Restores persisted session state.
    pub fn set_hidden(&mut self, hidden: bool) -> bool {
        let changed = self.hidden != hidden;
        self.hidden = hidden;
        changed
    }
}

/// Size of a hidden panel along its stacking axis.
pub const DEFAULT_REVEAL: u32 = 6;

impl Side {
    /// Surface extent when hidden: the reveal strip runs along the edge.
    pub fn reveal_size(self, reveal: u32) -> u32 {
        reveal.max(1)
    }
}

/// The size a layer surface should request.
///
/// `configured` is the compositor-provided size for the full panel; a hidden
/// panel only keeps a thin strip on the screen edge.
pub fn surface_size(
    side: Side,
    width: u32,
    bar_height: u32,
    reveal: u32,
    hidden: bool,
    configured: Option<(u32, u32)>,
) -> (u32, u32) {
    let reveal = side.reveal_size(reveal);
    match side {
        Side::Left | Side::Right => {
            let height = configured.map(|(_, height)| height).unwrap_or(0);
            let width = if hidden { reveal } else { width };
            (width, height)
        }
        Side::Top | Side::Bottom => {
            let width = configured.map(|(width, _)| width).unwrap_or(0);
            let height = if hidden { reveal } else { bar_height };
            (width, height)
        }
    }
}

/// A SIGHUP reload latch shared with a signal handler.
///
/// `install` registers a process-wide handler that stores into a global flag;
/// every [`ReloadSignal`] observes the same flag, so a test can drive it
/// directly with [`ReloadSignal::trigger`] without sending a real signal.
#[derive(Debug, Clone, Default)]
pub struct ReloadSignal {
    flag: Arc<AtomicBool>,
}

static SIGHUP_FLAG: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_sighup(_: libc::c_int) {
    SIGHUP_FLAG.store(true, Ordering::SeqCst);
}

impl ReloadSignal {
    /// Installs the SIGHUP handler (idempotent) and returns the latch.
    pub fn install() -> Self {
        #[cfg(unix)]
        // SAFETY: `on_sighup` only stores into an atomic flag, which is
        // async-signal-safe.
        unsafe {
            libc::signal(libc::SIGHUP, on_sighup as *const () as libc::sighandler_t);
        }
        Self::default()
    }

    /// A latch that is only driven by [`Self::trigger`] (tests).
    pub fn detached() -> Self {
        Self::default()
    }

    pub fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Returns true once after a reload was requested. The process-wide
    /// SIGHUP flag and this latch are both consumed.
    pub fn take(&self) -> bool {
        let local = self.flag.swap(false, Ordering::SeqCst);
        let global = SIGHUP_FLAG.swap(false, Ordering::SeqCst);
        local || global
    }
}

/// Module kinds that make a row useful for context rules.
pub fn kind_selector(kind: ModuleKind) -> String {
    format!("kind:{}", kind.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::{ModuleKind, ModuleSpec};
    use crate::{ActionSpec, Row};

    fn rows() -> Vec<Row> {
        vec![
            Row::label("Workspaces").with_kind(ModuleKind::Workspaces, 1),
            Row::label("Media").with_kind(ModuleKind::Media, 2),
            Row::label("Clock").with_kind(ModuleKind::Clock, 1),
            Row::separator(),
        ]
    }

    #[test]
    fn glob_matching_is_case_insensitive() {
        assert!(glob_matches("fire*", "Firefox"));
        assert!(glob_matches("spot", "Spotify"));
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("f?re*", "Firefox"));
        assert!(!glob_matches("fire*", "Chromium"));
        assert!(!glob_matches("", "x"));
    }

    #[test]
    fn rules_filter_and_reorder_matching_apps() {
        let rules = vec![AppRule {
            app: "fire*".to_owned(),
            hide: vec!["kind:media".to_owned()],
            order: vec!["Clock".to_owned(), "#2".to_owned()],
            ..AppRule::default()
        }];
        let context = Context::new(Some("Firefox"), Some("web"));
        let filtered = filter_rows(&rows(), &context, &rules);
        assert_eq!(filtered.len(), 3);
        assert_eq!(filtered[0].label, "Clock");
        assert_eq!(filtered[2].label, "Workspaces");
        assert!(!filtered.iter().any(|row| row.label == "Media"));

        // Another app keeps the original rows.
        let untouched = filter_rows(&rows(), &Context::new(Some("mpv"), None::<&str>), &rules);
        assert_eq!(untouched, rows());
    }

    #[test]
    fn show_whitelist_and_workspace_match() {
        let rules = vec![AppRule {
            app: String::new(),
            workspace: Some("game*".to_owned()),
            show: vec!["kind:clock".to_owned(), "Workspaces".to_owned()],
            ..AppRule::default()
        }];
        let filtered = filter_rows(&rows(), &Context::new(Some("Game"), Some("games")), &rules);
        assert_eq!(
            filtered
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            ["Workspaces", "Clock"]
        );
        let other = filter_rows(&rows(), &Context::new(None::<&str>, Some("web")), &rules);
        assert_eq!(other.len(), 4);
    }

    #[test]
    fn row_workspace_and_app_visibility() {
        let mut row = Row::label("Scratchpad");
        assert!(row_visible_on_workspace(&row, None));
        row.workspaces = Some(vec!["scratch".to_owned(), "game*".to_owned()]);
        assert!(row_visible_on_workspace(&row, Some("scratch")));
        assert!(row_visible_on_workspace(&row, Some("games")));
        assert!(!row_visible_on_workspace(&row, Some("web")));
        assert!(!row_visible_on_workspace(&row, None));

        row.apps = Some(vec!["foot*".to_owned()]);
        assert!(row_visible_for_app(&row, Some("footclient")));
        assert!(!row_visible_for_app(&row, Some("firefox")));
        assert!(!row_visible_for_app(&row, None));
    }

    #[test]
    fn peek_hides_and_reveals_once() {
        let mut peek = PeekState::new(true);
        assert!(peek.hidden());
        assert!(peek.enter());
        assert!(!peek.hidden());
        assert!(!peek.enter());
        assert!(peek.leave());
        assert!(peek.hidden());
        assert!(!peek.leave());
        assert!(peek.click());
        assert!(!peek.hidden());
        assert!(peek.set_hidden(true));
        assert!(!peek.set_hidden(true));
    }

    #[test]
    fn surface_size_uses_the_reveal_strip_when_hidden() {
        assert_eq!(
            surface_size(Side::Left, 260, 46, 6, false, Some((260, 900))),
            (260, 900)
        );
        assert_eq!(
            surface_size(Side::Left, 260, 46, 6, true, Some((260, 900))),
            (6, 900)
        );
        assert_eq!(
            surface_size(Side::Top, 1920, 46, 8, false, Some((1920, 46))),
            (1920, 46)
        );
        assert_eq!(
            surface_size(Side::Top, 1920, 46, 8, true, Some((1920, 46))),
            (1920, 8)
        );
        assert_eq!(surface_size(Side::Left, 260, 46, 0, true, None), (1, 0));
    }

    #[test]
    fn reload_latch_is_taken_once() {
        let signal = ReloadSignal::detached();
        assert!(!signal.take());
        signal.trigger();
        assert!(signal.take());
        assert!(!signal.take());
    }

    #[test]
    fn selectors_cover_label_kind_and_index() {
        let rows = rows();
        assert!(selector_matches("Media", 1, &rows[1]));
        assert!(selector_matches("kind:media", 1, &rows[1]));
        assert!(selector_matches("#2", 2, &rows[2]));
        assert!(!selector_matches("#1", 2, &rows[2]));
        assert!(!selector_matches("kind:clock", 1, &rows[1]));
        assert_eq!(
            kind_selector(ModuleKind::Workspaces),
            "kind:workspaces".to_owned()
        );
        let _ = ModuleSpec::of(ModuleKind::Clock, 1);
        let _ = ActionSpec::command("true");
    }
}
