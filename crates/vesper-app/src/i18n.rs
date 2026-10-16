//! Translations. The UI's `@tr` strings and the ones Rust shows (notices,
//! notifications, the tray menu, dialog titles) share one catalogue per
//! language, `lang/<lang>/LC_MESSAGES/vesper-app.po`, compiled in: Slint
//! bundles it for the UI and `build.rs` turns it into a table for [`tr`].
//! English is the source language. Placeholders are Slint's: `{}` takes the
//! next argument, `{0}` the first, `{{` and `}}` are braces.

use std::fmt::Display;
use std::sync::atomic::{AtomicBool, Ordering};

include!(concat!(env!("OUT_DIR"), "/french.rs"));

/// Rust strings are shown in French.
static FRENCH: AtomicBool = AtomicBool::new(false);

/// The language setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Language {
    /// The system's (`LANGUAGE`, `LC_ALL`, `LC_MESSAGES`, `LANG`).
    #[default]
    System,
    English,
    French,
}

impl Language {
    /// As in the settings file.
    pub fn id(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::English => "en",
            Self::French => "fr",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        [Self::System, Self::English, Self::French]
            .into_iter()
            .find(|l| l.id() == id.trim())
    }

    /// Its place in the settings' selector.
    pub fn index(self) -> i32 {
        self as i32
    }

    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::English,
            2 => Self::French,
            _ => Self::System,
        }
    }
}

/// `msgid` in the current language. A string missing from the catalogue
/// stays in English.
pub fn tr(msgid: &'static str) -> &'static str {
    if FRENCH.load(Ordering::Relaxed) {
        french(msgid).unwrap_or(msgid)
    } else {
        msgid
    }
}

/// `x` with `digits` decimals and the language's decimal separator.
pub fn decimal(x: f64, digits: usize) -> String {
    let text = format!("{x:.digits$}");
    if FRENCH.load(Ordering::Relaxed) {
        text.replace('.', ",")
    } else {
        text
    }
}

/// `template` with its placeholders replaced, as Slint's `@tr` does.
pub fn fill(template: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut next = 0;
    let mut rest = template;
    while let Some(i) = rest.find(['{', '}']) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            out.push_str(&tail[..1]);
            rest = &tail[2..];
            continue;
        }
        let Some(end) = tail.find('}').filter(|_| tail.starts_with('{')) else {
            out.push_str(&tail[..1]);
            rest = &tail[1..];
            continue;
        };
        let index = match tail[1..end].parse::<usize>() {
            Ok(n) => n,
            Err(_) if end == 1 => {
                next += 1;
                next - 1
            }
            Err(_) => {
                out.push_str(&tail[..=end]);
                rest = &tail[end + 1..];
                continue;
            }
        };
        if let Some(arg) = args.get(index) {
            out.push_str(&arg.to_string());
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Shows the UI and the Rust strings in `language`. On the UI thread, once
/// the window exists; applies at once.
pub fn apply(language: Language) {
    let french = match language {
        Language::System => system_is_french(|key| std::env::var(key).ok()),
        Language::English => false,
        Language::French => true,
    };
    FRENCH.store(french, Ordering::Relaxed);
    if let Err(e) = slint::select_bundled_translation(if french { "fr" } else { "" }) {
        log!("could not select the translation: {e}");
    }
}

/// Whether the locale `var` (an environment lookup) asks for French, as
/// gettext reads it: `LANGUAGE`'s first language Vesper has (English or
/// French), else the first set of `LC_ALL`, `LC_MESSAGES` and `LANG`.
fn system_is_french(var: impl Fn(&str) -> Option<String>) -> bool {
    let set = |key: &str| var(key).filter(|v| !v.is_empty());
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"].into_iter().find_map(set);
    let french = |l: &str| l == "fr" || l.starts_with("fr_") || l.starts_with("fr.");
    let english = |l: &str| l == "en" || l.starts_with("en_") || l.starts_with("en.");
    // gettext ignores LANGUAGE under the C locale. Unlike gettext, LANGUAGE
    // also counts when no locale variable is set at all (gettext reads that
    // as C): a session that only sets LANGUAGE still gets its language.
    let c = |l: &str| l == "C" || l == "POSIX" || l.starts_with("C.");
    if !locale.as_deref().is_some_and(c)
        && let Some(languages) = set("LANGUAGE")
        && let Some(l) = languages.split(':').find(|l| french(l) || english(l))
    {
        return french(l);
    }
    locale.as_deref().is_some_and(french)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::po::{self, Entry};
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    #[test]
    fn placeholders_are_filled_like_slint() {
        assert_eq!(fill("{} of {}", &[&"a", &2]), "a of 2");
        assert_eq!(fill("{1} before {0}", &[&"a", &"b"]), "b before a");
        assert_eq!(fill("{{}} {}", &[&1]), "{} 1");
        assert_eq!(fill("no args", &[]), "no args");
        assert_eq!(fill("missing {}", &[]), "missing ");
    }

    #[test]
    fn the_system_language_follows_gettext() {
        let fr = |vars: &[(&str, &str)]| {
            let vars: BTreeMap<_, _> = vars.iter().copied().collect();
            system_is_french(|k| vars.get(k).map(|v| v.to_string()))
        };
        assert!(fr(&[("LANG", "fr_FR.UTF-8")]));
        assert!(fr(&[("LANG", "fr_CA.UTF-8")]));
        assert!(!fr(&[("LANG", "en_US.UTF-8")]));
        assert!(!fr(&[]));
        assert!(!fr(&[
            ("LANG", "fr_FR.UTF-8"),
            ("LC_MESSAGES", "en_GB.UTF-8")
        ]));
        assert!(fr(&[("LANG", "en_US.UTF-8"), ("LC_ALL", "fr_BE.UTF-8")]));
        assert!(fr(&[("LANG", "en_US.UTF-8"), ("LANGUAGE", "de:fr:en")]));
        assert!(!fr(&[("LANG", "fr_FR.UTF-8"), ("LANGUAGE", "en_US:fr")]));
        // No language Vesper has: the locale decides.
        assert!(fr(&[("LANG", "fr_FR.UTF-8"), ("LANGUAGE", "de")]));
        assert!(!fr(&[("LC_ALL", "C"), ("LANGUAGE", "fr")]));
        assert!(!fr(&[("LANG", "french")]));
    }

    #[test]
    fn language_settings_round_trip() {
        for l in [Language::System, Language::English, Language::French] {
            assert_eq!(Language::from_id(l.id()), Some(l));
            assert_eq!(Language::from_index(l.index()), l);
        }
        assert_eq!(Language::from_id("de"), None);
    }

    /// A message the code asks for: context, msgid and plural.
    type Key = (Option<String>, String, Option<String>);

    /// The end of the string literal `s` starts (after its `"`), and its text.
    fn literal(s: &str) -> Option<(usize, String)> {
        let mut out = String::new();
        let mut chars = s.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => return Some((i + 1, out)),
                '\\' => match chars.next()?.1 {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    c @ ('"' | '\\' | '\'') => out.push(c),
                    // Rust's line continuation: the next line's indent goes.
                    '\n' => {
                        let rest = chars.as_str();
                        let indent = rest.len() - rest.trim_start().len();
                        for _ in rest[..indent].chars() {
                            chars.next();
                        }
                    }
                    _ => return None,
                },
                c => out.push(c),
            }
        }
        None
    }

    /// Skips white space then expects `token`.
    fn after<'a>(s: &'a str, token: &str) -> Option<&'a str> {
        s.trim_start().strip_prefix(token)
    }

    /// The messages of `@tr(...)` in Slint code: `"msg"`, `"ctx" => "msg"`,
    /// `"one" | "many" % n`.
    fn slint_messages(code: &str, found: &mut BTreeSet<Key>) {
        for (at, _) in code.match_indices("@tr(") {
            let s = &code[at + 4..];
            let (end, first) = after(s, "\"").and_then(literal).expect(&code[at..at + 60]);
            let s = &after(s, "\"").unwrap()[end..];
            let (context, id, s) = match after(s, "=>").and_then(|s| after(s, "\"")) {
                Some(rest) => {
                    let (end, id) = literal(rest).unwrap();
                    (Some(first), id, &rest[end..])
                }
                None => (None, first, s),
            };
            let plural = after(s, "|")
                .and_then(|s| after(s, "\""))
                .map(|rest| literal(rest).unwrap().1);
            found.insert((context, id, plural));
        }
    }

    /// The messages of `tr!("...")` in Rust code.
    fn rust_messages(code: &str, found: &mut BTreeSet<Key>) {
        for (at, _) in code.match_indices("tr!(") {
            // Not `include_str!(`.
            let word = code[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_');
            if !word && let Some((_, id)) = after(&code[at + 4..], "\"").and_then(literal) {
                found.insert((None, id, None));
            }
        }
    }

    fn sources(dir: &str, ext: &str) -> Vec<(String, String)> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == ext))
            .collect();
        files.sort();
        files
            .into_iter()
            .map(|p| {
                (
                    p.display().to_string(),
                    std::fs::read_to_string(&p).unwrap(),
                )
            })
            .collect()
    }

    /// The placeholders of `s` (`{}`, `{0}`, `{n}`...), sorted: a
    /// translation may reorder positional ones, never change them.
    fn placeholders(s: &str) -> Vec<String> {
        let s = s.replace("{{", "").replace("}}", "");
        let mut found: Vec<String> = s
            .split('{')
            .skip(1)
            .filter_map(|p| p.split_once('}').map(|(name, _)| name.to_owned()))
            .collect();
        found.sort();
        found
    }

    /// Every string the UI or Rust shows has a French translation (not
    /// fuzzy, plural forms included, the same placeholders), and the
    /// catalogue has nothing else. A failure prints the entries to add.
    #[test]
    fn every_string_has_a_french_translation() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lang/fr/LC_MESSAGES/vesper-app.po");
        let entries = po::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
        let header = entries.iter().find(|e| e.id.is_empty()).expect("no header");
        assert!(
            header.strs[0].contains("Plural-Forms: nplurals=2; plural=(n > 1);"),
            "French plural rule"
        );
        let mut used = BTreeSet::new();
        for (_, code) in sources("ui", "slint") {
            slint_messages(&code, &mut used);
        }
        for (name, code) in sources("src", "rs") {
            // This file's own `tr!(` are examples and test code.
            if !name.ends_with("i18n.rs") {
                rust_messages(&code, &mut used);
            }
        }
        assert!(used.len() > 300, "only {} strings found", used.len());
        let catalogue: BTreeMap<Key, &Entry> = entries
            .iter()
            .filter(|e| !e.id.is_empty())
            .map(|e| ((e.context.clone(), e.id.clone(), e.plural.clone()), e))
            .collect();
        let mut problems = Vec::new();
        for key in &used {
            match catalogue.get(key) {
                None => {
                    let (context, id, plural) = key;
                    let mut po = String::new();
                    if let Some(c) = context {
                        po += &format!("msgctxt {c:?}\n");
                    }
                    po += &format!("msgid {id:?}\n");
                    match plural {
                        Some(p) => {
                            po += &format!("msgid_plural {p:?}\nmsgstr[0] \"\"\nmsgstr[1] \"\"\n")
                        }
                        None => po += "msgstr \"\"\n",
                    }
                    problems.push(format!("missing:\n{po}"));
                }
                Some(e) if !e.translated() => {
                    problems.push(format!("untranslated or fuzzy: {key:?}"))
                }
                Some(e) if e.plural.is_some() && e.strs.len() != 2 => {
                    problems.push(format!("needs 2 plural forms: {key:?}"))
                }
                Some(e) => {
                    let want = placeholders(&e.id);
                    if e.strs.iter().any(|s| placeholders(s) != want) {
                        problems.push(format!("placeholders differ: {key:?}"));
                    }
                }
            }
        }
        for key in catalogue.keys().filter(|k| !used.contains(*k)) {
            problems.push(format!("not used any more: {key:?}"));
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    /// `friendly()` (main.rs) recognizes these recorder errors in English:
    /// translated, it would show the raw error instead of its sentence.
    #[test]
    fn errors_friendly_matches_are_not_translated() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("lang/fr/LC_MESSAGES/vesper-app.po");
        let entries = po::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
        for e in &entries {
            let id = &e.id;
            assert!(
                !id.starts_with("Could not create")
                    && !id.starts_with("Could not open the encoder")
                    && !id.contains("(saved up to there"),
                "{id:?} is matched by friendly(): keep it out of tr!"
            );
        }
    }

    #[test]
    fn rust_strings_are_looked_up() {
        assert_eq!(french("Recording saved"), Some("Enregistrement sauvegardé"));
        assert_eq!(french("not in the catalogue"), None);
    }
}
