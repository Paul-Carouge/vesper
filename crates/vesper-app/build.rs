use std::path::Path;
use std::process::Command;

#[path = "src/po.rs"]
mod po;

fn main() {
    // Pin the widget style so the look does not depend on which toolkits are
    // installed (only `Palette.color-scheme` is used from std-widgets).
    // The catalogues of `lang/` are compiled in (no gettext at run time);
    // a string without an explicit context has one entry for all components.
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent".into())
        .with_bundled_translations("lang")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/app.slint", config)
        .expect("failed to compile the Slint UI");
    rust_catalogue();
    git_revision();
}

/// `$OUT_DIR/french.rs`: `fn french(msgid) -> Option<&str>` over the French
/// catalogue's messages without context or plural: the strings Rust shows.
fn rust_catalogue() {
    let path = "lang/fr/LC_MESSAGES/vesper-app.po";
    println!("cargo:rerun-if-changed={path}");
    let text = std::fs::read_to_string(path).expect("failed to read the French catalogue");
    let entries = po::parse(&text).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut code = String::from("fn french(msgid: &str) -> Option<&'static str> {\n");
    code += "    Some(match msgid {\n";
    for e in entries
        .iter()
        .filter(|e| e.context.is_none() && e.plural.is_none() && !e.id.is_empty() && e.translated())
    {
        code += &format!("        {:?} => {:?},\n", e.id, e.strs[0]);
    }
    code += "        _ => return None,\n    })\n}\n";
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("french.rs");
    std::fs::write(out, code).expect("failed to write the Rust catalogue");
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
