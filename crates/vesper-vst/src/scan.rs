//! Finding the installed VST3 effects. Each bundle is opened by a host
//! process (`vesper-vst-host --scan <bundle>`): a plugin that crashes on
//! load only takes that process down. Results are cached by the module's
//! modification time, failures included, so a bad plugin is not retried
//! until it changes.

use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use tracing::warn;

use crate::client::HostCommand;
use crate::sidecar::{hex, hex_decode};

/// Longest a bundle may take to list its plugins.
const SCAN_TIMEOUT: Duration = Duration::from_secs(10);

/// An installed effect.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginInfo {
    /// Its `.vst3` bundle.
    pub path: PathBuf,
    /// Class id of its audio processor.
    pub cid: [u8; 16],
    pub name: String,
    pub vendor: String,
    /// VST3 sub-categories, `|`-separated (`Fx|EQ`).
    pub category: String,
}

impl PluginInfo {
    /// One line of `vesper-vst-host --scan`'s output (tab-separated cid,
    /// name, vendor, sub-categories).
    pub fn parse(path: &Path, line: &str) -> Option<Self> {
        let mut f = line.split('\t');
        let cid = hex_decode(f.next()?.as_bytes())?;
        let name = f.next()?.to_owned();
        Some(Self {
            path: path.to_owned(),
            cid,
            name,
            vendor: f.next().unwrap_or_default().to_owned(),
            category: f.next().unwrap_or_default().to_owned(),
        })
    }

    /// The scan line for this plugin.
    pub fn line(&self) -> String {
        let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
        format!(
            "{}\t{}\t{}\t{}",
            hex(&self.cid),
            clean(&self.name),
            clean(&self.vendor),
            clean(&self.category)
        )
    }

    /// An effect, not an instrument.
    pub fn is_effect(&self) -> bool {
        let cats: Vec<&str> = self.category.split('|').collect();
        cats.contains(&"Fx") || !cats.contains(&"Instrument")
    }
}

/// The standard Linux VST3 folders: `~/.vst3`, `/usr/lib/vst3`,
/// `/usr/local/lib/vst3`.
pub fn default_dirs() -> Vec<PathBuf> {
    let home = dirs::home_dir().map(|h| h.join(".vst3"));
    home.into_iter()
        .chain(["/usr/lib/vst3".into(), "/usr/local/lib/vst3".into()])
        // A relative $HOME would scan wherever Vesper was started.
        .filter(|d: &PathBuf| d.is_absolute())
        .collect()
}

/// The shared library of a bundle (`Foo.vst3/Contents/<arch>-linux/Foo.so`),
/// or the file itself when `path` is not a folder.
pub fn module_path(path: &Path) -> PathBuf {
    if !path.is_dir() {
        return path.to_owned();
    }
    let stem = path.file_stem().unwrap_or_default();
    let mut so = stem.to_owned();
    so.push(".so");
    path.join("Contents")
        .join(format!("{}-linux", std::env::consts::ARCH))
        .join(so)
}

/// `.vst3` bundles under `dir`, looking two folders deep for vendor folders.
fn bundles(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.extension().is_some_and(|e| e == "vst3") {
            out.push(path);
        } else if depth > 0 && path.is_dir() {
            bundles(&path, depth - 1, out);
        }
    }
}

fn mtime(path: &Path) -> u64 {
    std::fs::metadata(module_path(path))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// Lists the effects of every bundle in `dirs`, through `cache`.
pub fn scan(host: &HostCommand, dirs: &[PathBuf], cache: &Path) -> Vec<PluginInfo> {
    // Cache lines: path bytes (hex), mtime, then a scan line or nothing.
    if !host.exists() {
        warn!("cannot list VST3 plugins: {:?} not installed", host.program);
        return Vec::new();
    }
    let old = std::fs::read_to_string(cache).unwrap_or_default();
    let cached = |path: &Path, mtime: u64| -> Option<Vec<String>> {
        let key = format!("{}\t{mtime}\t", hex(path.as_os_str().as_bytes()));
        let lines: Vec<String> = old
            .lines()
            .filter_map(|l| l.strip_prefix(&key).map(str::to_owned))
            .collect();
        (!lines.is_empty()).then_some(lines)
    };
    let mut all = Vec::new();
    for dir in dirs {
        bundles(dir, 2, &mut all);
    }
    let mut text = String::new();
    let mut found = Vec::new();
    for path in all {
        let mtime = mtime(&path);
        let lines = cached(&path, mtime).unwrap_or_else(|| match scan_one(host, &path) {
            Ok(lines) if !lines.is_empty() => lines,
            Ok(_) => vec![String::new()],
            Err(e) => {
                warn!("skipping VST3 bundle {path:?}: {e}");
                vec![String::new()]
            }
        });
        for line in lines {
            text += &format!("{}\t{mtime}\t{line}\n", hex(path.as_os_str().as_bytes()));
            found.extend(PluginInfo::parse(&path, &line).filter(PluginInfo::is_effect));
        }
    }
    if text != old {
        if let Some(parent) = cache.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(cache, text) {
            warn!("cannot write the VST3 scan cache {cache:?}: {e}");
        }
    }
    found
}

/// Runs `host --scan path`: its stdout lines, or why it failed.
fn scan_one(host: &HostCommand, path: &Path) -> Result<Vec<String>, String> {
    let mut cmd = host.command();
    cmd.arg("--scan").arg(path).stdout(Stdio::piped());
    // SAFETY: an async-signal-safe syscall between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            crate::client::close_others(3);
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start {:?}: {e}", host.program))?;
    let mut stdout = child.stdout.take().expect("piped");
    let (tx, rx) = mpsc::channel();
    // Detached: a helper the plugin started may keep the pipe open.
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        let _ = tx.send(out);
    });
    let status = wait_timeout(&mut child, SCAN_TIMEOUT);
    match status {
        Some(s) if s.success() => {
            let out = rx
                .recv_timeout(Duration::from_secs(1))
                .map_err(|_| "output not closed".to_owned())?;
            Ok(String::from_utf8_lossy(&out)
                .lines()
                .map(str::to_owned)
                .collect())
        }
        Some(s) => Err(format!("scan failed ({s})")),
        None => Err("scan timed out".into()),
    }
}

/// Waits for `child` at most `timeout`, then kills it (None).
fn wait_timeout(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let until = Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_lines_round_trip_and_filter_instruments() {
        let p = PluginInfo {
            path: "/usr/lib/vst3/A.vst3".into(),
            cid: [0xab; 16],
            name: "Para\tEQ".into(),
            vendor: "LSP".into(),
            category: "Fx|EQ".into(),
        };
        let back = PluginInfo::parse(&p.path, &p.line()).unwrap();
        assert_eq!(back.name, "Para EQ");
        assert!(back.is_effect());
        let synth = PluginInfo {
            category: "Instrument|Synth".into(),
            ..p.clone()
        };
        assert!(!synth.is_effect());
        assert!(PluginInfo::parse(&p.path, "").is_none());
        assert!(PluginInfo::parse(&p.path, "zz\tname").is_none());
    }

    #[test]
    fn bundles_are_found_in_vendor_folders() {
        let dir = std::env::temp_dir().join(format!("vesper-vst-scan-{}", std::process::id()));
        for d in ["A.vst3/Contents", "Vendor/B.vst3", "Vendor/notes"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let mut found = Vec::new();
        bundles(&dir, 2, &mut found);
        assert_eq!(found, [dir.join("A.vst3"), dir.join("Vendor/B.vst3")]);
        assert_eq!(
            module_path(&dir.join("A.vst3")),
            dir.join(format!(
                "A.vst3/Contents/{}-linux/A.so",
                std::env::consts::ARCH
            ))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
