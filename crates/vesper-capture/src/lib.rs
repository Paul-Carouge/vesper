//! Screen capture through xdg-desktop-portal and PipeWire.
//!
//! [`Capture::start`] asks the ScreenCast portal for a monitor or a window,
//! then streams it on a dedicated PipeWire thread as DMA-BUF
//! [`VideoFrame`]s. Each frame holds its PipeWire buffer until dropped.

mod format;
mod pw;

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};
use std::thread::JoinHandle;

pub use ashpd::desktop::screencast::SourceType;
use std::collections::HashMap;

use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, StartCastOptions};
use ashpd::desktop::{PersistMode, Session};
use ashpd::enumflags2::BitFlags;
use ashpd::zbus::export::serde;
use ashpd::zvariant;
use futures_lite::future::block_on;
use tracing::warn;
use vesper_core::VideoFrame;

/// How long dropping a [`Capture`], or cancelling its picker, waits for the
/// portal to close its session.
const CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

/// What the user picked, as the portal describes its stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceInfo {
    /// Monitor, window, or virtual (e.g. KDE's full workspace or a region);
    /// `None` when the portal does not say.
    pub source_type: Option<SourceType>,
    /// Top-left corner in the compositor's logical space, if given.
    pub position: Option<(i32, i32)>,
    /// Logical size, if given.
    pub size: Option<(i32, i32)>,
}

/// A running screen capture. Dropping it stops the capture.
pub struct Capture {
    restore_token: Option<String>,
    source: SourceInfo,
    /// Taken by `drop` to close it.
    session: Option<Session<Screencast>>,
    control: ::pipewire::channel::Sender<pw::Msg>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Opens the portal picker (skipped when `restore_token` is still valid)
    /// and starts streaming, at most `max_fps` frames per second (the
    /// compositor may send fewer: frames come on damage). `on_frame` runs on
    /// the PipeWire thread: keep it short, e.g. send the frame to a channel. It gets one final `Err` when
    /// the capture ends on its own (source closed, stream error, no DMA-BUF);
    /// no frame follows. Dropping the `Capture` from `on_frame` is allowed,
    /// but blocks the PipeWire loop while the D-Bus session closes (at most
    /// [`CLOSE_TIMEOUT`]).
    /// Blocks until the user picks a source.
    pub fn start(
        restore_token: Option<&str>,
        max_fps: u32,
        on_frame: impl FnMut(Result<VideoFrame>) + Send + 'static,
    ) -> Result<Self> {
        let sources = SourceType::Monitor | SourceType::Window;
        Self::start_with(
            sources,
            restore_token,
            &Cancel::default(),
            max_fps,
            on_frame,
        )
    }

    /// [`Capture::start`], offering windows only (a game's window). `cancel`
    /// ends the wait for the portal, closing its picker.
    pub fn start_window(
        restore_token: Option<&str>,
        cancel: &Cancel,
        max_fps: u32,
        on_frame: impl FnMut(Result<VideoFrame>) + Send + 'static,
    ) -> Result<Self> {
        let sources = SourceType::Window.into();
        Self::start_with(sources, restore_token, cancel, max_fps, on_frame)
    }

    fn start_with(
        sources: BitFlags<SourceType>,
        restore_token: Option<&str>,
        cancel: &Cancel,
        max_fps: u32,
        on_frame: impl FnMut(Result<VideoFrame>) + Send + 'static,
    ) -> Result<Self> {
        let (session, node_id, fd, restore_token, source) =
            block_on(open_portal(sources, restore_token, cancel))?;
        let (control, rx) = ::pipewire::channel::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let tx = control.clone();
        let thread = std::thread::Builder::new()
            .name("vesper-pipewire".into())
            .spawn(move || pw::run(fd, node_id, max_fps, on_frame, tx, rx, ready_tx))?;
        let capture = Self {
            restore_token,
            source,
            session: Some(session),
            control,
            thread: Some(thread),
        };
        // On error, dropping `capture` joins the thread and closes the session.
        ready_rx.recv().map_err(|_| "pipewire thread died")??;
        Ok(capture)
    }

    /// Token that skips the picker next time (persist mode: until revoked).
    pub fn restore_token(&self) -> Option<&str> {
        self.restore_token.as_deref()
    }

    /// The picked monitor or window.
    pub fn source(&self) -> &SourceInfo {
        &self.source
    }

    /// Stops the capture. Same as dropping it.
    pub fn stop(self) {}
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.control.send(pw::Msg::Stop);
        // Dropped from `on_frame`: joining our own thread would deadlock, and
        // the loop exits on `Stop` once the callback returns.
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != std::thread::current().id()
            && thread.join().is_err()
        {
            warn!("pipewire thread panicked");
        }
        // A hung portal must not hold up the caller (finishing a file,
        // quitting): the session closes on its own thread, waited on a while.
        let Some(session) = self.session.take() else {
            return;
        };
        let (done_tx, done) = std::sync::mpsc::channel();
        let closing = std::thread::Builder::new()
            .name("vesper-portal-close".into())
            .spawn(move || {
                if let Err(e) = block_on(session.close()) {
                    warn!("closing the screencast session failed: {e}");
                }
                let _ = done_tx.send(());
            });
        if closing.is_ok() && done.recv_timeout(CLOSE_TIMEOUT).is_err() {
            warn!("the portal did not close the screencast session in time");
        }
    }
}

type Opened = (
    Session<Screencast>,
    u32,
    OwnedFd,
    Option<String>,
    SourceInfo,
);

/// Whether `error` is the user closing the picker without picking.
pub fn is_cancelled(error: &Error) -> bool {
    matches!(
        error.downcast_ref::<ashpd::Error>(),
        Some(ashpd::Error::Response(
            ashpd::desktop::ResponseError::Cancelled
        ))
    )
}

/// Whether the portal answered `error` to the request itself (a restore it
/// could not do, a refusal), not the user cancelling nor D-Bus failing.
pub fn is_refused(error: &Error) -> bool {
    matches!(
        error.downcast_ref::<ashpd::Error>(),
        Some(ashpd::Error::Response(_))
    ) && !is_cancelled(error)
}

const CANCELLED: &str = "the capture was cancelled";

/// Ends a [`Capture::start_window`] still waiting on the portal.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<CancelState>);

#[derive(Debug, Default)]
struct CancelState {
    done: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl Cancel {
    pub fn cancel(&self) {
        self.0.done.store(true, Ordering::SeqCst);
        let waker = self
            .0
            .waker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Ready once [`Cancel::cancel`] was called.
    fn wait(&self) -> impl Future<Output = ()> + '_ {
        std::future::poll_fn(move |cx| {
            // The waker first: a cancel in between still wakes it.
            *self.0.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.waker().clone());
            if self.0.done.load(Ordering::SeqCst) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
    }
}

/// The `handle_token` a request's options carry (ashpd draws it at random
/// and keeps it private).
fn handle_token(options: &(impl serde::Serialize + zvariant::Type)) -> Option<String> {
    use zvariant::serialized::Context;
    let data = zvariant::to_bytes(Context::new_dbus(zvariant::LE, 0), options).ok()?;
    let (map, _): (HashMap<String, zvariant::OwnedValue>, _) = data.deserialize().ok()?;
    String::try_from(map.get("handle_token")?.try_clone().ok()?).ok()
}

/// `/org/freedesktop/portal/desktop/request/SENDER/TOKEN`, SENDER being the
/// caller's unique bus name without its `:` and with `_` for `.`.
fn request_path(sender: &str, token: &str) -> Option<String> {
    let sender = sender.strip_prefix(':')?.replace('.', "_");
    Some(format!(
        "/org/freedesktop/portal/desktop/request/{sender}/{token}"
    ))
}

async fn open_portal(
    sources: BitFlags<SourceType>,
    restore_token: Option<&str>,
    cancel: &Cancel,
) -> Result<Opened> {
    let proxy = Screencast::new().await?;
    let cursor = if proxy
        .available_cursor_modes()
        .await?
        .contains(CursorMode::Embedded)
    {
        CursorMode::Embedded
    } else {
        warn!("portal cannot embed the cursor: recording without it");
        CursorMode::Hidden
    };
    // Cancelled while the portal was reached: no session, no picker.
    if cancel.0.done.load(Ordering::SeqCst) {
        return Err(CANCELLED.into());
    }
    let session = proxy.create_session(Default::default()).await?;
    let select = SelectSourcesOptions::default()
        .set_cursor_mode(cursor)
        .set_sources(sources)
        .set_multiple(false)
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(restore_token);
    let start = StartCastOptions::default();
    // The requests' paths, to close the one a cancel leaves open.
    let requests: Vec<String> = [handle_token(&select), handle_token(&start)]
        .into_iter()
        .flatten()
        .filter_map(|token| request_path(proxy.connection().unique_name()?.as_str(), &token))
        .collect();
    let result = async {
        proxy.select_sources(&session, select).await?.response()?;
        let streams = proxy.start(&session, None, start).await?.response()?;
        let stream = streams
            .streams()
            .first()
            .ok_or("portal returned no stream")?;
        let source = SourceInfo {
            source_type: stream.source_type(),
            position: stream.position(),
            size: stream.size(),
        };
        let node_id = stream.pipe_wire_node_id();
        let fd = proxy
            .open_pipe_wire_remote(&session, Default::default())
            .await?;
        let token = streams.restore_token().map(str::to_owned);
        Ok::<_, Error>((node_id, fd, token, source))
    };
    let cancelled = async {
        cancel.wait().await;
        Err::<(u32, OwnedFd, Option<String>, SourceInfo), Error>(CANCELLED.into())
    };
    // Cancelled: closing the session below ends the picker too.
    match futures_lite::future::or(result, cancelled).await {
        Ok((node_id, fd, token, source)) => Ok((session, node_id, fd, token, source)),
        Err(e) => {
            let close = async {
                if cancel.0.done.load(Ordering::SeqCst) {
                    // Ends the picker of the request in flight (the session's
                    // close below should too); a finished request just errors.
                    for path in &requests {
                        let _ = proxy
                            .connection()
                            .call_method(
                                Some("org.freedesktop.portal.Desktop"),
                                path.as_str(),
                                Some("org.freedesktop.portal.Request"),
                                "Close",
                                &(),
                            )
                            .await;
                    }
                }
                let _ = session.close().await;
            };
            // A frozen portal must not hang the capture thread.
            let gave_up = async {
                async_io::Timer::after(CLOSE_TIMEOUT).await;
                warn!("portal did not close the request in time");
            };
            futures_lite::future::or(close, gave_up).await;
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_paths_come_from_the_options_token() {
        let token = handle_token(&SelectSourcesOptions::default()).expect("a handle token");
        assert!(!token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        let other = handle_token(&StartCastOptions::default()).unwrap();
        assert_ne!(token, other, "each request draws its own");
        assert_eq!(
            request_path(":1.42", "ashpd_x").as_deref(),
            Some("/org/freedesktop/portal/desktop/request/1_42/ashpd_x")
        );
        assert_eq!(request_path("org.x", "t"), None);
    }

    #[test]
    fn cancel_wakes_a_waiting_portal_request() {
        let cancel = Cancel::default();
        let c = cancel.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            c.cancel();
        });
        // Stands for the portal: never answers.
        let never = futures_lite::future::pending::<bool>();
        let waited = futures_lite::future::or(never, async {
            cancel.wait().await;
            true
        });
        assert!(block_on(waited));
        t.join().unwrap();
        // Already cancelled: ready at once.
        block_on(cancel.wait());
    }
}
