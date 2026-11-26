//! FastCDC v1 content-defined chunking and `spawn-chunks-v1` index storage
//! (`D6 Phase 0` CAS format freeze).
//!
//! Logical identity is always the full-file SHA-256 digest; chunking only
//! lives on the physical layer under `chunk-index/sha256/<2 hex>/<file hex>` for
//! files larger than [`CHUNK_MAX`] (4 MiB).

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::sync::OnceLock;

use sha2::{Digest as _, Sha256};

use crate::digest::Hasher;
use crate::store::remove_missing_ok;
use crate::{CasError, Digest, Store, sha256_of};

/// Chunking algorithm identifier for FastCDC v1 (`spawn-chunks-v1`).
pub const CHUNKER_FASTCDC_V1: u8 = 1;

/// Minimum chunk size in bytes (256 KiB).
pub const CHUNK_MIN: usize = 262_144;

/// Target average chunk size in bytes (1 MiB).
pub const CHUNK_AVG: usize = 1_048_576;

/// Maximum chunk size and single-object write cap in bytes (4 MiB).
pub const CHUNK_MAX: usize = 4_194_304;

/// Magic header for `spawn-chunks-v1` index objects.
pub const CHUNKS_MAGIC: &[u8] = b"spawn-chunks-v1\n";

const MASK_S: u64 = 0xFFFF_FC00_0000_0000;
const MASK_L: u64 = 0xFFFF_C000_0000_0000;
const CHUNK_HEADER_LEN: usize = 16 + 32 + 8 + 1 + 4;
const CHUNK_ENTRY_LEN: usize = 32 + 4;

/// Deterministic 256-entry Gear lookup table (`sha256("spawn-fastcdc-v1" || byte)`
/// first 8 bytes, big-endian `u64`).
pub fn fastcdc_gear_table() -> &'static [u64; 256] {
    static TABLE: OnceLock<[u64; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut gear = [0_u64; 256];
        for (byte, slot) in gear.iter_mut().enumerate() {
            let mut hasher = Sha256::new();
            hasher.update(b"spawn-fastcdc-v1");
            let b = u8::try_from(byte).expect("0..256 fits in u8");
            hasher.update([b]);
            let digest = hasher.finalize();
            let first8: [u8; 8] = digest[..8].try_into().expect("sha256 is 32 bytes");
            *slot = u64::from_be_bytes(first8);
        }
        gear
    })
}

/// Compute the next FastCDC chunk length inside `buf` (`1..=CHUNK_MAX`).
///
/// The caller must pass either the full remaining stream (if `<= CHUNK_MAX`)
/// or at least `CHUNK_MAX` bytes so cut-point evaluation matches full-buffer
/// chunking bit-for-bit.
pub fn next_chunk_len(buf: &[u8]) -> usize {
    if buf.len() <= CHUNK_MIN {
        return buf.len();
    }
    let gear = fastcdc_gear_table();
    let limit = buf.len().min(CHUNK_MAX);
    let mut fp: u64 = 0;
    for (offset, &byte) in buf[CHUNK_MIN..limit].iter().enumerate() {
        fp = (fp << 1).wrapping_add(gear[usize::from(byte)]);
        let len = CHUNK_MIN + offset + 1;
        if len == CHUNK_MAX {
            return CHUNK_MAX;
        }
        if len < CHUNK_AVG && (fp & MASK_S) == 0 {
            return len;
        }
        if len >= CHUNK_AVG && (fp & MASK_L) == 0 {
            return len;
        }
    }
    limit
}

/// One physical chunk inside a [`ChunkIndex`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRef {
    /// Whole-chunk SHA-256 CAS object address.
    pub id: Digest,
    /// Byte length of this chunk (`1..=CHUNK_MAX`).
    pub len: u32,
}

/// Physical chunk index for a blob larger than [`CHUNK_MAX`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkIndex {
    /// Full-file SHA-256 identity.
    pub file_id: Digest,
    /// Total file size in bytes.
    pub file_size: u64,
    /// Chunker identifier ([`CHUNKER_FASTCDC_V1`]).
    pub chunker: u8,
    /// Ordered chunks concatenating to `file_id`.
    pub chunks: Vec<ChunkRef>,
}

/// Summary returned by [`Store::put_blob`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBlob {
    /// Full-file SHA-256 identity.
    pub digest: Digest,
    /// Byte length stored.
    pub size: u64,
    /// Whether the blob exceeded [`CHUNK_MAX`] and produced a `spawn-chunks-v1` index.
    pub chunked: bool,
    /// Number of physical chunks (`1` when unchunked).
    pub chunk_count: usize,
}

fn bad_format(reason: &'static str) -> CasError {
    CasError::BadFormat(reason)
}

impl ChunkIndex {
    /// Encode this chunk index into canonical `spawn-chunks-v1` bytes.
    pub fn encode(&self) -> Result<Vec<u8>, CasError> {
        if self.chunker != CHUNKER_FASTCDC_V1 {
            return Err(bad_format("unsupported chunker version"));
        }
        if self.chunks.is_empty() {
            return Err(bad_format("chunk index must contain at least one chunk"));
        }
        let count = u32::try_from(self.chunks.len()).map_err(|_| bad_format("too many chunks"))?;
        let mut sum: u64 = 0;
        for chunk in &self.chunks {
            let len =
                usize::try_from(chunk.len).map_err(|_| bad_format("chunk length overflow"))?;
            if len == 0 || len > CHUNK_MAX {
                return Err(bad_format("chunk length out of bounds"));
            }
            sum = sum
                .checked_add(u64::from(chunk.len))
                .ok_or_else(|| bad_format("chunk byte sum overflow"))?;
        }
        if sum != self.file_size {
            return Err(bad_format("chunk lengths do not sum to file_size"));
        }

        let total_bytes = CHUNK_HEADER_LEN
            .checked_add(self.chunks.len().saturating_mul(CHUNK_ENTRY_LEN))
            .ok_or_else(|| bad_format("chunk index size overflow"))?;
        let mut out = Vec::with_capacity(total_bytes);
        out.extend_from_slice(CHUNKS_MAGIC);
        out.extend_from_slice(&self.file_id.as_bytes());
        out.extend_from_slice(&self.file_size.to_be_bytes());
        out.push(self.chunker);
        out.extend_from_slice(&count.to_be_bytes());
        for chunk in &self.chunks {
            out.extend_from_slice(&chunk.id.as_bytes());
            out.extend_from_slice(&chunk.len.to_be_bytes());
        }
        Ok(out)
    }

    /// Decode and strictly validate `spawn-chunks-v1` bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, CasError> {
        if bytes.len() < CHUNK_HEADER_LEN || &bytes[..CHUNKS_MAGIC.len()] != CHUNKS_MAGIC {
            return Err(bad_format("missing spawn-chunks-v1 header"));
        }
        let mut pos = CHUNKS_MAGIC.len();
        let file_id_raw: [u8; 32] = bytes[pos..pos + 32]
            .try_into()
            .map_err(|_| bad_format("truncated file_id"))?;
        pos += 32;
        let file_size = u64::from_be_bytes(
            bytes[pos..pos + 8]
                .try_into()
                .map_err(|_| bad_format("truncated file_size"))?,
        );
        pos += 8;
        let chunker = bytes[pos];
        pos += 1;
        if chunker != CHUNKER_FASTCDC_V1 {
            return Err(bad_format("unsupported chunker version"));
        }
        let count = usize::try_from(u32::from_be_bytes(
            bytes[pos..pos + 4]
                .try_into()
                .map_err(|_| bad_format("truncated chunk count"))?,
        ))
        .map_err(|_| bad_format("chunk count overflow"))?;
        pos += 4;
        if count == 0 {
            return Err(bad_format("empty chunk index"));
        }
        let expected_len = count
            .checked_mul(CHUNK_ENTRY_LEN)
            .and_then(|n| CHUNK_HEADER_LEN.checked_add(n))
            .ok_or_else(|| bad_format("chunk index length overflow"))?;
        if bytes.len() != expected_len {
            return Err(bad_format("chunk index byte length mismatch"));
        }

        let mut chunks = Vec::with_capacity(count);
        let mut sum: u64 = 0;
        for _ in 0..count {
            let chunk_raw: [u8; 32] = bytes[pos..pos + 32]
                .try_into()
                .map_err(|_| bad_format("truncated chunk digest"))?;
            pos += 32;
            let len = u32::from_be_bytes(
                bytes[pos..pos + 4]
                    .try_into()
                    .map_err(|_| bad_format("truncated chunk length"))?,
            );
            pos += 4;
            let len_usize =
                usize::try_from(len).map_err(|_| bad_format("chunk length overflow"))?;
            if len_usize == 0 || len_usize > CHUNK_MAX {
                return Err(bad_format("chunk length out of bounds"));
            }
            sum = sum
                .checked_add(u64::from(len))
                .ok_or_else(|| bad_format("chunk sum overflow"))?;
            chunks.push(ChunkRef {
                id: Digest::from_raw_sha256(chunk_raw),
                len,
            });
        }
        if sum != file_size {
            return Err(bad_format("chunk lengths do not match file_size"));
        }
        Ok(Self {
            file_id: Digest::from_raw_sha256(file_id_raw),
            file_size,
            chunker,
            chunks,
        })
    }
}

fn read_up_to(reader: &mut impl Read, dst: &mut [u8]) -> io::Result<(usize, bool)> {
    let mut filled = 0;
    while filled < dst.len() {
        match reader.read(&mut dst[filled..]) {
            Ok(0) => return Ok((filled, true)),
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok((filled, false))
}

impl Store {
    /// Store any blob stream in `namespace` under D6 Phase 0 rules:
    /// `<= 4 MiB` stored whole under `objects/sha256/`;
    /// `> 4 MiB` split deterministically with FastCDC v1 into `<= 4 MiB` chunk
    /// blobs plus a `spawn-chunks-v1` index at `chunk-index/sha256/`.
    pub fn put_blob(
        &self,
        namespace: &str,
        reader: &mut impl Read,
    ) -> Result<StoredBlob, CasError> {
        let mut buf = vec![0_u8; CHUNK_MAX + 1];
        let (mut available, mut eof) = read_up_to(reader, &mut buf)?;
        if available <= CHUNK_MAX && eof {
            let slice = &buf[..available];
            let digest = sha256_of(slice);
            self.put_hashed_bytes(namespace, slice, &digest)?;
            return Ok(StoredBlob {
                digest,
                size: u64::try_from(available).map_err(|_| bad_format("blob size overflow"))?,
                chunked: false,
                chunk_count: 1,
            });
        }

        let mut whole_hasher = Hasher::from_sha256();
        let mut total_size: u64 = 0;
        let mut chunks = Vec::new();
        while available > 0 {
            if !eof && available < CHUNK_MAX {
                let (read_more, hit_eof) = read_up_to(reader, &mut buf[available..])?;
                available += read_more;
                eof = hit_eof;
            }
            let view = if available > CHUNK_MAX {
                &buf[..CHUNK_MAX]
            } else {
                &buf[..available]
            };
            let cut = next_chunk_len(view);
            let chunk_bytes = &buf[..cut];
            let chunk_digest = sha256_of(chunk_bytes);
            self.put_hashed_bytes(namespace, chunk_bytes, &chunk_digest)?;
            whole_hasher.update(chunk_bytes);
            total_size = total_size
                .checked_add(u64::try_from(cut).map_err(|_| bad_format("chunk len overflow"))?)
                .ok_or_else(|| bad_format("blob size overflow"))?;
            chunks.push(ChunkRef {
                id: chunk_digest,
                len: u32::try_from(cut).map_err(|_| bad_format("chunk len fits in u32"))?,
            });
            buf.copy_within(cut..available, 0);
            available -= cut;
            if !eof && available <= CHUNK_MAX {
                let (read_more, hit_eof) = read_up_to(reader, &mut buf[available..])?;
                available += read_more;
                eof = hit_eof;
            }
        }

        let file_id = whole_hasher.finish();
        let index = ChunkIndex {
            file_id: file_id.clone(),
            file_size: total_size,
            chunker: CHUNKER_FASTCDC_V1,
            chunks,
        };
        let encoded = index.encode()?;
        if encoded.len() > CHUNK_MAX {
            return Err(CasError::TooLarge {
                kind: "chunk index",
                size: encoded.len(),
                max: CHUNK_MAX,
            });
        }
        let (mut temp, staged) = self.temp_file("cidx")?;
        let write_result = temp
            .write_all(&encoded)
            .and_then(|()| temp.flush())
            .and_then(|()| temp.set_permissions(fs::Permissions::from_mode(0o444)));
        drop(temp);
        if let Err(error) = write_result {
            remove_missing_ok(&staged)?;
            return Err(error.into());
        }
        self.chunk_index_dir(&file_id)?;
        let target = self.chunk_index_path(&file_id);
        let link_result = fs::hard_link(&staged, &target);
        let _ = remove_missing_ok(&staged);
        match link_result {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let chunk_count = index.chunks.len();
        Ok(StoredBlob {
            digest: file_id,
            size: total_size,
            chunked: true,
            chunk_count,
        })
    }

    /// Read and validate the [`ChunkIndex`] for a chunked blob `file_digest`.
    pub fn read_chunk_index(&self, file_digest: &Digest) -> Result<ChunkIndex, CasError> {
        let bytes = self.read_plain_bytes(&self.chunk_index_path(file_digest))?;
        let index = ChunkIndex::decode(&bytes)?;
        if &index.file_id != file_digest {
            return Err(CasError::CorruptIndex {
                want: file_digest.clone(),
                reason: "index file_id mismatches requested digest",
            });
        }
        Ok(index)
    }

    /// Verify and stream a blob (`<= 4 MiB` whole object, `> 4 MiB` chunked blob,
    /// or legacy pre-D6 oversized whole object) to `writer`, returning bytes written.
    pub fn stream_blob(&self, digest: &Digest, writer: &mut impl Write) -> Result<u64, CasError> {
        if self.has(digest) {
            let meta = fs::symlink_metadata(self.object_path(digest))?;
            if meta.len() <= u64::try_from(CHUNK_MAX).unwrap_or(u64::MAX) {
                let bytes = self.read(digest)?;
                writer.write_all(&bytes)?;
                return u64::try_from(bytes.len()).map_err(|_| bad_format("size overflow"));
            }
            // Legacy pre-D6 oversized whole object: verify first before streaming.
            self.verify(digest)?;
            let mut file = self.open_object(digest)?;
            let copied = io::copy(&mut file, writer)?;
            return Ok(copied);
        }

        let index = self.read_chunk_index(digest)?;
        let mut whole_hasher = Hasher::from_sha256();
        let mut written: u64 = 0;
        for chunk in &index.chunks {
            let bytes = self.read(&chunk.id)?;
            if u64::try_from(bytes.len()).ok() != Some(u64::from(chunk.len)) {
                return Err(CasError::CorruptIndex {
                    want: digest.clone(),
                    reason: "chunk length mismatches index entry",
                });
            }
            whole_hasher.update(&bytes);
            writer.write_all(&bytes)?;
            written = written
                .checked_add(u64::from(chunk.len))
                .ok_or_else(|| bad_format("stream size overflow"))?;
        }
        let got_digest = whole_hasher.finish();
        if &got_digest != digest {
            return Err(CasError::Mismatch {
                want: digest.clone(),
                got: got_digest.to_string(),
            });
        }
        if written != index.file_size {
            return Err(CasError::CorruptIndex {
                want: digest.clone(),
                reason: "streamed byte total mismatches index file_size",
            });
        }
        Ok(written)
    }

    /// Verify and read an entire blob (chunked or unchunked) into memory.
    pub fn read_blob(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        let mut out = Vec::new();
        self.stream_blob(digest, &mut out)?;
        Ok(out)
    }
}

impl Hasher {
    pub(crate) fn from_sha256() -> Self {
        crate::Algo::Sha256.hasher()
    }
}
