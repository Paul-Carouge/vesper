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
use std::time::Duration;

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

/// How long a Secret Service call may stay unanswered: a bus that can start
/// one (activatable) that never comes up would otherwise hold go-live for
/// the bus's ~25 s activation timeout. Unlock dialogs are not bounded: the
/// `Prompt` call returns at once, and its answer comes as a signal.
const CALL_TIMEOUT: Duration = Duration::from_secs(4);

/// The error of a Secret Service that did not answer in time.
const NO_ANSWER: &str = "the Secret Service did not answer";

/// Whether `e` says the call went unanswered.
fn timed_out(e: &zbus::Error) -> bool {
    match e {
        zbus::Error::InputOutput(e) => e.kind() == std::io::ErrorKind::TimedOut,
        zbus::Error::MethodError(name, _, _) => matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.NoReply" | "org.freedesktop.DBus.Error.TimedOut"
        ),
        _ => false,
    }
}

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
/// encryption needed). [`ABSENT`] when there is no service, [`NO_ANSWER`]
/// when it does not answer within [`CALL_TIMEOUT`].
async fn open() -> Result<(zbus::Connection, OwnedObjectPath)> {
    let conn = match zbus::connection::Builder::session() {
        Ok(b) => b.method_timeout(CALL_TIMEOUT).build().await,
        Err(e) => Err(e),
    }
    .map_err(|_| ABSENT.to_owned())?;
    let service = proxy(&conn, SERVICE, "org.freedesktop.Secret.Service").await?;
    let (_, session): (OwnedValue, OwnedObjectPath) = service
        .call("OpenSession", &("plain", Value::from("")))
        .await
        .map_err(|e| {
            if absent(&e) {
                ABSENT.to_owned()
            } else if timed_out(&e) {
                NO_ANSWER.to_owned()
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

/// What a migration of the old key did.
#[derive(Debug, PartialEq)]
enum Migrated {
    /// No old key.
    Nothing,
    /// Moved to its service, read back equal, the old copy deleted.
    Moved,
    /// Left as it was: its service has a key already (never overwritten),
    /// or the keyring stayed locked. Tried again at the next launch.
    Kept,
}

/// Serializes saving, removing and migrating keys: a migration checks the
/// target has no key, then writes it.
static WRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn writing() -> std::sync::MutexGuard<'static, ()> {
    WRITING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Gives the old keyring item's key to `id`, the service it was saved for,
/// unless `id` has a key. The old item is deleted only once the new one
/// reads back equal.
async fn keyring_migrate(id: &str) -> Result<Migrated> {
    let (conn, session) = open().await?;
    let service = proxy(&conn, SERVICE, "org.freedesktop.Secret.Service").await?;
    let legacy = legacy_items(&conn, &service).await?;
    if legacy.0.is_empty() && legacy.1.is_empty() {
        return Ok(Migrated::Nothing);
    }
    let all = [legacy.0.clone(), legacy.1.clone()].concat();
    if keyring_has(id).await? {
        return Ok(Migrated::Kept);
    }
    let Some(key) = first_secret(&conn, &service, &session, legacy).await? else {
        // Locked and not unlocked.
        return Ok(Migrated::Kept);
    };
    // Again, right before writing: the unlock prompt may have taken long.
    if keyring_has(id).await? {
        return Ok(Migrated::Kept);
    }
    keyring_store(id, &key).await?;
    if keyring_load(id).await? != Some(key) {
        return Err(format!("the key saved for {id} does not read back"));
    }
    delete_items(&conn, &all).await?;
    Ok(Migrated::Moved)
}

/// A line of the keys file: a key, or a line kept as it is.
#[derive(Clone, Debug, PartialEq)]
enum Line {
    Key(String, StreamKey),
    Other(String),
}

/// `<service>\t<key>` lines; others are kept verbatim when rewriting.
fn parse_keys(text: &str) -> Vec<Line> {
    text.lines()
        .map(|line| {
            let key = line
                .split_once('\t')
                .and_then(|(id, key)| Some((id.to_owned(), StreamKey::new(key)?)));
            match key {
                Some((id, key)) => Line::Key(id, key),
                None => Line::Other(line.to_owned()),
            }
        })
        .collect()
}

fn serialize_keys(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|line| match line {
            Line::Key(id, key) => format!("{id}\t{}\n", key.expose()),
            Line::Other(text) => format!("{text}\n"),
        })
        .collect()
}

/// The file's lines; none if it does not exist, an error if it cannot be
/// read (then nothing may rewrite it).
fn read_keys(path: &Path) -> std::io::Result<Vec<Line>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse_keys(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Sets (`Some`) or removes `id`'s key in the file at `path`, the other
/// lines as they are; the file goes with its last line. `durable`: synced
/// to the disk before it returns.
fn write_key(path: &Path, id: &str, key: Option<&StreamKey>, durable: bool) -> std::io::Result<()> {
    let mut lines = read_keys(path)?;
    lines.retain(|line| !matches!(line, Line::Key(i, _) if i == id));
    if let Some(key) = key {
        // One line each: a key never spans lines (`StreamKey::new` trims).
        if key.expose().contains(['\n', '\r', '\t']) || id.contains(['\n', '\r', '\t']) {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        lines.push(Line::Key(id.to_owned(), key.clone()));
    }
    if lines.is_empty() {
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    crate::prefs::write_file(path, serialize_keys(&lines).as_bytes(), durable)
}

fn file_load_from(path: &Path, id: &str) -> Option<StreamKey> {
    read_keys(path)
        .inspect_err(|e| log!("could not read {}: {e}", path.display()))
        .ok()?
        .into_iter()
        .find_map(|line| match line {
            Line::Key(i, key) if i == id => Some(key),
            _ => None,
        })
}

fn file_load(id: &str) -> Option<StreamKey> {
    file_load_from(&file_path()?, id)
}

fn file_store(id: &str, key: &StreamKey) -> std::io::Result<()> {
    write_key(
        &file_path().ok_or(std::io::ErrorKind::NotFound)?,
        id,
        Some(key),
        false,
    )
}

fn file_clear(id: &str) {
    if let Some(path) = file_path()
        && let Err(e) = write_key(&path, id, None, false)
    {
        log!("could not remove a key from {}: {e}", path.display());
    }
}

/// Where the old file's key goes.
enum Target<'a> {
    /// The keyring (there is one): never the file in clear.
    Keyring,
    /// The keys file at this path (no keyring).
    File(&'a Path),
}

/// Gives the old file's key at `legacy` to `id`, the service it was saved
/// for, in `target`, unless `id` has a key there. The old file is removed
/// only once the new copy is durable and reads back equal.
fn file_migrate(legacy: &Path, id: &str, target: Target) -> Result<Migrated> {
    let key = match std::fs::read_to_string(legacy) {
        Ok(text) => StreamKey::new(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Migrated::Nothing),
        Err(e) => return Err(format!("could not read {}: {e}", legacy.display())),
    };
    if let Some(key) = &key {
        match target {
            Target::Keyring => {
                if block_on(keyring_has(id))? {
                    return Ok(Migrated::Kept);
                }
                block_on(keyring_store(id, key))?;
                if block_on(keyring_load(id))?.as_ref() != Some(key) {
                    return Err(format!("the key saved for {id} does not read back"));
                }
            }
            Target::File(path) => {
                let lines = read_keys(path)
                    .map_err(|e| format!("could not read {}: {e}", path.display()))?;
                if lines
                    .iter()
                    .any(|l| matches!(l, Line::Key(i, _) if i == id))
                {
                    return Ok(Migrated::Kept);
                }
                write_key(path, id, Some(key), true)
                    .map_err(|e| format!("could not save {}: {e}", path.display()))?;
                if file_load_from(path, id).as_ref() != Some(key) {
                    return Err(format!("the key saved for {id} does not read back"));
                }
            }
        }
    }
    std::fs::remove_file(legacy)
        .map_err(|e| format!("could not remove {}: {e}", legacy.display()))?;
    Ok(Migrated::Moved)
}

/// Gives the one key of an older version (keyring item or file) to `id`,
/// the service it was saved for (the caller reads it from the settings of
/// that version). Never to another one: a key is only read for the service
/// it is saved for, and a newer key of `id` is never overwritten.
pub fn migrate(id: &str) {
    let _writing = writing();
    let keyring = match block_on(keyring_migrate(id)) {
        Ok(done) => {
            if done != Migrated::Nothing {
                log!("old stream key for {id}: {done:?}");
            }
            true
        }
        Err(e) if e == ABSENT => false,
        Err(e) => {
            log!("secret service: {e}; the old stream key is kept");
            return;
        }
    };
    let Some(legacy) = legacy_file_path() else {
        return;
    };
    let path = file_path();
    let target = match (keyring, &path) {
        (true, _) => Target::Keyring,
        (false, Some(path)) => Target::File(path),
        (false, None) => return,
    };
    match file_migrate(&legacy, id, target) {
        Ok(Migrated::Nothing) => {}
        Ok(done) => log!("old stream key file for {id}: {done:?}"),
        Err(e) => log!("{e}; {} is kept", legacy.display()),
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
/// is no keyring. One that does not answer may still be there: no copy in
/// clear then either.
pub fn store(id: &str, key: &StreamKey) -> std::result::Result<Store, String> {
    let _writing = writing();
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
        Err(e) if e == NO_ANSWER => {
            log!("secret service: {e}");
            Err("The keyring did not answer: key not saved".into())
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
    let _writing = writing();
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

    /// A fresh folder for one test.
    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vesper-keys-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_file_keeps_one_key_per_service() {
        let dir = temp("file");
        let path = dir.join("stream-keys");
        write_key(&path, "twitch", Some(&key("tw")), false).unwrap();
        write_key(&path, "custom:a.example", Some(&key("ca")), false).unwrap();
        write_key(&path, "twitch", Some(&key("tw2")), true).unwrap();
        assert_eq!(file_load_from(&path, "twitch"), Some(key("tw2")));
        assert_eq!(file_load_from(&path, "custom:a.example"), Some(key("ca")));
        // Never another service's key.
        assert_eq!(file_load_from(&path, "youtube"), None);
        assert_eq!(file_load_from(&path, "custom:b.example"), None);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        write_key(&path, "twitch", None, false).unwrap();
        assert_eq!(file_load_from(&path, "twitch"), None);
        write_key(&path, "custom:a.example", None, false).unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lines_it_does_not_know_survive_and_an_unreadable_file_is_not_rewritten() {
        let dir = temp("lines");
        let path = dir.join("stream-keys");
        std::fs::write(&path, "# note\nyoutube\tyt\nno tab here\nfuture\t\n").unwrap();
        write_key(&path, "twitch", Some(&key("tw")), false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# note\nyoutube\tyt\nno tab here\nfuture\t\ntwitch\ttw\n"
        );
        // A folder in the way: EISDIR, nothing clobbered, no key read.
        let blocked = dir.join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        assert!(read_keys(&blocked).is_err());
        assert!(write_key(&blocked, "twitch", Some(&key("x")), false).is_err());
        assert!(blocked.is_dir());
        assert_eq!(file_load_from(&blocked, "twitch"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_old_key_file_goes_to_its_service_and_never_overwrites() {
        let dir = temp("old");
        let (legacy, path) = (dir.join("stream-key"), dir.join("stream-keys"));
        let to_file = || Target::File(&path);
        assert_eq!(
            file_migrate(&legacy, "youtube", to_file()),
            Ok(Migrated::Nothing)
        );
        std::fs::write(&legacy, "old_key\n").unwrap();
        assert_eq!(
            file_migrate(&legacy, "youtube", to_file()),
            Ok(Migrated::Moved)
        );
        assert!(!legacy.exists());
        assert_eq!(file_load_from(&path, "youtube"), Some(key("old_key")));
        assert_eq!(file_load_from(&path, "twitch"), None);
        // The service has a key: both are kept, the newer one untouched.
        std::fs::write(&legacy, "older").unwrap();
        assert_eq!(
            file_migrate(&legacy, "youtube", to_file()),
            Ok(Migrated::Kept)
        );
        assert_eq!(file_load_from(&path, "youtube"), Some(key("old_key")));
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "older");
        // An unreadable keys file: the old file stays.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(file_migrate(&legacy, "twitch", to_file()).is_err());
        assert!(legacy.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Against whatever Secret Service the session bus has: never run it on
    /// a user session, only in a private `dbus-run-session` with a
    /// throwaway service (`scripts/fake_secret_service.py`). Each keyring
    /// test uses ids of its own.
    #[test]
    #[ignore = "needs a throwaway Secret Service on a private session bus"]
    fn keyring_round_trip() {
        let (a, b) = ("test-round-a", "test-round-b");
        let k = key("live_test_key_123");
        assert_eq!(block_on(keyring_store(a, &k)), Ok(()));
        assert!(block_on(keyring_has(a)).unwrap());
        assert!(!block_on(keyring_has(b)).unwrap());
        assert_eq!(block_on(keyring_load(a)).unwrap(), Some(k.clone()));
        // Never another service's key.
        assert_eq!(block_on(keyring_load(b)).unwrap(), None);
        // Replaces, does not add.
        let other = key("other");
        block_on(keyring_store(a, &other)).unwrap();
        assert_eq!(block_on(keyring_load(a)).unwrap(), Some(other));
        block_on(keyring_store(b, &k)).unwrap();
        block_on(keyring_clear(a)).unwrap();
        assert!(!block_on(keyring_has(a)).unwrap());
        assert_eq!(block_on(keyring_load(a)).unwrap(), None);
        assert_eq!(block_on(keyring_load(b)).unwrap(), Some(k));
        block_on(keyring_clear(b)).unwrap();
    }

    /// Adds an item as versions before per-service keys saved it.
    async fn seed_old_item(secret: &[u8]) {
        let (conn, session) = open().await.unwrap();
        let service = proxy(&conn, SERVICE, "org.freedesktop.Secret.Service")
            .await
            .unwrap();
        let collection: OwnedObjectPath = service.call("ReadAlias", &("default",)).await.unwrap();
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
        let secret: Secret = (session, Vec::new(), secret.to_vec(), "text/plain".into());
        let _: (OwnedObjectPath, OwnedObjectPath) = c
            .call("CreateItem", &(properties, secret, true))
            .await
            .unwrap();
    }

    async fn old_items() -> usize {
        let (conn, _) = open().await.unwrap();
        let service = proxy(&conn, SERVICE, "org.freedesktop.Secret.Service")
            .await
            .unwrap();
        let (a, b) = legacy_items(&conn, &service).await.unwrap();
        a.len() + b.len()
    }

    /// The old item goes to its service only when that one has no key, and
    /// is deleted once moved. Same private bus as `keyring_round_trip`; the
    /// only test that makes old items.
    #[test]
    #[ignore = "needs a throwaway Secret Service on a private session bus"]
    fn keyring_old_key_goes_to_its_service_and_never_overwrites() {
        let (target, other) = ("test-old-target", "test-old-other");
        block_on(seed_old_item(b"old_key"));
        // The target has a newer key: both kept.
        block_on(keyring_store(target, &key("newer"))).unwrap();
        assert_eq!(block_on(keyring_migrate(target)), Ok(Migrated::Kept));
        assert_eq!(block_on(keyring_load(target)).unwrap(), Some(key("newer")));
        assert_eq!(block_on(old_items()), 1);
        // Without one: moved, read back, the old item deleted.
        block_on(keyring_clear(target)).unwrap();
        assert_eq!(block_on(keyring_migrate(target)), Ok(Migrated::Moved));
        assert_eq!(
            block_on(keyring_load(target)).unwrap(),
            Some(key("old_key"))
        );
        assert_eq!(block_on(keyring_load(other)).unwrap(), None);
        assert_eq!(block_on(old_items()), 0);
        assert_eq!(block_on(keyring_migrate(other)), Ok(Migrated::Nothing));
        block_on(keyring_clear(target)).unwrap();
    }

    /// With the fake started as `DISMISS=1 SEED_OLD=old_key`: the keyring
    /// stays locked, so the old item is kept and nothing is written.
    #[test]
    #[ignore = "needs the fake Secret Service with DISMISS=1 SEED_OLD=old_key"]
    fn keyring_locked_keeps_the_old_key() {
        assert_eq!(block_on(old_items()), 1);
        assert_eq!(block_on(keyring_migrate("test-locked")), Ok(Migrated::Kept));
        assert_eq!(block_on(old_items()), 1);
        assert!(!block_on(keyring_has("test-locked")).unwrap());
    }

    /// With the fake started as `HANG=1` (it never answers `OpenSession`):
    /// every call gives up after `CALL_TIMEOUT`, a load falls back to the
    /// file and a store says so instead of writing the key in clear.
    #[test]
    #[ignore = "needs the fake Secret Service with HANG=1"]
    fn a_hung_keyring_gives_up_quickly() {
        let started = std::time::Instant::now();
        assert_eq!(block_on(keyring_load("test-hung")), Err(NO_ANSWER.into()));
        assert_eq!(load("test-hung"), None);
        assert_eq!(
            store("test-hung", &key("live_test_key_123")),
            Err("The keyring did not answer: key not saved".into())
        );
        assert_eq!(file_load("test-hung"), None);
        clear("test-hung");
        assert_eq!(saved("test-hung"), None);
        // Five calls, each bounded.
        let limit = CALL_TIMEOUT * 5 + Duration::from_secs(2);
        assert!(started.elapsed() < limit, "{:?}", started.elapsed());
    }

    /// `EXPECT=absent` on a private bus without a Secret Service, or
    /// `EXPECT=refused` with the fake one started with `DISMISS=1`.
    #[test]
    #[ignore = "needs a private session bus prepared as EXPECT says"]
    fn a_refusing_keyring_is_not_an_absent_one() {
        let result = block_on(keyring_store("test-refused", &key("live_test_key_123")));
        match std::env::var("EXPECT").as_deref() {
            Ok("absent") => assert_eq!(result, Err(ABSENT.into())),
            Ok("refused") => assert!(result.is_err_and(|e| e != ABSENT)),
            other => panic!("EXPECT={other:?}"),
        }
    }
}
