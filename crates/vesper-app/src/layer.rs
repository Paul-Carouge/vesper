//! Per-user install of the game capture Vulkan layer (`vesper-vklayer`),
//! same layout as `crates/vesper-vklayer/install.sh`:
//! `$XDG_DATA_HOME/vesper/libvesper_vklayer.so` (and
//! `libvesper_vklayer32.so`, the i686 build, when it was built) with one
//! implicit layer manifest per architecture in
//! `$XDG_DATA_HOME/vulkan/implicit_layer.d/`. Only the settings toggle
//! installs it: it then loads into every Vulkan app of the user.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

const LIB: &str = "libvesper_vklayer.so";
/// Name of the installed copy of the 32-bit build (the build itself is
/// also called `LIB`).
const LIB32: &str = "libvesper_vklayer32.so";
const MANIFEST: &str = "vulkan/implicit_layer.d/vesper_capture_64.json";
const MANIFEST32: &str = "vulkan/implicit_layer.d/vesper_capture_32.json";
/// Single manifest of versions before the 32-bit layer: replaced on install.
const LEGACY_MANIFEST: &str = "vulkan/implicit_layer.d/vesper_capture.json";
/// Where a dev build puts the 32-bit layer, relative to `target/`.
const I686_TARGET: &str = "i686-unknown-linux-gnu";

/// The layer libraries built with this binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shipped {
    pub lib: PathBuf,
    /// The i686 build, for 32-bit games; optional (needs the i686 target).
    pub lib32: Option<PathBuf>,
}

/// `$XDG_DATA_HOME` (or `~/.local/share`).
pub fn data_home() -> Option<PathBuf> {
    dirs::data_dir().filter(|d| d.is_absolute())
}

/// Whether the layer is installed under `data` (also by an older version).
pub fn installed(data: &Path) -> bool {
    data.join(MANIFEST).is_file() || data.join(LEGACY_MANIFEST).is_file()
}

/// The layer built with this binary: next to it (`target/<profile>/`, the
/// 32-bit one in `target/i686-unknown-linux-gnu/<profile>/`), or in
/// `../lib/vesper/` and `../lib32/vesper/` (e.g. `~/.local/bin/vesper` and
/// `~/.local/lib/vesper/libvesper_vklayer.so`, or the Arch package's
/// `/usr/bin/vesper` and `/usr/lib/vesper/libvesper_vklayer.so`).
pub fn shipped() -> Option<Shipped> {
    shipped_beside(&std::env::current_exe().ok()?)
}

fn shipped_beside(exe: &Path) -> Option<Shipped> {
    let dir = exe.parent()?;
    let lib = [dir.join(LIB), dir.join("../lib/vesper").join(LIB)]
        .into_iter()
        .find(|p| p.is_file())?;
    let mut candidates = vec![dir.join("../lib32/vesper").join(LIB)];
    if let Some(profile) = dir.file_name() {
        candidates.push(dir.join("..").join(I686_TARGET).join(profile).join(LIB));
    }
    let lib32 = candidates.into_iter().find(|p| p.is_file());
    Some(Shipped { lib, lib32 })
}

fn manifest(lib: &str, name: &str, arch: &str) -> String {
    format!(
        r#"{{
    "file_format_version": "1.2.1",
    "layer": {{
        "name": "{name}",
        "type": "GLOBAL",
        "library_path": "{}",
        "library_arch": "{arch}",
        "api_version": "1.4.0",
        "implementation_version": "1",
        "description": "Vesper game capture (dormant until Vesper records)",
        "disable_environment": {{
            "VESPER_CAPTURE_DISABLE": "1"
        }}
    }}
}}
"#,
        lib.replace('\\', "\\\\").replace('"', "\\\"")
    )
}

/// Copies `src` to `dest` and writes `manifest_path` pointing at it. Both
/// are written aside and renamed: running games keep the old library
/// mapped, and the loader never reads a partial manifest.
fn install_one(
    src: &Path,
    dest: &Path,
    manifest_path: &Path,
    json: impl Fn(&str) -> String,
) -> io::Result<()> {
    let dest_str = dest
        .to_str()
        .filter(|s| !s.chars().any(char::is_control))
        .ok_or_else(|| io::Error::other("the data folder path is not plain text"))?;
    std::fs::create_dir_all(dest.parent().expect("has a parent"))?;
    std::fs::create_dir_all(manifest_path.parent().expect("has a parent"))?;
    let tmp = dest.with_extension("so.tmp");
    std::fs::copy(src, &tmp)?;
    std::fs::rename(&tmp, dest)?;
    let tmp = manifest_path.with_extension("json.tmp");
    std::fs::File::create(&tmp)?.write_all(json(dest_str).as_bytes())?;
    std::fs::rename(&tmp, manifest_path)
}

/// Copies the layer (both architectures when shipped) into `data` and
/// writes their manifests. A 32-bit layer installed earlier but no longer
/// shipped is removed.
pub fn install(data: &Path, shipped: &Shipped) -> io::Result<()> {
    if !data.is_absolute() {
        return Err(io::Error::other("the data folder must be an absolute path"));
    }
    let dir = data.join("vesper");
    install_one(&shipped.lib, &dir.join(LIB), &data.join(MANIFEST), |l| {
        manifest(l, "VK_LAYER_VESPER_capture", "64")
    })?;
    // The new 64-bit manifest is in place: the old one would load it twice.
    remove(&[data.join(LEGACY_MANIFEST)])?;
    match &shipped.lib32 {
        Some(lib32) => install_one(lib32, &dir.join(LIB32), &data.join(MANIFEST32), |l| {
            manifest(l, "VK_LAYER_VESPER_capture_32", "32")
        }),
        None => remove(&[data.join(MANIFEST32), dir.join(LIB32)]),
    }
}

/// Removes files, missing ones being fine.
fn remove(paths: &[PathBuf]) -> io::Result<()> {
    for path in paths {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Removes the manifests first (new Vulkan apps stop loading the layer),
/// then the libraries. Missing files are fine.
pub fn uninstall(data: &Path) -> io::Result<()> {
    let dir = data.join("vesper");
    let mut paths = Vec::new();
    for m in [MANIFEST, MANIFEST32, LEGACY_MANIFEST] {
        let m = data.join(m);
        paths.extend([m.with_extension("json.tmp"), m]);
    }
    for lib in [LIB, LIB32] {
        let lib = dir.join(lib);
        paths.extend([lib.with_extension("so.tmp"), lib]);
    }
    remove(&paths)?;
    // Only if empty: the folder may hold other files.
    let _ = std::fs::remove_dir(dir);
    Ok(())
}

/// Whether the installed layer differs from the shipped one (an updated
/// build, a 32-bit layer added or gone, or an install by an older version).
pub fn outdated(data: &Path, shipped: &Shipped) -> bool {
    let dir = data.join("vesper");
    let same = |installed: &Path, built: Option<&PathBuf>| {
        std::fs::read(installed).ok() == built.and_then(|b| std::fs::read(b).ok())
    };
    data.join(LEGACY_MANIFEST).exists()
        || !data.join(MANIFEST).is_file()
        || !same(&dir.join(LIB), Some(&shipped.lib))
        || !same(&dir.join(LIB32), shipped.lib32.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vesper-layer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn only64(lib: &Path) -> Shipped {
        Shipped {
            lib: lib.to_owned(),
            lib32: None,
        }
    }

    #[test]
    fn install_then_uninstall() {
        let root = temp("cycle");
        let data = root.join("data \"quoted\"");
        let lib = root.join("build.so");
        std::fs::write(&lib, b"v1").unwrap();
        assert!(!installed(&data));

        install(&data, &only64(&lib)).unwrap();
        assert!(installed(&data));
        assert!(!outdated(&data, &only64(&lib)));
        let json = std::fs::read_to_string(data.join(MANIFEST)).unwrap();
        let dest = data.join("vesper").join(LIB);
        let escaped = dest.to_str().unwrap().replace('"', "\\\"");
        assert!(
            json.contains(&format!(r#""library_path": "{escaped}""#)),
            "{json}"
        );
        assert!(json.contains(r#""library_arch": "64""#), "{json}");
        assert!(!data.join(MANIFEST32).exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"v1");

        std::fs::write(&lib, b"v2").unwrap();
        assert!(outdated(&data, &only64(&lib)));
        install(&data, &only64(&lib)).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"v2");
        let leftovers: Vec<_> = std::fs::read_dir(data.join("vesper")).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temporary file left");

        uninstall(&data).unwrap();
        assert!(!installed(&data));
        assert!(!dest.exists() && !data.join("vesper").exists());
        uninstall(&data).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn installs_the_32_bit_layer_beside() {
        let root = temp("both");
        let data = root.join("data");
        let (lib, lib32) = (root.join("64.so"), root.join("32.so"));
        std::fs::write(&lib, b"64").unwrap();
        std::fs::write(&lib32, b"32").unwrap();
        let both = Shipped {
            lib: lib.clone(),
            lib32: Some(lib32.clone()),
        };
        assert!(outdated(&data, &both));
        install(&data, &both).unwrap();
        assert!(!outdated(&data, &both));
        let json = std::fs::read_to_string(data.join(MANIFEST32)).unwrap();
        assert!(json.contains(r#""library_arch": "32""#), "{json}");
        assert!(json.contains("VK_LAYER_VESPER_capture_32"), "{json}");
        assert_eq!(
            std::fs::read(data.join("vesper").join(LIB32)).unwrap(),
            b"32"
        );
        // A build without the 32-bit layer: offered as an update, then removed.
        assert!(outdated(&data, &only64(&lib)));
        install(&data, &only64(&lib)).unwrap();
        assert!(!data.join(MANIFEST32).exists());
        assert!(!data.join("vesper").join(LIB32).exists());
        install(&data, &both).unwrap();
        uninstall(&data).unwrap();
        assert!(!installed(&data) && !data.join(MANIFEST32).exists());
        assert!(!data.join("vesper").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replaces_the_legacy_manifest() {
        let root = temp("legacy");
        let data = root.join("data");
        let lib = root.join("build.so");
        std::fs::write(&lib, b"v1").unwrap();
        let legacy = data.join(LEGACY_MANIFEST);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"{}").unwrap();
        assert!(installed(&data));
        assert!(outdated(&data, &only64(&lib)));
        install(&data, &only64(&lib)).unwrap();
        assert!(!legacy.exists() && installed(&data));
        std::fs::write(&legacy, b"{}").unwrap();
        uninstall(&data).unwrap();
        assert!(!legacy.exists() && !installed(&data));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finds_the_packaged_layer() {
        let root = temp("usr");
        let exe = root.join("usr/bin/vesper");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        assert_eq!(shipped_beside(&exe), None);
        let lib = root.join("usr/lib/vesper").join(LIB);
        std::fs::create_dir_all(lib.parent().unwrap()).unwrap();
        std::fs::write(&lib, b"v1").unwrap();
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        let found = shipped_beside(&exe).unwrap();
        assert_eq!(canon(&found.lib), canon(&lib));
        assert_eq!(found.lib32, None);
        let lib32 = root.join("usr/lib32/vesper").join(LIB);
        std::fs::create_dir_all(lib32.parent().unwrap()).unwrap();
        std::fs::write(&lib32, b"v1").unwrap();
        let found = shipped_beside(&exe).unwrap();
        assert_eq!(canon(&found.lib32.unwrap()), canon(&lib32));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finds_the_dev_build_32_bit_layer() {
        let root = temp("target");
        let exe = root.join("target/release/vesper");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(exe.with_file_name(LIB), b"64").unwrap();
        let lib32 = root.join("target/i686-unknown-linux-gnu/release").join(LIB);
        std::fs::create_dir_all(lib32.parent().unwrap()).unwrap();
        std::fs::write(&lib32, b"32").unwrap();
        let found = shipped_beside(&exe).unwrap();
        assert_eq!(std::fs::read(found.lib32.unwrap()).unwrap(), b"32");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_relative_or_control_char_paths() {
        let root = temp("bad");
        let lib = root.join("build.so");
        std::fs::write(&lib, b"v1").unwrap();
        assert!(install(Path::new("relative"), &only64(&lib)).is_err());
        assert!(install(&root.join("a\nb"), &only64(&lib)).is_err());
        assert!(!installed(&root.join("a\nb")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_library_installs_nothing() {
        let root = temp("missing");
        assert!(install(&root, &only64(&root.join("nope.so"))).is_err());
        assert!(!installed(&root));
        std::fs::remove_dir_all(root).unwrap();
    }
}
