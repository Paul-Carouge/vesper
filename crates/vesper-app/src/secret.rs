//! The stream key, kept out of the settings file.
//!
//! First choice: the Secret Service (`org.freedesktop.secrets` on the session
//! bus: KWallet's ksecretd on Plasma, GNOME Keyring, KeePassXC), which keeps
//! it encrypted at rest and unlocked with the session. The Secret portal is
//! for sandboxed apps only and would still leave the encryption to us, so it
//! is not used. Without a Secret Service (or when it refuses), the key goes
//! to `$XDG_CONFIG_HOME/vesper/stream-key` with mode 0600, like the portal
//! restore token: readable by this user's processes only, but in clear on
//! disk. The key is never logged.
//!
//! Every call may block (D-Bus, an unlock prompt): never on the UI thread.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use futures_lite::StreamExt;
use futures_lite::future::block_on;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::stream::StreamKey;

const DEST: &str = "org.freedesktop.secrets";
const SERVICE: &str = "/org/freedesktop/secrets";
const LABEL: &str = "Vesper stream key";

/// Where the key is kept, as the settings show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Store {
    Keyring,
    File,
}

/// `(session, parameters, value, content type)`, the Secret Service's `Secret`.
type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

fn attributes() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("application", crate::shortcuts::APP_ID),
        ("type", "stream-key"),
    ])
}

/// `$XDG_CONFIG_HOME/vesper/stream-key`.
fn file_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("vesper").join("stream-key"))
}

type Result<T> = std::result::Result<T, String>;

async fn proxy<'a>(
    conn: &zbus::Connection,
    path: impl TryInto<ObjectPath<'a>, Error: Into<zbus::Error>>,
    iface: &'a str,
) -> Result<zbus::Proxy<'a>> {
    zbus::Proxy::new(conn, DEST, path, iface)
        .await
        .map_err(|e| e.to_string())
}

/// A connection and a "plain" session (the bus is local: no transport
/// encryption needed).
async fn open() -> Result<(zbus::Connection, OwnedObjectPath)> {
    let conn = zbus::Connection::session()
        .await
        .map_err(|e| e.to_string())?;
    let service = proxy(&conn, SERVICE, "org.freedesktop.Secret.Service").await?;
    let (_, session): (OwnedValue, OwnedObjectPath) = service
        .call("OpenSession", &("plain", Value::from("")))
        .await
        .map_err(|e| e.to_string())?;
    Ok((conn, session))
}

/// Runs `prompt` (an unlock or confirmation dialog), unless it is `/`.
/// False when the user dismissed it.
async fn prompt(conn: &zbus::Connection, prompt: &OwnedObjectPath) -> Result<bool> {
    if prompt.as_str() == "/" {
        return Ok(true);
    }
    let p = proxy(conn, prompt.clone(), "org.freedesktop.Secret.Prompt").await?;
    let mut completed = p
        .receive_signal("Completed")
        .await
        .map_err(|e| e.to_string())?;
    p.call_method("Prompt", &("",))
        .await
        .map_err(|e| e.to_string())?;
    let signal = completed.next().await.ok_or("the prompt went away")?;
    let (dismissed, _): (bool, OwnedValue) =
        signal.body().deserialize().map_err(|e| e.to_string())?;
    Ok(!dismissed)
}

/// Unlocks `paths` if needed; false when the user refused.
async fn unlock(
    conn: &zbus::Connection,
    service: &zbus::Proxy<'_>,
    paths: &[OwnedObjectPath],
) -> Result<bool> {
    let (_, p): (Vec<OwnedObjectPath>, OwnedObjectPath) = service
        .call("Unlock", &(paths,))
        .await
        .map_err(|e| e.to_string())?;
    prompt(conn, &p).await
}

async fn items(service: &zbus::Proxy<'_>) -> Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>)> {
    service
        .call("SearchItems", &(attributes(),))
        .await
        .map_err(|e| e.to_string())
}

async fn keyring_load() -> Result<Option<StreamKey>> {
    let (conn, session) = open().await?;
    let service = proxy(&conn, SERVICE, "org.freedesktop.Secret.Service").await?;
    let (unlocked, locked) = items(&service).await?;
    let found = if unlocked.is_empty() {
        if locked.is_empty() || !unlock(&conn, &service, &locked).await? {
            return Ok(None);
        }
        locked
    } else {
        unlocked
    };
    let secrets: HashMap<OwnedObjectPath, Secret> = service
        .call("GetSecrets", &(&found, &session))
        .await
        .map_err(|e| e.to_string())?;
    Ok(found
        .iter()
        .find_map(|item| secrets.get(item))
        .and_then(|(_, _, value, _)| StreamKey::new(&String::from_utf8_lossy(value))))
}

async fn keyring_store(key: &StreamKey) -> Result<()> {
    let (conn, session) = open().await?;
    let service = proxy(&conn, SERVICE, "org.freedesktop.Secret.Service").await?;
    let collection: OwnedObjectPath = service
        .call("ReadAlias", &("default",))
        .await
        .map_err(|e| e.to_string())?;
    if collection.as_str() == "/" {
        return Err("no default keyring".into());
    }
    if !unlock(&conn, &service, std::slice::from_ref(&collection)).await? {
        return Err("the keyring stayed locked".into());
    }
    let c = proxy(&conn, collection, "org.freedesktop.Secret.Collection").await?;
    let properties = HashMap::from([
        ("org.freedesktop.Secret.Item.Label", Value::from(LABEL)),
        (
            "org.freedesktop.Secret.Item.Attributes",
            Value::from(attributes()),
        ),
    ]);
    let secret: Secret = (
        session,
        Vec::new(),
        key.expose().as_bytes().to_vec(),
        "text/plain".into(),
    );
    let (_, p): (OwnedObjectPath, OwnedObjectPath) = c
        .call("CreateItem", &(properties, secret, true))
        .await
        .map_err(|e| e.to_string())?;
    if !prompt(&conn, &p).await? {
        return Err("the keyring refused the key".into());
    }
    Ok(())
}

async fn keyring_clear() -> Result<()> {
    let (conn, _) = open().await?;
    let service = proxy(&conn, SERVICE, "org.freedesktop.Secret.Service").await?;
    let (unlocked, locked) = items(&service).await?;
    for item in unlocked.iter().chain(&locked) {
        let i = proxy(&conn, item.clone(), "org.freedesktop.Secret.Item").await?;
        let p: OwnedObjectPath = i.call("Delete", &()).await.map_err(|e| e.to_string())?;
        prompt(&conn, &p).await?;
    }
    Ok(())
}

async fn keyring_has() -> Result<bool> {
    let (conn, _) = open().await?;
    let service = proxy(&conn, SERVICE, "org.freedesktop.Secret.Service").await?;
    let (unlocked, locked) = items(&service).await?;
    Ok(!(unlocked.is_empty() && locked.is_empty()))
}

fn file_load() -> Option<StreamKey> {
    StreamKey::new(&std::fs::read_to_string(file_path()?).ok()?)
}

fn file_store(key: &StreamKey) -> std::io::Result<()> {
    write_private(&file_path().ok_or(std::io::ErrorKind::NotFound)?, key)
}

fn write_private(path: &std::path::Path, key: &StreamKey) -> std::io::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap_or(std::path::Path::new("/")))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // `mode` only applies on creation; fix an older file too.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(key.expose().as_bytes())
}

fn file_clear() {
    if let Some(path) = file_path()
        && let Err(e) = std::fs::remove_file(&path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log!("could not remove {}: {e}", path.display());
    }
}

/// The saved key: from the keyring (may ask to unlock it), else the file.
pub fn load() -> Option<StreamKey> {
    match block_on(keyring_load()) {
        Ok(Some(key)) => return Some(key),
        Ok(None) => {}
        Err(e) => log!("secret service: {e}"),
    }
    file_load()
}

/// Saves `key`: in the keyring, or in the private file if that fails.
pub fn store(key: &StreamKey) -> std::result::Result<Store, String> {
    match block_on(keyring_store(key)) {
        Ok(()) => {
            // A key saved before the keyring was there must not stay behind.
            file_clear();
            Ok(Store::Keyring)
        }
        // A key in a keyring that stayed locked would be read back first
        // later: never leave it behind a newer one in the file.
        Err(e) if block_on(keyring_has()).unwrap_or(false) => {
            log!("secret service: {e}");
            Err("The keyring stayed locked: unlock it to replace the stream key".into())
        }
        Err(e) => {
            log!("secret service: {e}; keeping the stream key in a private file");
            file_store(key)
                .map(|()| Store::File)
                .map_err(|e| format!("Could not save the stream key: {e}"))
        }
    }
}

/// Forgets the key, wherever it is.
pub fn clear() {
    if let Err(e) = block_on(keyring_clear()) {
        log!("secret service: {e}");
    }
    file_clear();
}

/// Where a key is saved, if any; does not unlock anything.
pub fn saved() -> Option<Store> {
    if block_on(keyring_has()).unwrap_or(false) {
        Some(Store::Keyring)
    } else if file_path().is_some_and(|p| p.exists()) {
        Some(Store::File)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against whatever Secret Service the session bus has: never run it on
    /// a user session, only in a private `dbus-run-session` with a
    /// throwaway service.
    #[test]
    #[ignore = "needs a throwaway Secret Service on a private session bus"]
    fn keyring_round_trip() {
        let key = StreamKey::new("live_test_key_123").unwrap();
        assert_eq!(block_on(keyring_store(&key)), Ok(()));
        assert!(block_on(keyring_has()).unwrap());
        assert_eq!(block_on(keyring_load()).unwrap(), Some(key));
        // Replaces, does not add.
        let other = StreamKey::new("other").unwrap();
        block_on(keyring_store(&other)).unwrap();
        assert_eq!(block_on(keyring_load()).unwrap(), Some(other));
        block_on(keyring_clear()).unwrap();
        assert!(!block_on(keyring_has()).unwrap());
        assert_eq!(block_on(keyring_load()).unwrap(), None);
    }

    #[test]
    fn the_fallback_file_is_private() {
        let dir = std::env::temp_dir().join(format!("vesper-secret-{}", std::process::id()));
        let path = dir.join("stream-key");
        std::fs::create_dir_all(&dir).unwrap();
        // An older, world-readable file is tightened.
        std::fs::write(&path, "an older, longer key").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, &StreamKey::new("k").unwrap()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "k");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
