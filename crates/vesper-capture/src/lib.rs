//! Screen capture through xdg-desktop-portal and PipeWire.
//!
//! [`Capture::start`] asks the ScreenCast portal for a monitor or a window,
//! then streams it on a dedicated PipeWire thread as DMA-BUF
//! [`VideoFrame`]s. Each frame holds its PipeWire buffer until dropped.

mod format;
mod pw;

use std::os::fd::OwnedFd;
use std::thread::JoinHandle;

use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};
use futures_lite::future::block_on;
use tracing::warn;
use vesper_core::VideoFrame;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

/// A running screen capture. Dropping it stops the capture.
pub struct Capture {
    restore_token: Option<String>,
    session: Session<Screencast>,
    control: ::pipewire::channel::Sender<pw::Msg>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Opens the portal picker (skipped when `restore_token` is still valid)
    /// and starts streaming. `on_frame` runs on the PipeWire thread: keep it
    /// short, e.g. send the frame to a channel. It gets one final `Err` when
    /// the capture ends on its own (source closed, stream error, no DMA-BUF);
    /// no frame follows. Dropping the `Capture` from `on_frame` is allowed,
    /// but blocks the PipeWire loop while the D-Bus session closes.
    /// Blocks until the user picks a source.
    pub fn start(
        restore_token: Option<&str>,
        on_frame: impl FnMut(Result<VideoFrame>) + Send + 'static,
    ) -> Result<Self> {
        let (session, node_id, fd, restore_token) = block_on(open_portal(restore_token))?;
        let (control, rx) = ::pipewire::channel::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let tx = control.clone();
        let thread = std::thread::Builder::new()
            .name("vesper-pipewire".into())
            .spawn(move || pw::run(fd, node_id, on_frame, tx, rx, ready_tx))?;
        let capture = Self {
            restore_token,
            session,
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
        if let Err(e) = block_on(self.session.close()) {
            warn!("closing the screencast session failed: {e}");
        }
    }
}

async fn open_portal(
    restore_token: Option<&str>,
) -> Result<(Session<Screencast>, u32, OwnedFd, Option<String>)> {
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
        let node_id = streams
            .streams()
            .first()
            .ok_or("portal returned no stream")?
            .pipe_wire_node_id();
        let fd = proxy
            .open_pipe_wire_remote(&session, Default::default())
            .await?;
        Ok::<_, Error>((node_id, fd, streams.restore_token().map(str::to_owned)))
    }
    .await;
    match result {
        Ok((node_id, fd, token)) => Ok((session, node_id, fd, token)),
        Err(e) => {
            let _ = session.close().await;
            Err(e)
        }
    }
}
