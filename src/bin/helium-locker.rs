//! Native screen locker for helium-d77.
//!
//! Locks the session through the `ext-session-lock-v1` Wayland protocol, so
//! the compositor itself enforces the lock rather than a window merely being
//! drawn on top.
//!
//! The protocol side is the Wayland backend from Amorphis
//! (<https://github.com/dani-77/amorphis>), driven through
//! smithay-client-toolkit, together with Amorphis's security modules
//! (vendored in `src/lock/`). The look is `ui/lock.slint`, compiled in at build
//! time and drawn with Slint's software renderer straight into `wl_shm`
//! buffers. layer-shika is deliberately not involved: its session-lock path
//! creates the lock and its surfaces in one request batch, does not report
//! `finished` back to the application, and has no flushed unlock, which is
//! what made the old layer-shika-based locker lose the lock or stay stuck.
//!
//! Lifecycle, all fail-closed:
//! 1. Refuse to start unless `/etc/pam.d/helium-locker` exists, core dumps
//!    are disabled and the user resolves through the passwd database.
//! 2. Bind every keyboard and let the compositor see it before asking for
//!    the lock: some compositors focus a lock surface only once, as it is
//!    created, and a keyboard bound later never gets that `enter`.
//! 3. Request the lock, round-trip, and only then create one lock surface per
//!    output, so the compositor's deferred focus clear can't take keyboard
//!    focus away from a brand-new lock surface.
//! 4. Keys go into an mlock'ed `SecretBuffer`, never into Slint. Enter hands
//!    the buffer to a PAM worker thread, so a slow PAM module (`pam_faildelay`,
//!    network auth) never freezes the screen.
//! 5. Only a PAM success, checked by the `LockMachine`, leads to
//!    `unlock_and_destroy`, which is round-tripped before the process exits.
//!    A compositor `finished` is never treated as an unlock.

#[path = "../lock/mod.rs"]
mod lock;

use std::cell::RefCell;
use std::fmt;
use std::path::Path;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType, TargetPixel,
};
use slint::platform::{Platform, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, PhysicalSize, PlatformError};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    reexports::{calloop::EventLoop, calloop_wayland_source::WaylandSource},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        Capability, SeatHandler, SeatState,
    },
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
        SessionLockSurfaceConfigure,
    },
    shm::{
        slot::{Buffer, SlotPool},
        Shm, ShmHandler,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

use lock::auth::{AuthOutcome, AuthWorker, PamProvider, SubmitError};
use lock::secret::SecretBuffer;
use lock::state::{LockEvent, LockMachine, LockState};

// Compiled at build time, so a .slint error fails the build instead of
// surfacing only once the session is already locked.
slint::slint! {
    export { LockScreen } from "ui/lock.slint";
}

const PAM_SERVICE: &str = "helium-locker";
const PAM_SERVICE_FILE: &str = "/etc/pam.d/helium-locker";

/// How long one event-loop dispatch may block. Bounds the latency of PAM
/// results and of the clock, which are polled rather than event-driven.
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Refuse absurd configure sizes instead of trying to allocate them.
const MAX_SURFACE_DIMENSION: u32 = 16_384;
const MAX_BUFFER_SCALE: i32 = 8;

fn main() -> ExitCode {
    env_logger::init();

    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => {
            println!("helium-locker {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("--help" | "-h") => {
            println!(
                "helium-locker {}\n\nLocks the Wayland session (ext-session-lock-v1) until the \
                 user's password is accepted by PAM.\nRequires the PAM service {PAM_SERVICE_FILE}.",
                env!("CARGO_PKG_VERSION")
            );
            return ExitCode::SUCCESS;
        }
        Some(_) => {
            eprintln!("helium-locker: unknown argument; try --help (session unchanged)");
            return ExitCode::from(2);
        }
        None => {}
    }

    let Some(username) = lock_identity() else {
        return ExitCode::from(2);
    };

    match run(&username) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("helium-locker: {error}");
            ExitCode::from(1)
        }
    }
}

/// Everything that must hold before the session is locked, so a lock that
/// could never be unlocked is never entered. Mirrors Amorphis's main.rs.
fn lock_identity() -> Option<String> {
    if !Path::new(PAM_SERVICE_FILE).is_file() {
        eprintln!(
            "helium-locker: refusing to lock: PAM service {PAM_SERVICE_FILE} is not installed"
        );
        return None;
    }
    if let Err(error) = lock::system::harden_process() {
        eprintln!("helium-locker: refusing to lock: {error}");
        return None;
    }
    match lock::system::current_username() {
        Ok(username) => Some(username),
        Err(error) => {
            eprintln!("helium-locker: refusing to lock: {error}");
            None
        }
    }
}

#[derive(Debug)]
enum LockerError {
    Connect(String),
    Registry(String),
    MissingGlobal(&'static str),
    EventLoop(String),
    EventSource(String),
    ConnectionLost(String),
    NoKeyboard,
    LockProtocolUnavailable,
    LockRefused,
    ProtocolState,
    Ui(String),
    AuthenticationWorker(String),
    AuthenticationStopped,
}

impl fmt::Display for LockerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "could not connect to the Wayland display: {e}"),
            Self::Registry(e) => write!(f, "could not read the Wayland registry: {e}"),
            Self::MissingGlobal(name) => write!(f, "compositor does not provide {name}"),
            Self::EventLoop(e) => write!(f, "could not create the event loop: {e}"),
            Self::EventSource(e) => write!(f, "could not watch the Wayland connection: {e}"),
            Self::ConnectionLost(e) => write!(f, "lost the Wayland connection: {e}"),
            Self::NoKeyboard => f.write_str("refusing to lock: no keyboard is available"),
            Self::LockProtocolUnavailable => {
                f.write_str("compositor does not support ext-session-lock-v1")
            }
            Self::LockRefused => f.write_str("compositor refused the session lock"),
            Self::ProtocolState => f.write_str(
                "session lock ended unexpectedly; the compositor keeps the session locked",
            ),
            Self::Ui(e) => write!(f, "could not create the lock screen UI: {e}"),
            Self::AuthenticationWorker(e) => {
                write!(f, "could not start the authentication worker: {e}")
            }
            Self::AuthenticationStopped => {
                f.write_str("authentication worker stopped while the session was locked")
            }
        }
    }
}

fn run(username: &str) -> Result<(), LockerError> {
    // Slint has to know how to make windows before the first component is
    // created. Each LockScreen instance takes the window parked in `next`.
    let platform = Rc::new(LockPlatform::new());
    slint::platform::set_platform(Box::new(SharedPlatform(Rc::clone(&platform))))
        .map_err(|e| LockerError::Ui(e.to_string()))?;

    let conn = Connection::connect_to_env().map_err(|e| LockerError::Connect(e.to_string()))?;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).map_err(|e| LockerError::Registry(e.to_string()))?;
    let qh: QueueHandle<Locker> = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh)
        .map_err(|_| LockerError::MissingGlobal("wl_compositor"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|_| LockerError::MissingGlobal("wl_shm"))?;
    let pool = SlotPool::new(4 * 1024 * 1024, &shm)
        .map_err(|e| LockerError::Ui(format!("could not allocate shared memory: {e}")))?;
    let session_lock_state = SessionLockState::new(&globals, &qh);
    let auth = AuthWorker::spawn(PamProvider::new(PAM_SERVICE, username))
        .map_err(|e| LockerError::AuthenticationWorker(e.to_string()))?;
    let secret =
        SecretBuffer::standard().map_err(|e| LockerError::AuthenticationWorker(e.to_string()))?;
    if !secret.is_memory_locked() {
        eprintln!("helium-locker: warning: password buffer could not be locked against swap");
    }
    let mut event_loop: EventLoop<Locker> =
        EventLoop::try_new().map_err(|e| LockerError::EventLoop(e.to_string()))?;

    let mut app = Locker {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor_state,
        shm,
        pool,
        session_lock_state,
        session_lock: None,
        platform,
        surfaces: Vec::new(),
        keyboards: Vec::new(),
        modifiers: Modifiers::default(),
        machine: LockMachine::new(),
        auth,
        active_attempt: None,
        secret,
        view: ViewState::default(),
        exit: None,
    };
    app.view.refresh_clock();

    // Step 2: bind keyboards, then make the get_keyboard requests visible to
    // the compositor before the lock request (second round-trip).
    event_queue
        .roundtrip(&mut app)
        .map_err(|e| LockerError::ConnectionLost(e.to_string()))?;
    if app.keyboards.is_empty() {
        return Err(LockerError::NoKeyboard);
    }
    event_queue
        .roundtrip(&mut app)
        .map_err(|e| LockerError::ConnectionLost(e.to_string()))?;

    // Step 3: lock, round-trip, then create the surfaces.
    app.machine
        .apply(LockEvent::RequestLock)
        .map_err(|_| LockerError::ProtocolState)?;
    let session_lock = match app.session_lock_state.lock(&qh) {
        Ok(lock) => lock,
        Err(_) => {
            let _ = app.machine.apply(LockEvent::LockRefused);
            return Err(LockerError::LockProtocolUnavailable);
        }
    };
    app.session_lock = Some(session_lock);

    // Some compositors confirm the lock right away, others only once the
    // surfaces have been committed, so both states are fine here.
    event_queue
        .roundtrip(&mut app)
        .map_err(|e| LockerError::ConnectionLost(e.to_string()))?;
    if let Some(exit) = app.exit.take() {
        return exit;
    }
    if !matches!(
        app.machine.state(),
        LockState::LockRequested | LockState::Locked
    ) {
        return Err(LockerError::ProtocolState);
    }

    let outputs: Vec<_> = app.output_state.outputs().collect();
    for output in outputs {
        app.add_output(&qh, output);
    }

    WaylandSource::new(conn.clone(), event_queue)
        .insert(event_loop.handle())
        .map_err(|e| LockerError::EventSource(e.to_string()))?;

    loop {
        event_loop
            .dispatch(EVENT_POLL_INTERVAL, &mut app)
            .map_err(|e| LockerError::ConnectionLost(e.to_string()))?;
        if let Some(exit) = app.exit.take() {
            return exit;
        }
        app.poll_authentication(&conn)?;
        if let Some(exit) = app.exit.take() {
            return exit;
        }
        if app.view.refresh_clock() {
            app.sync_view();
        }
        slint::platform::update_timers_and_animations();
        app.render_all();
    }
}

// ---------------------------------------------------------------------------
// Slint platform: one MinimalSoftwareWindow per lock surface.

struct LockPlatform {
    /// The window the next `LockScreen::new()` will be given.
    next_window: RefCell<Option<Rc<MinimalSoftwareWindow>>>,
    start: Instant,
}

impl LockPlatform {
    fn new() -> Self {
        Self {
            next_window: RefCell::new(None),
            start: Instant::now(),
        }
    }
}

struct SharedPlatform(Rc<LockPlatform>);

impl Platform for SharedPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        self.0
            .next_window
            .borrow_mut()
            .take()
            .map(|window| window as Rc<dyn WindowAdapter>)
            .ok_or_else(|| PlatformError::Other("no lock surface waiting for a window".into()))
    }

    fn duration_since_start(&self) -> Duration {
        self.0.start.elapsed()
    }
}

/// `wl_shm` XRGB8888 is a little-endian 32-bit word, i.e. B, G, R, X in
/// memory. Every bit pattern is valid and the alignment is 1, which is what
/// makes reinterpreting the mapped byte slice as pixels sound.
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct Xrgb8888 {
    b: u8,
    g: u8,
    r: u8,
    x: u8,
}

impl TargetPixel for Xrgb8888 {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let inverse = u16::from(u8::MAX - color.alpha);
        self.r = (u16::from(self.r) * inverse / 255) as u8 + color.red;
        self.g = (u16::from(self.g) * inverse / 255) as u8 + color.green;
        self.b = (u16::from(self.b) * inverse / 255) as u8 + color.blue;
    }

    fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self { b, g, r, x: 0xff }
    }
}

fn as_pixels(bytes: &mut [u8]) -> &mut [Xrgb8888] {
    const _: () = assert!(std::mem::size_of::<Xrgb8888>() == 4);
    const _: () = assert!(std::mem::align_of::<Xrgb8888>() == 1);
    let len = bytes.len() / 4;
    // SAFETY: Xrgb8888 is four u8 fields with repr(C): size 4, alignment 1,
    // no padding and no invalid bit patterns. `len * 4 <= bytes.len()`, and
    // the returned slice borrows `bytes` mutably for its whole lifetime.
    unsafe { std::slice::from_raw_parts_mut(bytes.as_mut_ptr().cast::<Xrgb8888>(), len) }
}

// ---------------------------------------------------------------------------
// What every LockScreen instance shows. Kept in Rust and pushed to all
// outputs so they stay identical.

#[derive(Default)]
struct ViewState {
    clock: String,
    date: String,
    password_length: usize,
    show_error: bool,
    checking: bool,
    caps_lock: bool,
}

impl ViewState {
    /// Returns whether the visible text changed.
    fn refresh_clock(&mut self) -> bool {
        let clock = helium_wsl::services::time::formatted("%H:%M");
        let date = helium_wsl::services::time::formatted("%A, %d %B");
        let changed = clock != self.clock || date != self.date;
        self.clock = clock;
        self.date = date;
        changed
    }

    fn apply(&self, ui: &LockScreen) {
        ui.set_clock_text(self.clock.as_str().into());
        ui.set_date_text(self.date.as_str().into());
        ui.set_password_length(i32::try_from(self.password_length).unwrap_or(i32::MAX));
        ui.set_show_error(self.show_error);
        ui.set_checking(self.checking);
        ui.set_caps_lock(self.caps_lock);
    }
}

// ---------------------------------------------------------------------------
// Wayland state.

struct LockedOutput {
    output: wl_output::WlOutput,
    // Retaining the protocol object keeps this output covered.
    surface: SessionLockSurface,
    window: Rc<MinimalSoftwareWindow>,
    ui: LockScreen,
    /// Logical size from the last configure; None until the first one.
    configured_size: Option<(u32, u32)>,
    scale: i32,
    /// Keeps the last committed buffer alive until the compositor releases it.
    buffer: Option<Buffer>,
}

struct Locker {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor_state: CompositorState,
    shm: Shm,
    pool: SlotPool,
    session_lock_state: SessionLockState,
    session_lock: Option<SessionLock>,
    platform: Rc<LockPlatform>,
    surfaces: Vec<LockedOutput>,
    keyboards: Vec<(wl_seat::WlSeat, wl_keyboard::WlKeyboard)>,
    modifiers: Modifiers,
    machine: LockMachine,
    auth: AuthWorker,
    active_attempt: Option<u64>,
    secret: SecretBuffer,
    view: ViewState,
    exit: Option<Result<(), LockerError>>,
}

impl Locker {
    fn add_output(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if self.surfaces.iter().any(|entry| entry.output == output) {
            return;
        }
        let Some(session_lock) = self.session_lock.as_ref() else {
            return;
        };

        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        *self.platform.next_window.borrow_mut() = Some(Rc::clone(&window));
        let ui = match LockScreen::new() {
            Ok(ui) => ui,
            Err(error) => {
                // Without a UI this output would show only the compositor's
                // fallback colour; still lock it, but say why.
                eprintln!("helium-locker: could not create lock screen UI: {error}");
                self.platform.next_window.borrow_mut().take();
                if self.machine.state() == LockState::LockRequested {
                    self.exit = Some(Err(LockerError::Ui(error.to_string())));
                }
                return;
            }
        };
        self.view.apply(&ui);
        if let Err(error) = ui.show() {
            eprintln!("helium-locker: could not show lock screen UI: {error}");
        }

        let scale = self
            .output_state
            .info(&output)
            .map_or(1, |info| info.scale_factor)
            .clamp(1, MAX_BUFFER_SCALE);

        let wl_surface = self.compositor_state.create_surface(qh);
        let surface = session_lock.create_lock_surface(wl_surface, &output, qh);
        self.surfaces.push(LockedOutput {
            output,
            surface,
            window,
            ui,
            configured_size: None,
            scale,
            buffer: None,
        });
    }

    fn sync_view(&self) {
        for entry in &self.surfaces {
            self.view.apply(&entry.ui);
        }
    }

    /// Push the current size and scale to the Slint window and force a full
    /// repaint: after a configure the compositor needs a buffer of exactly
    /// the new size, whether or not the content changed.
    fn resize_window(entry: &LockedOutput) {
        let Some((width, height)) = entry.configured_size else {
            return;
        };
        let scale = entry.scale as u32;
        entry
            .window
            .dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: entry.scale as f32,
            });
        entry
            .window
            .set_size(PhysicalSize::new(width * scale, height * scale));
        entry.window.request_redraw();
    }

    fn render_all(&mut self) {
        for index in 0..self.surfaces.len() {
            if let Err(error) = self.render(index) {
                eprintln!("helium-locker: could not draw the lock screen: {error}");
                if self.machine.state() == LockState::LockRequested {
                    // The compositor waits for a frame before confirming the
                    // lock; failing to produce one must not look like success.
                    self.exit = Some(Err(LockerError::ProtocolState));
                }
            }
        }
    }

    fn render(&mut self, index: usize) -> Result<(), String> {
        let pool = &mut self.pool;
        let entry = &mut self.surfaces[index];
        let Some((width, height)) = entry.configured_size else {
            return Ok(());
        };
        let scale = entry.scale as u32;
        let (buffer_width, buffer_height) = (width * scale, height * scale);
        let width_i32 = i32::try_from(buffer_width).map_err(|_| "surface too wide")?;
        let height_i32 = i32::try_from(buffer_height).map_err(|_| "surface too tall")?;
        let stride = width_i32.checked_mul(4).ok_or("stride overflow")?;

        let mut result = Ok(());
        let wl_surface = entry.surface.wl_surface().clone();
        let mut new_buffer = None;
        entry.window.draw_if_needed(|renderer| {
            let (buffer, canvas) =
                match pool.create_buffer(width_i32, height_i32, stride, wl_shm::Format::Xrgb8888) {
                    Ok(pair) => pair,
                    Err(error) => {
                        result = Err(format!("could not allocate a frame buffer: {error}"));
                        return;
                    }
                };
            renderer.render(as_pixels(canvas), buffer_width as usize);
            if let Err(error) = buffer.attach_to(&wl_surface) {
                result = Err(format!("could not attach the frame buffer: {error:?}"));
                return;
            }
            wl_surface.set_buffer_scale(entry.scale);
            wl_surface.damage_buffer(0, 0, width_i32, height_i32);
            wl_surface.commit();
            new_buffer = Some(buffer);
        });
        if new_buffer.is_some() {
            entry.buffer = new_buffer;
        }
        result
    }

    fn handle_key(&mut self, event: KeyEvent) {
        if self.machine.state() != LockState::Locked {
            return;
        }

        let mut edited = false;
        match event.keysym {
            Keysym::Escape => {
                self.secret.clear();
                edited = true;
            }
            Keysym::BackSpace => {
                self.secret.pop_char();
                edited = true;
            }
            Keysym::Return | Keysym::KP_Enter => {
                if !self.secret.is_empty() {
                    self.submit_authentication();
                }
                return;
            }
            Keysym::u if self.modifiers.ctrl => {
                self.secret.clear();
                edited = true;
            }
            _ if !self.modifiers.ctrl && !self.modifiers.alt && !self.modifiers.logo => {
                if let Some(text) = event.utf8 {
                    if !text.is_empty()
                        && text.chars().all(|character| !character.is_control())
                        && self.secret.push_str(&text).is_ok()
                    {
                        edited = true;
                    }
                }
            }
            _ => {}
        }

        if edited {
            self.view.password_length = self.secret_char_count();
            self.view.show_error = false;
            self.sync_view();
        }
    }

    fn secret_char_count(&self) -> usize {
        std::str::from_utf8(self.secret.as_bytes()).map_or(0, |text| text.chars().count())
    }

    fn submit_authentication(&mut self) {
        let Ok(replacement) = SecretBuffer::standard() else {
            return;
        };
        if !replacement.is_memory_locked() {
            eprintln!(
                "helium-locker: warning: replacement password buffer could not be locked against swap"
            );
        }
        let submitted = std::mem::replace(&mut self.secret, replacement);
        match self.auth.submit(submitted) {
            Ok(attempt) => {
                if self.machine.apply(LockEvent::BeginAuthentication).is_err() {
                    self.exit = Some(Err(LockerError::ProtocolState));
                    return;
                }
                self.active_attempt = Some(attempt);
                self.view.checking = true;
                self.view.show_error = false;
                self.view.password_length = 0;
                self.sync_view();
            }
            Err(SubmitError::Busy(secret)) => self.secret = secret,
            Err(SubmitError::Stopped(secret)) => {
                self.secret = secret;
                self.exit = Some(Err(LockerError::AuthenticationStopped));
            }
        }
    }

    fn poll_authentication(&mut self, conn: &Connection) -> Result<(), LockerError> {
        let response = self
            .auth
            .try_response()
            .map_err(|_| LockerError::AuthenticationStopped)?;
        let Some(response) = response else {
            return Ok(());
        };
        if self.active_attempt.take() != Some(response.attempt) {
            return Err(LockerError::ProtocolState);
        }

        match response.outcome {
            AuthOutcome::Success => {
                self.machine
                    .apply(LockEvent::AuthenticationSucceeded)
                    .map_err(|_| LockerError::ProtocolState)?;
                self.machine
                    .apply(LockEvent::BeginUnlock)
                    .map_err(|_| LockerError::ProtocolState)?;
                self.session_lock
                    .as_ref()
                    .ok_or(LockerError::ProtocolState)?
                    .unlock();
                // Make sure unlock_and_destroy has reached the compositor
                // before the process (and its connection) goes away.
                conn.roundtrip()
                    .map_err(|e| LockerError::ConnectionLost(e.to_string()))?;
                self.machine
                    .apply(LockEvent::UnlockCompleted)
                    .map_err(|_| LockerError::ProtocolState)?;
                self.exit = Some(Ok(()));
            }
            AuthOutcome::Denied { stage, code } | AuthOutcome::Error { stage, code } => {
                eprintln!("helium-locker: authentication failed ({stage:?}: {code:?})");
                self.machine
                    .apply(LockEvent::AuthenticationFailed)
                    .map_err(|_| LockerError::ProtocolState)?;
                self.view.checking = false;
                self.view.show_error = true;
                self.sync_view();
            }
        }
        Ok(())
    }

    fn remove_keyboard(&mut self, seat: &wl_seat::WlSeat) {
        if let Some(index) = self.keyboards.iter().position(|(known, _)| known == seat) {
            let (_, keyboard) = self.keyboards.swap_remove(index);
            keyboard.release();
        }
    }
}

impl SessionLockHandler for Locker {
    fn locked(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _lock: SessionLock) {
        if self.machine.apply(LockEvent::LockConfirmed).is_err() {
            self.exit = Some(Err(LockerError::ProtocolState));
        }
    }

    fn finished(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _lock: SessionLock) {
        if self.machine.state() == LockState::LockRequested {
            let _ = self.machine.apply(LockEvent::LockRefused);
            self.exit = Some(Err(LockerError::LockRefused));
        } else {
            // Never translate a compositor-side termination into an unlock.
            self.exit = Some(Err(LockerError::ProtocolState));
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _serial: u32,
    ) {
        let (width, height) = configure.new_size;
        let Some(index) = self
            .surfaces
            .iter()
            .position(|entry| entry.surface.wl_surface() == surface.wl_surface())
        else {
            return;
        };
        if width == 0
            || height == 0
            || width > MAX_SURFACE_DIMENSION
            || height > MAX_SURFACE_DIMENSION
        {
            eprintln!("helium-locker: ignoring invalid lock surface size {width}x{height}");
            if self.machine.state() == LockState::LockRequested {
                self.exit = Some(Err(LockerError::ProtocolState));
            }
            return;
        }
        self.surfaces[index].configured_size = Some((width, height));
        Self::resize_window(&self.surfaces[index]);
        // Answer the configure with a frame right away (sctk has already
        // acked it); the compositor confirms the lock once every output has
        // a committed buffer.
        if let Err(error) = self.render(index) {
            eprintln!("helium-locker: could not draw the lock screen: {error}");
            if self.machine.state() == LockState::LockRequested {
                self.exit = Some(Err(LockerError::ProtocolState));
            }
        }
    }
}

impl CompositorHandler for Locker {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        let Some(index) = self
            .surfaces
            .iter()
            .position(|entry| entry.surface.wl_surface() == surface)
        else {
            return;
        };
        let scale = new_factor.clamp(1, MAX_BUFFER_SCALE);
        if self.surfaces[index].scale == scale {
            return;
        }
        self.surfaces[index].scale = scale;
        Self::resize_window(&self.surfaces[index]);
        let _ = self.render(index);
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Locker {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // Hotplugged monitors must be covered too.
        self.add_output(qh, output);
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.surfaces.retain(|entry| entry.output != output);
    }
}

impl SeatHandler for Locker {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && !self.keyboards.iter().any(|(known, _)| *known == seat)
        {
            if let Ok(keyboard) = self.seat_state.get_keyboard(qh, &seat, None) {
                self.keyboards.push((seat, keyboard));
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            self.remove_keyboard(&seat);
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.remove_keyboard(&seat);
    }
}

impl KeyboardHandler for Locker {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.handle_key(event);
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.handle_key(event);
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
        self.modifiers = modifiers;
        if self.view.caps_lock != modifiers.caps_lock {
            self.view.caps_lock = modifiers.caps_lock;
            self.sync_view();
        }
    }
}

impl ShmHandler for Locker {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Locker {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_compositor!(Locker);
smithay_client_toolkit::delegate_output!(Locker);
smithay_client_toolkit::delegate_seat!(Locker);
smithay_client_toolkit::delegate_keyboard!(Locker);
smithay_client_toolkit::delegate_session_lock!(Locker);
smithay_client_toolkit::delegate_shm!(Locker);
smithay_client_toolkit::delegate_registry!(Locker);
