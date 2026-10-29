// SPDX-License-Identifier: MPL-2.0

mod colored;
mod draw;
mod fade;
mod img_source;
mod scaler;
mod wallpaper;

/// Access glibc malloc tunables.
#[cfg(target_env = "gnu")]
mod malloc {
    use std::os::raw::c_int;
    const M_MMAP_THRESHOLD: c_int = -3;

    unsafe extern "C" {
        fn malloc_trim(pad: usize);
        fn mallopt(param: c_int, value: c_int) -> c_int;
    }

    /// Prevents glibc from hoarding memory via memory fragmentation.
    pub fn limit_mmap_threshold() {
        unsafe {
            mallopt(M_MMAP_THRESHOLD, 65536);
        }
    }

    /// Asks glibc to trim malloc arenas.
    pub fn trim() {
        unsafe {
            malloc_trim(0);
        }
    }
}

use cosmic_bg_config::Config;
use cosmic_bg_config::state::State;
use cosmic_config::CosmicConfigEntry;
use cosmic_config::calloop::ConfigWatchSource;
use cosmic_protocols::session_lock_layer::v1::client::cosmic_session_lock_layer_manager_v1;
use eyre::Context;
use fade::{Fade, multiplier, slerp};
use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::output::{OutputHandler, OutputInfo, OutputState};
use sctk::reexports::calloop;
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::reexports::client::globals::registry_queue_init;
use sctk::reexports::client::protocol::wl_output::{self, WlOutput};
use sctk::reexports::client::protocol::wl_subcompositor::WlSubcompositor;
use sctk::reexports::client::protocol::wl_subsurface::WlSubsurface;
use sctk::reexports::client::protocol::wl_surface;
use sctk::reexports::client::{Connection, Dispatch, Proxy, QueueHandle, Weak, delegate_noop};
use sctk::reexports::protocols::wp::alpha_modifier::v1::client::{
    wp_alpha_modifier_surface_v1, wp_alpha_modifier_v1,
};
use sctk::reexports::protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use sctk::reexports::protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::shell::WaylandSurface;
use sctk::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use sctk::shm::slot::SlotPool;
use sctk::shm::{Shm, ShmHandler};
use sctk::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    registry_handlers,
};

use std::time::Duration;
use tracing::error;
use tracing_subscriber::prelude::*;
use wallpaper::Wallpaper;

#[derive(Debug)]
pub struct CosmicBgLayer {
    kind: LayerKind,
    viewport: wp_viewport::WpViewport,
    wl_output: WlOutput,
    output_info: OutputInfo,
    pool: Option<SlotPool>,
    needs_redraw: bool,
    size: Option<(u32, u32)>,
    fractional_scale: Option<u32>,
}

#[derive(Debug)]
enum LayerKind {
    Layer(LayerSurface),
    Subsurface {
        subsurface: WlSubsurface,
        wl_surface: wl_surface::WlSurface,
        fade: Option<FadeIn>,
    },
}

#[derive(Debug)]
struct FadeIn {
    alpha: wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1,
    fade: Fade,
}

impl CosmicBgLayer {
    fn surface(&self) -> &wl_surface::WlSurface {
        match &self.kind {
            LayerKind::Layer(layer) => layer.wl_surface(),
            LayerKind::Subsurface { wl_surface, .. } => wl_surface,
        }
    }

    fn is_layer(&self) -> bool {
        matches!(self.kind, LayerKind::Layer(_))
    }
}

impl Drop for CosmicBgLayer {
    fn drop(&mut self) {
        if let LayerKind::Subsurface {
            subsurface,
            wl_surface,
            fade,
        } = &mut self.kind
        {
            if let Some(fade) = fade.take() {
                fade.alpha.destroy();
            }
            subsurface.destroy();
            wl_surface.destroy();
        }
    }
}

#[allow(clippy::too_many_lines)]
fn main() -> color_eyre::Result<()> {
    // Prevents glibc from hoarding memory via memory fragmentation.
    #[cfg(target_env = "gnu")]
    malloc::limit_mmap_threshold();

    color_eyre::install()?;

    if std::env::var("RUST_SPANTRACE").is_err() {
        unsafe {
            std::env::set_var("RUST_SPANTRACE", "0");
        }
    }

    let _ = jxl_oxide::integration::register_image_decoding_hook();

    init_logger();

    let conn = Connection::connect_to_env().wrap_err("wayland client connection failed")?;

    let mut event_loop: calloop::EventLoop<'static, CosmicBg> =
        calloop::EventLoop::try_new().wrap_err("failed to create event loop")?;

    let (globals, event_queue) =
        registry_queue_init(&conn).wrap_err("failed to initialize registry queue")?;

    let qh = event_queue.handle();

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|err| err.error)
        .wrap_err("failed to insert main EventLoop into WaylandSource")?;

    let mode = cosmic_bg_config::Mode::current();

    let contexts = cosmic_bg_config::Mode::ALL
        .into_iter()
        .filter_map(|mode| match mode.context() {
            Ok(context) => Some((mode, context)),
            Err(why) => {
                tracing::error!(?why, ?mode, "failed to read background config");
                None
            }
        })
        .collect::<Vec<_>>();

    if let Ok(shared) = cosmic_bg_config::shared() {
        for (mode, context) in &contexts {
            match context.seed_from(&shared) {
                Ok(true) => tracing::debug!(?mode, "seeded backgrounds from the shared config"),
                Ok(false) => {}
                Err(why) => tracing::error!(?why, ?mode, "failed to seed background config"),
            }
        }
    }

    let config = contexts
        .iter()
        .find(|(context_mode, _)| *context_mode == mode)
        .map_or_else(Config::default, |(_, context)| {
            Config::load(context).unwrap_or_else(|why| {
                tracing::error!(?why, "Config file error, falling back to defaults");
                Config::default()
            })
        });

    match cosmic_bg_config::Mode::config() {
        Ok(mode_config) => match ConfigWatchSource::new(&mode_config) {
            Ok(source) => {
                event_loop
                    .handle()
                    .insert_source(source, |(_config, _keys), (), state| {
                        state.set_mode(cosmic_bg_config::Mode::current());
                    })
                    .expect("failed to insert mode watching source into event loop");
            }
            Err(why) => tracing::error!(?why, "failed to watch theme mode config"),
        },
        Err(why) => tracing::error!(?why, "failed to read theme mode config"),
    }

    for (context_mode, context) in &contexts {
        match ConfigWatchSource::new(&context.0) {
            Ok(source) => {
                let context_mode = *context_mode;
                event_loop
                    .handle()
                    .insert_source(source, move |(_config, _keys), (), state| {
                        state.mode_changed(context_mode);
                    })
                    .expect("failed to insert background watching source into event loop");
            }
            Err(why) => {
                tracing::error!(?why, ?context_mode, "failed to watch background config");
            }
        }
    }

    let source_tx = img_source::img_source(&event_loop.handle());

    // initial setup with all images
    let wallpapers = {
        let mut wallpapers = Vec::with_capacity(config.backgrounds.len() + 1);

        wallpapers.extend({
            config.backgrounds.iter().map(|bg| {
                Wallpaper::new(
                    bg.clone(),
                    qh.clone(),
                    event_loop.handle(),
                    source_tx.clone(),
                )
            })
        });

        wallpapers.sort_by(|a, b| a.entry.output.cmp(&b.entry.output));

        wallpapers.push(Wallpaper::new(
            config.default_background.clone(),
            qh.clone(),
            event_loop.handle(),
            source_tx.clone(),
        ));

        wallpapers
    };

    let mut bg_state = CosmicBg {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor_state: CompositorState::bind(&globals, &qh).unwrap(),
        shm_state: Shm::bind(&globals, &qh).unwrap(),
        layer_state: LayerShell::bind(&globals, &qh).unwrap(),
        viewporter: globals.bind(&qh, 1..=1, ()).unwrap(),
        alpha_modifier: globals.bind(&qh, 1..=1, ()).ok(),
        subcompositor: globals.bind(&qh, 1..=1, ()).ok(),
        fractional_scale_manager: globals.bind(&qh, 1..=1, ()).ok(),
        session_lock_layer_manager: globals.bind(&qh, 1..=1, ()).ok(),
        qh,
        source_tx,
        loop_handle: event_loop.handle(),
        exit: false,
        wallpapers,
        mode,
        contexts,
        config,
        active_outputs: Vec::new(),
    };

    loop {
        event_loop.dispatch(None, &mut bg_state)?;

        if bg_state.exit {
            break;
        }
    }

    Ok(())
}

#[derive(Debug)]
pub struct CosmicBg {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    shm_state: Shm,
    layer_state: LayerShell,
    viewporter: wp_viewporter::WpViewporter,
    alpha_modifier: Option<wp_alpha_modifier_v1::WpAlphaModifierV1>,
    subcompositor: Option<WlSubcompositor>,
    fractional_scale_manager: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    session_lock_layer_manager:
        Option<cosmic_session_lock_layer_manager_v1::CosmicSessionLockLayerManagerV1>,
    qh: QueueHandle<CosmicBg>,
    source_tx: calloop::channel::SyncSender<(String, notify::Event)>,
    loop_handle: calloop::LoopHandle<'static, CosmicBg>,
    exit: bool,
    wallpapers: Vec<Wallpaper>,
    mode: cosmic_bg_config::Mode,
    contexts: Vec<(cosmic_bg_config::Mode, cosmic_bg_config::Context)>,
    config: Config,
    active_outputs: Vec<WlOutput>,
}

impl CosmicBg {
    fn mode_changed(&mut self, mode: cosmic_bg_config::Mode) {
        if mode == self.mode {
            self.reload();
        }
    }

    fn set_mode(&mut self, mode: cosmic_bg_config::Mode) {
        if mode == self.mode {
            return;
        }

        tracing::debug!(?mode, "theme mode changed");
        self.mode = mode;
        self.reload();
    }

    fn reload(&mut self) {
        let Some((_, context)) = self.contexts.iter().find(|(mode, _)| *mode == self.mode) else {
            return;
        };

        let config = Config::load(context).unwrap_or_else(|why| {
            tracing::error!(?why, "Config file error, falling back to defaults");
            Config::default()
        });

        if config == self.config {
            return;
        }

        self.config = config;
        self.apply_backgrounds();

        #[cfg(target_env = "gnu")]
        malloc::trim();

        tracing::debug!(
            mode = ?self.mode,
            same_on_all = self.config.same_on_all,
            outputs = ?self.config.outputs,
            backgrounds = ?self.config.backgrounds,
            default_background = ?self.config.default_background.source,
            "new state"
        );
    }

    fn apply_backgrounds(&mut self) {
        let mut outgoing = std::mem::take(&mut self.wallpapers);
        let mut layers = Vec::new();
        for wallpaper in &mut outgoing {
            layers.append(&mut wallpaper.layers);
        }
        layers.retain(CosmicBgLayer::is_layer);

        let mut all_wallpaper = Wallpaper::new(
            self.config.default_background.clone(),
            self.qh.clone(),
            self.loop_handle.clone(),
            self.source_tx.clone(),
        );

        let mut backgrounds = self.config.backgrounds.clone();
        backgrounds.sort_by(|a, b| a.output.cmp(&b.output));
        let active_outputs = self.active_outputs.clone();

        'outer: for output in &active_outputs {
            let Some(output_info) = self.output_state.info(output) else {
                continue;
            };

            let o_name = output_info.name.clone().unwrap_or_default();
            for background in &backgrounds {
                if background.output == o_name {
                    let mut new_wallpaper = Wallpaper::new(
                        background.clone(),
                        self.qh.clone(),
                        self.loop_handle.clone(),
                        self.source_tx.clone(),
                    );

                    let layer = take_layer(&mut layers, output)
                        .unwrap_or_else(|| self.new_layer(output.clone(), output_info));
                    self.start_wallpaper(&mut new_wallpaper, layer, background.transition_duration);
                    new_wallpaper.draw();
                    _ = new_wallpaper.save_state();
                    self.wallpapers.push(new_wallpaper);

                    continue 'outer;
                }
            }

            let layer = take_layer(&mut layers, output)
                .unwrap_or_else(|| self.new_layer(output.clone(), output_info));
            self.start_wallpaper(
                &mut all_wallpaper,
                layer,
                self.config.default_background.transition_duration,
            );
        }

        all_wallpaper.draw();
        _ = all_wallpaper.save_state();
        self.wallpapers.push(all_wallpaper);
    }

    /// Show a wallpaper on a layer, fading in above it when the compositor can.
    fn start_wallpaper(
        &mut self,
        wallpaper: &mut Wallpaper,
        mut layer: CosmicBgLayer,
        duration: u64,
    ) {
        let fading = self.new_fading_surface(&layer, Duration::from_millis(duration));

        layer.needs_redraw = fading.is_none();
        wallpaper.layers.push(layer);
        if let Some(fading) = fading {
            wallpaper.layers.push(fading);
        }
    }

    /// A surface above a layer's own, which a wallpaper fades in on.
    fn new_fading_surface(
        &self,
        parent: &CosmicBgLayer,
        duration: Duration,
    ) -> Option<CosmicBgLayer> {
        if duration.is_zero() || self.alpha_modifier.is_none() {
            return None;
        }

        let (width, height) = parent.size?;
        let scale = parent.fractional_scale?;
        let subcompositor = self.subcompositor.as_ref()?;

        let surface = self.compositor_state.create_surface(&self.qh);
        let subsurface = subcompositor.get_subsurface(&surface, parent.surface(), &self.qh, ());
        subsurface.set_desync();
        subsurface.set_position(0, 0);
        subsurface.place_above(parent.surface());

        let viewport = self.viewporter.get_viewport(&surface, &self.qh, ());
        let alpha = self
            .alpha_modifier
            .as_ref()?
            .get_surface(&surface, &self.qh, ());
        alpha.set_multiplier(0);

        Some(CosmicBgLayer {
            kind: LayerKind::Subsurface {
                subsurface,
                wl_surface: surface,
                fade: Some(FadeIn {
                    alpha,
                    fade: Fade::new(duration),
                }),
            },
            viewport,
            wl_output: parent.wl_output.clone(),
            output_info: parent.output_info.clone(),
            pool: SlotPool::new(width as usize * height as usize * 4, &self.shm_state).ok(),
            needs_redraw: true,
            size: Some((width, height)),
            fractional_scale: Some(scale),
        })
    }

    /// Draw a wallpaper on fresh surfaces, fading in over what is beneath it.
    pub fn fade_wallpaper(&mut self, index: usize) {
        let Some(wallpaper) = self.wallpapers.get(index) else {
            return;
        };
        let duration = Duration::from_millis(wallpaper.entry.transition_duration);
        let fading = wallpaper
            .layers
            .iter()
            .filter(|layer| layer.is_layer())
            .filter_map(|layer| self.new_fading_surface(layer, duration))
            .collect::<Vec<_>>();

        let Some(wallpaper) = self.wallpapers.get_mut(index) else {
            return;
        };
        if fading.is_empty() {
            for layer in &mut wallpaper.layers {
                layer.needs_redraw = true;
            }
        } else {
            wallpaper.layers.extend(fading);
        }
        wallpaper.draw();
    }

    /// Take over from a subsurface that finished fading in, and drop it.
    fn finish_fade(&mut self, wallpaper_index: usize, layer_index: usize) {
        let output = self.wallpapers[wallpaper_index].layers[layer_index]
            .wl_output
            .clone();
        let wallpaper = &mut self.wallpapers[wallpaper_index];

        for layer in &mut wallpaper.layers {
            if layer.wl_output == output && layer.is_layer() {
                layer.needs_redraw = true;
            }
        }
        wallpaper.layers.remove(layer_index);
        wallpaper.draw();

        #[cfg(target_env = "gnu")]
        malloc::trim();
    }

    #[must_use]
    pub fn new_layer(&self, output: WlOutput, output_info: OutputInfo) -> CosmicBgLayer {
        let surface = self.compositor_state.create_surface(&self.qh);

        let layer = self.layer_state.create_layer_surface(
            &self.qh,
            surface.clone(),
            Layer::Background,
            "wallpaper".into(),
            Some(&output),
        );

        layer.set_anchor(Anchor::all());
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);

        if let Some(session_lock_layer_manager) = &self.session_lock_layer_manager {
            if let sctk::shell::wlr_layer::SurfaceKind::Wlr(surface) = layer.kind() {
                session_lock_layer_manager.set_show_on_lock(surface);
            }
        }

        surface.commit();

        let viewport = self.viewporter.get_viewport(&surface, &self.qh, ());

        let fractional_scale = if let Some(mngr) = self.fractional_scale_manager.as_ref() {
            mngr.get_fractional_scale(&surface, &self.qh, surface.downgrade());
            None
        } else {
            (self.compositor_state.wl_compositor().version() < 6)
                .then_some(output_info.scale_factor as u32 * 120)
        };

        CosmicBgLayer {
            kind: LayerKind::Layer(layer),
            viewport,
            wl_output: output,
            output_info,
            size: None,
            fractional_scale,
            needs_redraw: false,
            pool: None,
        }
    }
}

impl CompositorHandler for CosmicBg {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        if self.fractional_scale_manager.is_none() {
            for wallpaper in &mut self.wallpapers {
                if let Some(layer) = wallpaper
                    .layers
                    .iter_mut()
                    .find(|layer| layer.surface() == surface)
                {
                    layer.fractional_scale = Some(new_factor as u32 * 120);
                    wallpaper.draw();
                    break;
                }
            }
        }
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        time: u32,
    ) {
        let Some(wallpaper_index) = self
            .wallpapers
            .iter()
            .position(|wallpaper| wallpaper.layers.iter().any(|l| l.surface() == surface))
        else {
            return;
        };
        let Some(layer_index) = self.wallpapers[wallpaper_index]
            .layers
            .iter()
            .position(|layer| layer.surface() == surface)
        else {
            return;
        };

        let done = {
            let layer = &mut self.wallpapers[wallpaper_index].layers[layer_index];
            let LayerKind::Subsurface {
                fade: Some(fade_in),
                ..
            } = &mut layer.kind
            else {
                return;
            };

            let (progress, done) = fade_in.fade.progress(time);
            fade_in
                .alpha
                .set_multiplier(multiplier(slerp(0.0, 1.0, progress)));

            if !done {
                surface.frame(qh, surface.clone());
            }
            surface.commit();

            done
        };

        if done {
            self.finish_fade(wallpaper_index, layer_index);
        }
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
        // TODO
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &WlOutput,
    ) {
    }
}

impl OutputHandler for CosmicBg {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        wl_output: wl_output::WlOutput,
    ) {
        self.active_outputs.push(wl_output.clone());
        let Some(output_info) = self.output_state.info(&wl_output) else {
            return;
        };

        if let Some(pos) = self
            .wallpapers
            .iter()
            .position(|w| match w.entry.output.as_str() {
                "all" => !w.layers.iter().any(|l| l.wl_output == wl_output),
                name => {
                    Some(name) == output_info.name.as_deref()
                        && !w.layers.iter().any(|l| l.wl_output == wl_output)
                }
            })
        {
            let layer = self.new_layer(wl_output, output_info);
            self.wallpapers[pos].layers.push(layer);
            if let Err(err) = self.wallpapers[pos].save_state() {
                tracing::error!("{err}");
            }
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self.fractional_scale_manager.is_none()
            && self.compositor_state.wl_compositor().version() < 6
        {
            let Some(output_info) = self.output_state.info(&output) else {
                return;
            };
            for wallpaper in &mut self.wallpapers {
                if let Some(layer) = wallpaper
                    .layers
                    .iter_mut()
                    .find(|layer| layer.wl_output == output && layer.is_layer())
                {
                    layer.fractional_scale = Some(output_info.scale_factor as u32 * 120);
                    wallpaper.draw();
                    break;
                }
            }
        }
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.active_outputs.retain(|o| o != &output);
        let Some(output_info) = self.output_state.info(&output) else {
            return;
        };

        // state cleanup
        if let Ok(state_helper) = State::state() {
            let mut state = State::get_entry(&state_helper).unwrap_or_default();
            state
                .wallpapers
                .retain(|(o_name, _source)| Some(o_name) != output_info.name.as_ref());
            if let Err(err) = state.write_entry(&state_helper) {
                error!("{err}");
            }
        }

        let Some(output_wallpaper) =
            self.wallpapers
                .iter_mut()
                .find(|w| match w.entry.output.as_str() {
                    "all" => true,
                    name => Some(name) == output_info.name.as_deref(),
                })
        else {
            return;
        };

        let Some(layer_position) = output_wallpaper
            .layers
            .iter()
            .position(|bg_layer| bg_layer.is_layer() && bg_layer.wl_output == output)
        else {
            return;
        };

        output_wallpaper.layers.remove(layer_position);
    }
}

impl LayerShellHandler for CosmicBg {
    fn closed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        dropped_layer: &LayerSurface,
    ) {
        let dropped = dropped_layer.wl_surface();
        let output = self
            .wallpapers
            .iter()
            .flat_map(|wallpaper| wallpaper.layers.iter())
            .find(|layer| layer.surface() == dropped)
            .map(|layer| layer.wl_output.clone());

        for wallpaper in &mut self.wallpapers {
            wallpaper.layers.retain(|layer| {
                layer.surface() != dropped && Some(&layer.wl_output) != output.as_ref()
            });
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let span = tracing::debug_span!("<CosmicBg as LayerShellHandler>::configure");
        let _handle = span.enter();

        for wallpaper in &mut self.wallpapers {
            let (w, h) = configure.new_size;
            if let Some(w_layer) = wallpaper
                .layers
                .iter_mut()
                .find(|l| matches!(&l.kind, LayerKind::Layer(surface) if surface == layer))
            {
                w_layer.size = Some((w, h));
                w_layer.needs_redraw = true;

                if let Some(pool) = w_layer.pool.as_mut() {
                    if let Err(why) = pool.resize(w as usize * h as usize * 4) {
                        tracing::error!(?why, "failed to resize pool");
                        continue;
                    }
                } else {
                    match SlotPool::new(w as usize * h as usize * 4, &self.shm_state) {
                        Ok(pool) => {
                            w_layer.pool.replace(pool);
                        }

                        Err(why) => {
                            tracing::error!(?why, "failed to create pool");
                            continue;
                        }
                    }
                }

                wallpaper.draw();

                break;
            }
        }
    }
}

impl ShmHandler for CosmicBg {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

delegate_compositor!(CosmicBg);
delegate_output!(CosmicBg);
delegate_shm!(CosmicBg);
delegate_layer!(CosmicBg);
delegate_registry!(CosmicBg);
delegate_noop!(CosmicBg: WlSubcompositor);
delegate_noop!(CosmicBg: WlSubsurface);
delegate_noop!(CosmicBg: wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1);
delegate_noop!(CosmicBg: wp_alpha_modifier_v1::WpAlphaModifierV1);
delegate_noop!(CosmicBg: wp_viewporter::WpViewporter);
delegate_noop!(CosmicBg: wp_viewport::WpViewport);
delegate_noop!(CosmicBg: wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
delegate_noop!(CosmicBg: ignore cosmic_session_lock_layer_manager_v1::CosmicSessionLockLayerManagerV1);

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, Weak<wl_surface::WlSurface>>
    for CosmicBg
{
    fn event(
        state: &mut CosmicBg,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        surface: &Weak<wl_surface::WlSurface>,
        _: &Connection,
        _: &QueueHandle<CosmicBg>,
    ) {
        match event {
            wp_fractional_scale_v1::Event::PreferredScale { scale } => {
                if let Ok(surface) = surface.upgrade() {
                    for wallpaper in &mut state.wallpapers {
                        if let Some(layer) = wallpaper
                            .layers
                            .iter_mut()
                            .find(|layer| layer.surface() == &surface)
                        {
                            layer.fractional_scale = Some(scale);
                            wallpaper.draw();
                            break;
                        }
                    }
                }
            }
            _ => unreachable!(),
        }
    }
}

impl ProvidesRegistryState for CosmicBg {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

fn take_layer(layers: &mut Vec<CosmicBgLayer>, output: &WlOutput) -> Option<CosmicBgLayer> {
    let index = layers
        .iter()
        .position(|layer| layer.is_layer() && layer.wl_output == *output)?;
    Some(layers.remove(index))
}

fn init_logger() {
    let log_level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|level| level.parse::<tracing::Level>().ok())
        .unwrap_or(tracing::Level::INFO);

    let log_format = tracing_subscriber::fmt::format()
        .pretty()
        .without_time()
        .with_line_number(true)
        .with_file(true)
        .with_target(false)
        .with_thread_names(true);

    let log_filter = tracing_subscriber::fmt::Layer::default()
        .with_writer(std::io::stderr)
        .event_format(log_format)
        .with_filter(tracing_subscriber::filter::filter_fn(move |metadata| {
            metadata.level() == &tracing::Level::ERROR
                || (metadata.target().starts_with("cosmic_bg") && metadata.level() <= &log_level)
        }));

    tracing_subscriber::registry().with(log_filter).init();
}
