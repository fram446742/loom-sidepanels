//! Everything the panel modules touch outside the process.
//!
//! Modules never reach for the filesystem or a child process directly: they go
//! through [`Env`]. That keeps every formatter testable with synthetic inputs and
//! lets the Wayland daemon share one implementation.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Side effects used by module evaluation and click actions.
pub trait Env {
    /// Runs `sh -c command` and returns its first output line, trimmed.
    fn command_line(&mut self, command: &str) -> Option<String>;

    /// Spawns `sh -c command` detached from the panel.
    fn spawn(&mut self, command: &str) -> Result<(), String>;

    /// Reads a UTF-8 file, e.g. a `/proc` or `/sys` entry.
    fn read_file(&mut self, path: &str) -> Option<String>;

    /// Lists directory entry names, e.g. `/sys/class/power_supply`.
    fn list_dir(&mut self, path: &str) -> Vec<String>;

    /// Sends one request to niri's IPC socket and returns the reply.
    fn niri(&mut self, request: &Value) -> Option<Value>;

    /// Seconds since the Unix epoch.
    fn unix_time(&mut self) -> i64;

    /// UTC offset in seconds that applies at `unix`.
    fn utc_offset_secs(&mut self, unix: i64) -> i64;
}

/// Environment backed by the real system.
#[derive(Debug, Default)]
pub struct RealEnv;

impl RealEnv {
    pub fn new() -> Self {
        Self
    }
}

impl Env for RealEnv {
    fn command_line(&mut self, command: &str) -> Option<String> {
        let output = Command::new("sh").arg("-c").arg(command).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        text.lines().next().map(|line| line.trim().to_owned())
    }

    fn spawn(&mut self, command: &str) -> Result<(), String> {
        Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|err| format!("cannot run `{command}`: {err}"))
    }

    fn read_file(&mut self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn list_dir(&mut self, path: &str) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(Path::new(path)) else {
            return Vec::new();
        };
        let mut entries: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        entries
    }

    fn niri(&mut self, request: &Value) -> Option<Value> {
        crate::niri::request(request)
    }

    fn unix_time(&mut self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0)
    }

    fn utc_offset_secs(&mut self, unix: i64) -> i64 {
        local_offset_secs(unix)
    }
}

#[cfg(unix)]
fn local_offset_secs(unix: i64) -> i64 {
    let time = unix as libc::time_t;
    // SAFETY: `localtime_r` writes into a valid `tm` we own and does not retain
    // the pointer. The result is only used immediately.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() {
            0
        } else {
            tm.tm_gmtoff as i64
        }
    }
}

#[cfg(not(unix))]
fn local_offset_secs(_unix: i64) -> i64 {
    0
}

/// Reads one newline-terminated JSON reply from an already connected stream.
pub(crate) fn read_reply(stream: impl std::io::Read) -> std::io::Result<Value> {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(std::io::Error::other)
}

/// Writes one JSON request line.
pub(crate) fn write_request(mut stream: impl Write, request: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(request).map_err(std::io::Error::other)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.flush()
}

#[cfg(test)]
pub(crate) mod mock {
    //! Deterministic environment for model tests.

    use std::cell::RefCell;
    use std::collections::{BTreeMap, VecDeque};

    use serde_json::Value;

    use super::Env;

    #[derive(Debug, Default)]
    pub struct MockEnv {
        pub commands: BTreeMap<String, String>,
        pub files: BTreeMap<String, String>,
        pub dirs: BTreeMap<String, Vec<String>>,
        pub replies: BTreeMap<String, Value>,
        pub spawned: RefCell<Vec<String>>,
        pub niri_requests: RefCell<Vec<Value>>,
        pub unix: i64,
        pub offset: i64,
        pub command_queue: RefCell<VecDeque<Option<String>>>,
    }

    impl MockEnv {
        pub fn new(unix: i64) -> Self {
            Self {
                unix,
                ..Self::default()
            }
        }

        pub fn file(mut self, path: &str, contents: &str) -> Self {
            self.files.insert(path.to_owned(), contents.to_owned());
            self
        }

        pub fn dir(mut self, path: &str, entries: &[&str]) -> Self {
            self.dirs.insert(
                path.to_owned(),
                entries.iter().map(|entry| (*entry).to_owned()).collect(),
            );
            self
        }

        pub fn command(mut self, command: &str, output: &str) -> Self {
            self.commands.insert(command.to_owned(), output.to_owned());
            self
        }

        pub fn reply(mut self, request: &str, reply: Value) -> Self {
            self.replies.insert(request.to_owned(), reply);
            self
        }

        pub fn spawned(&self) -> Vec<String> {
            self.spawned.borrow().clone()
        }

        pub fn niri_requests(&self) -> Vec<Value> {
            self.niri_requests.borrow().clone()
        }
    }

    impl Env for MockEnv {
        fn command_line(&mut self, command: &str) -> Option<String> {
            if let Some(next) = self.command_queue.borrow_mut().pop_front() {
                return next;
            }
            self.commands.get(command).cloned()
        }

        fn spawn(&mut self, command: &str) -> Result<(), String> {
            self.spawned.borrow_mut().push(command.to_owned());
            Ok(())
        }

        fn read_file(&mut self, path: &str) -> Option<String> {
            self.files.get(path).cloned()
        }

        fn list_dir(&mut self, path: &str) -> Vec<String> {
            self.dirs.get(path).cloned().unwrap_or_default()
        }

        fn niri(&mut self, request: &Value) -> Option<Value> {
            self.niri_requests.borrow_mut().push(request.clone());
            let key = match request {
                Value::String(name) => name.clone(),
                other => other.to_string(),
            };
            self.replies.get(&key).cloned().or_else(|| {
                // Actions and unconfigured reads succeed by default.
                Some(serde_json::json!({ "Ok": "Handled" }))
            })
        }

        fn unix_time(&mut self) -> i64 {
            self.unix
        }

        fn utc_offset_secs(&mut self, _unix: i64) -> i64 {
            self.offset
        }
    }
}
