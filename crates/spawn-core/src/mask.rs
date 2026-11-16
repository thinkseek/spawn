//! The mask (objective 117; sync/design-secret-injection.md 3.4 and 5.A;
//! moved here from spawn-cli by objective 175, the marker the caller's):
//! known values replaced by `[<marker>:NAME]` in every form a program prints
//! them. `spawn mask` and `spawn mask-hook` use it with the marker `SECRET`;
//! the transcript archive with `REDACTED`
//! ([`crate::transcripts::ARCHIVE_MARKER`]).
//!
//! Each value is masked plain, JSON-escaped, URL-encoded (upper- and
//! lower-case hex: also the password of a `user:password@` URL), as base64
//! at each of the three alignments in the standard and URL-safe alphabets
//! (the characters that do not depend on the neighbouring bytes), and as hex
//! in both cases. A form under 6 bytes is not used; at one place the
//! longest form wins, and every form that starts inside a match and runs
//! past its end joins it, until none does: the union is one
//! `[<marker>:NAME]`, named by the form that matched first. A value
//! transformed another way (base64 wrapped across lines as MIME does,
//! reversed, split, encrypted) is not caught (design section 4).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{Read, Write};

/// The shortest form that is masked.
const SHORTEST_FORM: usize = 6;

/// How much a streaming read takes at once.
const READ_BYTES: usize = 64 * 1024;

/// Every form of every value, longest first under each first byte, and the
/// marker a match is replaced by. No `Debug`: it holds the values.
pub struct Mask {
    /// `(form, name)`.
    forms: Vec<(Vec<u8>, String)>,
    /// The indexes into `forms` by a form's first byte, longest first.
    by_first: Vec<Vec<usize>>,
    longest: usize,
    /// A match becomes `[<marker>:NAME]`.
    marker: String,
}

/// The values of the file at `path`, `(name, value)` in its order.
///
/// # Errors
///
/// The file and the reason, with the line when it is one line's: never a
/// value.
pub(crate) fn read_values(path: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let shown = path.display();
    let meta = std::fs::metadata(path).map_err(|error| format!("{shown}: cannot read: {error}"))?;
    if !meta.is_file() {
        return Err(format!("{shown}: not a file"));
    }
    {
        use std::os::unix::fs::PermissionsExt as _;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(format!("{shown}: has group or world access (chmod 600)"));
        }
    }
    let bytes = std::fs::read(path).map_err(|error| format!("{shown}: cannot read: {error}"))?;
    let mut values = Vec::new();
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        let number = index + 1;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        let Some(equals) = line.iter().position(|byte| *byte == b'=') else {
            return Err(format!("{shown}: line {number}: not NAME=value"));
        };
        let (name, value) = (&line[..equals], &line[equals + 1..]);
        if name.is_empty()
            || !name
                .iter()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
        {
            return Err(format!("{shown}: line {number}: not NAME=value"));
        }
        if value.len() < SHORTEST_VALUE {
            return Err(format!(
                "{shown}: line {number}: a value shorter than {SHORTEST_VALUE} bytes"
            ));
        }
        values.push((String::from_utf8_lossy(name).into_owned(), value.to_vec()));
    }
    if values.is_empty() {
        return Err(format!("{shown}: names no value"));
    }
    Ok(values)
}

/// `value` JSON-escaped: `"`, `\` and control characters, and either every
/// non-ASCII character as `\uXXXX` or none.
fn json_escaped(value: &str, ascii: bool) -> String {
    let mut out = String::new();
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (ascii && !c.is_ascii()) => {
                let mut units = [0_u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// `value` URL-encoded: every byte but `A-Za-z0-9-._~` as `%XX`.
fn url_encoded(value: &[u8], upper: bool) -> String {
    let mut out = String::new();
    for byte in value {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(*byte));
        } else if upper {
            let _ = write!(out, "%{byte:02X}");
        } else {
            let _ = write!(out, "%{byte:02x}");
        }
    }
    out
}

/// `bytes` in base64 without padding, in `alphabet`.
fn base64(bytes: &[u8], alphabet: &[u8; 64]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..=chunk.len() {
            out.push(alphabet[((n >> (18 - 6 * i)) & 63) as usize]);
        }
    }
    out
}

const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL_SAFE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// The base64 characters of `value` that do not depend on its neighbours,
/// when it starts `shift` bytes into a group of three.
fn base64_interior(value: &[u8], shift: usize, alphabet: &[u8; 64]) -> Vec<u8> {
    let mut padded = vec![0_u8; shift];
    padded.extend_from_slice(value);
    let encoded = base64(&padded, alphabet);
    let skip = [0, 2, 3][shift];
    let keep = if padded.len().is_multiple_of(3) {
        encoded.len()
    } else {
        encoded.len() - 1
    };
    encoded[skip.min(keep)..keep].to_vec()
}

/// Every form of `value`, each once, none under [`SHORTEST_FORM`] bytes.
pub fn forms(value: &[u8]) -> Vec<Vec<u8>> {
    let mut all: Vec<Vec<u8>> = vec![value.to_vec()];
    if let Ok(text) = std::str::from_utf8(value) {
        for ascii in [true, false] {
            let escaped = json_escaped(text, ascii);
            all.push(escaped.replace('/', "\\/").into_bytes());
            all.push(escaped.into_bytes());
        }
    }
    for upper in [true, false] {
        all.push(url_encoded(value, upper).into_bytes());
    }
    for shift in 0..3 {
        for alphabet in [STANDARD, URL_SAFE] {
            all.push(base64_interior(value, shift, alphabet));
        }
    }
    let hex = value.iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    all.push(hex.to_uppercase().into_bytes());
    all.push(hex.into_bytes());
    let mut seen = std::collections::BTreeSet::new();
    all.retain(|form| form.len() >= SHORTEST_FORM && seen.insert(form.clone()));
    all
}

impl Mask {
    /// The mask of `values`: each value's forms, named; a match is replaced
    /// by `[<marker>:NAME]`.
    pub fn new(values: &[(String, Vec<u8>)], marker: &str) -> Self {
        let mut named: BTreeMap<Vec<u8>, String> = BTreeMap::new();
        for (name, value) in values {
            for form in forms(value) {
                named.entry(form).or_insert_with(|| name.clone());
            }
        }
        let mut forms: Vec<(Vec<u8>, String)> = named.into_iter().collect();
        forms.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        let mut by_first = vec![Vec::new(); 256];
        for (index, (form, _)) in forms.iter().enumerate() {
            by_first[usize::from(form[0])].push(index);
        }
        let longest = forms.first().map_or(0, |(form, _)| form.len());
        Self {
            forms,
            by_first,
            longest,
            marker: marker.to_string(),
        }
    }

    /// The longest form at `data[at..]`: its length and name.
    fn at(&self, data: &[u8], at: usize) -> Option<(usize, &str)> {
        let rest = &data[at..];
        self.by_first[usize::from(rest[0])]
            .iter()
            .map(|index| &self.forms[*index])
            .find(|(form, _)| rest.starts_with(form))
            .map(|(form, name)| (form.len(), name.as_str()))
    }

    /// The end of the span that starts with a match at `data[start..first]`:
    /// every form that starts inside the span and runs past its end extends
    /// it. `None` when that cannot be decided yet: a place inside the span
    /// has less than the longest form after it and the input has not ended.
    fn span_end(&self, data: &[u8], start: usize, first: usize, end: bool) -> Option<usize> {
        let mut span_end = first;
        let mut j = start + 1;
        while j < span_end {
            if !end && data.len() - j < self.longest {
                return None;
            }
            if let Some((length, _)) = self.at(data, j) {
                span_end = span_end.max(j + length);
            }
            j += 1;
        }
        Some(span_end)
    }

    /// Mask `data` into `out`, as far as it can decide: all of it at the
    /// end, else up to where a form could still run past what is there.
    /// Returns how many bytes it took.
    pub fn step(&self, data: &[u8], end: bool, out: &mut Vec<u8>) -> usize {
        let mut at = 0;
        while at < data.len() {
            if !end && data.len() - at < self.longest {
                break;
            }
            if let Some((length, name)) = self.at(data, at) {
                let Some(span_end) = self.span_end(data, at, at + length, end) else {
                    break;
                };
                out.extend_from_slice(format!("[{}:{name}]", self.marker).as_bytes());
                at = span_end;
            } else {
                out.push(data[at]);
                at += 1;
            }
        }
        at
    }

    /// `data` masked whole.
    pub fn bytes(&self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        self.step(data, true, &mut out);
        out
    }

    /// `text` masked whole.
    pub fn text(&self, text: &str) -> String {
        String::from_utf8_lossy(&self.bytes(text.as_bytes())).into_owned()
    }

    /// `input` to `output` through the mask, as it streams: the last bytes
    /// of each read (as many as the longest form, less one, or a whole span
    /// still being joined) held for the next, so a value split across reads
    /// is caught and the output does not depend on where the input is cut.
    ///
    /// # Errors
    ///
    /// A read or a write that failed.
    pub fn stream(&self, input: &mut impl Read, output: &mut impl Write) -> std::io::Result<()> {
        let mut held: Vec<u8> = Vec::new();
        let mut buffer = vec![0_u8; READ_BYTES];
        let mut out = Vec::new();
        loop {
            let read = match input.read(&mut buffer) {
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            held.extend_from_slice(&buffer[..read]);
            out.clear();
            let taken = self.step(&held, read == 0, &mut out);
            held.drain(..taken);
            output.write_all(&out)?;
            output.flush()?;
            if read == 0 {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Mask, STANDARD, URL_SAFE, base64_interior, forms};

    const ONE: &[u8] = b"made-up:abc/one+xyz";
    const THREE: &[u8] = b"bb-made-up???value-1234";

    fn mask() -> Mask {
        Mask::new(
            &[
                ("BB_ONE".to_string(), ONE.to_vec()),
                ("BB_ONE".to_string(), b"retired-0123456789".to_vec()),
                ("BB_THREE".to_string(), THREE.to_vec()),
            ],
            "SECRET",
        )
    }

    #[test]
    fn every_form_at_each_alignment_is_masked_and_nothing_else() {
        let mask = mask();
        for form in forms(ONE) {
            let text = [b"<<".as_slice(), &form, b">>"].concat();
            assert_eq!(
                mask.bytes(&text),
                b"<<[SECRET:BB_ONE]>>",
                "{}",
                String::from_utf8_lossy(&form)
            );
        }
        for shift in 0..3 {
            let mut raw = b"x".repeat(shift);
            raw.extend_from_slice(THREE);
            raw.extend_from_slice(b"tail");
            for alphabet in [STANDARD, URL_SAFE] {
                let encoded = super::base64(&raw, alphabet);
                let masked = String::from_utf8_lossy(&mask.bytes(&encoded)).into_owned();
                assert!(
                    masked.contains("[SECRET:BB_THREE]"),
                    "shift {shift}: {masked}"
                );
            }
        }
        assert_eq!(mask.text("nothing secret here"), "nothing secret here");
        assert_eq!(
            mask.text("postgres://bb:made-up%3Aabc%2Fone%2Bxyz@h/db"),
            "postgres://bb:[SECRET:BB_ONE]@h/db"
        );
        assert_eq!(mask.text("old retired-0123456789!"), "old [SECRET:BB_ONE]!");
    }

    #[test]
    fn the_url_safe_base64_forms_differ_where_the_standard_ones_hold_a_plus_or_slash() {
        for shift in 0..3 {
            let standard = base64_interior(THREE, shift, STANDARD);
            let safe = base64_interior(THREE, shift, URL_SAFE);
            assert!(
                standard.iter().any(|byte| matches!(byte, b'+' | b'/')),
                "shift {shift}: {}",
                String::from_utf8_lossy(&standard)
            );
            assert_ne!(standard, safe, "shift {shift}");
            let all = forms(THREE);
            assert!(
                all.contains(&standard) && all.contains(&safe),
                "shift {shift}"
            );
        }
    }

    #[test]
    fn a_value_cut_at_every_byte_between_two_reads_is_caught() {
        let mask = mask();
        let text = [b"before ".as_slice(), ONE, b" after"].concat();
        let whole = mask.bytes(&text);
        for cut in 0..=text.len() {
            let mut out = Vec::new();
            let mut held = text[..cut].to_vec();
            let taken = mask.step(&held, false, &mut out);
            held.drain(..taken);
            held.extend_from_slice(&text[cut..]);
            mask.step(&held, true, &mut out);
            assert_eq!(out, whole, "cut at {cut}");
        }
        let mut streamed = Vec::new();
        mask.stream(&mut text.as_slice(), &mut streamed)
            .expect("stream");
        assert_eq!(streamed, whole);
    }

    const BB_A: &[u8] = b"made-up-AAAA-0123456789abcdefghij";
    const BB_C: &[u8] = b"PREFIX-made-up-AAAA";

    fn hex(bytes: &[u8]) -> Vec<u8> {
        bytes
            .iter()
            .flat_map(|byte| format!("{byte:02x}").into_bytes())
            .collect()
    }

    /// `text` masked by `mask` in two reads cut at `cut`.
    fn cut_in_two(mask: &Mask, text: &[u8], cut: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut held = text[..cut].to_vec();
        let taken = mask.step(&held, false, &mut out);
        held.drain(..taken);
        held.extend_from_slice(&text[cut..]);
        mask.step(&held, true, &mut out);
        out
    }

    #[test]
    fn overlapping_values_are_masked_as_one_span_however_the_input_is_cut() {
        let a = ("BB_A".to_string(), BB_A.to_vec());
        let c = ("BB_C".to_string(), BB_C.to_vec());
        for values in [[a.clone(), c.clone()], [c, a]] {
            let mask = Mask::new(&values, "SECRET");
            for overlap in [
                [b"PREFIX-".as_slice(), BB_A].concat(),
                hex(&[b"PREFIX-".as_slice(), BB_A].concat()),
            ] {
                let text = [b"x ".as_slice(), &overlap, b" y"].concat();
                assert_eq!(mask.bytes(&text), b"x [SECRET:BB_C] y");
                for cut in 0..=text.len() {
                    assert_eq!(
                        cut_in_two(&mask, &text, cut),
                        b"x [SECRET:BB_C] y",
                        "cut at {cut}"
                    );
                }
                let mut streamed = Vec::new();
                mask.stream(&mut text.as_slice(), &mut streamed)
                    .expect("stream");
                assert_eq!(streamed, b"x [SECRET:BB_C] y");
            }
            assert_eq!(
                mask.text("in PREFIX-made-up-AAAA-0123456789abcdefghij-and made-up-AAAA-0123456789abcdefghij."),
                "in [SECRET:BB_C]-and [SECRET:BB_A]."
            );
        }
    }

    /// The marker is the caller's: the same values masked for the archive
    /// read `[REDACTED:NAME]`, and nothing else changes.
    #[test]
    fn the_marker_is_the_callers() {
        let values = [("BB_ONE".to_string(), ONE.to_vec())];
        let secret = Mask::new(&values, "SECRET");
        let redacted = Mask::new(&values, "REDACTED");
        let text = b"a made-up:abc/one+xyz b made-up%3Aabc%2Fone%2Bxyz c";
        assert_eq!(
            secret.bytes(text),
            b"a [SECRET:BB_ONE] b [SECRET:BB_ONE] c".to_vec()
        );
        assert_eq!(
            redacted.bytes(text),
            b"a [REDACTED:BB_ONE] b [REDACTED:BB_ONE] c".to_vec()
        );
        assert_eq!(redacted.text("nothing here"), "nothing here");
    }
}
