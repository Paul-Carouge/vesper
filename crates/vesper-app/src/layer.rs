//! Per-user install of the game capture Vulkan layer (`vesper-vklayer`),
//! same layout as `crates/vesper-vklayer/install.sh`:
//! `$XDG_DATA_HOME/vesper/libvesper_vklayer.so` and its implicit layer
//! manifest in `$XDG_DATA_HOME/vulkan/implicit_layer.d/`. Only the settings
//! toggle installs it: it then loads into every Vulkan app of the user.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

const LIB: &str = "libvesper_vklayer.so";
const MANIFEST: &str = "vulkan/implicit_layer.d/vesper_capture.json";

/// `$XDG_DATA_HOME` (or `~/.local/share`).
pub fn data_home() -> Option<PathBuf> {
    dirs::data_dir().filter(|d| d.is_absolute())
}

/// Whether the layer is installed under `data`.
pub fn installed(data: &Path) -> bool {
    data.join(MANIFEST).is_file()
}

/// The layer built with this binary: next to it (`target/<profile>/`), or in
/// `../lib/vesper/` (e.g. `~/.local/bin/vesper` and
/// `~/.local/lib/vesper/libvesper_vklayer.so`).
pub fn shipped() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [dir.join(LIB), dir.join("../lib/vesper").join(LIB)]
        .into_iter()
        .find(|p| p.is_file())
}

/// Copies `lib` into `data` and writes the manifest pointing at the copy.
/// Both are written aside and renamed: running games keep the old library
/// mapped, and the loader never reads a partial manifest.
pub fn install(data: &Path, lib: &Path) -> io::Result<()> {
    if !data.is_absolute() {
        return Err(io::Error::other("the data folder must be an absolute path"));
    }
    let dest = data.join("vesper").join(LIB);
    let dest_str = dest
        .to_str()
        .filter(|s| !s.chars().any(char::is_control))
        .ok_or_else(|| io::Error::other("the data folder path is not plain text"))?;
    let manifest = data.join(MANIFEST);
    std::fs::create_dir_all(dest.parent().expect("has a parent"))?;
    std::fs::create_dir_all(manifest.parent().expect("has a parent"))?;
    let tmp = dest.with_extension("so.tmp");
    std::fs::copy(lib, &tmp)?;
    std::fs::rename(&tmp, &dest)?;
    let json = format!(
        r#"{{
    "file_format_version": "1.2.0",
    "layer": {{
        "name": "VK_LAYER_VESPER_capture",
        "type": "GLOBAL",
        "library_path": "{}",
        "api_version": "1.4.0",
        "implementation_version": "1",
        "description": "Vesper game capture (dormant until Vesper records)",
        "disable_environment": {{
            "VESPER_CAPTURE_DISABLE": "1"
        }}
    }}
}}
"#,
        dest_str.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let tmp = manifest.with_extension("json.tmp");
    std::fs::File::create(&tmp)?.write_all(json.as_bytes())?;
    std::fs::rename(&tmp, &manifest)
}

/// Removes the manifest first (new Vulkan apps stop loading the layer), then
/// the library. Missing files are fine.
pub fn uninstall(data: &Path) -> io::Result<()> {
    let manifest = data.join(MANIFEST);
    let dest = data.join("vesper").join(LIB);
    for path in [
        manifest.with_extension("json.tmp"),
        manifest,
        dest.with_extension("so.tmp"),
        dest,
    ] {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    // Only if empty: the folder may hold other files.
    let _ = std::fs::remove_dir(data.join("vesper"));
    Ok(())
}

/// Whether the installed library differs from `lib` (an updated build).
pub fn outdated(data: &Path, lib: &Path) -> bool {
    let installed = std::fs::read(data.join("vesper").join(LIB)).ok();
    installed.is_none() || installed != std::fs::read(lib).ok()
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

    #[test]
    fn install_then_uninstall() {
        let root = temp("cycle");
        let data = root.join("data \"quoted\"");
        let lib = root.join("build.so");
        std::fs::write(&lib, b"v1").unwrap();
        assert!(!installed(&data));

        install(&data, &lib).unwrap();
        assert!(installed(&data));
        assert!(!outdated(&data, &lib));
        let json = std::fs::read_to_string(data.join(MANIFEST)).unwrap();
        let dest = data.join("vesper").join(LIB);
        let escaped = dest.to_str().unwrap().replace('"', "\\\"");
        assert!(
            json.contains(&format!(r#""library_path": "{escaped}""#)),
            "{json}"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"v1");

        std::fs::write(&lib, b"v2").unwrap();
        assert!(outdated(&data, &lib));
        install(&data, &lib).unwrap();
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
    fn refuses_relative_or_control_char_paths() {
        let root = temp("bad");
        let lib = root.join("build.so");
        std::fs::write(&lib, b"v1").unwrap();
        assert!(install(Path::new("relative"), &lib).is_err());
        assert!(install(&root.join("a\nb"), &lib).is_err());
        assert!(!installed(&root.join("a\nb")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_library_installs_nothing() {
        let root = temp("missing");
        assert!(install(&root, &root.join("nope.so")).is_err());
        assert!(!installed(&root));
        std::fs::remove_dir_all(root).unwrap();
    }
}
