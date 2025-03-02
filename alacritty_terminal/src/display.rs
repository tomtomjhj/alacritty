// Copyright 2016 Joe Wilm, The Alacritty Project Contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The display subsystem including window management, font rasterization, and
//! GPU drawing.
use std::f64;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use std::ffi::c_void;
use std::sync::mpsc;

use glutin::dpi::{PhysicalPosition, PhysicalSize};
use glutin::EventsLoop;
use parking_lot::MutexGuard;

use crate::config::{Config, StartupMode};
use crate::index::Line;
use crate::message_bar::Message;
use crate::meter::Meter;
use crate::renderer::rects::{RenderRect, RenderLines};
use crate::renderer::{self, GlyphCache, QuadRenderer};
use crate::sync::FairMutex;
use crate::term::color::Rgb;
use crate::term::{RenderableCell, SizeInfo, Term};
use crate::window::{self, Window};
use font::{self, Rasterize};

#[derive(Debug)]
pub enum Error {
    /// Error with window management
    Window(window::Error),

    /// Error dealing with fonts
    Font(font::Error),

    /// Error in renderer
    Render(renderer::Error),
}

impl ::std::error::Error for Error {
    fn cause(&self) -> Option<&dyn (::std::error::Error)> {
        match *self {
            Error::Window(ref err) => Some(err),
            Error::Font(ref err) => Some(err),
            Error::Render(ref err) => Some(err),
        }
    }

    fn description(&self) -> &str {
        match *self {
            Error::Window(ref err) => err.description(),
            Error::Font(ref err) => err.description(),
            Error::Render(ref err) => err.description(),
        }
    }
}

impl ::std::fmt::Display for Error {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        match *self {
            Error::Window(ref err) => err.fmt(f),
            Error::Font(ref err) => err.fmt(f),
            Error::Render(ref err) => err.fmt(f),
        }
    }
}

impl From<window::Error> for Error {
    fn from(val: window::Error) -> Error {
        Error::Window(val)
    }
}

impl From<font::Error> for Error {
    fn from(val: font::Error) -> Error {
        Error::Font(val)
    }
}

impl From<renderer::Error> for Error {
    fn from(val: renderer::Error) -> Error {
        Error::Render(val)
    }
}

/// The display wraps a window, font rasterizer, and GPU renderer
pub struct Display {
    window: Window,
    renderer: QuadRenderer,
    glyph_cache: GlyphCache,
    render_timer: bool,
    rx: mpsc::Receiver<PhysicalSize>,
    tx: mpsc::Sender<PhysicalSize>,
    meter: Meter,
    font_size: font::Size,
    size_info: SizeInfo,
    last_message: Option<Message>,
}

/// Can wakeup the render loop from other threads
pub struct Notifier(window::Proxy);

/// Types that are interested in when the display is resized
pub trait OnResize {
    fn on_resize(&mut self, size: &SizeInfo);
}

impl Notifier {
    pub fn notify(&self) {
        self.0.wakeup_event_loop();
    }
}

impl Display {
    pub fn notifier(&self) -> Notifier {
        Notifier(self.window.create_window_proxy())
    }

    pub fn update_config(&mut self, config: &Config) {
        self.render_timer = config.render_timer();
    }

    /// Get size info about the display
    pub fn size(&self) -> &SizeInfo {
        &self.size_info
    }

    pub fn new(config: &Config) -> Result<Display, Error> {
        // Extract some properties from config
        let render_timer = config.render_timer();

        // Guess DPR based on first monitor
        let event_loop = EventsLoop::new();
        let estimated_dpr =
            event_loop.get_available_monitors().next().map(|m| m.get_hidpi_factor()).unwrap_or(1.);

        // Guess the target window dimensions
        let metrics = GlyphCache::static_metrics(config, estimated_dpr as f32)?;
        let (cell_width, cell_height) = Self::compute_cell_size(config, &metrics);
        let dimensions = Self::calculate_dimensions(config, estimated_dpr, cell_width, cell_height);

        debug!("Estimated DPR: {}", estimated_dpr);
        debug!("Estimated Cell Size: {} x {}", cell_width, cell_height);
        debug!("Estimated Dimensions: {:?}", dimensions);

        // Create the window where Alacritty will be displayed
        let logical = dimensions.map(|d| PhysicalSize::new(d.0, d.1).to_logical(estimated_dpr));
        let mut window = Window::new(event_loop, &config, logical)?;

        let dpr = window.hidpi_factor();
        info!("Device pixel ratio: {}", dpr);

        // get window properties for initializing the other subsystems
        let mut viewport_size =
            window.inner_size_pixels().expect("glutin returns window size").to_physical(dpr);

        // Create renderer
        let mut renderer = QuadRenderer::new()?;

        let (glyph_cache, cell_width, cell_height) =
            Self::new_glyph_cache(dpr, &mut renderer, config)?;

        let mut padding_x = f64::from(config.window.padding.x) * dpr;
        let mut padding_y = f64::from(config.window.padding.y) * dpr;

        if let Some((width, height)) =
            Self::calculate_dimensions(config, dpr, cell_width, cell_height)
        {
            if dimensions == Some((width, height)) {
                info!("Estimated DPR correctly, skipping resize");
            } else {
                viewport_size = PhysicalSize::new(width, height);
                window.set_inner_size(viewport_size.to_logical(dpr));
            }
        } else if config.window.dynamic_padding {
            // Make sure additional padding is spread evenly
            let cw = f64::from(cell_width);
            let ch = f64::from(cell_height);
            padding_x = padding_x + (viewport_size.width - 2. * padding_x) % cw / 2.;
            padding_y = padding_y + (viewport_size.height - 2. * padding_y) % ch / 2.;
        }

        padding_x = padding_x.floor();
        padding_y = padding_y.floor();

        // Update OpenGL projection
        renderer.resize(viewport_size, padding_x as f32, padding_y as f32);

        info!("Cell Size: {} x {}", cell_width, cell_height);
        info!("Padding: {} x {}", padding_x, padding_y);

        let size_info = SizeInfo {
            dpr,
            width: viewport_size.width as f32,
            height: viewport_size.height as f32,
            cell_width: cell_width as f32,
            cell_height: cell_height as f32,
            padding_x: padding_x as f32,
            padding_y: padding_y as f32,
        };

        // Channel for resize events
        //
        // macOS has a callback for getting resize events, the channel is used
        // to queue resize events until the next draw call. Unfortunately, it
        // seems that the event loop is blocked until the window is done
        // resizing. If any drawing were to happen during a resize, it would
        // need to be in the callback.
        let (tx, rx) = mpsc::channel();

        // Clear screen
        let background_color = config.colors.primary.background;
        renderer.with_api(config, &size_info, |api| {
            api.clear(background_color);
        });

        // We should call `clear` when window is offscreen, so when `window.show()` happens it
        // would be with background color instead of uninitialized surface.
        window.swap_buffers()?;

        window.show();

        // Set window position
        //
        // TODO: replace `set_position` with `with_position` once available
        // Upstream issue: https://github.com/tomaka/winit/issues/806
        if let Some(position) = config.window.position {
            let physical = PhysicalPosition::from((position.x, position.y));
            let logical = physical.to_logical(window.hidpi_factor());
            window.set_position(logical);
        }

        #[allow(clippy::single_match)]
        match config.window.startup_mode() {
            StartupMode::Fullscreen => window.set_fullscreen(true),
            #[cfg(target_os = "macos")]
            StartupMode::SimpleFullscreen => window.set_simple_fullscreen(true),
            #[cfg(not(any(target_os = "macos", windows)))]
            StartupMode::Maximized if window.is_x11() => window.set_maximized(true),
            _ => (),
        }

        Ok(Display {
            window,
            renderer,
            glyph_cache,
            render_timer,
            tx,
            rx,
            meter: Meter::new(),
            font_size: config.font.size,
            size_info,
            last_message: None,
        })
    }

    fn calculate_dimensions(
        config: &Config,
        dpr: f64,
        cell_width: f32,
        cell_height: f32,
    ) -> Option<(f64, f64)> {
        let dimensions = config.window.dimensions;

        if dimensions.columns_u32() == 0
            || dimensions.lines_u32() == 0
            || config.window.startup_mode() != StartupMode::Windowed
        {
            return None;
        }

        let padding_x = f64::from(config.window.padding.x) * dpr;
        let padding_y = f64::from(config.window.padding.y) * dpr;

        // Calculate new size based on cols/lines specified in config
        let grid_width = cell_width as u32 * dimensions.columns_u32();
        let grid_height = cell_height as u32 * dimensions.lines_u32();

        let width = (f64::from(grid_width) + 2. * padding_x).floor();
        let height = (f64::from(grid_height) + 2. * padding_y).floor();

        Some((width, height))
    }

    fn new_glyph_cache(
        dpr: f64,
        renderer: &mut QuadRenderer,
        config: &Config,
    ) -> Result<(GlyphCache, f32, f32), Error> {
        let font = config.font.clone();
        let rasterizer = font::Rasterizer::new(dpr as f32, config.font.use_thin_strokes())?;

        // Initialize glyph cache
        let glyph_cache = {
            info!("Initializing glyph cache...");
            let init_start = ::std::time::Instant::now();

            let cache =
                renderer.with_loader(|mut api| GlyphCache::new(rasterizer, &font, &mut api))?;

            let stop = init_start.elapsed();
            let stop_f = stop.as_secs() as f64 + f64::from(stop.subsec_nanos()) / 1_000_000_000f64;
            info!("... finished initializing glyph cache in {}s", stop_f);

            cache
        };

        // Need font metrics to resize the window properly. This suggests to me the
        // font metrics should be computed before creating the window in the first
        // place so that a resize is not needed.
        let (cw, ch) = Self::compute_cell_size(config, &glyph_cache.font_metrics());

        Ok((glyph_cache, cw, ch))
    }

    pub fn update_glyph_cache(&mut self, config: &Config) {
        let cache = &mut self.glyph_cache;
        let dpr = self.size_info.dpr;
        let size = self.font_size;

        self.renderer.with_loader(|mut api| {
            let _ = cache.update_font_size(&config.font, size, dpr, &mut api);
        });

        let (cw, ch) = Self::compute_cell_size(config, &cache.font_metrics());
        self.size_info.cell_width = cw;
        self.size_info.cell_height = ch;
    }

    fn compute_cell_size(config: &Config, metrics: &font::Metrics) -> (f32, f32) {
        let offset_x = f64::from(config.font.offset.x);
        let offset_y = f64::from(config.font.offset.y);
        (
            f32::max(1., ((metrics.average_advance + offset_x) as f32).floor()),
            f32::max(1., ((metrics.line_height + offset_y) as f32).floor()),
        )
    }

    #[inline]
    pub fn resize_channel(&self) -> mpsc::Sender<PhysicalSize> {
        self.tx.clone()
    }

    pub fn window(&mut self) -> &mut Window {
        &mut self.window
    }

    /// Process pending resize events
    pub fn handle_resize(
        &mut self,
        terminal: &mut MutexGuard<'_, Term>,
        config: &Config,
        pty_resize_handle: &mut dyn OnResize,
        processor_resize_handle: &mut dyn OnResize,
    ) {
        let previous_cols = self.size_info.cols();
        let previous_lines = self.size_info.lines();

        // Resize events new_size and are handled outside the poll_events
        // iterator. This has the effect of coalescing multiple resize
        // events into one.
        let mut new_size = None;

        // Take most recent resize event, if any
        while let Ok(size) = self.rx.try_recv() {
            new_size = Some(size);
        }

        // Update the DPR
        let dpr = self.window.hidpi_factor();

        // Font size/DPI factor modification detected
        let font_changed =
            terminal.font_size != self.font_size || (dpr - self.size_info.dpr).abs() > f64::EPSILON;

        // Skip resize if nothing changed
        if let Some(new_size) = new_size {
            if !font_changed
                && (new_size.width - f64::from(self.size_info.width)).abs() < f64::EPSILON
                && (new_size.height - f64::from(self.size_info.height)).abs() < f64::EPSILON
            {
                return;
            }
        }

        // Message bar update detected
        let message_bar_changed = self.last_message != terminal.message_buffer_mut().message();

        if font_changed || message_bar_changed {
            if new_size == None {
                // Force a resize to refresh things
                new_size = Some(PhysicalSize::new(
                    f64::from(self.size_info.width) / self.size_info.dpr * dpr,
                    f64::from(self.size_info.height) / self.size_info.dpr * dpr,
                ));
            }

            self.font_size = terminal.font_size;
            self.last_message = terminal.message_buffer_mut().message();
            self.size_info.dpr = dpr;
        }

        if font_changed {
            self.update_glyph_cache(config);
        }

        if let Some(psize) = new_size.take() {
            let width = psize.width as f32;
            let height = psize.height as f32;
            let cell_width = self.size_info.cell_width;
            let cell_height = self.size_info.cell_height;

            self.size_info.width = width;
            self.size_info.height = height;

            let mut padding_x = f32::from(config.window.padding.x) * dpr as f32;
            let mut padding_y = f32::from(config.window.padding.y) * dpr as f32;

            if config.window.dynamic_padding {
                padding_x = padding_x + ((width - 2. * padding_x) % cell_width) / 2.;
                padding_y = padding_y + ((height - 2. * padding_y) % cell_height) / 2.;
            }

            self.size_info.padding_x = padding_x.floor();
            self.size_info.padding_y = padding_y.floor();

            let size = &self.size_info;
            terminal.resize(size);
            processor_resize_handle.on_resize(size);

            // Subtract message bar lines for pty size
            let mut pty_size = *size;
            if let Some(message) = terminal.message_buffer_mut().message() {
                pty_size.height -= pty_size.cell_height * message.text(&size).len() as f32;
            }

            if message_bar_changed
                || previous_cols != pty_size.cols()
                || previous_lines != pty_size.lines()
            {
                pty_resize_handle.on_resize(&pty_size);
            }

            self.window.resize(psize);
            self.renderer.resize(psize, self.size_info.padding_x, self.size_info.padding_y);
        }
    }

    /// Draw the screen
    ///
    /// A reference to Term whose state is being drawn must be provided.
    ///
    /// This call may block if vsync is enabled
    pub fn draw(&mut self, terminal: &FairMutex<Term>, config: &Config) {
        let mut terminal = terminal.lock();
        let size_info = *terminal.size_info();
        let visual_bell_intensity = terminal.visual_bell.intensity();
        let background_color = terminal.background_color();
        let metrics = self.glyph_cache.font_metrics();

        let window_focused = self.window.is_focused;
        let grid_cells: Vec<RenderableCell> =
            terminal.renderable_cells(config, window_focused).collect();

        // Get message from terminal to ignore modifications after lock is dropped
        let message_buffer = terminal.message_buffer_mut().message();

        // Clear dirty flag
        terminal.dirty = !terminal.visual_bell.completed();

        if let Some(title) = terminal.get_next_title() {
            self.window.set_title(&title);
        }

        if let Some(mouse_cursor) = terminal.get_next_mouse_cursor() {
            self.window.set_mouse_cursor(mouse_cursor);
        }

        if let Some(is_urgent) = terminal.next_is_urgent.take() {
            // We don't need to set the urgent flag if we already have the
            // user's attention.
            if !is_urgent || !self.window.is_focused {
                self.window.set_urgent(is_urgent);
            }
        }

        // Clear when terminal mutex isn't held. Mesa for
        // some reason takes a long time to call glClear(). The driver descends
        // into xcb_connect_to_fd() which ends up calling __poll_nocancel()
        // which blocks for a while.
        //
        // By keeping this outside of the critical region, the Mesa bug is
        // worked around to some extent. Since this doesn't actually address the
        // issue of glClear being slow, less time is available for input
        // handling and rendering.
        drop(terminal);

        self.renderer.with_api(config, &size_info, |api| {
            api.clear(background_color);
        });

        {
            let glyph_cache = &mut self.glyph_cache;
            let mut lines = RenderLines::new();

            // Draw grid
            {
                let _sampler = self.meter.sampler();

                self.renderer.with_api(config, &size_info, |mut api| {
                    // Iterate over all non-empty cells in the grid
                    for cell in grid_cells {
                        // Update underline/strikeout
                        lines.update(&cell);

                        // Draw the cell
                        api.render_cell(cell, glyph_cache);
                    }
                });
            }

            let mut rects = lines.into_rects(&metrics, &size_info);

            if let Some(message) = message_buffer {
                let text = message.text(&size_info);

                // Create a new rectangle for the background
                let start_line = size_info.lines().0 - text.len();
                let y = size_info.padding_y + size_info.cell_height * start_line as f32;
                rects.push(RenderRect::new(
                    0.,
                    y,
                    size_info.width,
                    size_info.height - y,
                    message.color(),
                ));

                // Draw rectangles including the new background
                self.renderer.draw_rects(config, &size_info, visual_bell_intensity, rects);

                // Relay messages to the user
                let mut offset = 1;
                for message_text in text.iter().rev() {
                    self.renderer.with_api(config, &size_info, |mut api| {
                        api.render_string(
                            &message_text,
                            Line(size_info.lines().saturating_sub(offset)),
                            glyph_cache,
                            None,
                        );
                    });
                    offset += 1;
                }
            } else {
                // Draw rectangles
                self.renderer.draw_rects(config, &size_info, visual_bell_intensity, rects);
            }

            // Draw render timer
            if self.render_timer {
                let timing = format!("{:.3} usec", self.meter.average());
                let color = Rgb { r: 0xd5, g: 0x4e, b: 0x53 };
                self.renderer.with_api(config, &size_info, |mut api| {
                    api.render_string(&timing[..], size_info.lines() - 2, glyph_cache, Some(color));
                });
            }
        }

        self.window.swap_buffers().expect("swap buffers");
    }

    pub fn get_window_id(&self) -> Option<usize> {
        self.window.get_window_id()
    }

    /// Adjust the IME editor position according to the new location of the cursor
    pub fn update_ime_position(&mut self, terminal: &Term) {
        let point = terminal.cursor().point;
        let SizeInfo { cell_width: cw, cell_height: ch, padding_x: px, padding_y: py, .. } =
            *terminal.size_info();

        let dpr = self.window().hidpi_factor();
        let nspot_x = f64::from(px + point.col.0 as f32 * cw);
        let nspot_y = f64::from(py + (point.line.0 + 1) as f32 * ch);

        self.window().set_ime_spot(PhysicalPosition::from((nspot_x, nspot_y)).to_logical(dpr));
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    pub fn get_wayland_display(&self) -> Option<*mut c_void> {
        self.window.get_wayland_display()
    }
}
