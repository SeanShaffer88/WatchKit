//! Minimal JSON value model and parser — hand-rolled so
//! JobKit stays zero-dependency (house rule). The shape is the
//! family engine (same design as SettingsKit's `json` module):
//! objects preserve insertion order and the parser REJECTS
//! duplicate keys within one object — a config with two values
//! for one key is ambiguous, and silently picking one is how
//! configuration bugs hide.
//!
//! Numbers: an integral literal that fits i64 is stored as
//! `I64`; anything else is `F64`. Only finite floats parse.

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(String),
    Array(Vec<Value>),
    /// Insertion-ordered key/value pairs; keys unique (enforced
    /// by the parser).
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn keys(&self) -> Vec<&str> {
        match self {
            Value::Object(pairs) => pairs.iter().map(|(k, _)| k.as_str()).collect(),
            _ => Vec::new(),
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::I64(n) => Some(*n),
            Value::F64(f) if f.fract() == 0.0 && f.is_finite() => Some(*f as i64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Vec<(String, Value)>> {
        match self {
            Value::Object(pairs) => Some(pairs),
            _ => None,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::I64(_) | Value::F64(_) => "number",
            Value::Str(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }
}

/// Deepest nesting the parser accepts. Configs are shallow; the
/// cap exists so a hostile or corrupt input cannot overflow the
/// stack through the recursive parser.
pub const MAX_DEPTH: usize = 128;

pub fn parse(text: &str) -> Result<Value, String> {
    let mut p = Parser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let v = p.value(0)?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(format!("trailing characters at byte {}", p.pos));
    }
    Ok(v)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", b as char, self.pos))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err(format!("nesting deeper than {MAX_DEPTH} levels"));
        }
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(format!("unexpected character at byte {}", self.pos)),
        }
    }

    fn literal(&mut self, word: &str, v: Value) -> Result<Value, String> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(v)
        } else {
            Err(format!("invalid literal at byte {}", self.pos))
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, String> {
        self.expect(b'{')?;
        let mut pairs: Vec<(String, Value)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            if pairs.iter().any(|(k, _)| k == &key) {
                return Err(format!("duplicate key \"{key}\" in object"));
            }
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let val = self.value(depth + 1)?;
            pairs.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(pairs));
                }
                _ => return Err(format!("expected ',' or '}}' at byte {}", self.pos)),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(format!("expected ',' or ']' at byte {}", self.pos)),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let b = self.peek().ok_or("unterminated string")?;
            self.pos += 1;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let esc = self.peek().ok_or("unterminated escape")?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            if (0xD800..0xDC00).contains(&hi) {
                                // High surrogate: a \uXXXX low must follow.
                                if self.peek() == Some(b'\\') {
                                    self.pos += 1;
                                    self.expect(b'u')?;
                                    let lo = self.hex4()?;
                                    if !(0xDC00..0xE000).contains(&lo) {
                                        return Err("invalid low surrogate".into());
                                    }
                                    let c = 0x1_0000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                    out.push(char::from_u32(c).ok_or("invalid unicode escape")?);
                                } else {
                                    return Err("unpaired high surrogate".into());
                                }
                            } else if (0xDC00..0xE000).contains(&hi) {
                                return Err("unpaired low surrogate".into());
                            } else {
                                out.push(char::from_u32(hi).ok_or("invalid unicode escape")?);
                            }
                        }
                        _ => return Err(format!("invalid escape at byte {}", self.pos - 1)),
                    }
                }
                0x00..=0x1F => {
                    return Err(format!(
                        "unescaped control character at byte {}",
                        self.pos - 1
                    ))
                }
                _ => {
                    // Consume one full UTF-8 char (input is &str, so
                    // the bytes are valid UTF-8; copy the sequence).
                    let start = self.pos - 1;
                    let mut end = self.pos;
                    while end < self.bytes.len() && (self.bytes[end] & 0xC0) == 0x80 {
                        end += 1;
                    }
                    out.push_str(
                        std::str::from_utf8(&self.bytes[start..end])
                            .map_err(|_| "invalid UTF-8 in string".to_string())?,
                    );
                    self.pos = end;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let mut v: u32 = 0;
        for _ in 0..4 {
            let b = self.peek().ok_or("truncated \\u escape")?;
            self.pos += 1;
            let digit = (b as char)
                .to_digit(16)
                .ok_or("invalid hex in \\u escape")?;
            v = v * 16 + digit;
        }
        Ok(v)
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // Integer part: "0" or [1-9][0-9]* — leading zeros are invalid JSON.
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(format!("invalid number at byte {start}")),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(format!("invalid fraction at byte {}", self.pos));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(format!("invalid exponent at byte {}", self.pos));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap();
        if !is_float {
            if let Ok(n) = text.parse::<i64>() {
                return Ok(Value::I64(n));
            }
        }
        let f: f64 = text
            .parse()
            .map_err(|_| format!("invalid number '{text}'"))?;
        if !f.is_finite() {
            return Err(format!("number out of range '{text}'"));
        }
        Ok(Value::F64(f))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_shaped_json() {
        let v = parse(r#"{"exe":"x","args":["a b",2,true],"env":{"K":"V"},"n":null}"#).unwrap();
        assert_eq!(v.get("exe").unwrap().as_str(), Some("x"));
        let args = v.get("args").unwrap().as_array().unwrap();
        assert_eq!(args.len(), 3);
        assert_eq!(v.get("env").unwrap().get("K").unwrap().as_str(), Some("V"));
    }

    #[test]
    fn parses_scalars_and_surrogates() {
        assert_eq!(parse("null").unwrap(), Value::Null);
        assert_eq!(parse("-42").unwrap(), Value::I64(-42));
        assert_eq!(parse("2.5").unwrap(), Value::F64(2.5));
        assert_eq!(
            parse("\"\\uD83D\\uDE00\"").unwrap(),
            Value::Str("😀".into())
        );
        assert!(parse("\"\\uD83D\"").is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("").is_err());
        assert!(parse("{} extra").is_err());
        assert!(parse("{\"a\": }").is_err());
        assert!(parse("{\"a\": 1, \"a\": 2}").is_err()); // duplicate key
        assert!(parse("[1,]").is_err());
        assert!(parse("01").is_err());
        assert!(parse("\"unterminated").is_err());
        assert!(parse("1e999").is_err()); // out of f64 range
    }

    #[test]
    fn depth_cap() {
        let deep = "[".repeat(500) + &"]".repeat(500);
        assert!(parse(&deep).is_err());
    }
}
