//! "Start with the session": an XDG autostart entry,
//! `$XDG_CONFIG_HOME/autostart/<app id>.desktop`, that starts Vesper hidden
//! in the tray. Only the settings switch writes or removes it; the file is the
//! only record of the setting.

use std::path::{Path, PathBuf};

use crate::shortcuts::APP_ID;

/// The entry under the config folder `config` (`$XDG_CONFIG_HOME`).
fn path_in(config: &Path) -> PathBuf {
    config.join("autostart").join(format!("{APP_ID}.desktop"))
}

/// Whether the entry exists and is not disabled (`Hidden=true`, which the
/// desktop's own autostart settings may write).
pub fn enabled() -> bool {
    dirs::config_dir().is_some_and(|d| enabled_in(&d))
}

fn enabled_in(config: &Path) -> bool {
    std::fs::read_to_string(path_in(config))
        .is_ok_and(|text| !text.lines().any(|l| l.trim() == "Hidden=true"))
}

/// Writes (atomically) or removes the entry.
pub fn set(on: bool) -> std::io::Result<()> {
    let config = dirs::config_dir().ok_or(std::io::ErrorKind::NotFound)?;
    let program = if in_path("vesper") {
        "vesper".to_owned()
    } else {
        // E.g. run from a build folder: start that binary.
        exec_quote(&std::env::current_exe()?.to_string_lossy())
    };
    set_in(&config, on, &program)
}

fn set_in(config: &Path, on: bool, program: &str) -> std::io::Result<()> {
    let path = path_in(config);
    if on {
        crate::prefs::write_atomically(&path, entry(program).as_bytes())
    } else {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

fn entry(program: &str) -> String {
    format!(
        "[Desktop Entry]
Type=Application
Name=Vesper
Comment=Record your screen and save replays
Exec={program} --background
Icon={APP_ID}
Terminal=false
X-GNOME-Autostart-enabled=true
"
    )
}

/// Whether `name` is an executable file in a `PATH` folder.
fn in_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join(name))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

/// `program` as one quoted `Exec` argument: `"`, `` ` ``, `$` and `\` are
/// backslash-escaped inside the quotes, `%` is doubled, then the value's own
/// string escaping doubles every backslash (Desktop Entry spec).
fn exec_quote(program: &str) -> String {
    let mut quoted = String::from("\"");
    for c in program.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted.replace('\\', "\\\\")
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
        assert!(!enabled_in(&config));
        set_in(&config, true, "vesper").unwrap();
        assert!(enabled_in(&config));
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("[Desktop Entry]\n"));
        assert!(text.contains("\nExec=vesper --background\n"));
        set_in(&config, true, "vesper").unwrap();
        assert!(!file.with_extension("tmp").exists());
        // Turned off in the desktop's settings.
        std::fs::write(&file, text + "Hidden=true\n").unwrap();
        assert!(!enabled_in(&config));
        set_in(&config, false, "vesper").unwrap();
        assert!(!file.exists());
        // Removing twice is fine.
        set_in(&config, false, "vesper").unwrap();
        std::fs::remove_dir_all(config).unwrap();
    }

    #[test]
    fn exec_paths_are_quoted() {
        assert_eq!(exec_quote("/opt/v/vesper"), "\"/opt/v/vesper\"");
        assert_eq!(
            exec_quote("/a b/100% \"x\"/$y`z`\\w"),
            "\"/a b/100%% \\\\\"x\\\\\"/\\\\$y\\\\`z\\\\`\\\\\\\\w\""
        );
    }
}
