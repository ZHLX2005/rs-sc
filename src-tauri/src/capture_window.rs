//! Native fullscreen capture overlay: darkens the screen, lets the user drag a selection,
//! and returns the chosen rectangle.
//!
//! We use `winit` + `softbuffer` to draw directly to the OS window — no WebView overhead,
//! so the overlay is responsive on every machine.
//!
//! `winit` only allows one `EventLoop` per process on Windows. We therefore keep a
//! single background thread alive for the lifetime of the app and push start commands
//! to it through `OnceLock<EventLoopProxy>`.

use std::num::NonZeroU32;
use std::sync::{mpsc, Arc, OnceLock};

use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, MouseButton, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Fullscreen, Window, WindowId, WindowLevel};

// ── Public types ──────────────────────────────────────────────────────────────

pub enum CaptureCommand {
    StartCapture {
        /// Shared, Arc-wrapped RGBA buffer. The capture overlay uses it to
        /// paint the dimmed screen, and the caller keeps its own reference
        /// so it can crop the selection without recapturing the whole display.
        rgba: Arc<Vec<u8>>,
        img_w: u32,
        img_h: u32,
        monitor_x: i32,
        monitor_y: i32,
        event_tx: mpsc::Sender<CaptureEvent>,
    },
    Close,
}

pub enum CaptureEvent {
    Selection { x: u32, y: u32, w: u32, h: u32 },
    Cancelled,
}

// ── Singleton event loop proxy ────────────────────────────────────────────────

static CAPTURE_PROXY: OnceLock<EventLoopProxy<CaptureCommand>> = OnceLock::new();

pub fn capture_proxy() -> EventLoopProxy<CaptureCommand> {
    CAPTURE_PROXY
        .get_or_init(|| {
            let (proxy_tx, proxy_rx) =
                mpsc::sync_channel::<EventLoopProxy<CaptureCommand>>(1);
            std::thread::Builder::new()
                .name("rs-sc-capture-loop".into())
                .spawn(move || {
                    // On Windows, opt in to receiving pointer messages even when
                    // the system would otherwise suppress them. Some tablet
                    // drivers and Windows Ink configurations route pen events
                    // through WM_MOUSExxx rather than WM_POINTER; without this
                    // call those pens never reach our overlay.
                    #[cfg(target_os = "windows")]
                    unsafe {
                        extern "system" {
                            fn EnableMouseInPointer(fEnable: i32) -> i32;
                        }
                        let _ = EnableMouseInPointer(1); // TRUE
                    }

                    #[cfg(target_os = "windows")]
                    let event_loop = {
                        use winit::platform::windows::EventLoopBuilderExtWindows;
                        EventLoop::<CaptureCommand>::with_user_event()
                            .with_any_thread(true)
                            .build()
                            .expect("failed to build winit event loop")
                    };
                    #[cfg(not(target_os = "windows"))]
                    let event_loop = EventLoop::<CaptureCommand>::with_user_event()
                        .build()
                        .expect("failed to build winit event loop");

                    let proxy = event_loop.create_proxy();
                    let _ = proxy_tx.send(proxy);

                    let mut handler = CaptureHandler::idle();
                    event_loop
                        .run_app(&mut handler)
                        .expect("capture event loop crashed");
                })
                .expect("failed to spawn capture thread");

            proxy_rx
                .recv()
                .expect("capture event loop died before sending proxy")
        })
        .clone()
}

pub fn start_capture(
    rgba: Arc<Vec<u8>>,
    img_w: u32,
    img_h: u32,
    monitor_x: i32,
    monitor_y: i32,
    event_tx: mpsc::Sender<CaptureEvent>,
) {
    let _ = capture_proxy().send_event(CaptureCommand::StartCapture {
        rgba,
        img_w,
        img_h,
        monitor_x,
        monitor_y,
        event_tx,
    });
}

pub fn close_capture() {
    let _ = capture_proxy().send_event(CaptureCommand::Close);
}

// ── Internal handler ──────────────────────────────────────────────────────────

enum HandlerState {
    Idle,
    Selecting(CaptureSession),
}

struct CaptureSession {
    img_w: u32,
    img_h: u32,
    original_pixels: Vec<u32>,
    darkened_pixels: Vec<u32>,
    event_tx: mpsc::Sender<CaptureEvent>,
    window: Arc<Window>,
    surface: Surface<Arc<Window>, Arc<Window>>,
    drag_start: Option<PhysicalPosition<f64>>,
    selection: Option<(u32, u32, u32, u32)>,
    is_dragging: bool,
    mouse_pos: PhysicalPosition<f64>,
    surface_ready: bool,
    shown: bool,
}

struct CaptureHandler {
    state: HandlerState,
    _ctx_storage: Option<Context<Arc<Window>>>,
}

impl CaptureHandler {
    fn idle() -> Self {
        Self {
            state: HandlerState::Idle,
            _ctx_storage: None,
        }
    }

    fn open_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        rgba: Arc<Vec<u8>>,
        img_w: u32,
        img_h: u32,
        monitor_x: i32,
        monitor_y: i32,
        event_tx: mpsc::Sender<CaptureEvent>,
    ) {
        // Find the matching physical monitor for fullscreen.
        let target_monitor = event_loop.available_monitors().find(|m| {
            let p = m.position();
            p.x == monitor_x && p.y == monitor_y
        });

        let fullscreen = match target_monitor {
            Some(m) => Fullscreen::Borderless(Some(m)),
            None => Fullscreen::Borderless(None),
        };

        let attrs = Window::default_attributes()
            .with_title("rs-sc capture")
            .with_decorations(false)
            .with_resizable(false)
            .with_fullscreen(Some(fullscreen))
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_visible(false);

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("failed to create capture window: {e}");
                let _ = event_tx.send(CaptureEvent::Cancelled);
                return;
            }
        };

        let ctx = Context::new(window.clone()).expect("softbuffer context");
        let surface = Surface::new(&ctx, window.clone()).expect("softbuffer surface");

        let original_pixels = rgba_to_softbuffer(&rgba);
        let darkened_pixels = darken_pixels(&original_pixels, 0.55);

        self._ctx_storage = Some(ctx);
        let mut session = CaptureSession {
            img_w,
            img_h,
            original_pixels,
            darkened_pixels,
            event_tx,
            window,
            surface,
            drag_start: None,
            selection: None,
            is_dragging: false,
            mouse_pos: PhysicalPosition::new(0.0, 0.0),
            surface_ready: false,
            shown: false,
        };

        // Pre-paint before showing the window so the user never sees a white flash.
        if let (Some(nz_w), Some(nz_h)) = (NonZeroU32::new(img_w), NonZeroU32::new(img_h)) {
            if session.surface.resize(nz_w, nz_h).is_ok() {
                session.surface_ready = true;
                if let Ok(mut buffer) = session.surface.buffer_mut() {
                    if buffer.len() == (img_w * img_h) as usize {
                        buffer.copy_from_slice(&session.darkened_pixels);
                        let _ = buffer.present();
                        session.shown = true;
                        session.window.set_visible(true);
                    }
                }
            }
        }

        self.state = HandlerState::Selecting(session);
    }

    fn close_window(&mut self) {
        // Dropping the session releases the Arc<Window> → window closes automatically.
        self.state = HandlerState::Idle;
        self._ctx_storage = None;
    }

    /// Begin a drag selection at the current `mouse_pos`. Called from both the
    /// `MouseInput::Left::Pressed` arm (regular mouse / some tablet drivers) and
    /// from `WindowEvent::Touch::Started` (Windows Ink pen / digitizer that
    /// winit delivers via the WM_POINTER path).
    fn handle_drag_press(&mut self) {
        if let HandlerState::Selecting(session) = &mut self.state {
            session.drag_start = Some(session.mouse_pos);
            session.is_dragging = true;
            session.selection = None;
        }
    }

    /// End a drag selection. If the user actually moved enough to make a real
    /// rectangle, send it to the pipeline and close the dim overlay; otherwise
    /// treat it as a cancel (so a single tap doesn't leave them stuck in
    /// capture mode). Called from both `MouseInput::Left::Released` and
    /// `WindowEvent::Touch::Ended`.
    fn handle_drag_release(&mut self) {
        // Phase 1: read what we need out of the session, then drop the borrow
        // so phase 2 can mutate self.state via close_window().
        let to_send = if let HandlerState::Selecting(session) = &mut self.state {
            if session.is_dragging {
                if let Some(start) = session.drag_start {
                    let rect = normalize_rect(start, session.mouse_pos);
                    session.selection = Some(rect);
                }
                session.is_dragging = false;
                session.drag_start = None;
                session
                    .selection
                    .filter(|(_, _, w, h)| *w > 4 && *h > 4)
                    .map(|(x, y, w, h)| CaptureEvent::Selection { x, y, w, h })
            } else {
                None
            }
        } else {
            None
        };

        if let Some(ev) = to_send {
            let tx = if let HandlerState::Selecting(s) = &self.state {
                s.event_tx.clone()
            } else {
                return;
            };
            self.close_window();
            let _ = tx.send(ev);
        } else {
            self.close_window();
        }
    }

    /// Update the cursor position. Called from both `CursorMoved` and
    /// `WindowEvent::Touch::Moved` so tablet pens that don't generate
    /// mouse-move events still drive the live drag rectangle.
    fn handle_pen_move(&mut self, position: PhysicalPosition<f64>) {
        if let HandlerState::Selecting(session) = &mut self.state {
            session.mouse_pos = position;
            if session.is_dragging {
                session.window.request_redraw();
            }
        }
    }

    /// Cancel from a right-click, the close button, or a `Touch::Cancelled`
    /// event (e.g. focus loss).
    fn handle_cancel(&mut self) {
        let tx = if let HandlerState::Selecting(s) = &self.state {
            Some(s.event_tx.clone())
        } else {
            None
        };
        if let Some(tx) = tx {
            let _ = tx.send(CaptureEvent::Cancelled);
        }
        self.close_window();
    }
}

impl ApplicationHandler<CaptureCommand> for CaptureHandler {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        // We may need to call `self.close_window()` (which mutates self.state)
        // from several arms below. To make that possible, every arm first
        // captures whatever it needs from the session into local variables,
        // then drops the &mut borrow on self, then acts on self.
        match event {
            WindowEvent::Resized(size) => {
                if let HandlerState::Selecting(session) = &mut self.state {
                    if size.width > 0 && size.height > 0 {
                        let _ = session.surface.resize(
                            NonZeroU32::new(size.width).unwrap(),
                            NonZeroU32::new(size.height).unwrap(),
                        );
                        session.surface_ready = true;
                        session.window.request_redraw();
                    }
                }
            }

            WindowEvent::RedrawRequested => {
                if let HandlerState::Selecting(session) = &mut self.state {
                    redraw_session(session);
                }
            }

            WindowEvent::KeyboardInput { event: key_event, .. } => {
                use winit::keyboard::{KeyCode, PhysicalKey};
                if key_event.state == ElementState::Pressed {
                    if let PhysicalKey::Code(KeyCode::Escape) = key_event.physical_key {
                        // Drop the borrow before closing so we can mutate self.
                        let tx = if let HandlerState::Selecting(s) = &mut self.state {
                            Some(s.event_tx.clone())
                        } else {
                            None
                        };
                        if let Some(tx) = tx {
                            let _ = tx.send(CaptureEvent::Cancelled);
                        }
                        self.close_window();
                    }
                }
            }

            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => match state {
                ElementState::Pressed => self.handle_drag_press(),
                ElementState::Released => self.handle_drag_release(),
            },

            WindowEvent::CursorMoved { position, .. } => {
                self.handle_pen_move(position);
            }

            // Right-click cancels — matches every other screenshot tool on the planet.
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } => self.handle_cancel(),

            // Pen / digitizer / touch input. On Windows winit 0.30 delivers
            // these via the WM_POINTER path as `WindowEvent::Touch`, NOT as
            // `MouseInput`. Without this arm, a stylus / graphics tablet pen
            // tap would be silently ignored — the dim overlay shows up but
            // nothing you draw registers. We treat every `Touch` as a
            // single-pointer selection (multi-finger is overkill for a
            // region-capture tool).
            WindowEvent::Touch(touch) => {
                match touch.phase {
                    TouchPhase::Started => {
                        self.handle_pen_move(touch.location);
                        self.handle_drag_press();
                    }
                    TouchPhase::Moved => {
                        self.handle_pen_move(touch.location);
                    }
                    TouchPhase::Ended => {
                        self.handle_pen_move(touch.location);
                        self.handle_drag_release();
                    }
                    TouchPhase::Cancelled => {
                        self.handle_cancel();
                    }
                }
            }

            WindowEvent::CloseRequested => {
                self.handle_cancel();
            }

            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: CaptureCommand) {
        match event {
            CaptureCommand::StartCapture {
                rgba,
                img_w,
                img_h,
                monitor_x,
                monitor_y,
                event_tx,
            } => {
                self.close_window();
                self.open_window(event_loop, rgba, img_w, img_h, monitor_x, monitor_y, event_tx);
            }
            CaptureCommand::Close => {
                if let HandlerState::Selecting(session) = &self.state {
                    let _ = session.event_tx.send(CaptureEvent::Cancelled);
                }
                self.close_window();
            }
        }
    }
}

// ── Per-frame rendering ───────────────────────────────────────────────────────

fn redraw_session(session: &mut CaptureSession) {
    if !session.surface_ready {
        return;
    }

    let mut buffer = match session.surface.buffer_mut() {
        Ok(b) => b,
        Err(_) => return,
    };

    let expected = (session.img_w * session.img_h) as usize;
    if buffer.len() != expected {
        buffer.fill(0);
        let _ = buffer.present();
        return;
    }

    // 1. Start from the darkened full-screen image.
    buffer.copy_from_slice(&session.darkened_pixels);

    // 2. Reveal the original pixels for the current drag rectangle.
    let sel = if session.is_dragging {
        session
            .drag_start
            .map(|s| normalize_rect(s, session.mouse_pos))
    } else {
        session.selection
    };

    if let Some((sx, sy, sw, sh)) = sel {
        if sw > 0 && sh > 0 {
            blit_pixels(
                &mut buffer,
                session.img_w,
                &session.original_pixels,
                session.img_w,
                sx,
                sy,
                sx,
                sy,
                sw,
                sh,
            );
            // 3. Draw a blue border on the selection edge.
            draw_border(&mut buffer, session.img_w, session.img_h, sx, sy, sw, sh, 0x004A9EFF, 2);
        }
    }

    let _ = buffer.present();

    if !session.shown {
        session.shown = true;
        session.window.set_visible(true);
    }
}

// ── Pixel helpers ─────────────────────────────────────────────────────────────

fn rgba_to_softbuffer(rgba: &[u8]) -> Vec<u32> {
    rgba.chunks_exact(4)
        .map(|px| ((px[0] as u32) << 16) | ((px[1] as u32) << 8) | (px[2] as u32))
        .collect()
}

fn darken_pixels(pixels: &[u32], factor: f32) -> Vec<u32> {
    pixels
        .iter()
        .map(|&p| {
            let r = (((p >> 16) & 0xFF) as f32 * factor) as u32;
            let g = (((p >> 8) & 0xFF) as f32 * factor) as u32;
            let b = ((p & 0xFF) as f32 * factor) as u32;
            (r << 16) | (g << 8) | b
        })
        .collect()
}

/// Copy a `w×h` rectangle from `src` into `dst` at `(dx, dy)`. Source stride may differ
/// from the row length when the source is a sub-region of a wider buffer.
fn blit_pixels(
    dst: &mut [u32],
    dst_w: u32,
    src: &[u32],
    src_stride: u32,
    src_ox: u32,
    src_oy: u32,
    dx: u32,
    dy: u32,
    w: u32,
    h: u32,
) {
    let dst_w = dst_w as usize;
    let src_stride = src_stride as usize;
    let len = w as usize;
    for row in 0..(h as usize) {
        let dst_start = (dy as usize + row) * dst_w + dx as usize;
        let src_start = (src_oy as usize + row) * src_stride + src_ox as usize;
        if dst_start + len <= dst.len() && src_start + len <= src.len() {
            dst[dst_start..dst_start + len].copy_from_slice(&src[src_start..src_start + len]);
        }
    }
}

fn draw_border(
    buf: &mut [u32],
    buf_w: u32,
    buf_h: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    color: u32,
    thickness: u32,
) {
    let bw = buf_w as usize;
    let x2 = (x + w).min(buf_w);
    let y2 = (y + h).min(buf_h);
    for t in 0..thickness {
        let top = (y + t) as usize;
        let bot = y2.saturating_sub(1).saturating_sub(t) as usize;
        for col in x..x2 {
            let c = col as usize;
            if top < buf_h as usize {
                let i = top * bw + c;
                if i < buf.len() {
                    buf[i] = color;
                }
            }
            if bot != top && bot < buf_h as usize {
                let i = bot * bw + c;
                if i < buf.len() {
                    buf[i] = color;
                }
            }
        }
        let left = (x + t) as usize;
        let right = x2.saturating_sub(1).saturating_sub(t) as usize;
        for row in y..y2 {
            let r = row as usize;
            if r < buf_h as usize {
                let li = r * bw + left;
                if li < buf.len() {
                    buf[li] = color;
                }
                if right != left {
                    let ri = r * bw + right;
                    if ri < buf.len() {
                        buf[ri] = color;
                    }
                }
            }
        }
    }
}

fn normalize_rect(a: PhysicalPosition<f64>, b: PhysicalPosition<f64>) -> (u32, u32, u32, u32) {
    let x1 = a.x.min(b.x).max(0.0) as u32;
    let y1 = a.y.min(b.y).max(0.0) as u32;
    let x2 = a.x.max(b.x).max(0.0) as u32;
    let y2 = a.y.max(b.y).max(0.0) as u32;
    (x1, y1, x2.saturating_sub(x1), y2.saturating_sub(y1))
}
