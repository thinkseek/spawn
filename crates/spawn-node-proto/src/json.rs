//! Strict JSON (section 2.2 rule 6): UTF-8 without a BOM, no duplicate key
//! at any depth, integers only and every one below 2^53 in magnitude. The
//! check runs over the received bytes before anything parses them; serde
//! then reads those same bytes (unknown keys ignored), and nobody
//! re-serialises to verify a signature.
//!
//! [`OrderedMap`] keeps a JSON object's key order, so a typed model writes
//! the fixtures' exact bytes back (serde_json's own map is sorted here).

use std::collections::HashSet;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{DeserializeOwned, MapAccess, Visitor};
use serde::ser::SerializeMap as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Error;
use crate::codec::MAX_SAFE;

/// Deepest nesting a wire body may have.
const MAX_DEPTH: usize = 32;

/// Checks `bytes` against rule 6 and the JSON grammar (RFC 8259).
pub fn check(bytes: &[u8]) -> Result<(), Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::Json("not UTF-8"))?;
    if text.starts_with('\u{feff}') {
        return Err(Error::Json("byte order mark"));
    }
    let mut scan = Scan { bytes, at: 0 };
    scan.ws();
    scan.value(0)?;
    scan.ws();
    if scan.at != bytes.len() {
        return Err(Error::Json("trailing bytes"));
    }
    Ok(())
}

/// [`check`], then the typed parse of the same bytes.
pub fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    check(bytes)?;
    serde_json::from_slice(bytes).map_err(|e| Error::Shape(e.to_string()))
}

/// The compact serialisation of `value` (serde_json's, which is the wire's).
pub fn to_vec<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("wire models always serialise")
}

struct Scan<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Scan<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn next(&mut self) -> Result<u8, Error> {
        let byte = self.peek().ok_or(Error::Json("unexpected end"))?;
        self.at += 1;
        Ok(byte)
    }

    fn expect(&mut self, want: u8) -> Result<(), Error> {
        if self.next()? == want {
            Ok(())
        } else {
            Err(Error::Json("unexpected byte"))
        }
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn value(&mut self, depth: usize) -> Result<(), Error> {
        if depth > MAX_DEPTH {
            return Err(Error::Json("nested too deep"));
        }
        match self.peek().ok_or(Error::Json("unexpected end"))? {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => self.string().map(drop),
            b't' => self.literal(b"true"),
            b'f' => self.literal(b"false"),
            b'n' => self.literal(b"null"),
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(Error::Json("unexpected byte")),
        }
    }

    fn literal(&mut self, word: &[u8]) -> Result<(), Error> {
        if self.bytes[self.at..].starts_with(word) {
            self.at += word.len();
            Ok(())
        } else {
            Err(Error::Json("bad literal"))
        }
    }

    fn object(&mut self, depth: usize) -> Result<(), Error> {
        self.expect(b'{')?;
        self.ws();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(());
        }
        let mut keys = HashSet::new();
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return Err(Error::Json("object key is not a string"));
            }
            if !keys.insert(self.string()?) {
                return Err(Error::Json("duplicate key"));
            }
            self.ws();
            self.expect(b':')?;
            self.ws();
            self.value(depth + 1)?;
            self.ws();
            match self.next()? {
                b',' => {}
                b'}' => return Ok(()),
                _ => return Err(Error::Json("unexpected byte")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<(), Error> {
        self.expect(b'[')?;
        self.ws();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Ok(());
        }
        loop {
            self.ws();
            self.value(depth + 1)?;
            self.ws();
            match self.next()? {
                b',' => {}
                b']' => return Ok(()),
                _ => return Err(Error::Json("unexpected byte")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, Error> {
        let mut value = 0;
        for _ in 0..4 {
            let digit = char::from(self.next()?)
                .to_digit(16)
                .ok_or(Error::Json("bad \\u escape"))?;
            value = value * 16 + digit;
        }
        Ok(value)
    }

    /// A string, decoded (so `"v"` and `"\u0076"` are the same key).
    fn string(&mut self) -> Result<String, Error> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let start = self.at;
            while let Some(byte) = self.peek() {
                if byte == b'"' || byte == b'\\' || byte < 0x20 {
                    break;
                }
                self.at += 1;
            }
            // The input is valid UTF-8 and the run stops only at ASCII.
            out.push_str(std::str::from_utf8(&self.bytes[start..self.at]).expect("checked UTF-8"));
            match self.next()? {
                b'"' => return Ok(out),
                b'\\' => {}
                _ => return Err(Error::Json("control character in a string")),
            }
            let unit = match self.next()? {
                b'"' => '"',
                b'\\' => '\\',
                b'/' => '/',
                b'b' => '\u{8}',
                b'f' => '\u{c}',
                b'n' => '\n',
                b'r' => '\r',
                b't' => '\t',
                b'u' => {
                    let high = self.hex4()?;
                    let code = if (0xd800..0xdc00).contains(&high) {
                        self.expect(b'\\')?;
                        self.expect(b'u')?;
                        let low = self.hex4()?;
                        if !(0xdc00..0xe000).contains(&low) {
                            return Err(Error::Json("lone surrogate"));
                        }
                        0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00)
                    } else {
                        high
                    };
                    char::from_u32(code).ok_or(Error::Json("lone surrogate"))?
                }
                _ => return Err(Error::Json("bad escape")),
            };
            out.push(unit);
        }
    }

    fn number(&mut self) -> Result<(), Error> {
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        let start = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        let digits = &self.bytes[start..self.at];
        if digits.is_empty() {
            return Err(Error::Json("bad number"));
        }
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) {
            return Err(Error::Json("float"));
        }
        if digits.len() > 1 && digits[0] == b'0' {
            return Err(Error::Json("leading zero"));
        }
        let small = digits.len() <= 16
            && std::str::from_utf8(digits)
                .ok()
                .and_then(|text| text.parse::<u64>().ok())
                .is_some_and(|value| value < MAX_SAFE);
        if small {
            Ok(())
        } else {
            Err(Error::Json("integer not below 2^53"))
        }
    }
}

/// A JSON object whose keys keep the order they were read or built in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedMap<V>(pub Vec<(String, V)>);

impl<V> Default for OrderedMap<V> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<V> OrderedMap<V> {
    /// The value of `key`.
    pub fn get(&self, key: &str) -> Option<&V> {
        self.0.iter().find(|(name, _)| name == key).map(|(_, value)| value)
    }

    /// The keys, in order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(name, _)| name.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl<V, K: Into<String>> FromIterator<(K, V)> for OrderedMap<V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self(iter.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }
}

impl<V: Serialize> Serialize for OrderedMap<V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in &self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for OrderedMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Entries<V>(PhantomData<V>);
        impl<'de, V: Deserialize<'de>> Visitor<'de> for Entries<V> {
            type Value = OrderedMap<V>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut entries: Vec<(String, V)> = Vec::new();
                while let Some((key, value)) = access.next_entry::<String, V>()? {
                    if entries.iter().any(|(name, _)| *name == key) {
                        return Err(serde::de::Error::custom("duplicate key"));
                    }
                    entries.push((key, value));
                }
                Ok(OrderedMap(entries))
            }
        }
        deserializer.deserialize_map(Entries(PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_json_refuses_duplicates_floats_big_integers_and_a_bom() {
        check(br#"{"v":1,"a":[1,-2,{"b":null}],"s":"é😀"}"#).unwrap();
        check(b"9007199254740991").unwrap();
        check(b"-9007199254740991").unwrap();
        for (bad, why) in [
            (&br#"{"v":1,"v":1}"#[..], "duplicate key"),
            (br#"{"a":{"v":1,"v":2}}"#, "duplicate key"),
            (br#"{"v":1,"\u0076":1}"#, "duplicate key"),
            (br#"{"x":1.25}"#, "float"),
            (br#"{"x":1e3}"#, "float"),
            (br#"{"x":1E3}"#, "float"),
            (b"9007199254740992", "integer not below 2^53"),
            (b"-9007199254740992", "integer not below 2^53"),
            (b"01", "leading zero"),
            (b"\xef\xbb\xbf{}", "byte order mark"),
            (b"{\"a\":\"\xff\"}", "not UTF-8"),
            (br#"{"a":"\ud800"}"#, "lone surrogate"),
            (b"{\"a\":\"\x01\"}", "control character in a string"),
            (b"{} {}", "trailing bytes"),
            (b"{\"a\":1,}", "object key is not a string"),
            (b"[1,]", "unexpected byte"),
            (b"tru", "bad literal"),
            (b"", "unexpected end"),
        ] {
            assert_eq!(check(bad), Err(Error::Json(why)), "{}", String::from_utf8_lossy(bad));
        }
        let deep = "[".repeat(40) + &"]".repeat(40);
        assert_eq!(check(deep.as_bytes()), Err(Error::Json("nested too deep")));
    }

    #[test]
    fn an_ordered_map_keeps_its_key_order_both_ways() {
        let text = br#"{"gate_sandbox":4,"batch_cpu":160}"#;
        let map: OrderedMap<u64> = parse(text).unwrap();
        assert_eq!(map.keys().collect::<Vec<_>>(), ["gate_sandbox", "batch_cpu"]);
        assert_eq!(map.get("batch_cpu"), Some(&160));
        assert_eq!(to_vec(&map), text);
    }
}
