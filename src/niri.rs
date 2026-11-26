//! Minimal niri IPC client and typed action mapping.
//!
//! Only the requests the panel needs are sent, as plain JSON over the socket in
//! `$NIRI_SOCKET`. Reply parsing is deliberately tolerant: unknown fields are
//! ignored so a newer niri can still feed an older panel.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::system::{read_reply, write_request};

/// niri's IPC socket from the environment, falling back to the runtime dir.
pub fn socket_path() -> Option<PathBuf> {
    std::env::var_os("NIRI_SOCKET")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR").map(|dir| PathBuf::from(dir).join("niri.sock"))
        })
}

/// Sends one request to the default socket.
pub fn request(request: &Value) -> Option<Value> {
    request_at(&socket_path()?, request)
}

/// Sends one request to `path`. Used by tests with a synthetic socket.
pub fn request_at(path: &Path, request: &Value) -> Option<Value> {
    let mut stream = UnixStream::connect(path).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    write_request(&mut stream, request).ok()?;
    read_reply(stream).ok()
}

/// Extracts `{"Ok": {"<field>": ...}}` from a reply.
pub fn ok_field<'a>(reply: &'a Value, field: &str) -> Option<&'a Value> {
    reply.get("Ok")?.get(field)
}

/// A workspace as reported by `Request::Workspaces`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: u64,
    pub idx: u8,
    pub name: Option<String>,
    pub output: Option<String>,
    pub is_active: bool,
    pub is_focused: bool,
    pub is_urgent: bool,
    pub active_window_id: Option<u64>,
}

impl Workspace {
    pub fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_u64()?,
            idx: value.get("idx")?.as_u64()? as u8,
            name: value.get("name").and_then(Value::as_str).map(str::to_owned),
            output: value
                .get("output")
                .and_then(Value::as_str)
                .map(str::to_owned),
            is_active: value
                .get("is_active")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            is_focused: value
                .get("is_focused")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            is_urgent: value
                .get("is_urgent")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            active_window_id: value.get("active_window_id").and_then(Value::as_u64),
        })
    }

    /// Short label shown on the panel: the workspace name, or its index.
    pub fn label(&self) -> String {
        let base = self
            .name
            .as_deref()
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| self.idx.to_string());
        if self.is_urgent {
            format!("{base}!")
        } else {
            base
        }
    }

    pub fn is_occupied(&self) -> bool {
        self.active_window_id.is_some()
    }
}

/// Parses the workspace list out of a `Request::Workspaces` reply.
pub fn parse_workspaces(reply: &Value) -> Vec<Workspace> {
    ok_field(reply, "Workspaces")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Workspace::from_json).collect())
        .unwrap_or_default()
}

/// A focused window as reported by `Request::FocusedWindow`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub id: u64,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub is_focused: bool,
}

impl Window {
    pub fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_u64()?,
            title: value
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned),
            app_id: value
                .get("app_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            is_focused: value
                .get("is_focused")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// Parses `{"Ok": {"FocusedWindow": {...}|null}}`.
pub fn parse_focused_window(reply: &Value) -> Option<Window> {
    ok_field(reply, "FocusedWindow").and_then(Window::from_json)
}

/// Parses `{"Ok": {"Windows": [...]}}`.
pub fn parse_windows(reply: &Value) -> Vec<Window> {
    ok_field(reply, "Windows")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Window::from_json).collect())
        .unwrap_or_default()
}

/// niri actions a panel row can trigger without invoking the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NiriVerb {
    FocusWorkspace,
    FocusWindow,
    FocusWorkspaceUp,
    FocusWorkspaceDown,
    FocusWorkspacePrevious,
}

impl NiriVerb {
    pub fn describe(self) -> &'static str {
        match self {
            Self::FocusWorkspace => "focus-workspace",
            Self::FocusWindow => "focus-window",
            Self::FocusWorkspaceUp => "focus-workspace-up",
            Self::FocusWorkspaceDown => "focus-workspace-down",
            Self::FocusWorkspacePrevious => "focus-workspace-previous",
        }
    }
}

/// Config form of a niri action:
///
/// ```json
/// { "niri": "focus-workspace", "index": 2 }
/// { "niri": "focus-workspace-down" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NiriAction {
    pub niri: NiriVerb,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
}

impl NiriAction {
    pub fn focus_index(index: u8) -> Self {
        Self {
            niri: NiriVerb::FocusWorkspace,
            index: Some(index),
            name: None,
            id: None,
        }
    }

    pub fn focus_id(id: u64) -> Self {
        Self {
            niri: NiriVerb::FocusWorkspace,
            index: None,
            name: None,
            id: Some(id),
        }
    }

    pub fn focus_window(id: u64) -> Self {
        Self {
            niri: NiriVerb::FocusWindow,
            index: None,
            name: None,
            id: Some(id),
        }
    }

    pub fn up() -> Self {
        Self {
            niri: NiriVerb::FocusWorkspaceUp,
            index: None,
            name: None,
            id: None,
        }
    }

    pub fn down() -> Self {
        Self {
            niri: NiriVerb::FocusWorkspaceDown,
            index: None,
            name: None,
            id: None,
        }
    }

    pub fn previous() -> Self {
        Self {
            niri: NiriVerb::FocusWorkspacePrevious,
            index: None,
            name: None,
            id: None,
        }
    }

    /// Builds the JSON `Request::Action` for this action.
    pub fn request(&self) -> Result<Value, String> {
        match self.niri {
            NiriVerb::FocusWorkspace => {
                let reference = if let Some(id) = self.id {
                    json!({ "Id": id })
                } else if let Some(index) = self.index {
                    json!({ "Index": index })
                } else if let Some(name) = &self.name {
                    json!({ "Name": name })
                } else {
                    return Err("niri focus-workspace needs `index`, `name` or `id`".to_owned());
                };
                Ok(json!({ "Action": { "FocusWorkspace": { "reference": reference } } }))
            }
            NiriVerb::FocusWindow => {
                let id = self
                    .id
                    .ok_or_else(|| "focus-window needs a window id".to_owned())?;
                Ok(json!({ "Action": { "FocusWindow": { "id": id } } }))
            }
            NiriVerb::FocusWorkspaceUp => Ok(json!({ "Action": { "FocusWorkspaceUp": {} } })),
            NiriVerb::FocusWorkspaceDown => Ok(json!({ "Action": { "FocusWorkspaceDown": {} } })),
            NiriVerb::FocusWorkspacePrevious => {
                Ok(json!({ "Action": { "FocusWorkspacePrevious": {} } }))
            }
        }
    }

    pub fn describe(&self) -> String {
        match self.niri {
            NiriVerb::FocusWorkspace => {
                if let Some(id) = self.id {
                    format!("{} id={id}", self.niri.describe())
                } else if let Some(index) = self.index {
                    format!("{} index={index}", self.niri.describe())
                } else if let Some(name) = &self.name {
                    format!("{} name={name}", self.niri.describe())
                } else {
                    self.niri.describe().to_owned()
                }
            }
            _ => self.niri.describe().to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    use serde_json::json;

    use super::*;

    #[test]
    fn builds_action_requests() {
        assert_eq!(
            NiriAction::focus_index(2).request().unwrap(),
            json!({ "Action": { "FocusWorkspace": { "reference": { "Index": 2 } } } })
        );
        assert_eq!(
            NiriAction::focus_id(42).request().unwrap(),
            json!({ "Action": { "FocusWorkspace": { "reference": { "Id": 42 } } } })
        );
        assert_eq!(
            NiriAction::down().request().unwrap(),
            json!({ "Action": { "FocusWorkspaceDown": {} } })
        );
        assert_eq!(
            NiriAction::previous().request().unwrap(),
            json!({ "Action": { "FocusWorkspacePrevious": {} } })
        );
        let empty = NiriAction {
            niri: NiriVerb::FocusWorkspace,
            index: None,
            name: None,
            id: None,
        };
        assert!(empty.request().is_err());
    }

    #[test]
    fn parses_workspaces_and_focused_window() {
        let reply = json!({
            "Ok": {
                "Workspaces": [
                    {
                        "id": 10,
                        "idx": 1,
                        "name": "web",
                        "output": "DP-1",
                        "is_active": true,
                        "is_focused": true,
                        "is_urgent": false,
                        "active_window_id": 7
                    },
                    { "id": 11, "idx": 2, "output": "DP-1" }
                ]
            }
        });
        let workspaces = parse_workspaces(&reply);
        assert_eq!(workspaces.len(), 2);
        assert_eq!(workspaces[0].label(), "web");
        assert!(workspaces[0].is_occupied());
        assert!(!workspaces[1].is_occupied());
        assert_eq!(workspaces[1].label(), "2");

        let window = json!({
            "Ok": {
                "FocusedWindow": { "id": 3, "title": "Notes", "app_id": "org.example.Notes" }
            }
        });
        let window = parse_focused_window(&window).unwrap();
        assert_eq!(window.title.as_deref(), Some("Notes"));
        assert_eq!(
            parse_focused_window(&json!({ "Ok": { "FocusedWindow": null } })),
            None
        );
        assert!(parse_workspaces(&json!({ "Err": "nope" })).is_empty());
    }

    #[test]
    fn talks_to_a_synthetic_socket() {
        let dir = std::env::temp_dir().join(format!("loom-sidepanels-ipc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("niri.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line.trim(), "\"Workspaces\"");
            stream.write_all(b"{\"Ok\":{\"Workspaces\":[]}}\n").unwrap();
        });
        let reply = request_at(&path, &json!("Workspaces")).unwrap();
        server.join().unwrap();
        assert_eq!(parse_workspaces(&reply).len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
