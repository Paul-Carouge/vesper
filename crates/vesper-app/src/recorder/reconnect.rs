//! Getting a lost portal capture back while the pipeline keeps ticking
//! (KWin or the portal restarted, the monitor was turned off or unplugged).

use super::*;
use std::sync::mpsc::{Receiver, TryRecvError};
use vesper_capture::{Cancel, SourceInfo};

/// First wait before restarting a lost capture, doubled after each failure.
const RETRY_FIRST: Duration = Duration::from_secs(1);
/// Longest wait between two restarts.
const RETRY_MAX: Duration = Duration::from_secs(30);

/// A started capture: streams into the mailbox until dropped.
pub(super) struct Started {
    pub(super) info: SourceInfo,
    pub(super) _guard: Box<dyn Send>,
}

/// Starts the capture into the mailbox; `Cancel` closes a pending picker.
pub(super) type Starter =
    Arc<dyn Fn(&Cancel, Arc<Mailbox>) -> vesper_capture::Result<Started> + Send + Sync>;

/// The portal capture, started with the restore token (no picker when the
/// portal can restore the source) at most `max_fps`. A token the portal
/// refuses is dropped for the picker, once.
pub(super) fn portal(max_fps: u32) -> Starter {
    Arc::new(move |cancel, slot| {
        let start = |token: Option<&str>| {
            let slot = slot.clone();
            Capture::start(token, cancel, max_fps, move |event| {
                slot.park(event.map_err(|e| e.to_string()))
            })
        };
        let token = load_restore_token();
        let capture = match start(token.as_deref()) {
            Err(e) if token.is_some() && vesper_capture::is_refused(&e) => {
                log!("could not restore the source ({e}): asking again");
                start(None)
            }
            result => result,
        }?;
        // The portal hands out a new token on every start.
        if let Some(token) = capture.restore_token()
            && let Err(e) = save_restore_token(token)
        {
            log!("could not save the restore token: {e}");
        }
        Ok(Started {
            info: capture.source().clone(),
            _guard: Box::new(capture),
        })
    })
}

/// What [`Portal::tick`] saw.
#[derive(Debug)]
pub(super) enum Change {
    /// The capture streams again.
    Found(SourceInfo),
    /// The capture ended: a restart is on its way.
    Lost,
    /// The user closed the picker the portal showed again: stop.
    Refused,
    /// The restart itself failed (its thread panicked): stop with this.
    Failed(String),
}

/// The pipeline's portal capture, restarted in the background when lost.
pub(super) struct Portal {
    start: Starter,
    slot: Arc<Mailbox>,
    capture: Option<Started>,
    /// Closes the picker of a restart still waiting on the portal.
    cancel: Cancel,
    stop: Arc<(Mutex<bool>, Condvar)>,
    retry: Option<Retry>,
    /// Wait before the next restart, and when the capture last came back:
    /// a capture lost again soon after does not restart faster.
    delay: Duration,
    found_at: Option<Duration>,
}

struct Retry {
    rx: Receiver<(Result<Started, ()>, Duration)>,
    thread: JoinHandle<()>,
}

impl Portal {
    /// Starts the capture; blocks while the portal shows its picker.
    pub(super) fn start(start: Starter, slot: Arc<Mailbox>) -> vesper_capture::Result<Self> {
        let cancel = Cancel::default();
        let capture = start(&cancel, slot.clone())?;
        Ok(Self {
            start,
            slot,
            capture: Some(capture),
            cancel,
            stop: Arc::default(),
            retry: None,
            delay: RETRY_FIRST,
            found_at: None,
        })
    }

    pub(super) fn source(&self) -> Option<&SourceInfo> {
        self.capture.as_ref().map(|c| &c.info)
    }

    /// The capture ended on its own: restart it in the background, one
    /// request at a time, after 1, 2, 4… s (at most 30 s).
    fn lost(&mut self) -> std::io::Result<()> {
        if self.retry.is_some() {
            return Ok(());
        }
        let soon = self
            .found_at
            .is_some_and(|t| monotonic_now().saturating_sub(t) < RETRY_MAX);
        let first = if soon {
            (self.delay * 2).min(RETRY_MAX)
        } else {
            RETRY_FIRST
        };
        let old = self.capture.take();
        let (start, slot, cancel, stop) = (
            self.start.clone(),
            self.slot.clone(),
            self.cancel.clone(),
            self.stop.clone(),
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("vesper-reconnect".into())
            .spawn(move || {
                // Closing the old portal session may take a while: here,
                // not on the ticking thread.
                drop(old);
                let wait = |d: Duration| {
                    let (stopped, wake) = &*stop;
                    let s = lock(stopped);
                    let s = wake.wait_timeout_while(s, d, |s| !*s);
                    !*s.unwrap_or_else(PoisonError::into_inner).0
                };
                let attempt = || {
                    start(&cancel, slot.clone()).map_err(|e| {
                        log!("capture restart failed: {e}");
                        if vesper_capture::is_cancelled(&e) {
                            Failed::Refused
                        } else {
                            Failed::Error
                        }
                    })
                };
                if let Some(outcome) = retry(first, attempt, wait) {
                    let _ = tx.send(outcome);
                    // Wakes a pipeline still waiting for its first frame.
                    slot.poke();
                }
            });
        self.retry = Some(Retry {
            rx,
            thread: thread?,
        });
        Ok(())
    }

    /// Called every tick, `ended` when the capture's end was parked: the
    /// outcome of a restart first (the capture it brings may be the one that
    /// ended), then the end. Cheap while the capture streams.
    pub(super) fn tick(&mut self, ended: bool) -> std::io::Result<[Option<Change>; 2]> {
        let found = self.poll();
        let lost = if ended {
            self.lost()?;
            Some(Change::Lost)
        } else {
            None
        };
        Ok([found, lost])
    }

    /// The outcome of a restart, once there is one.
    fn poll(&mut self) -> Option<Change> {
        let outcome = match self.retry.as_ref()?.rx.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                self.retry = None;
                return Some(Change::Failed("the capture restart crashed".into()));
            }
        };
        if let Some(r) = self.retry.take() {
            let _ = r.thread.join();
        }
        self.delay = outcome.1;
        match outcome.0 {
            Ok(started) => {
                self.found_at = Some(monotonic_now());
                let info = started.info.clone();
                self.capture = Some(started);
                Some(Change::Found(info))
            }
            Err(()) => Some(Change::Refused),
        }
    }
}

impl Drop for Portal {
    fn drop(&mut self) {
        *lock(&self.stop.0) = true;
        self.stop.1.notify_all();
        // Not joined: a portal that never answers must not hang the quit.
        // The thread only holds the mailbox and drops what it gets.
        self.cancel.cancel();
    }
}

/// Why a restart failed.
enum Failed {
    /// Worth another try.
    Error,
    /// The user closed the picker: no more tries.
    Refused,
}

/// Calls `start` until it works or is refused, after waiting `first`, then
/// twice as long after each failure (at most [`RETRY_MAX`]). `wait` sleeps
/// and returns false once stopped. Gives what `start` returned last, with
/// the wait before it; `None` when stopped.
fn retry<T>(
    first: Duration,
    mut start: impl FnMut() -> Result<T, Failed>,
    mut wait: impl FnMut(Duration) -> bool,
) -> Option<(Result<T, ()>, Duration)> {
    let mut delay = first;
    loop {
        if !wait(delay) {
            return None;
        }
        match start() {
            Ok(t) => return Some((Ok(t), delay)),
            Err(Failed::Refused) => return Some((Err(()), delay)),
            Err(Failed::Error) => delay = (delay * 2).min(RETRY_MAX),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(d: &[Duration]) -> Vec<u64> {
        d.iter().map(Duration::as_secs).collect()
    }

    #[test]
    fn retries_back_off_up_to_30_s_then_succeed() {
        let mut waits = Vec::new();
        let mut tries = 0;
        let got = retry(
            RETRY_FIRST,
            || {
                tries += 1;
                if tries < 8 {
                    Err(Failed::Error)
                } else {
                    Ok(tries)
                }
            },
            |d| {
                waits.push(d);
                true
            },
        );
        assert_eq!(secs(&waits), [1, 2, 4, 8, 16, 30, 30, 30]);
        assert!(matches!(got, Some((Ok(8), d)) if d == RETRY_MAX));
    }

    #[test]
    fn a_refused_picker_stops_the_retries() {
        let mut tries = 0;
        let got = retry::<()>(
            RETRY_FIRST,
            || {
                tries += 1;
                if tries < 3 {
                    Err(Failed::Error)
                } else {
                    Err(Failed::Refused)
                }
            },
            |_| true,
        );
        assert_eq!(tries, 3);
        assert!(matches!(got, Some((Err(()), _))));
    }

    #[test]
    fn a_stop_ends_the_wait_without_another_try() {
        let mut tries = 0;
        let mut waits = 0;
        let got = retry::<()>(
            Duration::from_secs(4),
            || {
                tries += 1;
                Err(Failed::Error)
            },
            |_| {
                waits += 1;
                waits < 3
            },
        );
        assert!(got.is_none());
        assert_eq!(tries, 2);
    }

    /// A fake capture: counts starts, fails the first `fail` ones.
    fn starter(fail: usize, starts: Arc<AtomicUsize>) -> Starter {
        Arc::new(move |_, _| {
            let n = starts.fetch_add(1, Ordering::SeqCst);
            if n > 0 && n <= fail {
                return Err("no portal".into());
            }
            Ok(Started {
                info: SourceInfo::default(),
                _guard: Box::new(()),
            })
        })
    }

    #[test]
    fn a_lost_capture_comes_back_in_the_background() {
        let starts = Arc::new(AtomicUsize::new(0));
        let slot = Arc::new(Mailbox::default());
        let mut p = Portal::start(starter(1, starts.clone()), slot.clone()).unwrap();
        assert!(p.poll().is_none(), "nothing to poll while it streams");
        let t = std::time::Instant::now();
        p.lost().unwrap();
        assert!(p.source().is_none());
        // Lost twice (two errors parked): still one restart at a time.
        p.lost().unwrap();
        // First try after 1 s fails, the second after 2 more works.
        let change = loop {
            slot.wait(Some(Duration::from_secs(5)));
            if let Some(c) = p.poll() {
                break c;
            }
            assert!(t.elapsed() < Duration::from_secs(6), "never came back");
        };
        assert!(matches!(change, Change::Found(_)));
        assert!(t.elapsed() >= Duration::from_secs(3));
        assert_eq!(starts.load(Ordering::SeqCst), 3);
        assert!(p.source().is_some());
        // Lost again right away: the next restart waits twice as long as
        // the one that worked.
        p.lost().unwrap();
        assert_eq!(p.delay, Duration::from_secs(2));
        let t = std::time::Instant::now();
        slot.wait(Some(Duration::from_millis(3500)));
        assert!(p.poll().is_none() && t.elapsed() >= Duration::from_secs(3));
    }

    #[test]
    fn dropping_while_retrying_returns_at_once() {
        let starts = Arc::new(AtomicUsize::new(0));
        let mut p = Portal::start(starter(usize::MAX, starts), Arc::default()).unwrap();
        p.lost().unwrap();
        let t = std::time::Instant::now();
        drop(p);
        assert!(t.elapsed() < Duration::from_millis(500));
    }

    /// A restarted capture that ends before the pipeline saw it come back
    /// (its end parked with the restart's outcome) is restarted again,
    /// never kept as a dead source.
    #[test]
    fn a_capture_dying_as_it_comes_back_is_restarted_again() {
        let starts = Arc::new(AtomicUsize::new(0));
        let n = starts.clone();
        let start: Starter = Arc::new(move |_, slot| {
            if n.fetch_add(1, Ordering::SeqCst) == 1 {
                slot.park(Err("stream closed".into()));
            }
            Ok(Started {
                info: SourceInfo::default(),
                _guard: Box::new(()),
            })
        });
        let slot = Arc::new(Mailbox::default());
        let mut p = Portal::start(start, slot.clone()).unwrap();
        let first = p.tick(true).unwrap();
        assert!(matches!(first, [None, Some(Change::Lost)]), "{first:?}");
        let t = std::time::Instant::now();
        let changes = loop {
            slot.wait(Some(Duration::from_secs(3)));
            let (_, ended) = slot.take();
            let c = p.tick(ended.is_some()).unwrap();
            if c.iter().any(Option::is_some) {
                break c;
            }
            assert!(t.elapsed() < Duration::from_secs(3), "never came back");
        };
        assert!(
            matches!(changes, [Some(Change::Found(_)), Some(Change::Lost)]),
            "{changes:?}"
        );
        assert!(p.retry.is_some(), "restarting again");
        assert!(p.source().is_none());
    }
}
