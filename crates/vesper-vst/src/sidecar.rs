//! A plugin instance's settings, kept next to the settings file rather than
//! in it (states can be big): `<dir>/<id>.conf`, `key=value` lines, the
//! states in base64. The filter list only holds `vst3:id=<id>`.
//!
//! The folder is private (0700, files 0600): states may hold anything a
//! plugin keeps (license keys, sample paths). Ids are never reused, so a
//! late write of a removed instance cannot land on a new one.

use std::ffi::OsStr;
use std::fs::{DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sidecar {
    /// The `.vst3` bundle.
    pub path: PathBuf,
    /// Class id of the plugin's audio processor.
    pub cid: [u8; 16],
    /// Shown until the plugin is loaded.
    pub name: String,
    pub component: Vec<u8>,
    pub controller: Vec<u8>,
}

pub fn file(dir: &Path, id: u32) -> PathBuf {
    dir.join(format!("{id}.conf"))
}

/// Ids with a settings file in `dir`.
pub fn ids(dir: &Path) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            name.to_str()?.strip_suffix(".conf")?.parse().ok()
        })
        .collect()
}

impl Sidecar {
    pub fn load(dir: &Path, id: u32) -> io::Result<Self> {
        Self::parse(&std::fs::read(file(dir, id))?)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad plugin settings"))
    }

    fn parse(text: &[u8]) -> Option<Self> {
        let mut out = Self::default();
        let mut cid = None;
        for line in text.split(|&b| b == b'\n') {
            let Some(eq) = line.iter().position(|&b| b == b'=') else {
                continue;
            };
            let (key, value) = (&line[..eq], &line[eq + 1..]);
            match key {
                b"path" => out.path = PathBuf::from(OsStr::from_bytes(value)),
                b"cid" => cid = hex_decode(value),
                b"name" => out.name = String::from_utf8_lossy(value).into_owned(),
                b"component" => out.component = base64_decode(value)?,
                b"controller" => out.controller = base64_decode(value)?,
                _ => {}
            }
        }
        out.cid = cid?;
        (!out.path.as_os_str().is_empty()).then_some(out)
    }

    fn text(&self) -> io::Result<Vec<u8>> {
        let path = self.path.as_os_str().as_bytes();
        if path.contains(&b'\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "plugin path with a newline",
            ));
        }
        let mut out = b"path=".to_vec();
        out.extend_from_slice(path);
        out.extend_from_slice(format!("\ncid={}\n", hex(&self.cid)).as_bytes());
        let name = self.name.replace(['\n', '\r'], " ");
        out.extend_from_slice(format!("name={name}\n").as_bytes());
        out.extend_from_slice(format!("component={}\n", base64(&self.component)).as_bytes());
        out.extend_from_slice(format!("controller={}\n", base64(&self.controller)).as_bytes());
        Ok(out)
    }

    /// Writes the file atomically (a crash leaves the old one).
    pub fn save(&self, dir: &Path, id: u32) -> io::Result<()> {
        write_private(dir, &format!("{id}.conf"), &self.text()?)
    }
}

/// Creates `dir` private (0700), or makes it so.
fn private_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Writes `dir/name` (0600) through a synced temporary file.
fn write_private(dir: &Path, name: &str, data: &[u8]) -> io::Result<()> {
    private_dir(dir)?;
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(tmp, dir.join(name))
}

/// Deletes the temporary files a crash left in `dir`.
pub fn clean_temporary(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') && name.ends_with(".tmp") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// A new instance id: above every id ever handed out in `dir` (a counter
/// kept in `dir/next-id`) and every settings file there.
pub fn next_id(dir: &Path) -> io::Result<u32> {
    let counter = std::fs::read_to_string(dir.join("next-id"))
        .ok()
        .and_then(|t| t.trim().parse::<u32>().ok())
        .unwrap_or(1);
    let above_files = ids(dir)
        .into_iter()
        .max()
        .map_or(1, |m| m.saturating_add(1));
    let id = counter.max(above_files).max(1);
    if id == u32::MAX {
        return Err(io::Error::other("no instance id left"));
    }
    write_private(dir, "next-id", format!("{}\n", id + 1).as_bytes())?;
    Ok(id)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn hex_decode(text: &[u8]) -> Option<[u8; 16]> {
    let text = std::str::from_utf8(text).ok()?.trim();
    if text.len() != 32 {
        return None;
    }
    let mut out = [0; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                B64[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

pub fn base64_decode(text: &[u8]) -> Option<Vec<u8>> {
    let text = text.trim_ascii();
    if !text.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for (c, chunk) in text.chunks(4).enumerate() {
        let last = c == text.len() / 4 - 1;
        let pad = chunk.iter().rev().take_while(|&&b| b == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &b in &chunk[..4 - pad] {
            let v = B64.iter().position(|&x| x == b)? as u32;
            n = n << 6 | v;
        }
        n <<= 6 * pad as u32;
        out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard() {
        for (raw, text) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
            (&[0xff, 0xfe, 0x00], "//4A"),
        ] {
            assert_eq!(base64(raw), text);
            assert_eq!(base64_decode(text.as_bytes()).as_deref(), Some(raw));
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(base64_decode(base64(&all).as_bytes()), Some(all));
        for bad in ["Zg=", "Z===", "Zg==Zg==", "Zm9!", "===="] {
            assert_eq!(base64_decode(bad.as_bytes()), None, "{bad}");
        }
    }

    #[test]
    fn sidecar_round_trips() {
        let dir = std::env::temp_dir().join(format!("vesper-vst-sidecar-{}", std::process::id()));
        let s = Sidecar {
            path: PathBuf::from(OsStr::from_bytes(b"/opt/My \xff Plugin.vst3")),
            cid: *b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\xff",
            name: "My\nPlugin".into(),
            component: vec![7; 1000],
            controller: Vec::new(),
        };
        s.save(&dir, 12).unwrap();
        let back = Sidecar::load(&dir, 12).unwrap();
        assert_eq!(back.name, "My Plugin");
        assert_eq!(
            back,
            Sidecar {
                name: back.name.clone(),
                ..s.clone()
            }
        );
        assert_eq!(ids(&dir), [12]);
        std::fs::write(file(&dir, 13), "name=x\n").unwrap();
        assert!(Sidecar::load(&dir, 13).is_err(), "no path nor cid");
        let bad_path = Sidecar {
            path: "/a\nb".into(),
            ..s
        };
        assert!(bad_path.save(&dir, 14).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn files_are_private_and_ids_never_reused() {
        let dir = std::env::temp_dir().join(format!("vesper-vst-private-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let s = Sidecar {
            path: "/a.vst3".into(),
            ..Sidecar::default()
        };
        let a = next_id(&dir).unwrap();
        s.save(&dir, a).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&file(&dir, a)), 0o600);
        std::fs::remove_file(file(&dir, a)).unwrap();
        let b = next_id(&dir).unwrap();
        assert!(b > a, "{a} reused");
        // A settings file above the counter (copied in) is not clobbered.
        s.save(&dir, b + 10).unwrap();
        assert_eq!(next_id(&dir).unwrap(), b + 11);
        // Leftovers of a crash mid-save.
        std::fs::write(dir.join(".3.conf.tmp"), "x").unwrap();
        clean_temporary(&dir);
        assert!(!dir.join(".3.conf.tmp").exists());
        assert!(file(&dir, b + 10).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
