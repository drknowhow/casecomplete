//! Native windows for CaseComplete's local web UI, on WebView2 (via `wry`).
//!
//! [`Windows::new`] spawns a dedicated UI thread that owns a `tao` event loop
//! (built with `with_any_thread(true)`, since it is not the main thread). All
//! windows and WebViews live on that thread; the public methods only post
//! [`UserEvent`]s to it through an `EventLoopProxy`, so they never block and
//! can be called from any thread.
//!
//! Two windows exist, both created lazily on first use and then kept alive:
//!
//! * the **dashboard**: a normal resizable window. Closing it only hides it,
//!   so opening it again is instant;
//! * the **flyout**: a small borderless, always-on-top window in the
//!   bottom-right corner of the primary monitor's work area. It hides itself
//!   when it loses focus or on Esc.
//!
//! Notes on the flyout's focus handling. tao reports `Focused(false)` when
//! keyboard focus moves from the top-level window to the WebView2 child
//! window (which happens right after the flyout is shown), and reports nothing
//! when the user later activates another application. So `Focused(false)` is
//! only a hint to look again: the flyout hides when the *foreground window*
//! is no longer the flyout, checked on that hint and on a short timer while
//! the flyout is visible. Esc inside the page reaches Rust through
//! `window.ipc.postMessage`, because WebView2 swallows key events.

mod geometry;
mod icon;
mod native;

use std::path::PathBuf;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError};
use tao::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use tao::event::{ElementState, Event, KeyEvent, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::keyboard::Key;
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::windows::{
    EventLoopBuilderExtWindows, WindowBuilderExtWindows, WindowExtWindows,
};
use tao::window::{Icon, Window, WindowBuilder, WindowId};
use wry::{WebContext, WebView, WebViewBuilder};

use crate::geometry::{Area, DASHBOARD_LOGICAL, FLYOUT_LOGICAL};

const WINDOW_TITLE: &str = "CaseComplete";
const INIT_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
/// How often the flyout checks that it still owns the foreground.
const FLYOUT_POLL: Duration = Duration::from_millis(50);
/// After showing, the flyout gets this long to become the foreground window.
const FLYOUT_GRACE: Duration = Duration::from_millis(500);
/// A `toggle_flyout` this soon after the flyout hid itself on focus loss is
/// the click that caused the focus loss (a tray click while the flyout was
/// open): it must close the flyout, not re-open it.
const FLYOUT_REOPEN_GUARD: Duration = Duration::from_millis(400);
/// Message the flyout page posts when Esc is pressed.
const FLYOUT_ESC_MESSAGE: &str = "cc-window:escape";
const FLYOUT_ESC_SCRIPT: &str = r#"(function () {
  window.addEventListener('keydown', function (e) {
    if (e.key === 'Escape' && window.ipc) { window.ipc.postMessage('cc-window:escape'); }
  }, true);
})();"#;

/// Commands for the UI thread.
#[derive(Debug)]
enum UserEvent {
    OpenDashboard(String),
    ToggleFlyout(String),
    CloseFlyout,
    /// Esc pressed inside the flyout page.
    FlyoutEscape,
    Shutdown,
}

/// Handle to the UI thread. Dropping it destroys all windows and ends the thread.
pub struct Windows {
    proxy: EventLoopProxy<UserEvent>,
    /// Disconnects when the UI thread has finished.
    done_rx: Receiver<()>,
    handle: Option<JoinHandle<()>>,
}

impl Windows {
    /// Spawns the UI thread (its own event loop) and returns once it is running.
    ///
    /// Fails if the WebView2 runtime is not installed or the thread cannot start.
    pub fn new() -> Result<Windows> {
        wry::webview_version()
            .map_err(|e| anyhow!("the WebView2 runtime is not available: {e}"))?;

        let (init_tx, init_rx) = unbounded::<EventLoopProxy<UserEvent>>();
        let (done_tx, done_rx) = unbounded::<()>();

        let handle = thread::Builder::new()
            .name("cc-window".into())
            .spawn(move || {
                // Dropped when the thread ends (also on panic): wakes `Drop for Windows`.
                let _done = done_tx;
                let mut builder = EventLoopBuilder::<UserEvent>::with_user_event();
                builder.with_any_thread(true);
                let mut event_loop = builder.build();
                let proxy = event_loop.create_proxy();
                let mut ui = Ui::new(proxy.clone());
                if init_tx.send(proxy).is_err() {
                    return; // `new` gave up waiting
                }
                event_loop.run_return(move |event, target, flow| ui.handle(event, target, flow));
                log::debug!("cc-window: event loop ended");
                // Windows and WebViews were dropped with the closure, on this thread.
            })
            .context("failed to spawn the cc-window thread")?;

        match init_rx.recv_timeout(INIT_TIMEOUT) {
            Ok(proxy) => Ok(Windows {
                proxy,
                done_rx,
                handle: Some(handle),
            }),
            Err(RecvTimeoutError::Disconnected) => {
                let _ = handle.join();
                Err(anyhow!("the window thread died while starting"))
            }
            Err(RecvTimeoutError::Timeout) => {
                Err(anyhow!("timed out waiting for the window thread to start"))
            }
        }
    }

    /// Opens (or focuses) the dashboard window at `url`: a normal resizable
    /// window, 1280x820 by default, titled "CaseComplete", with the app icon.
    ///
    /// If the window already exists it is shown, restored if minimised and
    /// focused; the page is only reloaded when `url` differs from the one it
    /// currently shows.
    pub fn open_dashboard(&self, url: &str) {
        self.send(UserEvent::OpenDashboard(url.to_owned()));
    }

    /// Shows the flyout at `url` if hidden, hides it if visible: 380x620, no
    /// decorations, always on top, in the bottom-right corner of the primary
    /// monitor's work area (above the taskbar, 12 px margin). It hides itself
    /// when it loses focus or on Esc.
    ///
    /// A call right after the flyout hid itself because of focus loss does
    /// nothing: that call is the click that took the focus away.
    pub fn toggle_flyout(&self, url: &str) {
        self.send(UserEvent::ToggleFlyout(url.to_owned()));
    }

    /// Hides the flyout if it is visible.
    pub fn close_flyout(&self) {
        self.send(UserEvent::CloseFlyout);
    }

    fn send(&self, event: UserEvent) {
        if self.proxy.send_event(event).is_err() {
            log::warn!("cc-window: the window thread has ended, ignoring the command");
        }
    }
}

impl Drop for Windows {
    fn drop(&mut self) {
        self.send(UserEvent::Shutdown);
        let Some(handle) = self.handle.take() else {
            return;
        };
        match self.done_rx.recv_timeout(SHUTDOWN_TIMEOUT) {
            Err(RecvTimeoutError::Timeout) => {
                // Do not block the caller on a stuck UI thread.
                log::warn!("cc-window: window thread still busy, leaving it to finish on its own");
            }
            _ => {
                let _ = handle.join();
            }
        }
    }
}

/// A window with the WebView inside it. Field order matters: the WebView is
/// dropped before the window it is attached to.
struct Panel {
    webview: WebView,
    window: Window,
    /// The url last handed to the WebView.
    url: String,
}

impl Panel {
    /// Points the WebView at `url` unless it already shows it.
    fn navigate(&mut self, url: &str) {
        if self.url == url {
            return;
        }
        match self.webview.load_url(url) {
            Ok(()) => self.url = url.to_owned(),
            Err(e) => log::warn!("cc-window: failed to load {url}: {e}"),
        }
    }
}

/// Everything that lives on the UI thread (none of it is `Send`).
struct Ui {
    proxy: EventLoopProxy<UserEvent>,
    icon: Option<Icon>,
    dashboard: Option<Panel>,
    flyout: Option<Panel>,
    /// `Some(when it was shown)` while the flyout is visible.
    flyout_shown: Option<Instant>,
    /// When the flyout last hid itself because it lost the foreground.
    flyout_lost_focus_at: Option<Instant>,
    next_poll: Instant,
    /// Shared by both WebViews; dropped after them (declared last).
    web_context: WebContext,
}

impl Ui {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Ui {
        Ui {
            proxy,
            icon: icon::window_icon(),
            dashboard: None,
            flyout: None,
            flyout_shown: None,
            flyout_lost_focus_at: None,
            next_poll: Instant::now(),
            web_context: WebContext::new(webview_data_dir()),
        }
    }

    fn handle(
        &mut self,
        event: Event<'_, UserEvent>,
        target: &EventLoopWindowTarget<UserEvent>,
        flow: &mut ControlFlow,
    ) {
        match event {
            Event::UserEvent(UserEvent::OpenDashboard(url)) => self.open_dashboard(target, &url),
            Event::UserEvent(UserEvent::ToggleFlyout(url)) => self.toggle_flyout(target, &url),
            Event::UserEvent(UserEvent::CloseFlyout | UserEvent::FlyoutEscape) => {
                self.hide_flyout();
            }
            Event::UserEvent(UserEvent::Shutdown) => {
                // Destroy WebViews and windows now, on this thread.
                self.flyout = None;
                self.dashboard = None;
                *flow = ControlFlow::Exit;
                return;
            }
            Event::WindowEvent {
                window_id, event, ..
            } => self.on_window_event(window_id, event),
            _ => {}
        }

        // While the flyout is open, wake up regularly to check the foreground
        // window. A deadline (rather than "now + 50 ms" each time) keeps a
        // stream of mouse events from starving the check.
        if self.flyout_shown.is_some() {
            let now = Instant::now();
            if now >= self.next_poll {
                self.check_flyout_focus();
                self.next_poll = now + FLYOUT_POLL;
            }
            *flow = if self.flyout_shown.is_some() {
                ControlFlow::WaitUntil(self.next_poll)
            } else {
                ControlFlow::Wait
            };
        } else {
            *flow = ControlFlow::Wait;
        }
    }

    fn on_window_event(&mut self, id: WindowId, event: WindowEvent<'_>) {
        let is_dashboard = self.dashboard.as_ref().is_some_and(|p| p.window.id() == id);
        let is_flyout = self.flyout.as_ref().is_some_and(|p| p.window.id() == id);
        match event {
            // Closing never destroys: the WebView stays alive for an instant reopen.
            WindowEvent::CloseRequested if is_dashboard => {
                if let Some(p) = &self.dashboard {
                    p.window.set_visible(false);
                }
            }
            WindowEvent::CloseRequested if is_flyout => self.hide_flyout(),
            WindowEvent::Focused(false) if is_flyout => self.check_flyout_focus(),
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Escape,
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } if is_flyout => self.hide_flyout(),
            _ => {}
        }
    }

    // ----- dashboard -------------------------------------------------------

    fn open_dashboard(&mut self, target: &EventLoopWindowTarget<UserEvent>, url: &str) {
        if self.dashboard.is_none() {
            match self.build_dashboard(target, url) {
                Ok(panel) => self.dashboard = Some(panel),
                Err(e) => {
                    log::error!("cc-window: cannot open the dashboard: {e:#}");
                    return;
                }
            }
        }
        let Some(panel) = self.dashboard.as_mut() else {
            return;
        };
        panel.navigate(url);
        // Only restore a minimised window: SW_RESTORE would also un-maximise one.
        if panel.window.is_minimized() {
            panel.window.set_minimized(false);
        }
        panel.window.set_visible(true);
        panel.window.set_focus();
    }

    fn build_dashboard(
        &mut self,
        target: &EventLoopWindowTarget<UserEvent>,
        url: &str,
    ) -> Result<Panel> {
        let work = native::work_area();
        let scale = primary_scale(target);
        let (width, height) = work.map_or(DASHBOARD_LOGICAL, |a| {
            geometry::dashboard_logical_size(a, scale)
        });
        let window = WindowBuilder::new()
            .with_title(WINDOW_TITLE)
            .with_inner_size(LogicalSize::new(width, height))
            .with_resizable(true)
            .with_visible(false)
            .with_window_icon(self.icon.clone())
            .with_taskbar_icon(self.icon.clone())
            .build(target)
            .context("failed to create the dashboard window")?;
        if let Some(work) = work {
            let outer = window.outer_size();
            let (x, y) = geometry::centered(work, outer.width as i32, outer.height as i32);
            window.set_outer_position(PhysicalPosition::new(x, y));
        }
        let webview = WebViewBuilder::new_with_web_context(&mut self.web_context)
            .with_url(url)
            .build(&window)
            .context("failed to create the dashboard WebView2")?;
        Ok(Panel {
            webview,
            window,
            url: url.to_owned(),
        })
    }

    // ----- flyout ----------------------------------------------------------

    fn toggle_flyout(&mut self, target: &EventLoopWindowTarget<UserEvent>, url: &str) {
        if self.flyout_shown.is_some() {
            self.hide_flyout();
            return;
        }
        if let Some(at) = self.flyout_lost_focus_at.take() {
            if at.elapsed() < FLYOUT_REOPEN_GUARD {
                return;
            }
        }
        self.show_flyout(target, url);
    }

    fn show_flyout(&mut self, target: &EventLoopWindowTarget<UserEvent>, url: &str) {
        if self.flyout.is_none() {
            match self.build_flyout(target, url) {
                Ok(panel) => self.flyout = Some(panel),
                Err(e) => {
                    log::error!("cc-window: cannot open the flyout: {e:#}");
                    return;
                }
            }
        }
        let Some(panel) = self.flyout.as_mut() else {
            return;
        };
        panel.navigate(url);
        let target = flyout_target(&panel.window);
        if let Some(target) = target {
            // Rough placement first, so the window does not appear elsewhere...
            panel
                .window
                .set_inner_size(PhysicalSize::new(target.width() as u32, target.height() as u32));
            panel
                .window
                .set_outer_position(PhysicalPosition::new(target.left, target.top));
        }
        panel.window.set_visible(true);
        if let Some(target) = target {
            // ...then the exact one, once the window can be measured.
            native::fit_visible_rect(panel.window.hwnd(), target);
        }
        panel.window.set_focus();
        let now = Instant::now();
        self.flyout_shown = Some(now);
        self.flyout_lost_focus_at = None;
        self.next_poll = now + FLYOUT_POLL;
    }

    fn build_flyout(
        &mut self,
        target: &EventLoopWindowTarget<UserEvent>,
        url: &str,
    ) -> Result<Panel> {
        let window = WindowBuilder::new()
            .with_title(WINDOW_TITLE)
            .with_inner_size(LogicalSize::new(FLYOUT_LOGICAL.0, FLYOUT_LOGICAL.1))
            .with_decorations(false)
            .with_always_on_top(true)
            .with_resizable(false)
            .with_visible(false)
            .with_skip_taskbar(true)
            .with_window_icon(self.icon.clone())
            .build(target)
            .context("failed to create the flyout window")?;
        let proxy = self.proxy.clone();
        let webview = WebViewBuilder::new_with_web_context(&mut self.web_context)
            .with_url(url)
            .with_initialization_script_for_main_only(FLYOUT_ESC_SCRIPT, true)
            .with_ipc_handler(move |request| {
                if request.body() == FLYOUT_ESC_MESSAGE {
                    let _ = proxy.send_event(UserEvent::FlyoutEscape);
                }
            })
            .build(&window)
            .context("failed to create the flyout WebView2")?;
        Ok(Panel {
            webview,
            window,
            url: url.to_owned(),
        })
    }

    fn hide_flyout(&mut self) {
        if let Some(panel) = &self.flyout {
            panel.window.set_visible(false);
        }
        self.flyout_shown = None;
        self.flyout_lost_focus_at = None;
    }

    /// Hides the flyout if it is past its grace period and no longer the
    /// foreground window (or owner of it).
    fn check_flyout_focus(&mut self) {
        let Some(shown) = self.flyout_shown else {
            return;
        };
        if shown.elapsed() < FLYOUT_GRACE {
            return;
        }
        let Some(panel) = &self.flyout else {
            return;
        };
        if native::is_foreground(panel.window.hwnd()) {
            return;
        }
        log::debug!("cc-window: flyout lost the foreground, hiding it");
        self.hide_flyout();
        self.flyout_lost_focus_at = Some(Instant::now());
    }
}

/// The rectangle the flyout should occupy: its bottom-right corner in the
/// primary monitor's work area, in physical pixels.
fn flyout_target(window: &Window) -> Option<Area> {
    let scale = window
        .primary_monitor()
        .map_or_else(|| window.scale_factor(), |m| m.scale_factor());
    let work = native::work_area().or_else(|| {
        // No work area: fall back to the whole primary monitor.
        window.primary_monitor().map(|m| {
            let (pos, size) = (m.position(), m.size());
            Area {
                left: pos.x,
                top: pos.y,
                right: pos.x + size.width as i32,
                bottom: pos.y + size.height as i32,
            }
        })
    });
    if work.is_none() {
        log::warn!("cc-window: no monitor information, leaving the flyout where it is");
    }
    work.map(|work| geometry::flyout_rect(work, scale))
}

fn primary_scale(target: &EventLoopWindowTarget<UserEvent>) -> f64 {
    target.primary_monitor().map_or(1.0, |m| m.scale_factor())
}

/// Where WebView2 keeps cookies, cache and local storage:
/// `%LOCALAPPDATA%\CaseComplete\WebView2`. Without this it would use a folder
/// next to the executable, which is not writable under `Program Files`.
fn webview_data_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("CaseComplete").join("WebView2"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_handle_can_be_shared_between_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Windows>();
    }

    #[test]
    fn data_dir_is_under_local_app_data() {
        if let Some(dir) = webview_data_dir() {
            assert!(dir.ends_with(r"CaseComplete\WebView2"));
        }
    }

    /// Needs an interactive desktop session and the WebView2 runtime:
    /// `cargo test -p cc-window -- --ignored`. Uses `data:` URLs, so no network.
    #[test]
    #[ignore]
    fn windows_lifecycle_on_desktop() {
        let page = "data:text/html,<body style='font:20px sans-serif'>cc-window test</body>";
        let windows = Windows::new().expect("windows");
        windows.open_dashboard(page);
        windows.toggle_flyout(page);
        std::thread::sleep(Duration::from_millis(1500));
        windows.close_flyout();
        windows.toggle_flyout(page);
        std::thread::sleep(Duration::from_millis(1000));
        let started = Instant::now();
        drop(windows);
        assert!(started.elapsed() < Duration::from_secs(2), "drop joins the thread promptly");
    }
}
