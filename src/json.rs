//! A tiny order-preserving JSON value, parser and writer.
//!
//! Hand-rolled rather than pulled from crates.io for two reasons beyond keeping
//! the dependency count at zero:
//!
//! * **Key order is preserved.** We rewrite the user's `~/.claude.json`, which is
//!   ~80 KB of state we do not own. A `BTreeMap`-backed value would reorder every
//!   key on write; here the file comes back out in the order it went in.
//! * **Numbers round-trip byte-exactly.** They are kept as their source lexeme,
//!   never through `f64`. `expiresAt` is a 13-digit epoch-millis integer that a
//!   float would render as `1.787811738997e12`, which Claude Code does not accept.

use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// The number's source text, kept verbatim so it round-trips unchanged.
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn obj() -> Json {
        Json::Obj(Vec::new())
    }

    pub fn num(v: i64) -> Json {
        Json::Num(v.to_string())
    }

    pub fn str(v: impl Into<String>) -> Json {
        Json::Str(v.into())
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    #[cfg_attr(not(feature = "usage"), allow(dead_code))]
    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }

    #[cfg_attr(not(feature = "usage"), allow(dead_code))]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(s) => s.parse().ok(),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            // Tolerate `1.0`-style integers by truncating through f64 only when
            // the lexeme is not a plain integer.
            Json::Num(s) => s
                .parse()
                .ok()
                .or_else(|| s.parse::<f64>().ok().map(|f| f as i64)),
            _ => None,
        }
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Json::as_str)
    }

    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(Json::as_i64)
    }

    #[cfg_attr(not(feature = "usage"), allow(dead_code))]
    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(Json::as_f64)
    }

    pub fn is_obj(&self) -> bool {
        matches!(self, Json::Obj(_))
    }

    /// Set `key`, replacing in place (keeping its position) or appending.
    pub fn set(&mut self, key: &str, value: Json) {
        if let Json::Obj(entries) = self {
            match entries.iter_mut().find(|(k, _)| k == key) {
                Some(slot) => slot.1 = value,
                None => entries.push((key.to_string(), value)),
            }
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Json::Obj(entries) => entries.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<Json> {
        match self {
            Json::Obj(entries) => entries
                .iter()
                .position(|(k, _)| k == key)
                .map(|i| entries.remove(i).1),
            _ => None,
        }
    }

    pub fn parse(input: &str) -> Result<Json, String> {
        let bytes = input.as_bytes();
        let mut p = Parser { b: bytes, i: 0 };
        p.ws();
        let v = p.value()?;
        p.ws();
        if p.i != bytes.len() {
            return Err(format!("trailing input at byte {}", p.i));
        }
        Ok(v)
    }

    /// Compact, single-line. Used for request bodies and credential blobs.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        write_value(&mut out, self, None, 0);
        out
    }

    /// Two-space indented, matching what claude-swap and Claude Code write, so
    /// a config we rewrite keeps the same shape on disk.
    pub fn dump_pretty(&self) -> String {
        let mut out = String::new();
        write_value(&mut out, self, Some(2), 0);
        out
    }
}

fn write_value(out: &mut String, v: &Json, indent: Option<usize>, depth: usize) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Num(n) => out.push_str(n),
        Json::Str(s) => write_string(out, s),
        Json::Arr(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline_indent(out, indent, depth + 1);
                write_value(out, item, indent, depth + 1);
            }
            newline_indent(out, indent, depth);
            out.push(']');
        }
        Json::Obj(entries) => {
            if entries.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, val)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline_indent(out, indent, depth + 1);
                write_string(out, k);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_value(out, val, indent, depth + 1);
            }
            newline_indent(out, indent, depth);
            out.push('}');
        }
    }
}

fn newline_indent(out: &mut String, indent: Option<usize>, depth: usize) {
    if let Some(step) = indent {
        out.push('\n');
        for _ in 0..step * depth {
            out.push(' ');
        }
    }
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn eat(&mut self, lit: &[u8]) -> bool {
        if self.b[self.i..].starts_with(lit) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn err<T>(&self, what: &str) -> Result<T, String> {
        Err(format!("invalid JSON: expected {what} at byte {}", self.i))
    }

    fn value(&mut self) -> Result<Json, String> {
        match self.peek() {
            None => self.err("a value"),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') if self.eat(b"true") => Ok(Json::Bool(true)),
            Some(b'f') if self.eat(b"false") => Ok(Json::Bool(false)),
            Some(b'n') if self.eat(b"null") => Ok(Json::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => self.err("a value"),
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.i += 1; // '{'
        let mut entries = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Json::Obj(entries));
        }
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return self.err("an object key");
            }
            let key = self.string()?;
            self.ws();
            if self.peek() != Some(b':') {
                return self.err("':'");
            }
            self.i += 1;
            self.ws();
            let value = self.value()?;
            entries.push((key, value));
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(entries));
                }
                _ => return self.err("',' or '}'"),
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.i += 1; // '['
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.ws();
            items.push(self.value()?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return self.err("',' or ']'"),
            }
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).map_err(|e| e.to_string())?;
        if text.is_empty() || text == "-" {
            return self.err("a number");
        }
        Ok(Json::Num(text.to_string()))
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            let c = match self.peek() {
                Some(c) => c,
                None => return self.err("a closing '\"'"),
            };
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    let esc = match self.peek() {
                        Some(e) => e,
                        None => return self.err("an escape"),
                    };
                    self.i += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            // Surrogate pair: a high surrogate must be followed
                            // by \uDC00-\uDFFF to form one scalar value.
                            let ch = if (0xD800..0xDC00).contains(&hi) {
                                if !self.eat(b"\\u") {
                                    return self.err("a low surrogate");
                                }
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return self.err("a low surrogate");
                                }
                                let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                char::from_u32(cp)
                            } else {
                                char::from_u32(hi)
                            };
                            match ch {
                                Some(ch) => out.push(ch),
                                None => return self.err("a valid \\u escape"),
                            }
                        }
                        _ => return self.err("a valid escape"),
                    }
                }
                _ => {
                    // Copy the whole UTF-8 run up to the next quote/backslash in
                    // one go rather than char-by-char.
                    let start = self.i;
                    while matches!(self.peek(), Some(c) if c != b'"' && c != b'\\') {
                        self.i += 1;
                    }
                    match std::str::from_utf8(&self.b[start..self.i]) {
                        Ok(s) => out.push_str(s),
                        Err(_) => return self.err("valid UTF-8"),
                    }
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        if self.i + 4 > self.b.len() {
            return self.err("4 hex digits");
        }
        let s = match std::str::from_utf8(&self.b[self.i..self.i + 4]) {
            Ok(s) => s,
            Err(_) => return self.err("4 hex digits"),
        };
        match u32::from_str_radix(s, 16) {
            Ok(v) => {
                self.i += 4;
                Ok(v)
            }
            Err(_) => self.err("4 hex digits"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_key_order_and_number_lexemes() {
        // Key order is what keeps a rewritten ~/.claude.json from turning into a
        // whole-file diff, and the 13-digit lexeme is `expiresAt`: through an f64
        // it would come back as 1.787811738997e12, which Claude Code rejects.
        let src = r#"{"zeta":1787811738997,"alpha":1.5,"beta":[1,2,{"n":null}]}"#;
        let parsed = Json::parse(src).unwrap();
        assert_eq!(parsed.dump(), src);
        assert_eq!(parsed.get_i64("zeta"), Some(1787811738997));
    }

    #[test]
    fn set_replaces_in_place_without_moving_the_key() {
        let mut v = Json::parse(r#"{"a":1,"b":2,"c":3}"#).unwrap();
        v.set("b", Json::num(9));
        assert_eq!(v.dump(), r#"{"a":1,"b":9,"c":3}"#);
        v.set("d", Json::num(4));
        assert_eq!(v.dump(), r#"{"a":1,"b":9,"c":3,"d":4}"#);
    }

    #[test]
    fn handles_escapes_and_surrogate_pairs() {
        let v = Json::parse(r#"{"s":"a\"b\\c\né😀"}"#).unwrap();
        assert_eq!(v.get_str("s"), Some("a\"b\\c\né😀"));
        // Round-tripping re-emits the astral char literally, which is still valid.
        assert_eq!(Json::parse(&v.dump()).unwrap(), v);
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [r#"{"a":}"#, "{", "[1,]", r#"{"a" 1}"#, "", "nul"] {
            assert!(Json::parse(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn pretty_matches_two_space_indent() {
        let v = Json::parse(r#"{"a":{"b":[1]},"c":{}}"#).unwrap();
        assert_eq!(
            v.dump_pretty(),
            "{\n  \"a\": {\n    \"b\": [\n      1\n    ]\n  },\n  \"c\": {}\n}"
        );
    }
}
