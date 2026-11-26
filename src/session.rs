//! Persisted runtime panel state ("session integration").
//!
//! The config file describes what the panels *can* show; this state records
//! which named layout/profile was active and which auto-hidden panels were
//! revealed. It lives under `$XDG_STATE_HOME/loom-sidepanels/state.json` so
//! a session restart (or a `spawn-at-startup` run after a logout) restores the
//! same layout instead of resetting to the config default.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Mutable state saved across daemon runs.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionState {
    /// Named panel layout/profile selected at runtime.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Per-output auto-hide state: `true` means the panel was left visible.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub visible: BTreeMap<String, bool>,
}

impl SessionState {
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|err| format!("cannot parse {}: {err}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(format!("cannot read {}: {err}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|err| format!("cannot serialize state: {err}"))?;
        std::fs::write(path, bytes).map_err(|err| format!("cannot write {}: {err}", path.display()))
    }

    /// Whether an auto-hide panel on `output` should start visible.
    pub fn is_visible(&self, output: &str) -> bool {
        self.visible.get(output).copied().unwrap_or(false)
    }

    pub fn set_visible(&mut self, output: impl Into<String>, visible: bool) -> bool {
        let output = output.into();
        let previous = self.is_visible(&output);
        if visible {
            self.visible.insert(output, true);
        } else {
            self.visible.remove(&output);
        }
        previous != visible
    }
}

pub fn state_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_default();
    base.join("loom-sidepanels/state.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "loom-sidepanels-session-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn session_state_round_trips_and_tracks_visibility() {
        let dir = temp_dir("round-trip");
        let path = dir.join("nested/state.json");
        let mut state = SessionState {
            profile: Some("work".to_owned()),
            ..SessionState::default()
        };
        assert!(!state.is_visible("DP-1"));
        assert!(state.set_visible("DP-1", true));
        assert!(state.is_visible("DP-1"));
        assert!(!state.set_visible("DP-1", true));
        assert!(state.set_visible("DP-1", false));
        assert!(!state.is_visible("DP-1"));

        state.set_visible("HDMI-A-1", true);
        state.save(&path).unwrap();
        let loaded = SessionState::load(&path).unwrap();
        assert_eq!(loaded, state);
        assert_eq!(loaded.profile.as_deref(), Some("work"));
        assert!(loaded.is_visible("HDMI-A-1"));
    }

    #[test]
    fn missing_state_is_empty_and_malformed_state_is_reported() {
        let dir = temp_dir("errors");
        let missing = SessionState::load(&dir.join("missing.json")).unwrap();
        assert_eq!(missing, SessionState::default());

        let path = dir.join("bad.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(
            SessionState::load(&path)
                .unwrap_err()
                .contains("cannot parse")
        );
        std::fs::write(&path, br#"{"profile": 7}"#).unwrap();
        assert!(SessionState::load(&path).is_err());
    }

    #[test]
    fn unknown_fields_are_ignored_and_defaults_applied() {
        let state: SessionState = serde_json::from_str(r#"{"profile":"media"}"#).unwrap();
        assert_eq!(state.profile.as_deref(), Some("media"));
        assert!(state.visible.is_empty());
        assert!(serde_json::from_str::<SessionState>(r#"{"other":1}"#).is_err());
    }
}
