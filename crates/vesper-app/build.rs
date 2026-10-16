use std::path::Path;
use std::process::Command;

fn main() {
    // Pin the widget style so the look does not depend on which toolkits are
    // installed (only `Palette.color-scheme` is used from std-widgets).
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config)
        .expect("failed to compile the Slint UI");
    git_revision();
}

/// `VESPER_GIT_REV` = "r<count>.g<hash>", as in the Arch pkgver, when built
/// from Vesper's own git checkout; unset otherwise (tarball, no git).
fn git_revision() {
    // Only our own repository, not one the sources were unpacked into.
    if !Path::new("../../.git").exists() {
        return;
    }
    let git = |args: &[&str]| {
        let out = Command::new("git").args(args).output().ok()?;
        let text = String::from_utf8(out.stdout).ok()?;
        let text = text.trim();
        (out.status.success() && !text.is_empty()).then(|| text.to_owned())
    };
    // Rebuilt when HEAD moves (checkout, commit), not on every build.
    for path in ["HEAD", "packed-refs"] {
        if let Some(p) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    if let Some(p) = git(&["symbolic-ref", "-q", "HEAD"])
        .and_then(|r| git(&["rev-parse", "--git-path", &r]))
        .filter(|p| Path::new(p).exists())
    {
        println!("cargo:rerun-if-changed={p}");
    }
    if let (Some(count), Some(hash)) = (
        git(&["rev-list", "--count", "HEAD"]),
        git(&["rev-parse", "--short=7", "HEAD"]),
    ) {
        println!("cargo:rustc-env=VESPER_GIT_REV=r{count}.g{hash}");
    }
}
