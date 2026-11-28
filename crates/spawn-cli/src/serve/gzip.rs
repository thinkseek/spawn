//! Zero-dependency RFC 1952 `gzip` (RFC 1951 fixed-Huffman + LZ77 deflate)
//! response compressor for `spawn serve`.
//!
//! No external compression crate is vendored into this build's offline cargo
//! registry, so [`compress_if_smaller`] encodes large JSON boards and built
//! static assets directly in pure safe Rust (`unsafe_code = "forbid"`).

use std::cmp::Ordering;

/// Payloads smaller than this rarely justify compression framing overhead.
pub(super) const MIN_COMPRESS_BYTES: usize = 512;

const WINDOW_SIZE: usize = 32_768;
const WINDOW_MASK: usize = WINDOW_SIZE - 1;
const HASH_BITS: usize = 14;
const HASH_SIZE: usize = 1 << HASH_BITS;
const HASH_MASK: usize = HASH_SIZE - 1;
const MAX_CHAIN: usize = 24;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const NICE_MATCH: usize = 64;
const EMPTY_POS: u32 = u32::MAX;

/// Whether a client's `Accept-Encoding` header allows `gzip` (RFC 9110 §12.5.3).
///
/// Honors explicit `gzip` / `x-gzip` directives and falls back to `*` only when
/// `gzip` was not explicitly rejected with `q=0`.
pub(in crate::serve) fn accepts_gzip(accept_encoding: Option<&str>) -> bool {
    let Some(raw) = accept_encoding else {
        return false;
    };
    let mut explicit_gzip: Option<bool> = None;
    let mut wildcard_allowed = false;

    for item in raw.split(',') {
        let mut parts = item.split(';');
        let coding = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
        if coding.is_empty() {
            continue;
        }
        let mut allowed = true;
        for param in parts {
            let param = param.trim();
            if let Some(q_raw) = param
                .strip_prefix("q=")
                .or_else(|| param.strip_prefix("Q="))
            {
                let q_val = q_raw.trim().parse::<f32>().unwrap_or(0.0);
                if !q_val.is_finite() || q_val <= 0.0 {
                    allowed = false;
                }
            }
        }
        match coding.as_str() {
            "gzip" | "x-gzip" => explicit_gzip = Some(explicit_gzip.unwrap_or(false) || allowed),
            "*" => wildcard_allowed = wildcard_allowed || allowed,
            _ => {}
        }
    }

    explicit_gzip.unwrap_or(wildcard_allowed)
}

/// Compress `input` into a complete RFC 1952 `gzip` stream if doing so shrinks it
/// and `input.len() >= MIN_COMPRESS_BYTES`.
pub(in crate::serve) fn compress_if_smaller(input: &[u8]) -> Option<Vec<u8>> {
    if input.len() < MIN_COMPRESS_BYTES {
        return None;
    }
    let compressed = compress(input);
    (compressed.len() < input.len()).then_some(compressed)
}

/// Compress `input` into a complete RFC 1952 `gzip` byte stream.
pub(super) fn compress(input: &[u8]) -> Vec<u8> {
    let estimated_cap = (input.len() / 2).saturating_add(64);
    let mut bits = BitWriter::with_capacity(estimated_cap);

    // RFC 1952 §2.3.1 10-byte gzip header: ID1, ID2, CM=8 (deflate), FLG=0,
    // MTIME=0 (deterministic builds/responses), XFL=0, OS=255 (unknown).
    bits.write_header(&[0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff]);

    // Single final fixed-Huffman deflate block: BFINAL=1 (1 bit), BTYPE=01 (2 bits).
    bits.write_bits(0b011, 3);

    deflate_fixed(input, &mut bits);

    // End-of-block symbol 256 (7 zero bits in fixed Huffman).
    let (eob_code, eob_len) = FIXED_LIT[256];
    bits.write_bits(u32::from(eob_code), eob_len);

    let mut out = bits.finish();
    let crc = crc32(input);
    let isize_mod = u32::try_from(input.len() & 0xffff_ffff).unwrap_or(0);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&isize_mod.to_le_bytes());
    out
}

fn deflate_fixed(input: &[u8], bits: &mut BitWriter) {
    if input.is_empty() {
        return;
    }

    let mut head = vec![EMPTY_POS; HASH_SIZE];
    let mut prev = vec![EMPTY_POS; WINDOW_SIZE];
    let mut pos = 0usize;
    let len = input.len();

    while pos < len {
        let mut best_len = 0usize;
        let mut best_dist = 0usize;

        if pos + MIN_MATCH <= len {
            let h = hash3(input, pos);
            let max_match = (len - pos).min(MAX_MATCH);
            let mut cand = head[h];
            let mut chain = 0usize;

            while cand != EMPTY_POS && chain < MAX_CHAIN {
                let cand_pos = usize::try_from(cand).unwrap_or(usize::MAX);
                if cand_pos >= pos {
                    break;
                }
                let dist = pos - cand_pos;
                if dist > WINDOW_SIZE {
                    break;
                }
                let can_beat = best_len == 0
                    || (best_len < max_match
                        && input[cand_pos + best_len] == input[pos + best_len]);
                if can_beat
                    && input[cand_pos] == input[pos]
                    && input[cand_pos + 1] == input[pos + 1]
                    && input[cand_pos + 2] == input[pos + 2]
                {
                    let mut m = 3usize;
                    while m < max_match && input[cand_pos + m] == input[pos + m] {
                        m += 1;
                    }
                    if m > best_len {
                        best_len = m;
                        best_dist = dist;
                        if best_len >= NICE_MATCH || best_len == max_match {
                            break;
                        }
                    }
                }
                cand = prev[cand_pos & WINDOW_MASK];
                chain += 1;
            }

            if let Ok(pos_u32) = u32::try_from(pos) {
                prev[pos & WINDOW_MASK] = head[h];
                head[h] = pos_u32;
            }
        }

        if best_len >= MIN_MATCH {
            emit_match(best_len, best_dist, bits);
            let end = (pos + best_len).min(len);
            for skip in (pos + 1)..end {
                if skip + MIN_MATCH <= len
                    && let Ok(skip_u32) = u32::try_from(skip)
                {
                    let sh = hash3(input, skip);
                    prev[skip & WINDOW_MASK] = head[sh];
                    head[sh] = skip_u32;
                }
            }
            pos = end;
        } else {
            let (code, code_len) = FIXED_LIT[usize::from(input[pos])];
            bits.write_bits(u32::from(code), code_len);
            pos += 1;
        }
    }
}

fn emit_match(length: usize, dist: usize, bits: &mut BitWriter) {
    let len_u16 = u16::try_from(length).expect("match length fits in u16 (<= 258)");
    let idx = LENGTH_TABLE
        .binary_search_by(|&(sym, base, extra)| {
            let max = if sym == 284 {
                257
            } else {
                base + ((1u16 << extra) - 1)
            };
            if len_u16 < base {
                Ordering::Greater
            } else if len_u16 > max {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        })
        .expect("valid RFC 1951 match length 3..=258");
    let (sym, base_len, extra_bits) = LENGTH_TABLE[idx];
    let (lit_code, lit_len) = FIXED_LIT[usize::from(sym)];
    bits.write_bits(u32::from(lit_code), lit_len);
    if extra_bits > 0 {
        bits.write_bits(u32::from(len_u16 - base_len), extra_bits);
    }

    let dist_u16 = u16::try_from(dist).expect("match distance fits in u16 (<= 32768)");
    let d_idx = DIST_TABLE
        .binary_search_by(|&(base, extra)| {
            let max = base + ((1u16 << extra) - 1);
            if dist_u16 < base {
                Ordering::Greater
            } else if dist_u16 > max {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        })
        .expect("valid RFC 1951 distance 1..=32768");
    let (base_dist, d_extra) = DIST_TABLE[d_idx];
    bits.write_bits(u32::from(FIXED_DIST[d_idx]), 5);
    if d_extra > 0 {
        bits.write_bits(u32::from(dist_u16 - base_dist), d_extra);
    }
}

#[inline]
fn hash3(input: &[u8], pos: usize) -> usize {
    let h = (usize::from(input[pos]) << 10)
        ^ (usize::from(input[pos + 1]) << 5)
        ^ usize::from(input[pos + 2]);
    h & HASH_MASK
}

struct BitWriter {
    out: Vec<u8>,
    buf: u64,
    bits: u8,
}

impl BitWriter {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            out: Vec::with_capacity(capacity),
            buf: 0,
            bits: 0,
        }
    }

    fn write_header(&mut self, bytes: &[u8]) {
        debug_assert_eq!(self.bits, 0);
        self.out.extend_from_slice(bytes);
    }

    #[inline]
    fn write_bits(&mut self, value: u32, count: u8) {
        self.buf |= u64::from(value) << self.bits;
        self.bits += count;
        while self.bits >= 8 {
            self.out.push(self.buf.to_le_bytes()[0]);
            self.buf >>= 8;
            self.bits -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.out.push(self.buf.to_le_bytes()[0]);
        }
        self.out
    }
}

const fn reverse_bits(code: u16, bits: u8) -> u16 {
    code.reverse_bits() >> (16 - bits)
}

const FIXED_LIT: [(u16, u8); 286] = build_fixed_lit();

const fn build_fixed_lit() -> [(u16, u8); 286] {
    let mut table = [(0u16, 0u8); 286];
    let mut idx = 0usize;
    let mut sym = 0u16;
    while idx < 286 {
        let (code, len) = if sym <= 143 {
            (0x0030 + sym, 8)
        } else if sym <= 255 {
            (0x0190 + (sym - 144), 9)
        } else if sym <= 279 {
            (sym - 256, 7)
        } else {
            (0x00c0 + (sym - 280), 8)
        };
        table[idx] = (reverse_bits(code, len), len);
        idx += 1;
        sym += 1;
    }
    table
}

const FIXED_DIST: [u16; 30] = build_fixed_dist();

const fn build_fixed_dist() -> [u16; 30] {
    let mut table = [0u16; 30];
    let mut idx = 0usize;
    let mut d = 0u16;
    while idx < 30 {
        table[idx] = reverse_bits(d, 5);
        idx += 1;
        d += 1;
    }
    table
}

const LENGTH_TABLE: [(u16, u16, u8); 29] = [
    (257, 3, 0),
    (258, 4, 0),
    (259, 5, 0),
    (260, 6, 0),
    (261, 7, 0),
    (262, 8, 0),
    (263, 9, 0),
    (264, 10, 0),
    (265, 11, 1),
    (266, 13, 1),
    (267, 15, 1),
    (268, 17, 1),
    (269, 19, 2),
    (270, 23, 2),
    (271, 27, 2),
    (272, 31, 2),
    (273, 35, 3),
    (274, 43, 3),
    (275, 51, 3),
    (276, 59, 3),
    (277, 67, 4),
    (278, 83, 4),
    (279, 99, 4),
    (280, 115, 4),
    (281, 131, 5),
    (282, 163, 5),
    (283, 195, 5),
    (284, 227, 5),
    (285, 258, 0),
];

const DIST_TABLE: [(u16, u8); 30] = [
    (1, 0),
    (2, 0),
    (3, 0),
    (4, 0),
    (5, 1),
    (7, 1),
    (9, 2),
    (13, 2),
    (17, 3),
    (25, 3),
    (33, 4),
    (49, 4),
    (65, 5),
    (97, 5),
    (129, 6),
    (193, 6),
    (257, 7),
    (385, 7),
    (513, 8),
    (769, 8),
    (1025, 9),
    (1537, 9),
    (2049, 10),
    (3073, 10),
    (4097, 11),
    (6145, 11),
    (8193, 12),
    (12289, 12),
    (16385, 13),
    (24577, 13),
];

const CRC32_TABLE: [u32; 256] = build_crc32_table();

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    let mut init = 0u32;
    while i < 256 {
        let mut c = init;
        let mut k = 0;
        while k < 8 {
            c = if (c & 1) != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
        init += 1;
    }
    table
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        let idx = usize::from(crc.to_le_bytes()[0] ^ byte);
        crc = CRC32_TABLE[idx] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use super::{MIN_COMPRESS_BYTES, accepts_gzip, compress, compress_if_smaller};

    fn gunzip(bytes: &[u8]) -> Vec<u8> {
        let mut child = Command::new("gzip")
            .args(["-d", "-c"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("gzip CLI available in test environment");
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(bytes)
            .expect("write gzip bytes");
        let output = child.wait_with_output().expect("wait gzip");
        assert!(
            output.status.success(),
            "gzip -dc failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[test]
    fn accept_encoding_negotiation_handles_q_values_case_and_wildcards() {
        assert!(!accepts_gzip(None));
        assert!(!accepts_gzip(Some("")));
        assert!(!accepts_gzip(Some("br, deflate, zstd")));
        assert!(accepts_gzip(Some("gzip, deflate, br, zstd")));
        assert!(accepts_gzip(Some("x-gzip")));
        assert!(accepts_gzip(Some("GZIP;q=0.5, deflate")));
        assert!(!accepts_gzip(Some("gzip;q=0, deflate;q=1")));
        assert!(!accepts_gzip(Some("gzip; q=0.000, *;q=1")));
        assert!(accepts_gzip(Some("br, *;q=0.8")));
    }

    #[test]
    fn compress_produces_valid_rfc1952_streams_for_boundary_and_large_payloads() {
        assert_eq!(compress_if_smaller(b"tiny payload"), None);
        assert!(compress_if_smaller(&[0u8; MIN_COMPRESS_BYTES - 1]).is_none());

        let mut mixed_120k = Vec::with_capacity(120_000);
        for i in 0..1200 {
            let entry = format!(
                "{{\"id\":{i},\"state\":\"active\",\"mission\":\"性能调优 任务清单 #{i}\",\"free_bytes\":44206051328}}\n"
            );
            mixed_120k.extend_from_slice(entry.as_bytes());
        }

        let boundary_max_dist =
            [b"ABCD".as_slice(), &vec![b'x'; 32_764], b"ABCD".as_slice()].concat();
        let boundary_max_match = b"Z".repeat(600);
        let mut x = 0x9e37_79b9u32;
        let mut pseudorandom = Vec::with_capacity(2048);
        for _ in 0..2048 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            pseudorandom.push(x.to_le_bytes()[3]);
        }

        for case in [
            Vec::new(),
            vec![b'a'],
            "中文UTF-8混合JSON内容".repeat(40).into_bytes(),
            boundary_max_dist,
            boundary_max_match,
            pseudorandom.clone(),
            mixed_120k.clone(),
        ] {
            let gz = compress(&case);
            assert_eq!(&gz[..3], &[0x1f, 0x8b, 0x08], "RFC 1952 gzip header");
            let decoded = gunzip(&gz);
            assert_eq!(decoded, case, "roundtrip mismatch for {} B", case.len());
        }

        let shrunk = compress_if_smaller(&mixed_120k).expect("JSON shrinks well");
        assert!(
            shrunk.len() * 5 < mixed_120k.len(),
            "expected >5x compression on JSON"
        );
        assert_eq!(
            compress_if_smaller(&pseudorandom),
            None,
            "incompressible data skipped"
        );
    }
}
