//! Parser for plaintext SiiNunit files (manifest.sii, versions.sii,
//! decrypted profile.sii).
//!
//! ```text
//! SiiNunit
//! {
//! mod_package: .package
//! {
//!     package_version: "1.0.0"
//!     display_name: "Some Name"
//!     category[]: "sound"
//!     compatible_versions[]: "1.59.*"
//! }
//! }
//! ```
//!
//! Values are kept as raw text: every caller only ever needs strings, so
//! there is no int/float/bool coercion step to get wrong.

pub mod crypto;

use std::collections::{BTreeMap, HashMap};

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum SiiValue {
    Scalar(String),
    List(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SiiUnit {
    pub class: String,
    pub name: String,
    pub properties: HashMap<String, SiiValue>,
}

impl SiiUnit {
    /// Scalar property as text; `None` for a missing key or a list.
    pub fn text(&self, key: &str) -> Option<&str> {
        match self.properties.get(key) {
            Some(SiiValue::Scalar(s)) => Some(s),
            _ => None,
        }
    }

    /// List property. A scalar counts as a one-element list only when
    /// `scalar_as_single` is set (manifest `category: "x"` without `[]`).
    pub fn list(&self, key: &str, scalar_as_single: bool) -> Vec<String> {
        match self.properties.get(key) {
            Some(SiiValue::List(items)) => items.clone(),
            Some(SiiValue::Scalar(s)) if scalar_as_single => vec![s.clone()],
            _ => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ident,
    Str,
    LBrace,
    RBrace,
    LBrack,
    RBrack,
    Colon,
}

#[derive(Debug)]
struct Token {
    kind: Kind,
    value: String,
    line: usize,
}

fn structural(ch: char) -> Option<Kind> {
    Some(match ch {
        '{' => Kind::LBrace,
        '}' => Kind::RBrace,
        '[' => Kind::LBrack,
        ']' => Kind::RBrack,
        ':' => Kind::Colon,
        _ => return None,
    })
}

/// `\xNN` escapes produce code points 0-255. When that byte sequence is
/// valid UTF-8, recombine it; otherwise keep the latin-1 view.
fn decode_byte_string(chars: Vec<char>) -> String {
    if chars.iter().all(|c| (*c as u32) <= 127) {
        return chars.into_iter().collect();
    }
    if chars.iter().all(|c| (*c as u32) <= 255) {
        let bytes: Vec<u8> = chars.iter().map(|c| *c as u32 as u8).collect();
        if let Ok(s) = String::from_utf8(bytes) {
            return s;
        }
    }
    chars.into_iter().collect()
}

fn tokenize(text: &str) -> Result<Vec<Token>> {
    let t: Vec<char> = text.chars().collect();
    let n = t.len();
    let mut tokens = Vec::new();
    let mut i = 0;
    let mut line = 1;
    while i < n {
        let ch = t[i];
        if ch == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if ch == ' ' || ch == '\t' || ch == '\r' {
            i += 1;
            continue;
        }
        if ch == '#' || (ch == '/' && i + 1 < n && t[i + 1] == '/') {
            while i < n && t[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if ch == '"' {
            let start_line = line;
            i += 1;
            let mut chars = Vec::new();
            let mut terminated = false;
            while i < n {
                let c = t[i];
                if c == '\\' && i + 1 < n {
                    let nxt = t[i + 1];
                    if nxt == 'x' && i + 3 < n {
                        let hex: String = t[i + 2..i + 4].iter().collect();
                        if let Ok(v) = u8::from_str_radix(&hex, 16) {
                            chars.push(v as char);
                            i += 4;
                            continue;
                        }
                    }
                    chars.push(match nxt {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    });
                    i += 2;
                    continue;
                }
                if c == '"' {
                    i += 1;
                    terminated = true;
                    break;
                }
                if c == '\n' {
                    break;
                }
                chars.push(c);
                i += 1;
            }
            if !terminated {
                return Err(Error::SiiParse(format!("Unterminated string at line {start_line}")));
            }
            tokens.push(Token { kind: Kind::Str, value: decode_byte_string(chars), line: start_line });
            continue;
        }
        if let Some(kind) = structural(ch) {
            tokens.push(Token { kind, value: ch.to_string(), line });
            i += 1;
            continue;
        }
        // identifier-ish: read until whitespace or a structural/comment start
        let start = i;
        while i < n && !matches!(t[i], ' ' | '\t' | '\r' | '\n' | '{' | '}' | '[' | ']' | ':' | '#') {
            if t[i] == '/' && i + 1 < n && t[i + 1] == '/' {
                break;
            }
            i += 1;
        }
        tokens.push(Token { kind: Kind::Ident, value: t[start..i].iter().collect(), line });
    }
    Ok(tokens)
}

enum Index {
    None,
    Append,
    At(i64),
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn parse(&mut self) -> Result<Vec<SiiUnit>> {
        let head = self.consume(Kind::Ident, "Expected 'SiiNunit' header")?;
        if head != "SiiNunit" {
            return Err(Error::SiiParse(format!("Expected 'SiiNunit' header (got '{head}')")));
        }
        self.consume(Kind::LBrace, "Expected '{' after SiiNunit")?;
        let mut units = Vec::new();
        while !self.matches(Kind::RBrace) {
            units.push(self.parse_unit()?);
        }
        Ok(units)
    }

    fn parse_unit(&mut self) -> Result<SiiUnit> {
        let class = self.consume(Kind::Ident, "Expected unit class name")?;
        self.consume(Kind::Colon, "Expected ':' after unit class")?;
        let name = self.consume(Kind::Ident, "Expected unit name")?;
        self.consume(Kind::LBrace, "Expected '{' after unit name")?;

        let mut properties: HashMap<String, SiiValue> = HashMap::new();
        // indexed arrays may arrive out of order; merge once the unit closes
        let mut indexed: Vec<(String, BTreeMap<i64, String>)> = Vec::new();
        let mut appended: Vec<(String, Vec<String>)> = Vec::new();

        while !self.matches(Kind::RBrace) {
            let (key, index, value) = self.parse_property()?;
            match index {
                Index::Append => match appended.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, items)) => items.push(value),
                    None => appended.push((key, vec![value])),
                },
                Index::At(i) => match indexed.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, by_index)) => {
                        by_index.insert(i, value);
                    }
                    None => indexed.push((key, BTreeMap::from([(i, value)]))),
                },
                Index::None => {
                    properties.insert(key, SiiValue::Scalar(value));
                }
            }
        }
        for (key, items) in appended {
            properties.insert(key, SiiValue::List(items));
        }
        // indexed arrays override a same-named scalar (the count property
        // "active_mods: 5" precedes the indexed entries)
        for (key, by_index) in indexed {
            properties.insert(key, SiiValue::List(by_index.into_values().collect()));
        }
        Ok(SiiUnit { class, name, properties })
    }

    fn parse_property(&mut self) -> Result<(String, Index, String)> {
        let key = self.consume(Kind::Ident, "Expected property name")?;
        let mut index = Index::None;
        if self.peek_is(Kind::LBrack) {
            self.pos += 1;
            if self.peek_is(Kind::RBrack) {
                self.pos += 1;
                index = Index::Append;
            } else {
                let raw = self.consume(Kind::Ident, "Expected index inside '[]'")?;
                let i = raw
                    .parse::<i64>()
                    .map_err(|_| Error::SiiParse(format!("Array index must be an integer (got '{raw}')")))?;
                self.consume(Kind::RBrack, "Expected ']' after index")?;
                index = Index::At(i);
            }
        }
        self.consume(Kind::Colon, &format!("Expected ':' after property '{key}'"))?;
        let Some(tok) = self.tokens.get(self.pos) else {
            return Err(Error::SiiParse("Expected property value (reached end of input)".into()));
        };
        if !matches!(tok.kind, Kind::Ident | Kind::Str) {
            return Err(Error::SiiParse(format!("Expected property value (got '{}' at line {})", tok.value, tok.line)));
        }
        let is_ident = tok.kind == Kind::Ident;
        let mut value = tok.value.clone();
        self.pos += 1;
        // tuple values such as "(1, 2, 3)" tokenize as several identifiers;
        // glue them back together instead of mistaking them for properties
        if is_ident && value.starts_with('(') {
            while !value.ends_with(')') && self.peek_is(Kind::Ident) {
                value.push(' ');
                value.push_str(&self.tokens[self.pos].value);
                self.pos += 1;
            }
        }
        Ok((key, index, value))
    }

    fn consume(&mut self, kind: Kind, message: &str) -> Result<String> {
        let Some(tok) = self.tokens.get(self.pos) else {
            return Err(Error::SiiParse(format!("{message} (reached end of input)")));
        };
        if tok.kind != kind {
            return Err(Error::SiiParse(format!("{message} (got '{}' at line {})", tok.value, tok.line)));
        }
        self.pos += 1;
        Ok(tok.value.clone())
    }

    fn matches(&mut self, kind: Kind) -> bool {
        if self.peek_is(kind) {
            self.pos += 1;
            return true;
        }
        false
    }

    fn peek_is(&self, kind: Kind) -> bool {
        self.tokens.get(self.pos).is_some_and(|t| t.kind == kind)
    }
}

/// Parse a plaintext SiiNunit document into its units.
pub fn parse_sii(text: &str) -> Result<Vec<SiiUnit>> {
    // mod authors on Windows often save manifest.sii with a UTF-8 BOM
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let tokens = tokenize(text)?;
    Parser { tokens, pos: 0 }.parse()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"SiiNunit
{
mod_package: .package
{
    package_version: "1.2"   # trailing comment
    display_name: "Some \"Name\""
    author: "me"
    category[]: "sound"
    category[]: "ui"
    // another comment
    compatible_versions[]: "1.59.*"
}
}
"#;

    #[test]
    fn parses_manifest() {
        let units = parse_sii(MANIFEST).unwrap();
        assert_eq!(units.len(), 1);
        let u = &units[0];
        assert_eq!(u.class, "mod_package");
        assert_eq!(u.name, ".package");
        assert_eq!(u.text("display_name"), Some("Some \"Name\""));
        assert_eq!(u.text("package_version"), Some("1.2"));
        assert_eq!(u.list("category", true), vec!["sound", "ui"]);
        assert_eq!(u.list("compatible_versions", true), vec!["1.59.*"]);
    }

    #[test]
    fn strips_bom_and_reads_scalar_category() {
        let text = "\u{feff}SiiNunit{mod_package:.p{category:\"map\"}}";
        let units = parse_sii(text).unwrap();
        assert_eq!(units[0].list("category", true), vec!["map"]);
        assert!(units[0].list("category", false).is_empty());
    }

    #[test]
    fn indexed_array_overrides_count_and_sorts() {
        let text = r#"SiiNunit { user_profile : _nameless.1 {
            active_mods: 3
            active_mods[2]: "c|C"
            active_mods[0]: "a|A"
            active_mods[1]: "b|B"
            profile_name: "Jo"
        } }"#;
        let units = parse_sii(text).unwrap();
        assert_eq!(units[0].list("active_mods", false), vec!["a|A", "b|B", "c|C"]);
        assert_eq!(units[0].text("profile_name"), Some("Jo"));
    }

    #[test]
    fn zero_count_stays_scalar() {
        let text = "SiiNunit { user_profile : x { active_mods: 0 } }";
        let units = parse_sii(text).unwrap();
        assert!(units[0].list("active_mods", false).is_empty());
    }

    #[test]
    fn hex_escapes_recombine_to_utf8() {
        // "한" = ED 95 9C
        let text = r#"SiiNunit { a : b { n: "\xed\x95\x9c" } }"#;
        let units = parse_sii(text).unwrap();
        assert_eq!(units[0].text("n"), Some("한"));
    }

    #[test]
    fn tuple_values_do_not_break_the_unit() {
        let text = "SiiNunit { a : b { pos: (1, 2, 3)\n next: 5 } }";
        let units = parse_sii(text).unwrap();
        assert_eq!(units[0].text("pos"), Some("(1, 2, 3)"));
        assert_eq!(units[0].text("next"), Some("5"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_sii("not sii").is_err());
        assert!(parse_sii("SiiNunit { a : b { n: \"open } }").is_err());
    }
}
