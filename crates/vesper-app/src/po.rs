//! A reader for the gettext `.po` catalogues in `lang/`: what `build.rs`
//! needs for the Rust strings' table, and the tests to check the catalogue.
//! (Slint compiles the same files for the UI with its own reader.)

/// One message of a catalogue.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub context: Option<String>,
    pub id: String,
    pub plural: Option<String>,
    /// `msgstr`, or `msgstr[0]`, `msgstr[1]`... for a plural.
    pub strs: Vec<String>,
    /// Marked `#, fuzzy`: not used.
    pub fuzzy: bool,
}

impl Entry {
    /// Has a translation that is used: not fuzzy, no empty form.
    pub fn translated(&self) -> bool {
        !self.fuzzy && !self.strs.is_empty() && self.strs.iter().all(|s| !s.is_empty())
    }
}

/// The messages of `text`, the header (empty `msgid`) included. Obsolete
/// (`#~`) messages are left out.
pub fn parse(text: &str) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    let mut entry = Entry::default();
    let mut fuzzy = false;
    // The string the next continuation line extends.
    let mut target: Option<&'static str> = None;
    let mut index = 0;
    let mut started = false;
    for (n, line) in text.lines().enumerate() {
        let n = n + 1;
        let line = line.trim();
        if line.is_empty() {
            target = None;
            continue;
        }
        if line.starts_with("#,") {
            fuzzy |= line.split([',', ' ']).any(|f| f == "fuzzy");
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let (keyword, rest) = match line.find(' ') {
            Some(i) if !line.starts_with('"') => (&line[..i], line[i..].trim()),
            _ => ("", line),
        };
        let value = unquote(rest).map_err(|e| format!("line {n}: {e}"))?;
        let starts_entry = matches!(keyword, "msgctxt")
            || (keyword == "msgid" && (entry.context.is_none() || !entry.id.is_empty()));
        if starts_entry && started {
            if entry.strs.is_empty() {
                return Err(format!("line {n}: the previous message has no msgstr"));
            }
            entries.push(std::mem::take(&mut entry));
        }
        if starts_entry {
            started = true;
            entry.fuzzy = std::mem::take(&mut fuzzy);
        }
        match keyword {
            "msgctxt" => {
                entry.context = Some(value);
                target = Some("ctx");
            }
            "msgid" => {
                entry.id = value;
                target = Some("id");
            }
            "msgid_plural" => {
                entry.plural = Some(value);
                target = Some("plural");
            }
            "msgstr" => {
                entry.strs.push(value);
                index = entry.strs.len() - 1;
                target = Some("str");
            }
            k if k.starts_with("msgstr[") => {
                let i: usize = k["msgstr[".len()..k.len() - 1]
                    .parse()
                    .map_err(|_| format!("line {n}: bad {k}"))?;
                if i != entry.strs.len() {
                    return Err(format!("line {n}: {k} out of order"));
                }
                entry.strs.push(value);
                index = i;
                target = Some("str");
            }
            "" => {
                let s = match target {
                    Some("ctx") => entry.context.as_mut().unwrap(),
                    Some("id") => &mut entry.id,
                    Some("plural") => entry.plural.as_mut().unwrap(),
                    Some(_) => &mut entry.strs[index],
                    None => return Err(format!("line {n}: a string outside a message")),
                };
                s.push_str(&value);
            }
            k => return Err(format!("line {n}: unknown keyword {k}")),
        }
    }
    if started {
        if entry.strs.is_empty() {
            return Err("the last message has no msgstr".into());
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// The text of a `"..."` string with C escapes.
fn unquote(s: &str) -> Result<String, String> {
    let inner = s
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .filter(|_| s.len() >= 2)
        .ok_or_else(|| format!("not a quoted string: {s}"))?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                e => return Err(format!("unknown escape \\{}", e.unwrap_or(' '))),
            },
            '"' => return Err(format!("unescaped quote in {s}")),
            c => out.push(c),
        }
    }
    Ok(out)
}
