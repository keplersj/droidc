//! The unit file format, as `systemd.syntax(7)` describes it.
//!
//! - `[Section]` headers; settings before the first header are an error.
//! - `Key=Value`, with whitespace around the key and the value trimmed.
//! - Lines starting with `#` or `;` are comments; empty lines are ignored.
//! - A line ending in `\` continues on the next line, the backslash becoming a space. Comment lines
//!   inside a continued line are skipped.
//!
//! Values are kept as written: quoting, escapes and specifiers are the business of each setting.

use std::fmt;

/// One `Key=Value` assignment, in file order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub section: String,
    pub key: String,
    pub value: String,
    /// 1-based line of the key (the first line, for a continued value).
    pub line: usize,
}

/// A problem in the file. systemd logs these and carries on, and so does the stand-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntaxWarning {
    pub line: usize,
    pub kind: SyntaxWarningKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyntaxWarningKind {
    /// A setting before any `[Section]` header.
    OutsideSection,
    /// A line that is neither a header, a comment nor `Key=Value`.
    NotAnAssignment,
    /// `[` without a closing `]`, or an empty section name.
    BadSectionHeader,
    /// `=` with nothing before it.
    EmptyKey,
    /// `.include`, removed from systemd; the stand-in doesn't follow it either.
    Include,
}

impl fmt::Display for SyntaxWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            SyntaxWarningKind::OutsideSection => "assignment outside of a section, ignoring",
            SyntaxWarningKind::NotAnAssignment => "missing '=', ignoring line",
            SyntaxWarningKind::BadSectionHeader => "invalid section header, ignoring",
            SyntaxWarningKind::EmptyKey => "empty key, ignoring line",
            SyntaxWarningKind::Include => ".include is not supported, ignoring",
        };
        write!(f, "line {}: {what}", self.line)
    }
}

/// A parsed file: its assignments in order, and what was skipped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub entries: Vec<Entry>,
    /// Section headers in order, including empty sections.
    pub sections: Vec<String>,
    pub warnings: Vec<SyntaxWarning>,
}

/// Parses unit file text. Never fails: problems become [`SyntaxWarning`]s, as in systemd.
pub fn parse(text: &str) -> Parsed {
    let mut out = Parsed::default();
    let mut section: Option<String> = None;
    let mut lines = text.lines().enumerate().map(|(i, l)| (i + 1, l));

    while let Some((number, raw)) = lines.next() {
        let first = raw.trim();
        if first.is_empty() || is_comment(first) {
            continue;
        }

        // Join continuation lines. A trailing backslash becomes a space; comment lines in between
        // are dropped.
        let mut logical = String::new();
        let mut current = first.to_string();
        loop {
            match current.strip_suffix('\\') {
                Some(head) => {
                    logical.push_str(head);
                    logical.push(' ');
                    let mut next = None;
                    for (_, l) in lines.by_ref() {
                        let t = l.trim();
                        if is_comment(t) {
                            continue;
                        }
                        next = Some(t.to_string());
                        break;
                    }
                    match next {
                        Some(t) => current = t,
                        None => break,
                    }
                }
                None => {
                    logical.push_str(&current);
                    break;
                }
            }
        }
        let line = logical.trim();

        if let Some(rest) = line.strip_prefix('[') {
            match rest.strip_suffix(']') {
                Some(name) if !name.is_empty() && !name.contains(['[', ']']) => {
                    out.sections.push(name.to_string());
                    section = Some(name.to_string());
                }
                _ => {
                    out.warnings.push(SyntaxWarning {
                        line: number,
                        kind: SyntaxWarningKind::BadSectionHeader,
                    });
                    // systemd ignores the rest of a section it can't name.
                    section = None;
                }
            }
            continue;
        }

        if line.starts_with(".include") {
            out.warnings.push(SyntaxWarning {
                line: number,
                kind: SyntaxWarningKind::Include,
            });
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            out.warnings.push(SyntaxWarning {
                line: number,
                kind: SyntaxWarningKind::NotAnAssignment,
            });
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            out.warnings.push(SyntaxWarning {
                line: number,
                kind: SyntaxWarningKind::EmptyKey,
            });
            continue;
        }
        let Some(section) = &section else {
            out.warnings.push(SyntaxWarning {
                line: number,
                kind: SyntaxWarningKind::OutsideSection,
            });
            continue;
        };
        out.entries.push(Entry {
            section: section.clone(),
            key: key.to_string(),
            value: value.trim().to_string(),
            line: number,
        });
    }
    out
}

fn is_comment(trimmed: &str) -> bool {
    trimmed.starts_with('#') || trimmed.starts_with(';')
}

/// Splits a list value on whitespace, honouring double and single quotes and `\` escapes, as list
/// settings like `Wants=` and `Environment=` do (`systemd.syntax(7)`, "Quoting").
///
/// Returns `None` for an unterminated quote.
pub fn split_words(value: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut chars = value.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            return Some(words);
        }
        let mut word = String::new();
        let mut quote: Option<char> = None;
        while let Some(c) = chars.next() {
            match (quote, c) {
                (None, c) if c.is_whitespace() => break,
                (None, '"' | '\'') => quote = Some(c),
                (Some(q), c) if c == q => quote = None,
                (_, '\\') => match chars.next() {
                    Some(n) => word.push(unescape_char(n)),
                    None => word.push('\\'),
                },
                (_, c) => word.push(c),
            }
        }
        if quote.is_some() {
            return None;
        }
        words.push(word);
    }
}

fn unescape_char(c: char) -> char {
    match c {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        's' => ' ',
        other => other,
    }
}

/// Parses a boolean the way systemd does: `1`, `yes`, `y`, `true`, `t`, `on` and their opposites,
/// case-insensitively.
pub fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "yes" | "y" | "true" | "t" | "on" => Some(true),
        "0" | "no" | "n" | "false" | "f" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(p: &Parsed) -> Vec<(&str, &str, &str)> {
        p.entries
            .iter()
            .map(|e| (e.section.as_str(), e.key.as_str(), e.value.as_str()))
            .collect()
    }

    #[test]
    fn sections_keys_and_comments() {
        let p = parse(
            "# top\n[Unit]\nDescription = Hello world \n; semi\n\n[Service]\nExecStart=/bin/true\n",
        );
        assert_eq!(
            kv(&p),
            vec![
                ("Unit", "Description", "Hello world"),
                ("Service", "ExecStart", "/bin/true")
            ]
        );
        assert_eq!(p.sections, vec!["Unit", "Service"]);
        assert!(p.warnings.is_empty());
        assert_eq!(p.entries[1].line, 7);
    }

    #[test]
    fn continuation_joins_with_a_space_and_skips_comments() {
        let p = parse("[Service]\nExecStart=/bin/echo \\\n  one \\\n# a comment\n  two\nAfter=x\n");
        assert_eq!(kv(&p)[0], ("Service", "ExecStart", "/bin/echo  one  two"));
        assert_eq!(p.entries[0].line, 2);
        assert_eq!(kv(&p)[1], ("Service", "After", "x"));
    }

    #[test]
    fn continuation_at_end_of_file() {
        let p = parse("[Service]\nExecStart=/bin/echo \\");
        assert_eq!(kv(&p), vec![("Service", "ExecStart", "/bin/echo")]);
    }

    #[test]
    fn empty_value_is_kept_for_list_resets() {
        let p = parse("[Unit]\nAfter=\n");
        assert_eq!(kv(&p), vec![("Unit", "After", "")]);
    }

    #[test]
    fn equals_in_value() {
        let p = parse("[Service]\nEnvironment=A=1 B=2\n");
        assert_eq!(kv(&p), vec![("Service", "Environment", "A=1 B=2")]);
    }

    #[test]
    fn warnings() {
        let p = parse("Early=1\n[Unit]\nnonsense\n=x\n[Bad\n.include /x\n[]\n");
        let kinds: Vec<_> = p
            .warnings
            .iter()
            .map(|w| (w.line, w.kind.clone()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (1, SyntaxWarningKind::OutsideSection),
                (3, SyntaxWarningKind::NotAnAssignment),
                (4, SyntaxWarningKind::EmptyKey),
                (5, SyntaxWarningKind::BadSectionHeader),
                (6, SyntaxWarningKind::Include),
                (7, SyntaxWarningKind::BadSectionHeader),
            ]
        );
        assert_eq!(
            p.warnings[1].to_string(),
            "line 3: missing '=', ignoring line"
        );
    }

    #[test]
    fn settings_after_a_bad_header_are_dropped() {
        let p = parse("[Unit]\nA=1\n[Bad\nB=2\n[Service]\nC=3\n");
        assert_eq!(kv(&p), vec![("Unit", "A", "1"), ("Service", "C", "3")]);
        assert!(p
            .warnings
            .iter()
            .any(|w| w.kind == SyntaxWarningKind::OutsideSection && w.line == 4));
    }

    #[test]
    fn words() {
        assert_eq!(
            split_words("a.service  b.service").unwrap(),
            vec!["a.service", "b.service"]
        );
        assert_eq!(
            split_words(r#"A="x y" 'B=z w' C=\"q"#).unwrap(),
            vec!["A=x y", "B=z w", "C=\"q"]
        );
        assert_eq!(split_words("  ").unwrap(), Vec::<String>::new());
        assert_eq!(split_words(r"a\sb \n").unwrap(), vec!["a b", "\n"]);
        assert_eq!(split_words(r"trail\").unwrap(), vec!["trail\\"]);
        assert!(split_words("\"open").is_none());
    }

    #[test]
    fn bools() {
        for t in ["1", "yes", "Y", "true", "t", "ON"] {
            assert_eq!(parse_bool(t), Some(true), "{t}");
        }
        for f in ["0", "no", "n", "False", "f", "off"] {
            assert_eq!(parse_bool(f), Some(false), "{f}");
        }
        assert_eq!(parse_bool("maybe"), None);
    }
}
