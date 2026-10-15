//! Desktop integration: show or play a saved file, pick the output folder,
//! and notify when a save ends while the window is in the background.
//!
//! Every call here may wait on D-Bus or a portal dialog: each runs on a
//! short-lived thread of its own, never on the UI thread.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError};

use ashpd::desktop::file_chooser::SelectedFiles;
use ashpd::desktop::notification::{Button, Notification, NotificationProxy, Priority};
use futures_lite::StreamExt;
use futures_lite::future::block_on;

/// Runs `f` on a new named thread; logs when the thread cannot start.
fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
        log!("could not start {name}: {e}");
    }
}

/// Opens the folder of `file` in the file manager with `file` selected
/// (`org.freedesktop.FileManager1.ShowItems`), or just the folder.
pub fn show_in_folder(file: PathBuf) {
    spawn("vesper-open", move || {
        if let Err(e) = block_on(show_items(&file)) {
            log!("ShowItems failed ({e}): opening the folder instead");
            open(file.parent().unwrap_or(Path::new("/")));
        }
    });
}

async fn show_items(file: &Path) -> ashpd::zbus::Result<()> {
    let conn = ashpd::zbus::Connection::session().await?;
    conn.call_method(
        Some("org.freedesktop.FileManager1"),
        "/org/freedesktop/FileManager1",
        Some("org.freedesktop.FileManager1"),
        "ShowItems",
        &(vec![file_uri(file)], ""),
    )
    .await?;
    Ok(())
}

/// Opens `path` with its default app (`xdg-open`), without waiting for it.
pub fn open(path: &Path) {
    let child = Command::new("xdg-open")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn();
    match child {
        // Reaped on its own thread: xdg-open may wait for the app.
        Ok(mut child) => spawn("vesper-xdg-open", move || drop(child.wait())),
        Err(e) => log!("could not run xdg-open: {e}"),
    }
}

/// Asks for a folder through the FileChooser portal, starting in `current`.
/// `done` gets the folder, `Ok(None)` when cancelled.
pub fn pick_folder(
    current: PathBuf,
    done: impl FnOnce(Result<Option<PathBuf>, String>) + Send + 'static,
) {
    spawn("vesper-folder", move || {
        done(block_on(choose_folder(&current)).map_err(|e| e.to_string()));
    });
}

async fn choose_folder(current: &Path) -> ashpd::Result<Option<PathBuf>> {
    let mut request = SelectedFiles::open_file()
        .title("Choose the output folder")
        .accept_label("Choose")
        .directory(true)
        .modal(true)
        .multiple(false);
    if current.is_dir() {
        request = request.current_folder(current)?;
    }
    match request.send().await?.response() {
        Ok(files) => Ok(files.uris().first().and_then(|uri| uri_path(uri.as_str()))),
        Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The file the last notification is about, for its buttons.
static NOTIFIED: Mutex<Option<PathBuf>> = Mutex::new(None);
const NOTIFICATION_ID: &str = "saved";

/// Shows a desktop notification (Notification portal). With `file`, it gets
/// Open and Play buttons; a click on the body opens the folder too.
pub fn notify(title: String, body: String, file: Option<PathBuf>) {
    let actions = file.is_some();
    *NOTIFIED.lock().unwrap_or_else(PoisonError::into_inner) = file;
    if actions {
        listen_for_actions();
    }
    spawn("vesper-notify", move || {
        let mut notification = Notification::new(&title)
            .body(body.as_str())
            .priority(Priority::Normal);
        if actions {
            notification = notification
                .default_action("open")
                .button(Button::new("Open folder", "open"))
                .button(Button::new("Play", "play"));
        }
        let sent = block_on(async {
            NotificationProxy::new()
                .await?
                .add_notification(NOTIFICATION_ID, notification)
                .await
        });
        if let Err(e) = sent {
            log!("could not show a notification: {e}");
        }
    });
}

/// Starts, once, the thread that runs the notification buttons.
fn listen_for_actions() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    spawn("vesper-notify-actions", || {
        let listened = block_on(async {
            let proxy = NotificationProxy::new().await?;
            let mut actions = proxy.receive_action_invoked().await?;
            while let Some(action) = actions.next().await {
                if action.id() != NOTIFICATION_ID {
                    continue;
                }
                let file = NOTIFIED
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                match (action.name(), file) {
                    ("open", Some(file)) => show_in_folder(file),
                    ("play", Some(file)) => open(&file),
                    _ => {}
                }
            }
            ashpd::Result::Ok(())
        });
        if let Err(e) = listened {
            log!("notification buttons unavailable: {e}");
        }
    });
}

/// Characters kept as is in a `file://` URI path; the rest is %-encoded.
fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-._~/".contains(&b)
}

/// `file://` URI of an absolute path, bytes %-encoded as needed.
pub fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if unreserved(b) {
            uri.push(char::from(b));
        } else {
            uri += &format!("%{b:02X}");
        }
    }
    uri
}

/// The path of a local `file://` URI; `None` for other URIs.
pub fn uri_path(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let rest = uri.strip_prefix("file://")?;
    // An authority, if any, must be the local host.
    let path = match rest.find('/')? {
        0 => rest,
        i if &rest[..i] == "localhost" => &rest[i..],
        _ => return None,
    };
    let mut bytes = Vec::with_capacity(path.len());
    let mut iter = path.bytes();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let hex = [iter.next()?, iter.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_round_trip() {
        for path in [
            "/home/u/Vidéos/Vesper",
            "/a b/c#d?e%f",
            "/mnt/clips/Vesper 2026-10-15 12-00-00 (2).mkv",
        ] {
            let uri = file_uri(Path::new(path));
            assert!(uri.is_ascii() && !uri.contains(' '), "{uri}");
            assert_eq!(uri_path(&uri).as_deref(), Some(Path::new(path)));
        }
        assert_eq!(file_uri(Path::new("/a b")), "file:///a%20b");
    }

    #[test]
    fn only_local_file_uris_are_paths() {
        assert_eq!(
            uri_path("file://localhost/tmp/x").as_deref(),
            Some(Path::new("/tmp/x"))
        );
        assert_eq!(uri_path("file://host/tmp/x"), None);
        assert_eq!(uri_path("https://example.com/x"), None);
        assert_eq!(uri_path("file:///bad%zz"), None);
        assert_eq!(uri_path("file:///cut%2"), None);
    }
}
