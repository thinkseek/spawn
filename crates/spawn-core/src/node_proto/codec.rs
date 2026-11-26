//! The wire's scalar encodings that need no `base64`: lowercase hex, SHA-256,
//! and canonical decimals without sign, leading zeros or spaces, below 2^53.

use sha2::{Digest as _, Sha256};

use super::Error;

/// 2^53: every wire integer is below it (JSON numbers, `seq`, `ts_ms`).
pub const MAX_SAFE: u64 = 1 << 53;

/// The SHA-256 of the empty string, the signed-body hash of a GET.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// The raw SHA-256 of `bytes`.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&sha256(bytes))
}

/// True when `text` is exactly `len` lowercase hex digits.
pub fn is_lower_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The bytes of `len` lowercase hex digits; `None` for anything else.
pub fn unhex(text: &str, len: usize) -> Option<Vec<u8>> {
    if !is_lower_hex(text, len) || !len.is_multiple_of(2) {
        return None;
    }
    let nibble = |b: u8| match b {
        b'0'..=b'9' => b - b'0',
        _ => b - b'a' + 10,
    };
    Some(
        text.as_bytes()
            .chunks_exact(2)
            .map(|pair| (nibble(pair[0]) << 4) | nibble(pair[1]))
            .collect(),
    )
}

/// A wire decimal: ASCII digits only, no sign, no leading zero (except `0`
/// itself), no spaces, below 2^53.
pub fn parse_decimal(text: &str) -> Result<u64, Error> {
    let bytes = text.as_bytes();
    let canonical = !bytes.is_empty()
        && bytes.len() <= 16
        && bytes.iter().all(u8::is_ascii_digit)
        && (bytes[0] != b'0' || bytes.len() == 1);
    if !canonical {
        return Err(Error::Decimal);
    }
    let value: u64 = text.parse().map_err(|_| Error::Decimal)?;
    if value >= MAX_SAFE {
        return Err(Error::Decimal);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_refuse_sign_leading_zero_space_and_two_to_the_53() {
        assert_eq!(parse_decimal("0").unwrap(), 0);
        assert_eq!(parse_decimal("1795600000000").unwrap(), 1_795_600_000_000);
        assert_eq!(parse_decimal("9007199254740991").unwrap(), MAX_SAFE - 1);
        for bad in [
            "",
            "01",
            "+1",
            "-1",
            " 1",
            "1 ",
            "1.0",
            "9007199254740992",
            "1e3",
        ] {
            assert!(parse_decimal(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn hex_round_trips_lowercase_only() {
        assert_eq!(hex(&[0, 0xab, 0xff]), "00abff");
        assert_eq!(unhex("00abff", 6).unwrap(), vec![0, 0xab, 0xff]);
        assert!(unhex("00ABFF", 6).is_none());
        assert_eq!(sha256_hex(b""), EMPTY_SHA256);
    }
}
