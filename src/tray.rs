//! StatusNotifierItem (SNI) tray host.
//!
//! A background thread registers as a host with `org.kde.StatusNotifierWatcher`
//! and keeps a snapshot of the registered items (title, status, icon pixmap).
//! The panel reads the snapshot on its module tick; clicks call
//! `Activate`/`SecondaryActivate`/`ContextMenu` on the item.
//!
//! Pixmaps are converted from the protocol's network-order ARGB32 to Cairo's
//! native premultiplied ARGB32. Icon *names* are resolved from the common PNG
//! icon-theme locations; SVG-only icons fall back to the item title.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread;
use std::time::Duration;

use zbus::blocking::{Connection, Proxy};

const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_IFACE: &str = "org.kde.StatusNotifierWatcher";
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const ITEM_PATH: &str = "/StatusNotifierItem";

/// A decoded icon, Cairo `ARgb32` premultiplied, native byte order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pixmap {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, stride `width * 4`.
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub service: String,
    pub path: String,
    pub title: String,
    pub status: String,
    pub pixmap: Option<Pixmap>,
}

impl Item {
    /// Stable identifier used by the `tray-click` helper.
    pub fn key(&self) -> String {
        format!("{}{}", self.service, self.path)
    }
}

struct Store {
    items: Mutex<Vec<Item>>,
    started: AtomicBool,
    ready: Mutex<bool>,
    ready_changed: Condvar,
}

fn store() -> &'static Arc<Store> {
    static STORE: LazyLock<Arc<Store>> = LazyLock::new(|| {
        Arc::new(Store {
            items: Mutex::new(Vec::new()),
            started: AtomicBool::new(false),
            ready: Mutex::new(false),
            ready_changed: Condvar::new(),
        })
    });
    &STORE
}

/// Current items. Starts the D-Bus thread on first use.
pub fn snapshot() -> Vec<Item> {
    start();
    let store = store();
    // The first caller waits for the initial watcher query instead of
    // rendering an empty tray for one tick.
    if let Ok(ready) = store.ready.lock() {
        let _ = store
            .ready_changed
            .wait_timeout_while(ready, Duration::from_secs(2), |ready| !*ready);
    }
    store
        .items
        .lock()
        .map(|items| items.clone())
        .unwrap_or_default()
}

fn start() {
    let store = store();
    if store.started.swap(true, Ordering::SeqCst) {
        return;
    }
    let store = Arc::clone(store);
    thread::Builder::new()
        .name("sni-tray".to_owned())
        .spawn(move || run(&store))
        .ok();
}

fn run(store: &Arc<Store>) {
    loop {
        if let Err(err) = run_session(store) {
            if std::env::var_os("LOOM_SIDEPANELS_TRAY_DEBUG").is_some() {
                eprintln!("loom-sidepanels: tray: {err}");
            }
            thread::sleep(Duration::from_secs(10));
        }
    }
}

fn run_session(store: &Arc<Store>) -> Result<(), String> {
    let connection =
        Connection::session().map_err(|err| format!("cannot connect to the session bus: {err}"))?;
    let watcher = Proxy::new(&connection, WATCHER, WATCHER_PATH, WATCHER_IFACE)
        .map_err(|err| format!("cannot create the StatusNotifierWatcher proxy: {err}"))?;
    let unique = connection
        .unique_name()
        .map(|name| name.to_string())
        .ok_or_else(|| "the session bus has no unique name".to_owned())?;
    watcher
        .call::<_, _, ()>("RegisterStatusNotifierHost", &(unique.as_str(),))
        .map_err(|err| format!("cannot register as a StatusNotifier host: {err}"))?;

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    for signal in [
        "StatusNotifierItemRegistered",
        "StatusNotifierItemUnregistered",
    ] {
        let tx = tx.clone();
        let _ = thread::Builder::new()
            .name(format!("sni-{signal}"))
            .spawn(move || listen(signal, tx));
    }
    drop(tx);

    refresh(store, &connection, &watcher);
    if let Ok(mut ready) = store.ready.lock() {
        *ready = true;
        store.ready_changed.notify_all();
    }
    loop {
        // Signals wake us early; the timeout also refreshes titles/status that
        // are not covered by the watcher's registration signals.
        let _ = rx.recv_timeout(Duration::from_secs(30));
        refresh(store, &connection, &watcher);
    }
}

fn listen(signal: &str, tx: std::sync::mpsc::Sender<()>) {
    let Ok(connection) = Connection::session() else {
        return;
    };
    let Ok(watcher) = Proxy::new(&connection, WATCHER, WATCHER_PATH, WATCHER_IFACE) else {
        return;
    };
    let Ok(signals) = watcher.receive_signal(signal) else {
        return;
    };
    for _ in signals {
        if tx.send(()).is_err() {
            return;
        }
    }
}

fn refresh(store: &Store, connection: &Connection, watcher: &Proxy<'_>) {
    let names: Vec<String> = watcher
        .get_property("RegisteredStatusNotifierItems")
        .unwrap_or_default();
    let mut items = Vec::with_capacity(names.len());
    for name in names {
        if let Some(item) = read_item(connection, &name) {
            items.push(item);
        }
    }
    items.sort_by(|a, b| a.service.cmp(&b.service).then(a.path.cmp(&b.path)));
    if let Ok(mut current) = store.items.lock() {
        *current = items;
    }
}

fn split_name(name: &str) -> (String, String) {
    match name.split_once('/') {
        Some((service, path)) if !service.is_empty() => (service.to_owned(), format!("/{path}")),
        _ => (name.to_owned(), ITEM_PATH.to_owned()),
    }
}

fn read_item(connection: &Connection, name: &str) -> Option<Item> {
    let (service, path) = split_name(name);
    let proxy = Proxy::new(connection, service.as_str(), path.as_str(), ITEM_IFACE).ok()?;
    let title: String = proxy
        .get_property("Title")
        .or_else(|_| proxy.get_property("Id"))
        .unwrap_or_default();
    let status: String = proxy.get_property("Status").unwrap_or_default();
    let pixmap = proxy
        .get_property::<Vec<(i32, i32, Vec<u8>)>>("IconPixmap")
        .ok()
        .and_then(|pixmaps| pick_pixmap(&pixmaps))
        .or_else(|| {
            proxy
                .get_property::<String>("IconName")
                .ok()
                .and_then(|name| load_named_icon(&name))
        });
    drop(proxy);
    Some(Item {
        service,
        path,
        title,
        status,
        pixmap,
    })
}

/// Picks the largest pixmap up to 32x32, or the smallest larger one.
fn pick_pixmap(pixmaps: &[(i32, i32, Vec<u8>)]) -> Option<Pixmap> {
    pixmaps
        .iter()
        .filter(|(width, height, data)| {
            *width > 0 && *height > 0 && data.len() >= (*width as usize) * (*height as usize) * 4
        })
        .min_by_key(|(width, height, _)| {
            let side = (*width).max(*height);
            (side > 32, if side > 32 { side } else { 32 - side })
        })
        .and_then(|(width, height, data)| convert_pixmap(*width as u32, *height as u32, data))
}

/// Converts protocol ARGB32 (network order, straight alpha) to Cairo
/// `ARgb32` premultiplied native order.
pub fn convert_pixmap(width: u32, height: u32, data: &[u8]) -> Option<Pixmap> {
    let len = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if len == 0 || data.len() < len {
        return None;
    }
    let mut out = vec![0u8; len];
    let (sources, _) = data.as_chunks::<4>();
    let (targets, _) = out.as_chunks_mut::<4>();
    for (source, target) in sources.iter().zip(targets.iter_mut()) {
        let a = source[0] as u32;
        let r = source[1] as u32 * a / 255;
        let g = source[2] as u32 * a / 255;
        let b = source[3] as u32 * a / 255;
        target[0] = b as u8;
        target[1] = g as u8;
        target[2] = r as u8;
        target[3] = a as u8;
    }
    Some(Pixmap {
        width,
        height,
        data: out,
    })
}

fn load_named_icon(name: &str) -> Option<Pixmap> {
    static CACHE: LazyLock<Mutex<HashMap<String, Option<Pixmap>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = CACHE.lock()
        && let Some(hit) = cache.get(name)
    {
        return hit.clone();
    }
    let pixmap = find_icon_png(name).and_then(|path| load_png(&path));
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(name.to_owned(), pixmap.clone());
    }
    pixmap
}

fn find_icon_png(name: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(name);
    if direct.is_absolute() && direct.is_file() {
        return Some(direct);
    }
    let file = format!("{name}.png");
    let svg = format!("{name}.svg");
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        roots.push(PathBuf::from(data).join("icons"));
    } else if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".local/share/icons"));
        roots.push(home.join(".icons"));
    }
    let data_dirs =
        std::env::var_os("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    roots.extend(std::env::split_paths(&data_dirs).map(|dir| dir.join("icons")));
    roots.push(PathBuf::from("/usr/share/pixmaps"));

    for root in &roots {
        if root.ends_with("pixmaps") {
            let candidate = root.join(&file);
            if candidate.is_file() {
                return Some(candidate);
            }
            continue;
        }
        for theme in ["hicolor", "Adwaita", "breeze", "Papirus", "Papirus-Dark"] {
            for size in [
                "16x16", "22x22", "24x24", "32x32", "48x48", "64x64", "symbolic",
            ] {
                let candidate = root.join(theme).join(size).join("apps").join(&file);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        if let Some(found) = search_tree(root, &file, 4) {
            return Some(found);
        }
        if let Some(found) = search_tree(root, &svg, 4) {
            return Some(found);
        }
    }
    None
}

fn search_tree(dir: &Path, file: &str, depth: u8) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if path.file_name().is_some_and(|name| name == file) {
                return Some(path);
            }
        } else if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.into_iter()
        .find_map(|dir| search_tree(&dir, file, depth - 1))
}

/// Renders an SVG through `rsvg-convert` at icon size. SVG-only themes are
/// common, and librsvg is already present wherever a full desktop theme is.
fn render_svg(path: &Path) -> Option<Vec<u8>> {
    let output = std::process::Command::new("rsvg-convert")
        .args(["-w", "32", "-h", "32"])
        .arg(path)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn load_png(path: &Path) -> Option<Pixmap> {
    let bytes = if path.extension().and_then(|ext| ext.to_str()) == Some("svg") {
        render_svg(path)?
    } else {
        std::fs::read(path).ok()?
    };
    let mut slice = bytes.as_slice();
    let mut surface = cairo::ImageSurface::create_from_png(&mut slice).ok()?;
    let width = surface.width() as u32;
    let height = surface.height() as u32;
    let stride = surface.stride() as usize;
    let data = surface.data().ok()?;
    let row = (width as usize) * 4;
    let mut out = Vec::with_capacity(row * height as usize);
    for line in 0..height as usize {
        let start = line * stride;
        out.extend_from_slice(data.get(start..start + row)?);
    }
    if out.len() != row * height as usize {
        return None;
    }
    Some(Pixmap {
        width,
        height,
        data: out,
    })
}

/// Calls the item's activation method. `button` is `left`, `right` or `middle`.
pub fn click(service: &str, path: &str, button: &str) -> Result<(), String> {
    let connection =
        Connection::session().map_err(|err| format!("cannot connect to the session bus: {err}"))?;
    let proxy = Proxy::new(&connection, service, path, ITEM_IFACE)
        .map_err(|err| format!("cannot create the item proxy: {err}"))?;
    let (method, args): (&str, (i32, i32)) = match button {
        "right" => ("ContextMenu", (0, 0)),
        "middle" => ("SecondaryActivate", (0, 0)),
        _ => ("Activate", (0, 0)),
    };
    proxy
        .call::<_, _, ()>(method, &args)
        .map_err(|err| format!("{method} failed: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_network_argb_to_cairo_premultiplied() {
        // Opaque red: A=255, R=255, G=0, B=0 in network order.
        let pixmap = convert_pixmap(1, 1, &[255, 255, 0, 0]).unwrap();
        assert_eq!(pixmap.data, vec![0, 0, 255, 255]);

        // Half-transparent white premultiplies each channel.
        let pixmap = convert_pixmap(1, 1, &[128, 255, 255, 255]).unwrap();
        assert_eq!(pixmap.data, vec![128, 128, 128, 128]);
    }

    #[test]
    fn rejects_truncated_pixmaps() {
        assert!(convert_pixmap(2, 2, &[0; 15]).is_none());
        assert!(convert_pixmap(0, 1, &[]).is_none());
    }

    #[test]
    fn picks_a_readable_size() {
        let small = (16, 16, vec![255u8; 16 * 16 * 4]);
        let large = (48, 48, vec![255u8; 48 * 48 * 4]);
        let pixmap = pick_pixmap(&[small, large]).unwrap();
        assert_eq!((pixmap.width, pixmap.height), (16, 16));
        let tiny = (8, 8, vec![255u8; 8 * 8 * 4]);
        let large = (22, 22, vec![255u8; 22 * 22 * 4]);
        let pixmap = pick_pixmap(&[tiny, large]).unwrap();
        assert_eq!((pixmap.width, pixmap.height), (22, 22));
    }

    #[test]
    fn parses_item_names() {
        assert_eq!(
            split_name("org.kde.StatusNotifierItem-1-1/StatusNotifierItem"),
            (
                "org.kde.StatusNotifierItem-1-1".to_owned(),
                "/StatusNotifierItem".to_owned()
            )
        );
        assert_eq!(
            split_name(":1.42"),
            (":1.42".to_owned(), "/StatusNotifierItem".to_owned())
        );
    }
}
