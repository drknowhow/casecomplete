//! System tray icon and menu for the CaseComplete typing assistant.
//!
//! On Windows a tray icon only works when the thread that created it runs a
//! Win32 message loop. [`Tray::new`] therefore spawns a dedicated thread that
//! owns the icon and the menu and pumps messages. Menu clicks are read from
//! `tray_icon::menu::MenuEvent::receiver()` on that thread, translated into
//! [`TrayEvent`]s and forwarded over a crossbeam channel. `set_status` and
//! `set_state` are marshalled to the tray thread through a command channel,
//! and the thread is woken with `PostThreadMessageW` (the loop also wakes
//! every 100 ms as a fallback).
//!
//! A left click on the icon itself does not open the menu (that stays on the
//! right button); it is reported as [`TrayEvent::IconClick`] so the app can
//! toggle a flyout window. Windows delivers a down and an up event per click;
//! only the up event is forwarded, so one click yields exactly one event.
//!
//! Note: `MenuEvent::receiver()` and `TrayIconEvent::receiver()` are
//! process-global channels. This crate drains both on its own thread and
//! only forwards events for its own menu items, so it assumes it is the
//! only user of `tray-icon` / `muda` in the process.

use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use tray_icon::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, PostThreadMessageW,
    TranslateMessage, MSG, PM_REMOVE, QS_ALLINPUT, WM_APP, WM_QUIT,
};

/// Something the user did in the tray menu.
#[derive(Debug, Clone, PartialEq)]
pub enum TrayEvent {
    /// "Enabled" checkbox toggled; carries the new state.
    Enabled(bool),
    /// "Pause 10 min" / "Pause 60 min".
    PauseMinutes(u32),
    /// Auto-fix level chosen: `"off"` | `"S0"` | `"S1"` | `"S2"`.
    AutoFixLevel(String),
    /// Model chosen in the "Model" submenu; carries the model id, e.g. `"gemma-3-1b-pt"`.
    Model(String),
    /// "Open dashboard" menu item.
    OpenDashboard,
    OpenProfileFolder,
    ShowMetrics,
    /// Left click (button released) on the tray icon itself, not on a menu item.
    IconClick,
    Quit,
}

/// What the menu should currently show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayState {
    pub enabled: bool,
    /// `"off"` | `"S0"` | `"S1"` | `"S2"` (case-insensitive).
    pub auto_fix_level: String,
    /// Text of the first (disabled) menu line, also used for the tooltip.
    pub status_line: String,
    /// Model id: `"gemma-3-1b-pt"` | `"smollm2-360m"` | `"qwen2.5-1.5b"` (case-insensitive).
    pub model: String,
}

const TOOLTIP_BASE: &str = "CaseComplete";
/// The Windows tooltip buffer is 128 UTF-16 units including the terminator.
const TOOLTIP_MAX_CHARS: usize = 100;
/// Fallback wake-up interval of the message loop.
const POLL_MS: u32 = 100;
const INIT_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

const ID_STATUS: &str = "cc-tray.status";
const ID_OPEN_DASHBOARD: &str = "cc-tray.open_dashboard";
const ID_ENABLED: &str = "cc-tray.enabled";
const ID_PAUSE_10: &str = "cc-tray.pause10";
const ID_PAUSE_60: &str = "cc-tray.pause60";
const ID_PROFILE: &str = "cc-tray.profile";
const ID_METRICS: &str = "cc-tray.metrics";
const ID_QUIT: &str = "cc-tray.quit";

/// Auto-fix levels: (menu id, value reported in events, menu label).
const LEVELS: [(&str, &str, &str); 4] = [
    ("cc-tray.level.off", "off", "Off"),
    ("cc-tray.level.s0", "S0", "Typos (S0)"),
    ("cc-tray.level.s1", "S1", "Typos + grammar (S1)"),
    ("cc-tray.level.s2", "S2", "Also ambiguous (S2)"),
];

/// Models: (menu id, value reported in events, menu label).
const MODELS: [(&str, &str, &str); 3] = [
    ("cc-tray.model.gemma-3-1b-pt", "gemma-3-1b-pt", "Gemma 3 1B (Google)"),
    ("cc-tray.model.smollm2-360m", "smollm2-360m", "SmolLM2 360M (Hugging Face)"),
    ("cc-tray.model.qwen2.5-1.5b", "qwen2.5-1.5b", "Qwen2.5 1.5B (Alibaba)"),
];

enum Cmd {
    SetStatus(String),
    SetState(TrayState),
    Shutdown,
}

/// Handle to the tray thread. Dropping it removes the icon and ends the thread.
pub struct Tray {
    events_rx: Receiver<TrayEvent>,
    cmd_tx: Sender<Cmd>,
    thread_id: u32,
    /// Disconnects when the tray thread has finished.
    done_rx: Receiver<()>,
    handle: Option<JoinHandle<()>>,
}

impl Tray {
    /// Spawns the tray thread (message loop) and creates the icon. Returns when the icon exists.
    pub fn new(initial: TrayState) -> Result<Tray> {
        let (events_tx, events_rx) = unbounded::<TrayEvent>();
        let (cmd_tx, cmd_rx) = unbounded::<Cmd>();
        let (init_tx, init_rx) = unbounded::<std::result::Result<u32, String>>();
        let (done_tx, done_rx) = unbounded::<()>();

        let handle = thread::Builder::new()
            .name("cc-tray".into())
            .spawn(move || {
                // Dropped when the thread ends (also on panic): wakes `Drop for Tray`.
                let _done = done_tx;
                let mut ui = match Ui::build(&initial) {
                    Ok(ui) => ui,
                    Err(e) => {
                        let _ = init_tx.send(Err(format!("{e:#}")));
                        return;
                    }
                };
                // SAFETY: plain query of the calling thread's id.
                let thread_id = unsafe { GetCurrentThreadId() };
                let _ = init_tx.send(Ok(thread_id));
                ui.run(&cmd_rx, &events_tx);
                log::debug!("cc-tray: message loop ended, removing icon");
                // `ui` (and with it the TrayIcon) drops here, on the thread that created it.
            })
            .context("failed to spawn the cc-tray thread")?;

        match init_rx.recv_timeout(INIT_TIMEOUT) {
            Ok(Ok(thread_id)) => Ok(Tray {
                events_rx,
                cmd_tx,
                thread_id,
                done_rx,
                handle: Some(handle),
            }),
            Ok(Err(msg)) => {
                let _ = handle.join();
                bail!("failed to create the tray icon: {msg}")
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = handle.join();
                Err(anyhow!("the tray thread died while creating the icon"))
            }
            Err(RecvTimeoutError::Timeout) => {
                Err(anyhow!("timed out waiting for the tray icon to be created"))
            }
        }
    }

    /// Events, forwarded from the tray thread.
    pub fn events(&self) -> &Receiver<TrayEvent> {
        &self.events_rx
    }

    /// Updates the tooltip and the "status" menu line (e.g. "Ready - 12 fixes kept today").
    pub fn set_status(&self, line: &str) {
        self.send(Cmd::SetStatus(line.to_owned()));
    }

    /// Reflects state changed elsewhere (e.g. keyboard shortcut) in the menu check marks.
    ///
    /// An empty `status_line` leaves the current status untouched. This does
    /// not produce any [`TrayEvent`].
    pub fn set_state(&self, state: &TrayState) {
        self.send(Cmd::SetState(state.clone()));
    }

    fn send(&self, cmd: Cmd) {
        if self.cmd_tx.send(cmd).is_ok() {
            self.wake();
        }
    }

    /// Wakes the tray thread's message loop so it picks up commands at once.
    fn wake(&self) {
        // SAFETY: posting a thread message with no pointer payload. Failure
        // (thread gone, queue not ready) is harmless: the loop polls anyway.
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_APP, WPARAM(0), LPARAM(0));
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.send(Cmd::Shutdown);
        let Some(handle) = self.handle.take() else {
            return;
        };
        match self.done_rx.recv_timeout(SHUTDOWN_TIMEOUT) {
            Err(RecvTimeoutError::Timeout) => {
                // Typically a context menu is open: its modal loop only returns
                // once the user dismisses it. The thread then removes the icon
                // and exits on its own, so do not block the caller on it.
                log::warn!("cc-tray: tray thread still busy, leaving it to finish on its own");
            }
            _ => {
                let _ = handle.join();
            }
        }
    }
}

/// Everything that lives on the tray thread (the menu objects are not `Send`).
struct Ui {
    tray: TrayIcon,
    /// Kept alive for as long as the icon exists.
    _menu: Menu,
    status: MenuItem,
    enabled: CheckMenuItem,
    levels: Vec<CheckMenuItem>,
    level: usize,
    models: Vec<CheckMenuItem>,
    model: usize,
}

impl Ui {
    fn build(initial: &TrayState) -> Result<Ui> {
        let level = level_index(&initial.auto_fix_level).unwrap_or_else(|| {
            log::warn!(
                "cc-tray: unknown auto-fix level {:?}, showing \"off\"",
                initial.auto_fix_level
            );
            0
        });

        let model = model_index(&initial.model).unwrap_or_else(|| {
            log::warn!("cc-tray: unknown model {:?}, showing the first one", initial.model);
            0
        });

        let status = MenuItem::with_id(ID_STATUS, status_text(&initial.status_line), false, None);
        let open_dashboard = MenuItem::with_id(ID_OPEN_DASHBOARD, "Open dashboard", true, None);
        let enabled = CheckMenuItem::with_id(ID_ENABLED, "Enabled", true, initial.enabled, None);
        let pause10 = MenuItem::with_id(ID_PAUSE_10, "Pause 10 min", true, None);
        let pause60 = MenuItem::with_id(ID_PAUSE_60, "Pause 60 min", true, None);
        let levels: Vec<CheckMenuItem> = LEVELS
            .iter()
            .enumerate()
            .map(|(i, (id, _, label))| CheckMenuItem::with_id(*id, *label, true, i == level, None))
            .collect();
        let level_refs: Vec<&dyn IsMenuItem> =
            levels.iter().map(|item| item as &dyn IsMenuItem).collect();
        let autofix = Submenu::with_items("Auto-fix", true, &level_refs)
            .context("failed to build the Auto-fix submenu")?;
        let models: Vec<CheckMenuItem> = MODELS
            .iter()
            .enumerate()
            .map(|(i, (id, _, label))| CheckMenuItem::with_id(*id, *label, true, i == model, None))
            .collect();
        let model_refs: Vec<&dyn IsMenuItem> =
            models.iter().map(|item| item as &dyn IsMenuItem).collect();
        let model_menu = Submenu::with_items("Model", true, &model_refs)
            .context("failed to build the Model submenu")?;
        let profile = MenuItem::with_id(ID_PROFILE, "Open profile folder", true, None);
        let metrics = MenuItem::with_id(ID_METRICS, "Show metrics", true, None);
        let quit = MenuItem::with_id(ID_QUIT, "Quit", true, None);
        let sep1 = PredefinedMenuItem::separator();
        let sep2 = PredefinedMenuItem::separator();
        let sep3 = PredefinedMenuItem::separator();

        let menu = Menu::with_items(&[
            &status,
            &open_dashboard,
            &sep1,
            &enabled,
            &pause10,
            &pause60,
            &autofix,
            &model_menu,
            &sep2,
            &profile,
            &metrics,
            &sep3,
            &quit,
        ])
        .context("failed to build the tray menu")?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            // Left click is reported as `TrayEvent::IconClick`; the menu stays on the right button.
            .with_menu_on_left_click(false)
            .with_icon(make_icon()?)
            .with_tooltip(tooltip_for(&initial.status_line))
            .build()
            .context("failed to create the tray icon")?;

        Ok(Ui {
            tray,
            _menu: menu,
            status,
            enabled,
            levels,
            level,
            models,
            model,
        })
    }

    /// The Win32 message loop of the tray thread.
    fn run(&mut self, cmd_rx: &Receiver<Cmd>, events_tx: &Sender<TrayEvent>) {
        loop {
            // SAFETY: standard message-loop calls on this thread; `msg` outlives them.
            unsafe {
                MsgWaitForMultipleObjects(None, false, POLL_MS, QS_ALLINPUT);
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        return;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            // Menu clicks are produced while dispatching the tray window's messages above.
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if let Some(out) = self.on_menu_event(event.id.as_ref()) {
                    log::debug!("cc-tray: {out:?}");
                    if events_tx.send(out).is_err() {
                        return; // nobody is listening any more
                    }
                }
            }
            // Click / hover events on the icon itself: forward left clicks and
            // drain the rest (the global channel is unbounded).
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if let Some(out) = icon_click_event(&event) {
                    log::debug!("cc-tray: {out:?}");
                    if events_tx.send(out).is_err() {
                        return; // nobody is listening any more
                    }
                }
            }

            loop {
                match cmd_rx.try_recv() {
                    Ok(Cmd::SetStatus(line)) => self.apply_status(&line),
                    Ok(Cmd::SetState(state)) => self.apply_state(&state),
                    Ok(Cmd::Shutdown) => return,
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => return,
                }
            }
        }
    }

    /// Translates a clicked menu item into a [`TrayEvent`], keeping the
    /// radio-style auto-fix items consistent.
    fn on_menu_event(&mut self, id: &str) -> Option<TrayEvent> {
        match id {
            // muda has already flipped the check mark when the event is sent.
            ID_ENABLED => Some(TrayEvent::Enabled(self.enabled.is_checked())),
            ID_OPEN_DASHBOARD => Some(TrayEvent::OpenDashboard),
            ID_PAUSE_10 => Some(TrayEvent::PauseMinutes(10)),
            ID_PAUSE_60 => Some(TrayEvent::PauseMinutes(60)),
            ID_PROFILE => Some(TrayEvent::OpenProfileFolder),
            ID_METRICS => Some(TrayEvent::ShowMetrics),
            ID_QUIT => Some(TrayEvent::Quit),
            other => {
                if let Some(idx) = MODELS.iter().position(|(model_id, _, _)| *model_id == other) {
                    let changed = idx != self.model;
                    self.select_model(idx);
                    return changed.then(|| TrayEvent::Model(MODELS[idx].1.to_owned()));
                }
                let idx = LEVELS.iter().position(|(level_id, _, _)| *level_id == other)?;
                let changed = idx != self.level;
                // Clicking a check item toggles it; a radio group must always keep one selected.
                self.select_level(idx);
                changed.then(|| TrayEvent::AutoFixLevel(LEVELS[idx].1.to_owned()))
            }
        }
    }

    fn select_level(&mut self, idx: usize) {
        for (i, item) in self.levels.iter().enumerate() {
            item.set_checked(i == idx);
        }
        self.level = idx;
    }

    fn select_model(&mut self, idx: usize) {
        for (i, item) in self.models.iter().enumerate() {
            item.set_checked(i == idx);
        }
        self.model = idx;
    }

    fn apply_status(&mut self, line: &str) {
        self.status.set_text(status_text(line));
        if let Err(e) = self.tray.set_tooltip(Some(tooltip_for(line))) {
            log::warn!("cc-tray: failed to update the tooltip: {e}");
        }
    }

    fn apply_state(&mut self, state: &TrayState) {
        self.enabled.set_checked(state.enabled);
        match level_index(&state.auto_fix_level) {
            Some(idx) => self.select_level(idx),
            None => log::warn!(
                "cc-tray: unknown auto-fix level {:?}, keeping the current one",
                state.auto_fix_level
            ),
        }
        match model_index(&state.model) {
            Some(idx) => self.select_model(idx),
            None => log::warn!(
                "cc-tray: unknown model {:?}, keeping the current one",
                state.model
            ),
        }
        if !state.status_line.is_empty() {
            self.apply_status(&state.status_line);
        }
    }
}

/// A left-button *release* on the tray icon becomes [`TrayEvent::IconClick`].
///
/// Windows reports a click as a button-down and a button-up event; taking only
/// the up half makes one physical click produce one event. Other buttons,
/// double-click notifications and hover events are ignored.
fn icon_click_event(event: &TrayIconEvent) -> Option<TrayEvent> {
    match event {
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } => Some(TrayEvent::IconClick),
        _ => None,
    }
}

fn level_index(level: &str) -> Option<usize> {
    LEVELS
        .iter()
        .position(|(_, value, _)| value.eq_ignore_ascii_case(level.trim()))
}

fn model_index(model: &str) -> Option<usize> {
    MODELS
        .iter()
        .position(|(_, value, _)| value.eq_ignore_ascii_case(model.trim()))
}

/// Menu text for the status line. `&` would be read as a mnemonic marker.
fn status_text(line: &str) -> String {
    let line = line.trim();
    if line.is_empty() {
        TOOLTIP_BASE.to_owned()
    } else {
        line.replace('&', "&&")
    }
}

fn tooltip_for(status: &str) -> String {
    let status = status.trim();
    if status.is_empty() {
        return TOOLTIP_BASE.to_owned();
    }
    let full = format!("{TOOLTIP_BASE}\n{status}");
    if full.chars().count() <= TOOLTIP_MAX_CHARS {
        full
    } else {
        let mut cut: String = full.chars().take(TOOLTIP_MAX_CHARS - 3).collect();
        cut.push_str("...");
        cut
    }
}

/// A 32x32 rounded blue square with a white "C", drawn with 4x4 supersampling.
fn make_icon() -> Result<Icon> {
    let (rgba, size) = icon_rgba();
    Icon::from_rgba(rgba, size, size).context("failed to build the tray icon image")
}

const ICON_SIZE: u32 = 32;

fn icon_rgba() -> (Vec<u8>, u32) {
    const SAMPLES: u32 = 4;
    const BLUE: [f32; 3] = [37.0, 99.0, 235.0];
    let total = (SAMPLES * SAMPLES) as f32;
    let mut rgba = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for py in 0..ICON_SIZE {
        for px in 0..ICON_SIZE {
            let (mut in_square, mut in_letter) = (0u32, 0u32);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = px as f32 + (sx as f32 + 0.5) / SAMPLES as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SAMPLES as f32;
                    if in_rounded_square(x, y) {
                        in_square += 1;
                        if in_letter_c(x, y) {
                            in_letter += 1;
                        }
                    }
                }
            }
            if in_square == 0 {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let white = in_letter as f32 / in_square as f32;
            for channel in BLUE {
                rgba.push((channel * (1.0 - white) + 255.0 * white).round() as u8);
            }
            rgba.push((in_square as f32 / total * 255.0).round() as u8);
        }
    }
    (rgba, ICON_SIZE)
}

/// Square inset by one pixel with a corner radius of 7 px.
fn in_rounded_square(x: f32, y: f32) -> bool {
    const LO: f32 = 1.0;
    const HI: f32 = ICON_SIZE as f32 - 1.0;
    const R: f32 = 7.0;
    if !(LO..=HI).contains(&x) || !(LO..=HI).contains(&y) {
        return false;
    }
    let dx = x - x.clamp(LO + R, HI - R);
    let dy = y - y.clamp(LO + R, HI - R);
    dx * dx + dy * dy <= R * R
}

/// A ring (radius 5..10 around the centre) open towards the right.
fn in_letter_c(x: f32, y: f32) -> bool {
    const CENTER: f32 = ICON_SIZE as f32 / 2.0;
    const OPENING_HALF_ANGLE: f32 = 0.7; // radians, about 40 degrees
    let (dx, dy) = (x - CENTER, y - CENTER);
    let dist = (dx * dx + dy * dy).sqrt();
    (5.0..=10.0).contains(&dist) && dy.atan2(dx).abs() > OPENING_HALF_ANGLE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_has_expected_size_and_content() {
        let (rgba, size) = icon_rgba();
        assert_eq!(size, 32);
        assert_eq!(rgba.len(), 32 * 32 * 4);
        let px = |x: usize, y: usize| &rgba[(y * 32 + x) * 4..(y * 32 + x) * 4 + 4];
        assert_eq!(px(0, 0)[3], 0, "corner is transparent");
        assert_eq!(px(16, 3), &[37, 99, 235, 255], "background is opaque blue");
        assert_eq!(px(6, 16), &[255, 255, 255, 255], "left stroke of the C is white");
        assert_eq!(px(26, 16)[..3], [37, 99, 235], "opening of the C shows the background");
        assert!(make_icon().is_ok());
    }

    /// Needs an interactive desktop session: `cargo test -p cc-tray -- --ignored`.
    #[test]
    #[ignore]
    fn tray_lifecycle_on_desktop() {
        let state = TrayState {
            enabled: true,
            auto_fix_level: "S1".into(),
            status_line: "Ready & waiting".into(),
            model: "smollm2-360m".into(),
        };
        let tray = Tray::new(state.clone()).expect("tray icon");
        tray.set_status("Ready - 12 fixes kept today");
        tray.set_state(&TrayState {
            enabled: false,
            auto_fix_level: "off".into(),
            status_line: String::new(),
            model: "smollm2-360m".into(),
        });
        tray.set_state(&TrayState {
            auto_fix_level: "bogus".into(),
            ..state
        });
        std::thread::sleep(Duration::from_millis(500));
        assert!(tray.events().try_recv().is_err(), "programmatic changes emit no events");
        let started = std::time::Instant::now();
        drop(tray);
        assert!(started.elapsed() < Duration::from_secs(1), "drop joins the thread promptly");
    }

    fn click(button: MouseButton, button_state: MouseButtonState) -> TrayIconEvent {
        TrayIconEvent::Click {
            id: tray_icon::TrayIconId::new("test"),
            position: tray_icon::dpi::PhysicalPosition::new(10.0, 20.0),
            rect: tray_icon::Rect::default(),
            button,
            button_state,
        }
    }

    #[test]
    fn left_button_release_is_one_icon_click() {
        // A physical left click arrives as Down then Up; only Up counts.
        let down = click(MouseButton::Left, MouseButtonState::Down);
        let up = click(MouseButton::Left, MouseButtonState::Up);
        let emitted = [down, up].iter().filter_map(icon_click_event).count();
        assert_eq!(emitted, 1);
        assert_eq!(
            icon_click_event(&click(MouseButton::Left, MouseButtonState::Up)),
            Some(TrayEvent::IconClick)
        );
    }

    #[test]
    fn other_icon_events_are_ignored() {
        for button in [MouseButton::Right, MouseButton::Middle] {
            for state in [MouseButtonState::Down, MouseButtonState::Up] {
                assert_eq!(icon_click_event(&click(button, state)), None);
            }
        }
        let id = tray_icon::TrayIconId::new("test");
        let position = tray_icon::dpi::PhysicalPosition::new(0.0, 0.0);
        let rect = tray_icon::Rect::default();
        let double = TrayIconEvent::DoubleClick {
            id: id.clone(),
            position,
            rect,
            button: MouseButton::Left,
        };
        let enter = TrayIconEvent::Enter { id: id.clone(), position, rect };
        let moved = TrayIconEvent::Move { id: id.clone(), position, rect };
        let leave = TrayIconEvent::Leave { id, position, rect };
        for event in [double, enter, moved, leave] {
            assert_eq!(icon_click_event(&event), None);
        }
    }

    #[test]
    fn menu_ids_are_unique() {
        let mut ids = vec![
            ID_STATUS,
            ID_OPEN_DASHBOARD,
            ID_ENABLED,
            ID_PAUSE_10,
            ID_PAUSE_60,
            ID_PROFILE,
            ID_METRICS,
            ID_QUIT,
        ];
        ids.extend(LEVELS.iter().map(|(id, _, _)| *id));
        ids.extend(MODELS.iter().map(|(id, _, _)| *id));
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "every menu item needs its own id");
        assert!(ids.iter().all(|id| id.starts_with("cc-tray.")));
    }

    #[test]
    fn level_parsing() {
        assert_eq!(level_index("off"), Some(0));
        assert_eq!(level_index("OFF"), Some(0));
        assert_eq!(level_index("S0"), Some(1));
        assert_eq!(level_index("s2"), Some(3));
        assert_eq!(level_index("S3"), None);
    }

    #[test]
    fn model_parsing() {
        assert_eq!(model_index("gemma-3-1b-pt"), Some(0));
        assert_eq!(model_index("SmolLM2-360M"), Some(1));
        assert_eq!(model_index(" qwen2.5-1.5b "), Some(2));
        assert_eq!(model_index("qwen2.5-7b"), None);
    }

    #[test]
    fn text_helpers() {
        assert_eq!(status_text("a & b"), "a && b");
        assert_eq!(status_text("  "), "CaseComplete");
        assert_eq!(tooltip_for(""), "CaseComplete");
        assert_eq!(tooltip_for("Ready"), "CaseComplete\nReady");
        let long = tooltip_for(&"x".repeat(500));
        assert_eq!(long.chars().count(), TOOLTIP_MAX_CHARS);
        assert!(long.ends_with("..."));
    }
}
