//! A reader for `nats.conf`, enough for `doctor` to check the accounts the hub runs with
//! (`hub accounts`, hgl-ai#146): maps, arrays, quoted and bare strings, `#` and `//`
//! comments, `:`, `=` or a space between key and value, and `$NAME` references to keys
//! defined before them. Numbers and booleans stay strings. `include` is refused: the
//! file must say all of it.

/// A value: a string (anything that is not a map or an array), an array or a map in file
/// order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    List(Vec<Value>),
    Map(Vec<(String, Value)>),
}

impl Value {
    /// The last value of `key` in a map, the key compared without case as the server does.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries()
            .iter()
            .rev()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    }

    /// A map's entries; none for anything else.
    pub fn entries(&self) -> &[(String, Value)] {
        match self {
            Value::Map(m) => m,
            _ => &[],
        }
    }

    /// An array's items; a lone value is an array of one.
    pub fn items(&self) -> Vec<&Value> {
        match self {
            Value::List(l) => l.iter().collect(),
            v => vec![v],
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// Parse a whole file into its top-level map.
pub fn parse(text: &str) -> Result<Value, String> {
    let mut p = Parser {
        s: text.chars().collect(),
        i: 0,
        scopes: vec![Vec::new()],
    };
    p.map_body(false)?;
    Ok(Value::Map(p.scopes.pop().unwrap_or_default()))
}

struct Parser {
    s: Vec<char>,
    i: usize,
    /// The entries of each map being read, outermost first: where `$NAME` is looked up.
    scopes: Vec<Vec<(String, Value)>>,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn at(&self, text: &str) -> bool {
        text.chars()
            .enumerate()
            .all(|(n, c)| self.s.get(self.i + n) == Some(&c))
    }

    fn line(&self) -> usize {
        1 + self.s[..self.i.min(self.s.len())]
            .iter()
            .filter(|c| **c == '\n')
            .count()
    }

    fn err(&self, what: &str) -> String {
        format!("line {}: {what}", self.line())
    }

    /// Skip blanks and comments; newlines too when `newlines`.
    fn skip(&mut self, newlines: bool) {
        while let Some(c) = self.peek() {
            if c == '#' || self.at("//") {
                while self.peek().is_some_and(|c| c != '\n') {
                    self.i += 1;
                }
            } else if c == ' ' || c == '\t' || c == '\r' || (newlines && c == '\n') {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    /// Entries up to the closing `}` (`braced`) or the end of the file, into the innermost
    /// scope.
    fn map_body(&mut self, braced: bool) -> Result<(), String> {
        loop {
            self.skip(true);
            match self.peek() {
                None if braced => return Err(self.err("a map is not closed")),
                None => return Ok(()),
                Some('}') if braced => {
                    self.i += 1;
                    return Ok(());
                }
                Some(',' | ';') => self.i += 1,
                Some(_) => {
                    let key = self.key()?;
                    self.skip(false);
                    if matches!(self.peek(), Some(':' | '=')) {
                        self.i += 1;
                        self.skip(false);
                    }
                    if key.eq_ignore_ascii_case("include") {
                        return Err(self.err("`include` is not supported"));
                    }
                    let v = self.value()?;
                    if let Some(scope) = self.scopes.last_mut() {
                        scope.push((key, v));
                    }
                }
            }
        }
    }

    fn key(&mut self) -> Result<String, String> {
        match self.peek() {
            Some(q @ ('"' | '\'')) => self.quoted(q),
            _ => {
                let start = self.i;
                while self.peek().is_some_and(|c| {
                    !c.is_whitespace() && !matches!(c, ':' | '=' | '{' | '[' | '}' | ',' | '#')
                }) {
                    self.i += 1;
                }
                if self.i == start {
                    return Err(self.err("expected a key"));
                }
                Ok(self.s[start..self.i].iter().collect())
            }
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.peek() {
            Some('{') => {
                self.i += 1;
                self.scopes.push(Vec::new());
                let r = self.map_body(true);
                let m = self.scopes.pop().unwrap_or_default();
                r.map(|()| Value::Map(m))
            }
            Some('[') => {
                self.i += 1;
                let mut items = Vec::new();
                loop {
                    self.skip(true);
                    match self.peek() {
                        None => return Err(self.err("an array is not closed")),
                        Some(']') => {
                            self.i += 1;
                            return Ok(Value::List(items));
                        }
                        Some(',') => self.i += 1,
                        Some(_) => items.push(self.value()?),
                    }
                }
            }
            Some(q @ ('"' | '\'')) => self.quoted(q).map(Value::Str),
            Some('$') => {
                self.i += 1;
                let name = self.bare();
                Ok(self.lookup(&name).unwrap_or(Value::Str(format!("${name}"))))
            }
            Some(_) => {
                let v = self.bare();
                if v.is_empty() {
                    Err(self.err("expected a value"))
                } else {
                    Ok(Value::Str(v))
                }
            }
            None => Err(self.err("expected a value")),
        }
    }

    /// A bare value: up to the end of the line, a separator, a closing bracket or a
    /// comment, without trailing blanks.
    fn bare(&mut self) -> String {
        let start = self.i;
        while self.peek().is_some_and(|c| {
            !matches!(c, '\n' | ',' | ';' | '}' | ']' | '#') && !(c == '/' && self.at("//"))
        }) {
            self.i += 1;
        }
        self.s[start..self.i]
            .iter()
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn quoted(&mut self, q: char) -> Result<String, String> {
        self.i += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(self.err("a string is not closed")),
                Some(c) if c == q => {
                    self.i += 1;
                    return Ok(out);
                }
                Some('\\') if q == '"' => {
                    self.i += 1;
                    let c = self
                        .peek()
                        .ok_or_else(|| self.err("a string is not closed"))?;
                    out.push(match c {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        c => c,
                    });
                    self.i += 1;
                }
                Some(c) => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
    }

    fn lookup(&self, name: &str) -> Option<Value> {
        self.scopes.iter().rev().find_map(|scope| {
            scope
                .iter()
                .rev()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_arrays_strings_and_references() {
        let v = parse(
            "# a comment\nhost: \"127.0.0.1\"\nport = 4222 // another\n\
             P = { allow = [\"a.>\", 'b'] }\n\
             accounts {\n  A { users: [ { nkey: \"U1\", permissions: { publish: $P } } ]\n\
             exports: [ { service: \"x.y\", accounts: [B, C] }, {stream: s} ] }\n  B: {}\n}\n",
        )
        .unwrap();
        assert_eq!(v.get("host").and_then(Value::as_str), Some("127.0.0.1"));
        assert_eq!(v.get("PORT").and_then(Value::as_str), Some("4222"));
        let a = v.get("accounts").unwrap().get("A").unwrap();
        let user = a.get("users").unwrap().items()[0];
        assert_eq!(user.get("nkey").and_then(Value::as_str), Some("U1"));
        let publish = user.get("permissions").unwrap().get("publish").unwrap();
        assert_eq!(publish, v.get("P").unwrap(), "{publish:?}");
        let exports = a.get("exports").unwrap().items();
        assert_eq!(
            exports[0]
                .get("accounts")
                .unwrap()
                .items()
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>(),
            ["B", "C"]
        );
        assert_eq!(exports[1].get("stream").and_then(Value::as_str), Some("s"));
        assert!(v.get("accounts").unwrap().get("B").is_some());
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        for bad in [
            "a { b: 1",
            "a: [1, 2",
            "a: \"open",
            "include ./more.conf\n",
            "a:\n",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
