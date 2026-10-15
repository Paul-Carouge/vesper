//! "Start with the session": an XDG autostart entry,
//! `$XDG_CONFIG_HOME/autostart/<app id>.desktop`, that starts Vesper hidden
//! in the tray. Only the settings switch writes or removes it.

use std::path::{Path, PathBuf};

use crate::shortcuts::APP_ID;

const ENTRY: &str = "[Desktop Entry]
Type=Application
Name=Vesper
Comment=Record your screen and save replays
Exec=vesper --background
Icon=io.github.PaulCarouge.Vesper
Terminal=false
X-GNOME-Autostart-enabled=true
";

/// The entry under the config folder `config` (`$XDG_CONFIG_HOME`).
fn path_in(config: &Path) -> PathBuf {
    config.join("autostart").join(format!("{APP_ID}.desktop"))
}

/// Whether the entry exists (the user may have removed it elsewhere).
pub fn enabled() -> bool {
    dirs::config_dir().is_some_and(|d| path_in(&d).exists())
}

/// Writes (atomically) or removes the entry.
pub fn set(on: bool) -> std::io::Result<()> {
    let config = dirs::config_dir().ok_or(std::io::ErrorKind::NotFound)?;
    set_in(&config, on)
}

fn set_in(config: &Path, on: bool) -> std::io::Result<()> {
    let path = path_in(config);
    if on {
        crate::prefs::write_atomically(&path, ENTRY.as_bytes())
    } else {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_is_written_and_removed() {
        let config = std::env::temp_dir().join(format!("vesper-autostart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        let file = path_in(&config);
        assert!(file.ends_with("autostart/io.github.PaulCarouge.Vesper.desktop"));
        set_in(&config, true).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("[Desktop Entry]\n"));
        assert!(text.contains("\nExec=vesper --background\n"));
        set_in(&config, true).unwrap();
        assert!(!file.with_extension("tmp").exists());
        set_in(&config, false).unwrap();
        assert!(!file.exists());
        // Removing twice is fine.
        set_in(&config, false).unwrap();
        std::fs::remove_dir_all(config).unwrap();
    }
}
