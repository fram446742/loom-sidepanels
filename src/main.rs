//! `loom-sidepanels`: the Wayland layer-shell daemon.
//!
//! Creates persistent panels per output (one per configured panel definition),
//! with an exclusive zone so tiled windows do not overlap them. Rows can be
//! clicked or scrolled; module rows refresh on their interval. The config file
//! is watched and reloaded in place, without restarting the layer surfaces when
//! the panel structure is unchanged.

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result};
use calloop::EventLoop;
use calloop::timer::{TimeoutAction, Timer};
use calloop_wayland_source::WaylandSource;
use loom_sidepanels::niri::socket_path;
use loom_sidepanels::{
    ActionSpec, Config, ConfigWatcher, Panel, PanelConfig, PeekState, ReloadSignal,
    ScrollDirection, SessionState, Side, config_path, session,
};
use loom_state::StateClient;
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::pointer::{
    AxisScroll, PointerEvent, PointerEventKind, PointerHandler,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use wayland_client::{Connection, QueueHandle};

const BTN_LEFT: u32 = 0x110;
const TICK: Duration = Duration::from_millis(250);

struct PanelSurface {
    output: wl_output::WlOutput,
    layer: LayerSurface,
    pool: SlotPool,
    panel: Panel,
    size: (u32, u32),
    configured: bool,
    hovered: Option<usize>,
    peek: PeekState,
    output_name: Option<String>,
}

struct Sidepanels {
    config: Config,
    registry_state: RegistryState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    seat_state: SeatState,
    output_state: OutputState,
    surfaces: Vec<PanelSurface>,
    pointer: Option<wl_pointer::WlPointer>,
    watcher: ConfigWatcher,
    session: SessionState,
    session_path: PathBuf,
    reload: ReloadSignal,
    state_client: Option<StateClient>,
    exit: bool,
}

fn usage() -> &'static str {
    "loom-sidepanels [--config PATH] [--profile NAME] [--check|--validate|--init]\n\n\
     Creates persistent layer-shell panels on every output. Without a config\n\
     file, a default bar (workspaces, window title, media, cpu, memory, battery,\n\
     network, volume, clock) is used.\n\n\
     --profile NAME  Start with a named layout from `profiles`/`layouts`\n\
                     (also remembered across runs). `--layout` is an alias.\n\
     --validate      Parse and validate the configuration, then exit.\n\
     --check         Print the loaded configuration and exit.\n\
     --init          Write the default configuration to PATH and exit.\n\n\
     Config defaults to $LOOM_SIDEPANELS_CONFIG, then\n\
     $XDG_CONFIG_HOME/loom-sidepanels/config.json. Runtime state defaults to\n\
     $LOOM_SIDEPANELS_STATE, then\n\
     $XDG_STATE_HOME/loom-sidepanels/state.json. SIGHUP reloads the config."
}

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("tray-list") {
        let items: Vec<_> = loom_sidepanels::tray::snapshot()
            .into_iter()
            .map(|item| {
                serde_json::json!({
                    "service": item.service,
                    "path": item.path,
                    "title": item.title,
                    "status": item.status,
                    "icon": item.pixmap.as_ref().map(|pixmap| [pixmap.width, pixmap.height]),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }
    if argv.first().map(String::as_str) == Some("tray-click") {
        let (_, service, path, button) = match argv.as_slice() {
            [_, service, path, button] => ("tray-click", service, path, button),
            [_, service, path] => ("tray-click", service, path, &"left".to_owned()),
            _ => anyhow::bail!("tray-click needs SERVICE PATH [left|right|middle]"),
        };
        loom_sidepanels::tray::click(service, path, button).map_err(anyhow::Error::msg)?;
        return Ok(());
    }

    let mut path = std::env::var_os("LOOM_SIDEPANELS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(config_path);
    let mut check = false;
    let mut validate = false;
    let mut init = false;
    let mut profile = None;
    let mut args = argv.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                path = PathBuf::from(args.next().context("--config needs a path")?);
            }
            "--profile" | "--layout" => {
                profile = Some(args.next().context("--profile needs a name")?);
            }
            "--check" => check = true,
            "--validate" => validate = true,
            "--init" => init = true,
            "--version" | "-V" => {
                println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                print!("{}", usage());
                return Ok(());
            }
            other => anyhow::bail!("unknown argument `{other}`\n\n{}", usage()),
        }
    }

    if init {
        let config = Config::default();
        config.save(&path).map_err(anyhow::Error::msg)?;
        println!("wrote {}", path.display());
        return Ok(());
    }
    let config = Config::load(&path).map_err(anyhow::Error::msg)?;
    let state_path = std::env::var_os("LOOM_SIDEPANELS_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(session::state_path);
    let mut session = SessionState::load(&state_path).unwrap_or_else(|err| {
        eprintln!("loom-sidepanels: ignoring session state: {err}");
        SessionState::default()
    });
    let explicit_profile = profile.clone();
    let wanted = profile.as_deref().or(session.profile.as_deref());
    let mut config = config;
    if let Some(name) = wanted {
        if config.select_profile(name) {
            session.profile = Some(name.to_owned());
        } else if explicit_profile.is_some() {
            anyhow::bail!(
                "unknown layout `{name}` (known: {})",
                config.profile_names().join(", ")
            );
        } else {
            eprintln!(
                "loom-sidepanels: session layout `{name}` no longer exists; using the default"
            );
            session.profile = None;
        }
    }
    if validate {
        match config.validate() {
            Ok(()) => println!("{}: ok", path.display()),
            Err(errors) => anyhow::bail!(
                "{}: {} problem(s)\n  {}",
                path.display(),
                errors.len(),
                errors.join("\n  ")
            ),
        }
        return Ok(());
    }
    if check {
        println!("{}", serde_json::to_string_pretty(&config)?);
        return Ok(());
    }
    config
        .validate()
        .map_err(|errors| anyhow::anyhow!("{}", errors.join("\n")))?;
    run(config, path, state_path, session)
}

fn run(
    config: Config,
    config_path: PathBuf,
    session_path: PathBuf,
    session: SessionState,
) -> Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<Sidepanels>(&conn)?;
    let qh = queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    let mut state = Sidepanels {
        config,
        registry_state: RegistryState::new(&globals),
        compositor,
        layer_shell,
        shm,
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        surfaces: Vec::new(),
        pointer: None,
        watcher: ConfigWatcher::new(config_path),
        session,
        session_path,
        reload: ReloadSignal::install(),
        state_client: socket_path().map(|_| StateClient::connect().ok()).flatten(),
        exit: false,
    };
    if let Err(err) = state.save_session() {
        eprintln!("loom-sidepanels: cannot save session state: {err}");
    }

    queue.roundtrip(&mut state)?;
    let outputs: Vec<_> = state.output_state.outputs().collect();
    for output in outputs {
        // `new_output` may already have created this output's panels during the
        // roundtrip; only fill in outputs that are still missing.
        if !state.has_surfaces_for(&output) {
            state.create_surfaces_for_output(&qh, output);
        }
    }
    state.refresh_all_with_snapshot();
    queue.roundtrip(&mut state)?;
    state.redraw_all(&qh);

    if socket_path().is_none() {
        eprintln!(
            "loom-sidepanels: NIRI_SOCKET is not set; workspaces and window-title modules \
             will stay blank"
        );
    }

    let mut event_loop: EventLoop<Sidepanels> =
        EventLoop::try_new().map_err(|err| anyhow::anyhow!("calloop: {err}"))?;
    let handle = event_loop.handle();
    let signal = event_loop.get_signal();

    // Module refresh and config-watch tick; a bounded timer wakes the loop
    // while the panel idles.
    let qh_tick = qh.clone();
    handle
        .insert_source(
            Timer::from_duration(TICK),
            move |_, _, state: &mut Sidepanels| {
                let mut redraw = false;
                let snapshot = state.state_client.as_ref().map(StateClient::snapshot);
                for surface in &mut state.surfaces {
                    if surface
                        .panel
                        .refresh_at_with_state(std::time::Instant::now(), snapshot.as_ref())
                    {
                        redraw = true;
                    }
                }
                if state.reload.take() {
                    state.reload_config(&qh_tick);
                    redraw = true;
                }
                if state.watcher.poll() {
                    state.reload(&qh_tick);
                    redraw = true;
                }
                if redraw {
                    state.redraw_all(&qh_tick);
                }
                TimeoutAction::ToDuration(TICK)
            },
        )
        .map_err(|err| anyhow::anyhow!("calloop timer: {err}"))?;

    WaylandSource::new(conn, queue)
        .insert(handle.clone())
        .map_err(|err| anyhow::anyhow!("calloop wayland source: {err}"))?;

    event_loop
        .run(None, &mut state, move |state| {
            if state.exit {
                signal.stop();
            }
        })
        .map_err(|err| anyhow::anyhow!("calloop: {err}"))?;
    Ok(())
}

/// Applies side, anchor, size, margin, exclusive zone and peek state to a
/// layer surface.
fn configure_layer(layer: &LayerSurface, config: &PanelConfig, hidden: bool) {
    let anchor = match config.side {
        Side::Left => Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM,
        Side::Right => Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM,
        Side::Top => Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
        Side::Bottom => Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
    };
    let anchor = if config.side.is_vertical() && config.height.is_some() {
        // A fixed-height vertical panel hangs from the top instead of spanning
        // the whole output.
        anchor & !Anchor::BOTTOM
    } else {
        anchor
    };
    layer.set_anchor(anchor);
    layer.set_exclusive_zone(config.exclusive_zone_with(hidden) as i32);
    let reveal = config.reveal;
    match config.side {
        Side::Left | Side::Right => {
            let width = if hidden { reveal.max(1) } else { config.width };
            let height = config.height.unwrap_or(0);
            layer.set_size(width, height);
        }
        Side::Top | Side::Bottom => {
            let height = if hidden {
                reveal.max(1)
            } else {
                config.bar_height()
            };
            layer.set_size(0, height);
        }
    }
    let margin = config.margin as i32;
    let (top, right, bottom, left) = match config.side {
        Side::Left => (0, 0, 0, margin),
        Side::Right => (0, margin, 0, 0),
        Side::Top => (margin, 0, 0, 0),
        Side::Bottom => (0, 0, margin, 0),
    };
    layer.set_margin(top, right, bottom, left);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
}

impl Sidepanels {
    fn save_session(&self) -> Result<(), String> {
        self.session.save(&self.session_path)
    }

    /// Re-reads the config file, keeping the active layout if it still exists.
    fn reload_config(&mut self, qh: &QueueHandle<Self>) {
        let path = self.watcher.path().to_path_buf();
        match Config::load(&path) {
            Ok(mut config) => {
                if let Some(name) = self.session.profile.as_deref()
                    && config.profiles.contains_key(name)
                {
                    config.active_profile = Some(name.to_owned());
                }
                match config.validate() {
                    Ok(()) => self.apply_config(config, qh),
                    Err(errors) => eprintln!(
                        "loom-sidepanels: keeping previous config, {} problem(s):\n  {}",
                        errors.len(),
                        errors.join("\n  ")
                    ),
                }
            }
            Err(err) => eprintln!("loom-sidepanels: keeping previous config: {err}"),
        }
    }
    fn output_name(&self, output: &wl_output::WlOutput) -> Option<String> {
        self.output_state
            .info(output)
            .and_then(|info| info.name.clone())
    }

    fn create_surfaces_for_output(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let name = self.output_name(&output);
        let panels = self.config.panels_for(name.as_deref().unwrap_or(""));
        for config in panels {
            self.create_surface(qh, output.clone(), name.clone(), config);
        }
    }

    fn has_surfaces_for(&self, output: &wl_output::WlOutput) -> bool {
        self.surfaces
            .iter()
            .any(|surface| surface.output == *output)
    }

    fn create_surface(
        &mut self,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
        output_name: Option<String>,
        config: PanelConfig,
    ) {
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Top,
            Some("loom-sidepanels"),
            Some(&output),
        );
        let visible = output_name
            .as_deref()
            .is_some_and(|name| self.session.is_visible(name));
        let hidden = config.auto_hide && !visible;
        configure_layer(&layer, &config, hidden);
        layer.commit();

        let pool_size = (config.width as usize * 64).max(4096);
        let pool = match SlotPool::new(pool_size, &self.shm) {
            Ok(pool) => pool,
            Err(err) => {
                eprintln!("loom-sidepanels: shm pool: {err}");
                return;
            }
        };
        let rules = self.config.rules.clone();
        let panel = match output_name.as_deref() {
            Some(name) => Panel::new(config).with_output(name).with_rules(rules),
            None => Panel::new(config).with_rules(rules),
        };
        self.surfaces.push(PanelSurface {
            output,
            layer,
            pool,
            panel,
            size: (0, 0),
            configured: false,
            hovered: None,
            peek: PeekState::new(hidden),
            output_name,
        });
    }

    fn refresh_all(&mut self) {
        self.refresh_all_with_snapshot();
    }

    fn refresh_all_with_snapshot(&mut self) {
        let snapshot = self.state_client.as_ref().map(StateClient::snapshot);
        for surface in &mut self.surfaces {
            surface.panel.refresh_all_with_state(
                &mut loom_sidepanels::system::RealEnv::new(),
                snapshot.as_ref(),
            );
        }
    }

    fn reload(&mut self, qh: &QueueHandle<Self>) {
        self.reload_config(qh);
    }

    fn apply_config(&mut self, config: Config, qh: &QueueHandle<Self>) {
        let outputs: Vec<_> = self.output_state.outputs().collect();
        let mut layout = Vec::new();
        for output in &outputs {
            let name = self.output_name(output);
            for panel in config.panels_for(name.as_deref().unwrap_or("")) {
                layout.push((output.clone(), panel));
            }
        }
        let same_structure = layout.len() == self.surfaces.len()
            && layout
                .iter()
                .zip(&self.surfaces)
                .all(|((output, panel), surface)| {
                    surface.output == *output
                        && surface.panel.config.side == panel.side
                        && surface.panel.config.output == panel.output
                        && surface.panel.config.width == panel.width
                        && surface.panel.config.height == panel.height
                        && surface.panel.config.margin == panel.margin
                        && surface.panel.config.auto_hide == panel.auto_hide
                        && surface.panel.config.reveal == panel.reveal
                        && surface.panel.config.floating == panel.floating
                });
        if same_structure {
            // Keep the layer surfaces; swap config and refresh modules.
            let rules = config.rules.clone();
            for ((_, panel), surface) in layout.into_iter().zip(&mut self.surfaces) {
                surface.panel.set_rules(rules.clone());
                surface.panel.set_config(panel);
                surface.panel.refresh_all();
            }
            self.config = config;
            self.redraw_all(qh);
            return;
        }
        // Panels were added/removed or moved to another edge: recreate surfaces.
        self.surfaces.clear();
        self.config = config;
        for output in outputs {
            self.create_surfaces_for_output(qh, output);
        }
        self.refresh_all();
        self.redraw_all(qh);
    }

    fn redraw_all(&mut self, qh: &QueueHandle<Self>) {
        for index in 0..self.surfaces.len() {
            self.draw(qh, index);
        }
    }

    fn draw(&mut self, qh: &QueueHandle<Self>, index: usize) {
        let Some(surface) = self.surfaces.get_mut(index) else {
            return;
        };
        if !surface.configured {
            return;
        }
        let (mut width, mut height) = surface.size;
        if width == 0 {
            width = surface.panel.config.width;
        }
        if height == 0 {
            height = if surface.panel.config.side.is_vertical() {
                1
            } else {
                surface.panel.config.bar_height()
            };
        }
        if width == 0 || height == 0 {
            return;
        }
        let image = if surface.peek.hidden() {
            surface.panel.render_strip(width, height)
        } else {
            surface.panel.render_with(width, height, surface.hovered)
        };
        let image = match image {
            Ok(image) => image,
            Err(err) => {
                eprintln!("loom-sidepanels: render: {err}");
                return;
            }
        };
        let stride = width as i32 * 4;
        let (buffer, canvas) = match surface.pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) {
            Ok(buffer) => buffer,
            Err(err) => {
                eprintln!("loom-sidepanels: buffer: {err}");
                return;
            }
        };
        let len = canvas.len().min(image.data.len());
        canvas[..len].copy_from_slice(&image.data[..len]);

        let wl_surface = surface.layer.wl_surface();
        wl_surface.damage_buffer(0, 0, width as i32, height as i32);
        if buffer.attach_to(wl_surface).is_ok() {
            surface.layer.commit();
        }
        let _ = qh;
    }

    /// Shows or hides an auto-hide panel and persists the choice for the run.
    fn set_peek(&mut self, index: usize, hidden: bool) {
        let Some(surface) = self.surfaces.get_mut(index) else {
            return;
        };
        if !surface.panel.config.auto_hide || !surface.peek.set_hidden(hidden) {
            return;
        }
        let config = surface.panel.config.clone();
        configure_layer(&surface.layer, &config, hidden);
        surface.layer.commit();
        surface.configured = false;
        surface.size = (0, 0);
        let output = surface.output_name.clone();
        if let Some(output) = output {
            self.session.set_visible(output, !hidden);
            if let Err(err) = self.save_session() {
                eprintln!("loom-sidepanels: cannot save session state: {err}");
            }
        }
    }

    /// Switches to a named layout/profile and recreates the surfaces.
    fn switch_layout(&mut self, qh: &QueueHandle<Self>, name: &str) {
        if !self.config.select_profile(name) {
            eprintln!(
                "loom-sidepanels: unknown layout `{name}` (known: {})",
                self.config.profile_names().join(", ")
            );
            return;
        }
        self.session.profile = Some(name.to_owned());
        if let Err(err) = self.save_session() {
            eprintln!("loom-sidepanels: cannot save session state: {err}");
        }
        let config = self.config.clone();
        self.apply_config(config, qh);
    }

    fn index_of(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|entry| entry.layer.wl_surface() == surface)
    }

    fn surface_size(&self, index: usize) -> (u32, u32) {
        let Some(surface) = self.surfaces.get(index) else {
            return (0, 0);
        };
        let (mut width, mut height) = surface.size;
        if width == 0 {
            width = surface.panel.config.width;
        }
        if height == 0 {
            height = surface.panel.config.bar_height();
        }
        (width, height)
    }

    /// Action under a pointer position on one surface.
    fn action_at(&self, index: usize, x: f64, y: f64) -> Option<(usize, ActionSpec)> {
        let surface = self.surfaces.get(index)?;
        let (width, height) = self.surface_size(index);
        let map = surface.panel.interaction_map(width, height).ok()?;
        let region = map.at(x, y)?;
        Some((region.row, region.action.clone()?))
    }

    fn run_surface_action(&mut self, qh: &QueueHandle<Self>, index: usize, action: &ActionSpec) {
        if let Some(name) = action.layout_name() {
            let name = name.to_owned();
            self.switch_layout(qh, &name);
            return;
        }
        let result = self
            .surfaces
            .get(index)
            .map(|surface| surface.panel.run_action(action));
        match result {
            Some(Ok(())) => {
                if action.is_niri()
                    && let Some(surface) = self.surfaces.get_mut(index)
                {
                    // Workspaces changed: repaint from fresh IPC data.
                    surface.panel.refresh_all();
                }
                self.draw(qh, index);
            }
            Some(Err(err)) => eprintln!("loom-sidepanels: {err}"),
            None => {}
        }
    }
}

fn scroll_amount(axis: &AxisScroll) -> i32 {
    if axis.value120 != 0 {
        axis.value120
    } else if axis.discrete != 0 {
        axis.discrete
    } else {
        axis.absolute.signum() as i32
    }
}

impl CompositorHandler for Sidepanels {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if let Some(index) = self.index_of(surface) {
            self.draw(qh, index);
        }
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Sidepanels {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.create_surfaces_for_output(qh, output);
        self.redraw_all(qh);
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.surfaces.retain(|surface| surface.output != output);
    }
}

impl LayerShellHandler for Sidepanels {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        // A dismissed surface is gone for good; drop it and stop when nothing
        // is left. Config reloads never take this path.
        if let Some(index) = self.index_of(layer.wl_surface()) {
            self.surfaces.remove(index);
        }
        if self.surfaces.is_empty() {
            self.exit = true;
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let Some(index) = self.index_of(layer.wl_surface()) else {
            return;
        };
        let vertical = self.surfaces[index].panel.config.side.is_vertical();
        let hidden = self.surfaces[index].peek.hidden();
        let width = NonZeroU32::new(configure.new_size.0)
            .map(NonZeroU32::get)
            .unwrap_or_else(|| {
                if vertical {
                    if hidden {
                        self.surfaces[index].panel.config.reveal.max(1)
                    } else {
                        self.surfaces[index].panel.config.width
                    }
                } else {
                    0
                }
            });
        let height = NonZeroU32::new(configure.new_size.1)
            .map(NonZeroU32::get)
            .unwrap_or_else(|| {
                if vertical {
                    self.surfaces[index]
                        .panel
                        .config
                        .height
                        .filter(|height| *height > 0)
                        .unwrap_or(1)
                } else if hidden {
                    self.surfaces[index].panel.config.reveal.max(1)
                } else {
                    self.surfaces[index].panel.config.bar_height()
                }
            });
        self.surfaces[index].size = (width, height);
        self.surfaces[index].configured = true;
        self.draw(qh, index);
    }
}

impl SeatHandler for Sidepanels {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(pointer) => self.pointer = Some(pointer),
                Err(err) => eprintln!("loom-sidepanels: pointer: {err}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer
            && let Some(pointer) = self.pointer.take()
        {
            pointer.release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for Sidepanels {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let Some(index) = self.index_of(&event.surface) else {
                continue;
            };
            let (x, y) = event.position;
            match event.kind {
                PointerEventKind::Leave { .. } => {
                    let hovered = self.surfaces[index].hovered.take().is_some();
                    if hovered {
                        self.draw(qh, index);
                    }
                    self.set_peek(index, true);
                }
                PointerEventKind::Enter { .. } => {
                    self.set_peek(index, false);
                    let (width, height) = self.surface_size(index);
                    let hovered = self.surfaces[index]
                        .panel
                        .interaction_map(width, height)
                        .ok()
                        .and_then(|map| map.row_at(x, y));
                    if self.surfaces[index].hovered != hovered {
                        self.surfaces[index].hovered = hovered;
                        self.draw(qh, index);
                    }
                }
                PointerEventKind::Motion { .. } => {
                    let (width, height) = self.surface_size(index);
                    let hovered = self.surfaces[index]
                        .panel
                        .interaction_map(width, height)
                        .ok()
                        .and_then(|map| map.row_at(x, y));
                    if self.surfaces[index].hovered != hovered {
                        self.surfaces[index].hovered = hovered;
                        self.draw(qh, index);
                    }
                }
                PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                    self.set_peek(index, false);
                    if let Some((_, action)) = self.action_at(index, x, y) {
                        self.run_surface_action(qh, index, &action);
                    }
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    let delta = if !vertical.is_none() {
                        scroll_amount(&vertical)
                    } else {
                        scroll_amount(&horizontal)
                    };
                    if delta == 0 {
                        continue;
                    }
                    let direction = if delta < 0 {
                        ScrollDirection::Up
                    } else {
                        ScrollDirection::Down
                    };
                    let (width, height) = self.surface_size(index);
                    let row = self.surfaces[index]
                        .panel
                        .interaction_map(width, height)
                        .ok()
                        .and_then(|map| map.row_at(x, y));
                    let Some(row) = row else {
                        continue;
                    };
                    if let Some(action) = self.surfaces[index].panel.scroll_action(row, direction) {
                        self.run_surface_action(qh, index, &action);
                    }
                }
                _ => {}
            }
        }
    }
}

impl ShmHandler for Sidepanels {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Sidepanels {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

delegate_registry!(Sidepanels);
smithay_client_toolkit::delegate_dispatch2!(Sidepanels);
