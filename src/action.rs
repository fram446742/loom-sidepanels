//! Click and scroll actions.
//!
//! Actions are either a shell command (config-authored) or a niri IPC action.
//! Panel module output is never interpolated into a command: window titles and
//! media metadata are display-only, so a hostile title cannot reach a shell.

use serde::{Deserialize, Serialize};

use crate::niri::NiriAction;
use crate::system::Env;

/// A row action from the config.
///
/// A bare string is a shell command, keeping the v1 `"command": "..."` shape:
///
/// ```json
/// "click": "niri msg action focus-workspace 2"
/// "click": { "niri": "focus-workspace-down" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ActionSpec {
    Command(String),
    Niri(NiriAction),
    /// Switches to another named panel layout; handled by the daemon.
    Layout {
        /// Profile/layout name from `Config::profiles`.
        layout: String,
    },
}

impl ActionSpec {
    pub fn command(command: impl Into<String>) -> Self {
        Self::Command(command.into())
    }

    /// Action that switches the active panel layout/profile.
    pub fn layout(name: impl Into<String>) -> Self {
        Self::Layout {
            layout: name.into(),
        }
    }

    pub fn is_niri(&self) -> bool {
        matches!(self, Self::Niri(_))
    }

    /// Name of the layout this action switches to, if it is a layout action.
    pub fn layout_name(&self) -> Option<&str> {
        match self {
            Self::Layout { layout } => Some(layout),
            _ => None,
        }
    }

    /// Executes the action with the given environment.
    pub fn run_with(&self, env: &mut dyn Env) -> Result<(), String> {
        match self {
            Self::Command(command) => env.spawn(command),
            Self::Niri(action) => {
                let request = action.request()?;
                match env.niri(&request) {
                    Some(reply) if reply.get("Err").is_none() => Ok(()),
                    Some(reply) => Err(format!(
                        "niri rejected `{}`: {}",
                        action.describe(),
                        reply.get("Err").cloned().unwrap_or_default()
                    )),
                    None => Err(format!("niri did not answer `{}`", action.describe())),
                }
            }
            Self::Layout { layout } => Err(format!(
                "layout `{layout}` must be switched by the panel daemon"
            )),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Command(command) => command.clone(),
            Self::Niri(action) => action.describe(),
            Self::Layout { layout } => format!("layout {layout}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::system::mock::MockEnv;

    #[test]
    fn actions_accept_bare_commands_and_niri_objects() {
        let command: ActionSpec = serde_json::from_value(json!("touch /tmp/x")).unwrap();
        assert_eq!(command, ActionSpec::command("touch /tmp/x"));
        assert!(!command.is_niri());

        let niri: ActionSpec =
            serde_json::from_value(json!({ "niri": "focus-workspace", "index": 3 })).unwrap();
        assert!(niri.is_niri());
        assert_eq!(niri.describe(), "focus-workspace index=3");

        let scroll: ActionSpec =
            serde_json::from_value(json!({ "niri": "focus-workspace-down" })).unwrap();
        assert_eq!(scroll.describe(), "focus-workspace-down");
        assert!(serde_json::from_value::<ActionSpec>(json!({ "niri": "bogus" })).is_err());

        let layout: ActionSpec = serde_json::from_value(json!({ "layout": "media" })).unwrap();
        assert_eq!(layout, ActionSpec::layout("media"));
        assert_eq!(layout.layout_name(), Some("media"));
        assert_eq!(layout.describe(), "layout media");
        assert!(!layout.is_niri());
        assert!(layout.run_with(&mut MockEnv::new(0)).is_err());
        assert_eq!(ActionSpec::command("true").layout_name(), None);
    }

    #[test]
    fn running_actions_uses_the_environment() {
        let mut env = MockEnv::new(0);
        ActionSpec::command("notify-send hi")
            .run_with(&mut env)
            .unwrap();
        assert_eq!(env.spawned(), vec!["notify-send hi".to_owned()]);

        let niri = ActionSpec::Niri(NiriAction::focus_index(2));
        niri.run_with(&mut env).unwrap();
        assert_eq!(
            env.niri_requests(),
            vec![json!({ "Action": { "FocusWorkspace": { "reference": { "Index": 2 } } } })]
        );

        env.replies.insert(
            json!({ "Action": { "FocusWorkspace": { "reference": { "Index": 2 } } } }).to_string(),
            json!({ "Err": "no such workspace" }),
        );
        let error = niri.run_with(&mut env).unwrap_err();
        assert!(error.contains("no such workspace"), "{error}");
    }
}
