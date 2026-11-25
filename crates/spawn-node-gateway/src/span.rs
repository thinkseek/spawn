//! The exact bytes of one member of a JSON object: an enroll body carries
//! the node's spec as its `spec` member, and the gateway hashes those bytes
//! as received (`spec_sha256`), as it hashes a `POST /node/v1/spec` body,
//! instead of re-serialising anything. The body has passed the strict JSON
//! check before this runs.

/// The byte range of the value of the top-level member `key` (written
/// without escapes) of the JSON object `body`; `None` when absent.
pub fn member(body: &[u8], key: &str) -> Option<std::ops::Range<usize>> {
    let mut at = skip_ws(body, 0);
    if body.get(at) != Some(&b'{') {
        return None;
    }
    at = skip_ws(body, at + 1);
    if body.get(at) == Some(&b'}') {
        return None;
    }
    loop {
        let name_end = string_end(body, at)?;
        let name = &body[at + 1..name_end - 1];
        at = skip_ws(body, name_end);
        if body.get(at) != Some(&b':') {
            return None;
        }
        let start = skip_ws(body, at + 1);
        let end = value_end(body, start)?;
        if name == key.as_bytes() {
            return Some(start..end);
        }
        at = skip_ws(body, end);
        match body.get(at) {
            Some(b',') => at = skip_ws(body, at + 1),
            _ => return None,
        }
    }
}

fn skip_ws(body: &[u8], mut at: usize) -> usize {
    while matches!(body.get(at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        at += 1;
    }
    at
}

/// The index just past the string starting at `at` (its opening quote).
fn string_end(body: &[u8], at: usize) -> Option<usize> {
    if body.get(at) != Some(&b'"') {
        return None;
    }
    let mut i = at + 1;
    loop {
        match body.get(i)? {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
}

/// The index just past the value starting at `at`.
fn value_end(body: &[u8], at: usize) -> Option<usize> {
    match body.get(at)? {
        b'"' => string_end(body, at),
        b'{' | b'[' => {
            let mut depth = 0_usize;
            let mut i = at;
            loop {
                match body.get(i)? {
                    b'"' => {
                        i = string_end(body, i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        _ => {
            let mut i = at;
            while let Some(b) = body.get(i) {
                if matches!(b, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                    break;
                }
                i += 1;
            }
            (i > at).then_some(i)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_member_s_bytes_exactly() {
        let body = br#"{"v":1,"token":"a\"}b","spec":{"x":[1,{"y":"}"}]} ,"z":true}"#;
        let range = member(body, "spec").unwrap();
        assert_eq!(&body[range], br#"{"x":[1,{"y":"}"}]}"#);
        assert_eq!(&body[member(body, "v").unwrap()], b"1");
        assert_eq!(&body[member(body, "z").unwrap()], b"true");
        assert_eq!(&body[member(body, "token").unwrap()], br#""a\"}b""#);
        assert!(member(body, "nope").is_none());
        assert!(member(b"{}", "spec").is_none());
        assert!(member(b"[]", "spec").is_none());
    }
}
