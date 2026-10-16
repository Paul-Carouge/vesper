//! The stream keys, kept out of the settings file: one per service (Twitch,
//! YouTube, and each custom server host, see [`key_id`]), so a key is only
//! ever sent to the service it was saved for, whichever profile is on.
//!
//! First choice: the Secret Service (`org.freedesktop.secrets` on the session
//! bus: KWallet's ksecretd on Plasma, GNOME Keyring, KeePassXC), which keeps
//! them encrypted at rest and unlocked with the session, one item per
//! service (attribute `service`). The Secret portal is for sandboxed apps
//! only and would still leave the encryption to us, so it is not used. Only
//! when the session has no Secret Service at all do they go to
//! `$XDG_CONFIG_HOME/vesper/stream-keys` (`<service>\t<key>` lines, mode
//! 0600, written atomically), like the portal restore token: readable by
//! this user's processes only, but in clear on disk. A keyring that is there
//! but stays locked (the user dismissed the prompt) never sends a key to the
//! file. Keys are never logged.
//!
//! Versions before per-service keys kept one key (an item without the
//! `service` attribute, or the `stream-key` file): [`migrate`] gives it to
//! the service selected at startup.
//!
//! Every call may block (D-Bus, an unlock prompt): never on the UI thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use futures_lite::StreamExt;
use futures_lite::future::block_on;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::stream::{Service, StreamKey, StreamSettings};

const DEST: &str = "org.freedesktop.secrets";
const SERVICE: &str = "/org/freedesktop/secrets";

/// Where the key is kept, as the settings show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Store {
    Keyring,
    File,
}

/// `(session, parameters, value, content type)`, the Secret Service's `Secret`.
type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

/// Which key a stream with `settings` uses: `twitch`, `youtube`, or
/// `custom:<host>` (lowercase, no port or user); `None` for a custom URL
/// without a host.
pub fn key_id(settings: &StreamSettings) -> Option<String> {
    match settings.service {
        Service::Twitch => Some("twitch".into()),
        Service::YouTube => Some("youtube".into()),
        Service::Custom => custom_host(&settings.custom_url).map(|h| format!("custom:{h}")),
    }
}

/// The host of a server URL: between `://` (and a `user@`) and the port,
/// path or query.
fn custom_host(url: &str) -> Option<String> {
    let (_, rest) = url.trim().split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host_port.strip_prefix('[') {
        // [IPv6]:port
        Some(v6) => &host_port[..v6.find(']')? + 2],
        None => host_port.split(':').next()?,
    };
    let valid = !host.is_empty() && !host.chars().any(|c| c.is_whitespace() || c.is_control());
    valid.then(|| host.to_lowercase())
}

/// The items of every key.
fn base_attributes() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("application", crate::shortcuts::APP_ID),
        ("type", "stream-key"),
    ])
}

/// The item of `id`'s key.
fn attributes(id: &str) -> HashMap<&str, &str> {
    let mut a = base_attributes();
    a.insert("service", id);
    a
}

/// `$XDG_CONFIG_HOME/vesper/<name>`.
fn config_file(name: &str) -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("vesper").join(name))
}

/// The fallback file of the keys.
fn file_path() -> Option<PathBuf> {
    config_file("stream-keys")
}

/// The one key of versions before per-service keys.
fn legacy_file_path() -> Option<PathBuf> {
    config_file("stream-key")
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

/// The error of every call when the session has no Secret Service.
const ABSENT: &str = "no Secret Service on the session bus";

/// Whether `e` says nothing provides the Secret Service.
fn absent(e: &zbus::Error) -> bool {
    match e {
        zbus::Error::MethodError(name, _, _) => matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.ServiceUnknown"
                | "org.freedesktop.DBus.Error.NameHasNoOwner"
        ),
        _ => false,
    }
}

/// A connection and a "plain" session (the bus is local: no transport
/// encryption needed). [`ABSENT`] when there is no service.
async fn open() -> Result<(zbus::Connection, OwnedObjectPath)> {
    let conn = zbus::Connection::session()
        .await
        .map_err(|_| ABSENT.to_owned())?;
    let service = proxy(&conn, SERVICE, "org.freedesktop.Secret.Service").await?;
    let (_, session): (OwnedValue, OwnedObjectPath) = service
        .call("OpenSession", &("plain", Value::from("")))
        .await
        .map_err(|e| {
            if absent(&e) {
                ABSENT.to_owned()
            } else {
                e.to_string()
            }
        })?;
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

/// The items with `attributes`: unlocked, locked.
async fn items(
    service: &zbus::Proxy<'_>,
    attributes: HashMap<&str, &str>,
) -> Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>)> {
    service
        .call("SearchItems", &(attributes,))
        .await
        .map_err(|e| e.to_string())
}

/// The secret of the first of `found` (unlocked, locked), unlocking them
/// if needed.
async fn first_secret(
    conn: &zbus::Connection,
    service: &zbus::Proxy<'_>,
    session: &OwnedObjectPath,
    (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>),
) -> Result<Option<StreamKey>> {
    let found = if unlocked.is_empty() {
        if locked.is_empty() || !unlock(conn, service, &locked).await? {
            return Ok(None);
        }
        locked
    } else {
        unlocked
    };
    let secrets: HashMap<OwnedObjectPath, Secret> = service
        .call("GetSecrets", &(&found, session))
        .await
        .map_err(|e| e.to_string())?;
    Ok(found
        .iter()
        .find_map(|item| secrets.get(item))
        .and_then(|(_, _, value, _)| StreamKey::new(&String::from_utf8_lossy(value))))
}

async fn keyring_load(id: &str) -> Result<Option<StreamKey>> {
    let (conn, session) = open().await?;
    let service = proxy(&conn, SERVICE, "org.freedesktop.Secret.Service").await?;
    let found = items(&service, attributes(id)).await?;
    first_secret(&conn, &service, &session, found).await
}

async fn keyring_store(id: &str, key: &StreamKey) -> Result<()> {
    let (conn, session) = open().await?;
    let service = proxy(&conn, SERVICE, "org.freedesktop.Secret.Service").await?;
    let collection: OwnedObjectPath = service
        .call("ReadAlias", &("default",))
        .await
        .map_err(|e| e.to_string())?;
    // A service without a keyring to store in: as good as none.
    if collection.as_str() == "/" {
        return Err(ABSENT.into());
    }
    if !unlock(&conn, &service, std::slice::from_ref(&collection)).await? {
        return Err("the keyring stayed locked".into());
    }
    let c = proxy(&conn, collection, "org.freedesktop.Secret.Collection").await?;
    let label = format!("Vesper stream key ({id})");
    let properties = HashMap::from([
        ("org.freedesktop.Secret.Item.Label", Value::from(label)),
        (
            "org.freedesktop.Secret.Item.Attributes",
            Value::from(attributes(id)),
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

async fn delete_items(conn: &zbus::Connection, paths: &[OwnedObjectPath]) -> Result<()> {
    for item in paths {
        let i = proxy(conn, item.clone(), "org.freedesktop.Secret.Item").await?;
        let p: OwnedObjectPath = i.call("Delete", &()).await.map_err(|e| e.to_string())?;
        prompt(conn, &p).await?;
    }
    Ok(())
}

async fn keyring_clear(id: &str) -> Result<()> {
    let (conn, _) = open().await?;
    let service = proxy(&conn, SERVICE, "org.freedesktop.Secret.Service").await?;
    let (unlocked, locked) = items(&service, attributes(id)).await?;
    delete_items(&conn, &[unlocked, locked].concat()).await
}

async fn keyring_has(id: &str) -> Result<bool> {
    let (conn, _) = open().await?;
    let service = proxy(&conn, SERVICE, "org.freedesktop.Secret.Service").await?;
    let (unlocked, locked) = items(&service, attributes(id)).await?;
    Ok(!(unlocked.is_empty() && locked.is_empty()))
}

/// The items of the one key of older versions: no `service` attribute.
async fn legacy_items(
    conn: &zbus::Connection,
    service: &zbus::Proxy<'_>,
) -> Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>)> {
    let (unlocked, locked) = items(service, base_attributes()).await?;
    let mut legacy = (Vec::new(), Vec::new());
    for (list, out) in [(unlocked, &mut legacy.0), (locked, &mut legacy.1)] {
        for item in list {
            let p = proxy(conn, item.clone(), "org.freedesktop.DBus.Properties").await?;
            let value: OwnedValue = p
                .call("Get", &("org.freedesktop.Secret.Item", "Attributes"))
                .await
                .map_err(|e| e.to_string())?;
            let attributes =
                HashMap::<String, String>::try_from(value).map_err(|e| e.to_string())?;
            if !attributes.contains_key("service") {
                out.push(item);
            }
        }
    }
    Ok(legacy)
}

/// Gives the key of an older version to `id` (unless `id` has one), then
/// deletes it. Ok(false) when there is none.
async fn keyring_migrate(id: &str) -> Result<bool> {
    let (conn, session) = open().await?;
    let service = proxy(&conn, SERVICE, "org.freedesktop.Secret.Service").await?;
    let legacy = legacy_items(&conn, &service).await?;
    if legacy.0.is_empty() && legacy.1.is_empty() {
        return Ok(false);
    }
    let all = [legacy.0.clone(), legacy.1.clone()].concat();
    if !keyring_has(id).await? {
        let Some(key) = first_secret(&conn, &service, &session, legacy).await? else {
            // Locked and not unlocked: kept for the next launch.
            return Ok(false);
        };
        keyring_store(id, &key).await?;
    }
    delete_items(&conn, &all).await?;
    Ok(true)
}

/// `<service>\t<key>` lines; a line that is not one is left out.
fn parse_keys(text: &str) -> Vec<(String, StreamKey)> {
    text.lines()
        .filter_map(|line| {
            let (id, key) = line.split_once('\t')?;
            Some((id.to_owned(), StreamKey::new(key)?))
        })
        .collect()
}

fn serialize_keys(keys: &[(String, StreamKey)]) -> String {
    keys.iter()
        .map(|(id, key)| format!("{id}\t{}\n", key.expose()))
        .collect()
}

fn read_keys(path: &Path) -> Vec<(String, StreamKey)> {
    std::fs::read_to_string(path)
        .map(|t| parse_keys(&t))
        .unwrap_or_default()
}

/// Sets (`Some`) or removes `id`'s key in the file at `path`; the file goes
/// with its last key.
fn write_key(path: &Path, id: &str, key: Option<&StreamKey>) -> std::io::Result<()> {
    let mut keys = read_keys(path);
    keys.retain(|(i, _)| i != id);
    if let Some(key) = key {
        // One line each: a key never spans lines (`StreamKey::new` trims).
        if key.expose().contains(['\n', '\r', '\t']) {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        keys.push((id.to_owned(), key.clone()));
    }
    if keys.is_empty() {
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    crate::prefs::write_atomically(path, serialize_keys(&keys).as_bytes())
}

fn file_load_from(path: &Path, id: &str) -> Option<StreamKey> {
    read_keys(path)
        .into_iter()
        .find_map(|(i, key)| (i == id).then_some(key))
}

fn file_load(id: &str) -> Option<StreamKey> {
    file_load_from(&file_path()?, id)
}

fn file_store(id: &str, key: &StreamKey) -> std::io::Result<()> {
    write_key(
        &file_path().ok_or(std::io::ErrorKind::NotFound)?,
        id,
        Some(key),
    )
}

fn file_clear(id: &str) {
    if let Some(path) = file_path()
        && let Err(e) = write_key(&path, id, None)
    {
        log!("could not remove a key from {}: {e}", path.display());
    }
}

/// Moves the key of an older version's file at `legacy` to `id` in `path`
/// (unless `id` has one). True if there was one.
fn file_migrate(legacy: &Path, path: &Path, id: &str) -> std::io::Result<bool> {
    let key = match std::fs::read_to_string(legacy) {
        Ok(text) => StreamKey::new(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if let Some(key) = key
        && file_load_from(path, id).is_none()
    {
        write_key(path, id, Some(&key))?;
    }
    std::fs::remove_file(legacy)?;
    Ok(true)
}

/// Gives the one key of an older version (keyring item or file) to `id`,
/// the service selected at startup. Never to another one later: a key is
/// only read for the service it is saved for.
pub fn migrate(id: &str) {
    match block_on(keyring_migrate(id)) {
        Ok(true) => log!("the stream key now belongs to {id}"),
        Ok(false) => {}
        Err(e) if e == ABSENT => {}
        Err(e) => log!("secret service: {e}"),
    }
    if let (Some(legacy), Some(path)) = (legacy_file_path(), file_path()) {
        match file_migrate(&legacy, &path, id) {
            Ok(true) => log!("the stream key file now belongs to {id}"),
            Ok(false) => {}
            Err(e) => log!("could not move {}: {e}", legacy.display()),
        }
    }
}

/// `id`'s key: from the keyring (may ask to unlock it), else the file.
pub fn load(id: &str) -> Option<StreamKey> {
    match block_on(keyring_load(id)) {
        Ok(Some(key)) => return Some(key),
        Ok(None) => {}
        Err(e) => log!("secret service: {e}"),
    }
    file_load(id)
}

/// Saves `key` for `id`: in the keyring, or in the private file if there
/// is no keyring.
pub fn store(id: &str, key: &StreamKey) -> std::result::Result<Store, String> {
    match block_on(keyring_store(id, key)) {
        Ok(()) => {
            // A key saved before the keyring was there must not stay behind.
            file_clear(id);
            Ok(Store::Keyring)
        }
        Err(e) if e == ABSENT => {
            log!("secret service: {e}; keeping the stream key in a private file");
            file_store(id, key)
                .map(|()| Store::File)
                .map_err(|e| format!("Could not save the stream key: {e}"))
        }
        // There is a keyring, which refused (locked, prompt dismissed): no
        // copy in clear.
        Err(e) => {
            log!("secret service: {e}");
            Err("The keyring stayed locked: key not saved".into())
        }
    }
}

/// Forgets `id`'s key, wherever it is.
pub fn clear(id: &str) {
    if let Err(e) = block_on(keyring_clear(id)) {
        log!("secret service: {e}");
    }
    file_clear(id);
}

/// Where `id`'s key is saved, if anywhere; does not unlock anything.
pub fn saved(id: &str) -> Option<Store> {
    if block_on(keyring_has(id)).unwrap_or(false) {
        Some(Store::Keyring)
    } else if file_load(id).is_some() {
        Some(Store::File)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn custom(url: &str) -> StreamSettings {
        StreamSettings {
            service: Service::Custom,
            custom_url: url.into(),
            ..StreamSettings::default()
        }
    }

    fn key(k: &str) -> StreamKey {
        StreamKey::new(k).unwrap()
    }

    #[test]
    fn each_service_and_custom_host_has_its_own_key() {
        let twitch = StreamSettings::default();
        assert_eq!(key_id(&twitch).as_deref(), Some("twitch"));
        let youtube = StreamSettings {
            service: Service::YouTube,
            ..twitch.clone()
        };
        assert_eq!(key_id(&youtube).as_deref(), Some("youtube"));
        for (url, id) in [
            ("rtmp://Live.Example.com/app", "custom:live.example.com"),
            (
                "rtmps://live.example.com:443/app/",
                "custom:live.example.com",
            ),
            ("srt://u:p@10.0.0.2:9000?latency=1", "custom:10.0.0.2"),
            ("srt://[fd00::1]:9000", "custom:[fd00::1]"),
        ] {
            assert_eq!(key_id(&custom(url)).as_deref(), Some(id), "{url}");
        }
        for url in [
            "",
            "rtmp://",
            "rtmp:///app",
            "live.example.com/app",
            "rtmp://a b/x",
        ] {
            assert_eq!(key_id(&custom(url)), None, "{url}");
        }
    }

    #[test]
    fn the_file_keeps_one_key_per_service() {
        let dir = std::env::temp_dir().join(format!("vesper-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("stream-keys");
        write_key(&path, "twitch", Some(&key("tw"))).unwrap();
        write_key(&path, "custom:a.example", Some(&key("ca"))).unwrap();
        write_key(&path, "twitch", Some(&key("tw2"))).unwrap();
        assert_eq!(file_load_from(&path, "twitch"), Some(key("tw2")));
        assert_eq!(file_load_from(&path, "custom:a.example"), Some(key("ca")));
        // Never another service's key.
        assert_eq!(file_load_from(&path, "youtube"), None);
        assert_eq!(file_load_from(&path, "custom:b.example"), None);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        write_key(&path, "twitch", None).unwrap();
        assert_eq!(file_load_from(&path, "twitch"), None);
        write_key(&path, "custom:a.example", None).unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_old_key_file_goes_to_the_selected_service() {
        let dir = std::env::temp_dir().join(format!("vesper-keys-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (legacy, path) = (dir.join("stream-key"), dir.join("stream-keys"));
        assert!(!file_migrate(&legacy, &path, "youtube").unwrap());
        std::fs::write(&legacy, "old_key\n").unwrap();
        assert!(file_migrate(&legacy, &path, "youtube").unwrap());
        assert!(!legacy.exists());
        assert_eq!(file_load_from(&path, "youtube"), Some(key("old_key")));
        assert_eq!(file_load_from(&path, "twitch"), None);
        // A key already saved for that service wins.
        std::fs::write(&legacy, "older").unwrap();
        file_migrate(&legacy, &path, "youtube").unwrap();
        assert_eq!(file_load_from(&path, "youtube"), Some(key("old_key")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Against whatever Secret Service the session bus has: never run it on
    /// a user session, only in a private `dbus-run-session` with a
    /// throwaway service (`scripts/fake_secret_service.py`).
    #[test]
    #[ignore = "needs a throwaway Secret Service on a private session bus"]
    fn keyring_round_trip() {
        let k = key("live_test_key_123");
        assert_eq!(block_on(keyring_store("twitch", &k)), Ok(()));
        assert!(block_on(keyring_has("twitch")).unwrap());
        assert!(!block_on(keyring_has("youtube")).unwrap());
        assert_eq!(block_on(keyring_load("twitch")).unwrap(), Some(k.clone()));
        // Never another service's key.
        assert_eq!(block_on(keyring_load("youtube")).unwrap(), None);
        // Replaces, does not add.
        let other = key("other");
        block_on(keyring_store("twitch", &other)).unwrap();
        assert_eq!(block_on(keyring_load("twitch")).unwrap(), Some(other));
        block_on(keyring_store("youtube", &k)).unwrap();
        block_on(keyring_clear("twitch")).unwrap();
        assert!(!block_on(keyring_has("twitch")).unwrap());
        assert_eq!(block_on(keyring_load("twitch")).unwrap(), None);
        assert_eq!(block_on(keyring_load("youtube")).unwrap(), Some(k));
        block_on(keyring_clear("youtube")).unwrap();
    }

    /// An item of an older version (no `service` attribute) goes to the
    /// service given, once. Same private bus as `keyring_round_trip`.
    #[test]
    #[ignore = "needs a throwaway Secret Service on a private session bus"]
    fn keyring_old_key_goes_to_the_selected_service() {
        block_on(async {
            let (conn, session) = open().await.unwrap();
            let service = proxy(&conn, SERVICE, "org.freedesktop.Secret.Service")
                .await
                .unwrap();
            let collection: OwnedObjectPath =
                service.call("ReadAlias", &("default",)).await.unwrap();
            assert!(
                unlock(&conn, &service, std::slice::from_ref(&collection))
                    .await
                    .unwrap()
            );
            let c = proxy(&conn, collection, "org.freedesktop.Secret.Collection")
                .await
                .unwrap();
            let properties = HashMap::from([(
                "org.freedesktop.Secret.Item.Attributes",
                Value::from(base_attributes()),
            )]);
            let secret: Secret = (
                session,
                Vec::new(),
                b"old_key".to_vec(),
                "text/plain".into(),
            );
            let _: (OwnedObjectPath, OwnedObjectPath) = c
                .call("CreateItem", &(properties, secret, true))
                .await
                .unwrap();
        });
        assert_eq!(block_on(keyring_load("youtube")).unwrap(), None);
        assert_eq!(block_on(keyring_migrate("youtube")), Ok(true));
        assert_eq!(
            block_on(keyring_load("youtube")).unwrap(),
            Some(key("old_key"))
        );
        assert_eq!(block_on(keyring_load("twitch")).unwrap(), None);
        // Done once: the old item is gone.
        assert_eq!(block_on(keyring_migrate("twitch")), Ok(false));
        block_on(keyring_clear("youtube")).unwrap();
    }

    /// `EXPECT=absent` on a private bus without a Secret Service, or
    /// `EXPECT=refused` with the fake one started with `DISMISS=1`.
    #[test]
    #[ignore = "needs a private session bus prepared as EXPECT says"]
    fn a_refusing_keyring_is_not_an_absent_one() {
        let result = block_on(keyring_store("twitch", &key("live_test_key_123")));
        match std::env::var("EXPECT").as_deref() {
            Ok("absent") => assert_eq!(result, Err(ABSENT.into())),
            Ok("refused") => assert!(result.is_err_and(|e| e != ABSENT)),
            other => panic!("EXPECT={other:?}"),
        }
    }
}
