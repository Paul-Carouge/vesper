//! Screen capture through xdg-desktop-portal and PipeWire.
//!
//! [`Capture::start`] asks the ScreenCast portal for a monitor or a window,
//! then streams it on a dedicated PipeWire thread as DMA-BUF
//! [`VideoFrame`]s. Each frame holds its PipeWire buffer until dropped.

mod format;
mod pw;

use std::os::fd::OwnedFd;
use std::thread::JoinHandle;

pub use ashpd::desktop::screencast::SourceType;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions};
use ashpd::desktop::{PersistMode, Session};
use futures_lite::future::block_on;
use tracing::warn;
use vesper_core::VideoFrame;

/// How long dropping a [`Capture`] waits for the portal to close its session.
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
        let (session, node_id, fd, restore_token, source) = block_on(open_portal(restore_token))?;
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

async fn open_portal(restore_token: Option<&str>) -> Result<Opened> {
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
    let session = proxy.create_session(Default::default()).await?;
    let result = async {
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(cursor)
                    .set_sources(SourceType::Monitor | SourceType::Window)
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(restore_token),
            )
            .await?
            .response()?;
        let streams = proxy
            .start(&session, None, Default::default())
            .await?
            .response()?;
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
    }
    .await;
    match result {
        Ok((node_id, fd, token, source)) => Ok((session, node_id, fd, token, source)),
        Err(e) => {
            let _ = session.close().await;
            Err(e)
        }
    }
}
