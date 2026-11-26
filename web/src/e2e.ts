// Pure WebCrypto implementation of the frozen `spawn-e2e-v2` channel and
// `pkcs8-ed25519-v2` key format matrix (`docs/spawn-e2e-v2.md`).
// Uses only standard Web APIs (`globalThis.crypto.subtle`, `TextEncoder`,
// `Uint8Array`) with zero Node-only APIs or external dependencies.

export const REQUEST_VERSION = "spawn-e2e-v2" as const;
export const REPLY_VERSION = "spawn-e2e-reply-v2" as const;

const REQUEST_HEADER_PREFIX = "spawn-e2e-req-v2";
const REQUEST_INFO_PREFIX = "spawn-e2e-v2:request\n";
const REPLY_INFO_PREFIX = "spawn-e2e-v2:reply\n";

/** Maximum allowed clock skew and replay cache window (120 seconds). */
export const DEFAULT_MAX_SKEW_MS = 120_000;

export type E2EErrorCode =
  | "invalid_pkcs8"
  | "invalid_spki"
  | "invalid_envelope"
  | "untrusted_sender"
  | "bad_signature"
  | "clock_skew"
  | "replay"
  | "low_order_point"
  | "decrypt_failed";

export class E2EError extends Error {
  readonly code: E2EErrorCode;

  constructor(code: E2EErrorCode, message: string) {
    super(message);
    this.name = "E2EError";
    this.code = code;
  }
}

export type Ed25519Pkcs8Format = "v1" | "v2_ring" | "v2_rfc8410";

export interface ParsedEd25519Pkcs8 {
  format: Ed25519Pkcs8Format;
  version: 1 | 2;
  seed: Uint8Array;
  embeddedPublicKey: Uint8Array | null;
}

export interface Ed25519Identity {
  format: Ed25519Pkcs8Format;
  version: 1 | 2;
  seed: Uint8Array;
  privateKey: CryptoKey;
  publicKey: CryptoKey;
  publicKeyBytes: Uint8Array;
  keyId: string;
  spkiDer: Uint8Array;
}

export interface X25519KeyPair {
  privateKey: CryptoKey;
  publicKey: CryptoKey;
  publicKeyBytes: Uint8Array;
}

export interface RequestEnvelope {
  v: string;
  from: string;
  from_pub: string;
  to: string;
  epoch: number;
  to_x25519: string;
  channel_id: string;
  at_ms: number;
  eph: string;
  req_nonce: string;
  salt: string;
  ct: string;
  sig: string;
}

export interface ReplyEnvelope {
  v: string;
  from: string;
  to: string;
  channel_id: string;
  req_nonce: string;
  reply_nonce: string;
  reply_salt: string;
  ct: string;
}

export interface SealRequestParams {
  sender: Pick<Ed25519Identity, "privateKey" | "publicKeyBytes" | "keyId">;
  responderId: string;
  responderX25519Pub: Uint8Array;
  epoch: number;
  channelId: string;
  atMs: number;
  plaintext: Uint8Array;
  /** Deterministic overrides reserved for test vectors. */
  ephemeralSeed?: Uint8Array;
  reqNonce?: Uint8Array;
  salt?: Uint8Array;
}

export interface OpenRequestOptions {
  nowMs: number;
  maxSkewMs?: number;
  replay?: ReplayCache;
  allowSender?: (fromId: string) => boolean;
  responderId?: string;
}

const PKCS8_V1_PREFIX = Uint8Array.from([
  0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
]);
const PKCS8_V2_BODY = Uint8Array.from([
  0x02, 0x01, 0x01, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
]);
const PKCS8_V2_RING_TAG = Uint8Array.from([0x81, 0x21, 0x00]);
const PKCS8_V2_RFC8410_TAG = Uint8Array.from([0xa1, 0x23, 0x03, 0x21, 0x00]);
const ED25519_SPKI_PREFIX = Uint8Array.from([
  0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
]);
const X25519_PKCS8_V1_PREFIX = Uint8Array.from([
  0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x6e, 0x04, 0x22, 0x04, 0x20,
]);

const B64U_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const B64U_LOOKUP = (() => {
  const table = new Int16Array(256).fill(-1);
  for (let i = 0; i < B64U_ALPHABET.length; i += 1) {
    table[B64U_ALPHABET.charCodeAt(i)] = i;
  }
  return table;
})();

const textEncoder = new TextEncoder();

function subtle(): SubtleCrypto {
  const cryptoObj = globalThis.crypto;
  if (!cryptoObj || !cryptoObj.subtle) {
    throw new E2EError("invalid_envelope", "WebCrypto crypto.subtle is unavailable");
  }
  return cryptoObj.subtle;
}

function randomBytes(length: number): Uint8Array {
  const out = new Uint8Array(length);
  globalThis.crypto.getRandomValues(out);
  return out;
}

function toArrayBuffer(bytes: Uint8Array): ArrayBuffer {
  const copy = new Uint8Array(bytes.byteLength);
  copy.set(bytes);
  return copy.buffer;
}

function bytesEqualRange(a: Uint8Array, aOffset: number, b: Uint8Array, len: number): boolean {
  if (a.length < aOffset + len || b.length < len) {
    return false;
  }
  let diff = 0;
  for (let i = 0; i < len; i += 1) {
    diff |= a[aOffset + i]! ^ b[i]!;
  }
  return diff === 0;
}

export function constantTimeEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) {
    return false;
  }
  let diff = 0;
  for (let i = 0; i < a.length; i += 1) {
    diff |= a[i]! ^ b[i]!;
  }
  return diff === 0;
}

function concatBytes(...parts: Uint8Array[]): Uint8Array {
  let total = 0;
  for (const part of parts) {
    total += part.length;
  }
  const out = new Uint8Array(total);
  let offset = 0;
  for (const part of parts) {
    out.set(part, offset);
    offset += part.length;
  }
  return out;
}

/** Encode bytes using strict unpadded RFC 4648 base64url. */
export function encodeBase64Url(bytes: Uint8Array): string {
  let out = "";
  let i = 0;
  while (i + 3 <= bytes.length) {
    const n = (bytes[i]! << 16) | (bytes[i + 1]! << 8) | bytes[i + 2]!;
    out +=
      B64U_ALPHABET[(n >>> 18) & 0x3f]! +
      B64U_ALPHABET[(n >>> 12) & 0x3f]! +
      B64U_ALPHABET[(n >>> 6) & 0x3f]! +
      B64U_ALPHABET[n & 0x3f]!;
    i += 3;
  }
  const rem = bytes.length - i;
  if (rem === 1) {
    const n = bytes[i]!;
    out += B64U_ALPHABET[n >>> 2]! + B64U_ALPHABET[(n & 0x03) << 4]!;
  } else if (rem === 2) {
    const n = (bytes[i]! << 8) | bytes[i + 1]!;
    out +=
      B64U_ALPHABET[n >>> 10]! +
      B64U_ALPHABET[(n >>> 4) & 0x3f]! +
      B64U_ALPHABET[(n & 0x0f) << 2]!;
  }
  return out;
}

/** Decode strict unpadded base64url, rejecting padding, non-alphabet chars, and non-zero unused bits. */
export function decodeBase64Url(encoded: string, expectedLength?: number): Uint8Array {
  if (typeof encoded !== "string" || encoded.length === 0) {
    throw new E2EError("invalid_envelope", "empty base64url field");
  }
  const rem = encoded.length % 4;
  if (rem === 1) {
    throw new E2EError("invalid_envelope", "non-canonical base64url length");
  }
  const sextets = new Uint8Array(encoded.length);
  for (let i = 0; i < encoded.length; i += 1) {
    const code = encoded.charCodeAt(i);
    const val = code < 256 ? B64U_LOOKUP[code]! : -1;
    if (val < 0) {
      throw new E2EError("invalid_envelope", "non-canonical base64url character");
    }
    sextets[i] = val;
  }
  if (rem === 2 && (sextets[encoded.length - 1]! & 0x0f) !== 0) {
    throw new E2EError("invalid_envelope", "non-canonical base64url trailing bits");
  }
  if (rem === 3 && (sextets[encoded.length - 1]! & 0x03) !== 0) {
    throw new E2EError("invalid_envelope", "non-canonical base64url trailing bits");
  }
  const outLen = Math.floor((encoded.length * 3) / 4);
  if (expectedLength !== undefined && outLen !== expectedLength) {
    throw new E2EError("invalid_envelope", `decoded ${outLen} bytes, expected ${expectedLength}`);
  }
  const out = new Uint8Array(outLen);
  let si = 0;
  let oi = 0;
  while (si + 4 <= sextets.length) {
    const n =
      (sextets[si]! << 18) | (sextets[si + 1]! << 12) | (sextets[si + 2]! << 6) | sextets[si + 3]!;
    out[oi++] = (n >>> 16) & 0xff;
    out[oi++] = (n >>> 8) & 0xff;
    out[oi++] = n & 0xff;
    si += 4;
  }
  if (rem === 2) {
    const n = (sextets[si]! << 6) | sextets[si + 1]!;
    out[oi++] = (n >>> 4) & 0xff;
  } else if (rem === 3) {
    const n = (sextets[si]! << 12) | (sextets[si + 1]! << 6) | sextets[si + 2]!;
    out[oi++] = (n >>> 10) & 0xff;
    out[oi++] = (n >>> 2) & 0xff;
  }
  return out;
}

export function bytesToHex(bytes: Uint8Array): string {
  let out = "";
  for (let i = 0; i < bytes.length; i += 1) {
    out += bytes[i]!.toString(16).padStart(2, "0");
  }
  return out;
}

export function hexToBytes(hex: string): Uint8Array {
  if (hex.length % 2 !== 0 || !/^[0-9a-fA-F]*$/.test(hex)) {
    throw new E2EError("invalid_envelope", "invalid hex string");
  }
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i += 1) {
    out[i] = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}

export async function sha256(bytes: Uint8Array): Promise<Uint8Array> {
  const digest = await subtle().digest("SHA-256", toArrayBuffer(bytes));
  return new Uint8Array(digest);
}

/** Canonical 64-hex lowercase SHA-256 digest of a raw 32-byte public key. */
export async function keyId(rawPublicKey32: Uint8Array): Promise<string> {
  if (rawPublicKey32.length !== 32) {
    throw new E2EError("invalid_spki", `expected 32-byte public key, got ${rawPublicKey32.length}`);
  }
  return bytesToHex(await sha256(rawPublicKey32));
}

/** Parse PKCS#8 Ed25519 DER across v1 (48B), v2_ring (83B), and v2_rfc8410 (85B). */
export function parseEd25519Pkcs8(der: Uint8Array): ParsedEd25519Pkcs8 {
  if (der.length === 48) {
    if (!bytesEqualRange(der, 0, PKCS8_V1_PREFIX, 16)) {
      throw new E2EError("invalid_pkcs8", "unexpected Ed25519 PKCS#8 v1 header or OID");
    }
    return {
      format: "v1",
      version: 1,
      seed: der.slice(16, 48),
      embeddedPublicKey: null,
    };
  }
  if (der.length === 83) {
    if (der[0] !== 0x30 || der[1] !== 0x51 || !bytesEqualRange(der, 2, PKCS8_V2_BODY, 14)) {
      throw new E2EError("invalid_pkcs8", "unexpected Ed25519 PKCS#8 v2_ring header or OID");
    }
    if (!bytesEqualRange(der, 48, PKCS8_V2_RING_TAG, 3)) {
      throw new E2EError("invalid_pkcs8", "unexpected Ed25519 PKCS#8 v2_ring public key tag");
    }
    return {
      format: "v2_ring",
      version: 2,
      seed: der.slice(16, 48),
      embeddedPublicKey: der.slice(51, 83),
    };
  }
  if (der.length === 85) {
    if (der[0] !== 0x30 || der[1] !== 0x53 || !bytesEqualRange(der, 2, PKCS8_V2_BODY, 14)) {
      throw new E2EError("invalid_pkcs8", "unexpected Ed25519 PKCS#8 v2_rfc8410 header or OID");
    }
    if (!bytesEqualRange(der, 48, PKCS8_V2_RFC8410_TAG, 5)) {
      throw new E2EError("invalid_pkcs8", "unexpected Ed25519 PKCS#8 v2_rfc8410 public key tag");
    }
    return {
      format: "v2_rfc8410",
      version: 2,
      seed: der.slice(16, 48),
      embeddedPublicKey: der.slice(53, 85),
    };
  }
  throw new E2EError("invalid_pkcs8", `unsupported Ed25519 PKCS#8 length ${der.length}`);
}

export function encodeEd25519Pkcs8V1(seed32: Uint8Array): Uint8Array {
  if (seed32.length !== 32) {
    throw new E2EError("invalid_pkcs8", `expected 32-byte seed, got ${seed32.length}`);
  }
  return concatBytes(PKCS8_V1_PREFIX, seed32);
}

export function encodeEd25519Pkcs8V2Ring(seed32: Uint8Array, pub32: Uint8Array): Uint8Array {
  if (seed32.length !== 32 || pub32.length !== 32) {
    throw new E2EError("invalid_pkcs8", "v2_ring encoding requires 32-byte seed and public key");
  }
  return concatBytes(Uint8Array.from([0x30, 0x51]), PKCS8_V2_BODY, seed32, PKCS8_V2_RING_TAG, pub32);
}

export function encodeEd25519Pkcs8V2Rfc8410(seed32: Uint8Array, pub32: Uint8Array): Uint8Array {
  if (seed32.length !== 32 || pub32.length !== 32) {
    throw new E2EError("invalid_pkcs8", "v2_rfc8410 encoding requires 32-byte seed and public key");
  }
  return concatBytes(Uint8Array.from([0x30, 0x53]), PKCS8_V2_BODY, seed32, PKCS8_V2_RFC8410_TAG, pub32);
}

export function encodeEd25519Spki(pub32: Uint8Array): Uint8Array {
  if (pub32.length !== 32) {
    throw new E2EError("invalid_spki", `expected 32-byte public key, got ${pub32.length}`);
  }
  return concatBytes(ED25519_SPKI_PREFIX, pub32);
}

export function parseEd25519Spki(der: Uint8Array): Uint8Array {
  if (der.length !== 44 || !bytesEqualRange(der, 0, ED25519_SPKI_PREFIX, 12)) {
    throw new E2EError("invalid_spki", "invalid Ed25519 SPKI DER encoding");
  }
  return der.slice(12, 44);
}

/**
 * Import an Ed25519 private key from any canonical `pkcs8-ed25519-v2` DER encoding
 * (`v1`, `v2_ring`, or `v2_rfc8410`) via pure WebCrypto `crypto.subtle`.
 */
export async function importEd25519Pkcs8(
  der: Uint8Array,
  options: { extractable?: boolean } = {},
): Promise<Ed25519Identity> {
  const parsed = parseEd25519Pkcs8(der);
  const v1Der = encodeEd25519Pkcs8V1(parsed.seed);
  // Import temporarily extractable when needed to derive public key via JWK export;
  // if caller requested non-extractable, re-import a fresh non-extractable CryptoKey handle.
  const extractable = options.extractable ?? true;
  let privateKey = await subtle().importKey("pkcs8", toArrayBuffer(v1Der), { name: "Ed25519" }, true, ["sign"]);
  const jwk = await subtle().exportKey("jwk", privateKey);
  if (typeof jwk.x !== "string") {
    throw new E2EError("invalid_pkcs8", "exported Ed25519 JWK lacks public key x");
  }
  const publicKeyBytes = decodeBase64Url(jwk.x, 32);
  if (parsed.embeddedPublicKey !== null && !constantTimeEqual(publicKeyBytes, parsed.embeddedPublicKey)) {
    throw new E2EError("invalid_pkcs8", `${parsed.format} embedded public key mismatch`);
  }
  if (!extractable) {
    privateKey = await subtle().importKey("pkcs8", toArrayBuffer(v1Der), { name: "Ed25519" }, false, ["sign"]);
  }
  const publicKey = await subtle().importKey(
    "raw",
    toArrayBuffer(publicKeyBytes),
    { name: "Ed25519" },
    true,
    ["verify"],
  );
  return {
    format: parsed.format,
    version: parsed.version,
    seed: parsed.seed,
    privateKey,
    publicKey,
    publicKeyBytes,
    keyId: await keyId(publicKeyBytes),
    spkiDer: encodeEd25519Spki(publicKeyBytes),
  };
}

export async function signEd25519(privateKey: CryptoKey, message: Uint8Array): Promise<Uint8Array> {
  const sig = await subtle().sign({ name: "Ed25519" }, privateKey, toArrayBuffer(message));
  return new Uint8Array(sig);
}

export async function verifyEd25519(
  publicKeyBytes: Uint8Array,
  message: Uint8Array,
  signature: Uint8Array,
): Promise<boolean> {
  if (publicKeyBytes.length !== 32 || signature.length !== 64) {
    return false;
  }
  try {
    const key = await subtle().importKey(
      "raw",
      toArrayBuffer(publicKeyBytes),
      { name: "Ed25519" },
      false,
      ["verify"],
    );
    return await subtle().verify({ name: "Ed25519" }, key, toArrayBuffer(signature), toArrayBuffer(message));
  } catch {
    return false;
  }
}

export async function importX25519PrivateKey(
  seed32: Uint8Array,
  options: { extractable?: boolean } = {},
): Promise<X25519KeyPair> {
  if (seed32.length !== 32) {
    throw new E2EError("invalid_envelope", `expected 32-byte X25519 seed, got ${seed32.length}`);
  }
  const v1Der = concatBytes(X25519_PKCS8_V1_PREFIX, seed32);
  const extractable = options.extractable ?? true;
  let privateKey = await subtle().importKey("pkcs8", toArrayBuffer(v1Der), { name: "X25519" }, true, ["deriveBits"]);
  const jwk = await subtle().exportKey("jwk", privateKey);
  if (typeof jwk.x !== "string") {
    throw new E2EError("invalid_envelope", "exported X25519 JWK lacks public key x");
  }
  const publicKeyBytes = decodeBase64Url(jwk.x, 32);
  if (!extractable) {
    privateKey = await subtle().importKey(
      "pkcs8",
      toArrayBuffer(v1Der),
      { name: "X25519" },
      false,
      ["deriveBits"],
    );
  }
  const publicKey = await subtle().importKey(
    "raw",
    toArrayBuffer(publicKeyBytes),
    { name: "X25519" },
    true,
    [],
  );
  return { privateKey, publicKey, publicKeyBytes };
}

export async function generateX25519KeyPair(
  options: { extractable?: boolean } = {},
): Promise<X25519KeyPair> {
  const generated = (await subtle().generateKey(
    { name: "X25519" },
    options.extractable ?? false,
    ["deriveBits"],
  )) as CryptoKeyPair;
  const rawPub = await subtle().exportKey("raw", generated.publicKey);
  return {
    privateKey: generated.privateKey,
    publicKey: generated.publicKey,
    publicKeyBytes: new Uint8Array(rawPub),
  };
}

/** RFC 7748 X25519 agreement via WebCrypto, rejecting low-order points and all-zero secrets. */
export async function x25519Agree(privateKey: CryptoKey, peerPublicKeyBytes: Uint8Array): Promise<Uint8Array> {
  if (peerPublicKeyBytes.length !== 32) {
    throw new E2EError("low_order_point", `X25519 public key must be 32 bytes, got ${peerPublicKeyBytes.length}`);
  }
  let shared: Uint8Array;
  try {
    const peerKey = await subtle().importKey(
      "raw",
      toArrayBuffer(peerPublicKeyBytes),
      { name: "X25519" },
      false,
      [],
    );
    const bits = await subtle().deriveBits(
      { name: "X25519", public: peerKey },
      privateKey,
      256,
    );
    shared = new Uint8Array(bits);
  } catch (err) {
    throw new E2EError("low_order_point", `X25519 agreement rejected: ${(err as Error).message}`);
  }
  let acc = 0;
  for (let i = 0; i < shared.length; i += 1) {
    acc |= shared[i]!;
  }
  if (shared.length !== 32 || acc === 0) {
    throw new E2EError("low_order_point", "rejected all-zero X25519 shared secret");
  }
  return shared;
}

async function hkdfSha256(ikm: Uint8Array, salt32: Uint8Array, info: Uint8Array): Promise<Uint8Array> {
  if (salt32.length !== 32) {
    throw new E2EError("invalid_envelope", `HKDF salt must be 32 bytes, got ${salt32.length}`);
  }
  const baseKey = await subtle().importKey("raw", toArrayBuffer(ikm), "HKDF", false, ["deriveBits"]);
  const bits = await subtle().deriveBits(
    {
      name: "HKDF",
      hash: "SHA-256",
      salt: toArrayBuffer(salt32),
      info: toArrayBuffer(info),
    },
    baseKey,
    256,
  );
  return new Uint8Array(bits);
}

/** Exact `\n`-joined request transcript signed with Ed25519 and passed as AES-GCM AAD. */
export function formatRequestHeader(env: RequestEnvelope): string {
  return [
    REQUEST_HEADER_PREFIX,
    env.from,
    env.to,
    String(env.epoch),
    env.to_x25519,
    env.channel_id,
    String(env.at_ms),
    env.eph,
    env.req_nonce,
    env.salt,
  ].join("\n");
}

/** Exact HKDF-SHA256 `info` string for reply key derivation. */
export function formatReplyInfo(
  req: Pick<RequestEnvelope, "from" | "to" | "channel_id" | "req_nonce">,
  replyNonce: string,
): string {
  return (
    REPLY_INFO_PREFIX + [req.from, req.to, req.channel_id, req.req_nonce, replyNonce].join("\n")
  );
}

/** Exact `\n`-joined AES-GCM AAD for a reply envelope. */
export function formatReplyAad(reply: ReplyEnvelope): string {
  return [
    REPLY_VERSION,
    reply.from,
    reply.to,
    reply.channel_id,
    reply.req_nonce,
    reply.reply_nonce,
    reply.reply_salt,
  ].join("\n");
}

export async function deriveRequestKey(
  sharedSecret: Uint8Array,
  salt: Uint8Array,
  reqHeader: string,
): Promise<Uint8Array> {
  return hkdfSha256(sharedSecret, salt, textEncoder.encode(REQUEST_INFO_PREFIX + reqHeader));
}

export async function deriveReplyKey(
  sharedSecret: Uint8Array,
  replySalt: Uint8Array,
  req: Pick<RequestEnvelope, "from" | "to" | "channel_id" | "req_nonce">,
  replyNonce: string,
): Promise<Uint8Array> {
  return hkdfSha256(sharedSecret, replySalt, textEncoder.encode(formatReplyInfo(req, replyNonce)));
}

export async function sealAesGcm(
  keyBytes32: Uint8Array,
  nonce12: Uint8Array,
  aad: Uint8Array,
  plaintext: Uint8Array,
): Promise<Uint8Array> {
  if (keyBytes32.length !== 32 || nonce12.length !== 12) {
    throw new E2EError("invalid_envelope", "AES-256-GCM requires 32-byte key and 12-byte nonce");
  }
  const key = await subtle().importKey("raw", toArrayBuffer(keyBytes32), { name: "AES-GCM" }, false, ["encrypt"]);
  const sealed = await subtle().encrypt(
    { name: "AES-GCM", iv: toArrayBuffer(nonce12), additionalData: toArrayBuffer(aad), tagLength: 128 },
    key,
    toArrayBuffer(plaintext),
  );
  return new Uint8Array(sealed);
}

export async function openAesGcm(
  keyBytes32: Uint8Array,
  nonce12: Uint8Array,
  aad: Uint8Array,
  ciphertextAndTag: Uint8Array,
): Promise<Uint8Array> {
  if (keyBytes32.length !== 32 || nonce12.length !== 12 || ciphertextAndTag.length < 16) {
    throw new E2EError("decrypt_failed", "AES-256-GCM ciphertext or parameters invalid");
  }
  try {
    const key = await subtle().importKey("raw", toArrayBuffer(keyBytes32), { name: "AES-GCM" }, false, ["decrypt"]);
    const opened = await subtle().decrypt(
      { name: "AES-GCM", iv: toArrayBuffer(nonce12), additionalData: toArrayBuffer(aad), tagLength: 128 },
      key,
      toArrayBuffer(ciphertextAndTag),
    );
    return new Uint8Array(opened);
  } catch {
    throw new E2EError("decrypt_failed", "AES-256-GCM AEAD authentication failed");
  }
}

/** Tracks accepted `(from, req_nonce)` pairs within the skew window to prevent replay. */
export class ReplayCache {
  private readonly seen = new Map<string, number>();

  checkAndInsert(
    from: string,
    reqNonce: string,
    nowMs: number,
    windowMs: number = DEFAULT_MAX_SKEW_MS,
  ): void {
    const cutoff = nowMs - windowMs;
    for (const [key, seenAt] of this.seen) {
      if (seenAt < cutoff) {
        this.seen.delete(key);
      }
    }
    const cacheKey = `${from}\n${reqNonce}`;
    if (this.seen.has(cacheKey)) {
      throw new E2EError("replay", "replayed request nonce");
    }
    this.seen.set(cacheKey, nowMs);
  }

  get size(): number {
    return this.seen.size;
  }
}

function assertCleanTranscriptFields(values: readonly string[], label: string): void {
  for (const val of values) {
    if (typeof val !== "string" || val.length === 0 || /[\r\n]/.test(val)) {
      throw new E2EError("invalid_envelope", `invalid ${label} field in transcript`);
    }
  }
}

/** Encrypt `plaintext` into a signed `spawn-e2e-v2` RequestEnvelope using WebCrypto. */
export async function sealRequest(
  params: SealRequestParams,
): Promise<{ envelope: RequestEnvelope; sharedSecret: Uint8Array }> {
  assertCleanTranscriptFields([params.sender.keyId, params.responderId, params.channelId], "request");
  if (!Number.isSafeInteger(params.epoch) || params.epoch < 0 || !Number.isSafeInteger(params.atMs)) {
    throw new E2EError("invalid_envelope", "invalid epoch or at_ms timestamp");
  }
  if (params.responderX25519Pub.length !== 32) {
    throw new E2EError("invalid_envelope", "responder X25519 public key must be 32 bytes");
  }
  const eph = params.ephemeralSeed
    ? await importX25519PrivateKey(params.ephemeralSeed)
    : await generateX25519KeyPair();
  const reqNonce = params.reqNonce ?? randomBytes(12);
  const salt = params.salt ?? randomBytes(32);
  if (reqNonce.length !== 12 || salt.length !== 32) {
    throw new E2EError("invalid_envelope", "invalid req_nonce or salt length");
  }
  const sharedSecret = await x25519Agree(eph.privateKey, params.responderX25519Pub);
  const envelope: RequestEnvelope = {
    v: REQUEST_VERSION,
    from: params.sender.keyId,
    from_pub: encodeBase64Url(params.sender.publicKeyBytes),
    to: params.responderId,
    epoch: params.epoch,
    to_x25519: encodeBase64Url(params.responderX25519Pub),
    channel_id: params.channelId,
    at_ms: params.atMs,
    eph: encodeBase64Url(eph.publicKeyBytes),
    req_nonce: encodeBase64Url(reqNonce),
    salt: encodeBase64Url(salt),
    ct: "",
    sig: "",
  };
  const header = formatRequestHeader(envelope);
  const headerBytes = textEncoder.encode(header);
  const reqKey = await deriveRequestKey(sharedSecret, salt, header);
  const ct = await sealAesGcm(reqKey, reqNonce, headerBytes, params.plaintext);
  const sig = await signEd25519(params.sender.privateKey, headerBytes);
  envelope.ct = encodeBase64Url(ct);
  envelope.sig = encodeBase64Url(sig);
  return { envelope, sharedSecret };
}

/**
 * Verify and open a `spawn-e2e-v2` RequestEnvelope in strict pre-open order:
 * identity binding -> sender allowlist -> clock skew -> Ed25519 signature (before
 * ECDH/AEAD) -> replay cache -> X25519 agreement -> HKDF -> AES-256-GCM open.
 */
export async function openRequest(
  env: RequestEnvelope,
  responderX25519: Pick<X25519KeyPair, "privateKey" | "publicKeyBytes"> | null,
  options: OpenRequestOptions,
): Promise<{ plaintext: Uint8Array; sharedSecret: Uint8Array }> {
  if (env.v !== REQUEST_VERSION) {
    throw new E2EError("invalid_envelope", `unexpected envelope version ${env.v}`);
  }
  if (options.responderId !== undefined && env.to !== options.responderId) {
    throw new E2EError("invalid_envelope", "responder key_id mismatch");
  }
  assertCleanTranscriptFields([env.from, env.to, env.channel_id], "request");
  if (!Number.isSafeInteger(env.epoch) || env.epoch < 0 || !Number.isSafeInteger(env.at_ms)) {
    throw new E2EError("invalid_envelope", "invalid epoch or at_ms");
  }

  const fromPubBytes = decodeBase64Url(env.from_pub, 32);
  if ((await keyId(fromPubBytes)) !== env.from) {
    throw new E2EError("untrusted_sender", "from_pub digest does not match from key_id");
  }
  if (options.allowSender && !options.allowSender(env.from)) {
    throw new E2EError("untrusted_sender", "sender not allowlisted");
  }

  const maxSkewMs = options.maxSkewMs && options.maxSkewMs > 0 ? options.maxSkewMs : DEFAULT_MAX_SKEW_MS;
  if (Math.abs(options.nowMs - env.at_ms) > maxSkewMs) {
    throw new E2EError("clock_skew", "envelope timestamp outside allowed skew window");
  }

  const reqNonce = decodeBase64Url(env.req_nonce, 12);
  const salt = decodeBase64Url(env.salt, 32);
  const ephBytes = decodeBase64Url(env.eph, 32);
  const toX25519Bytes = decodeBase64Url(env.to_x25519, 32);
  const sig = decodeBase64Url(env.sig, 64);
  const ct = decodeBase64Url(env.ct);
  if (ct.length < 16) {
    throw new E2EError("invalid_envelope", "ciphertext shorter than GCM tag");
  }

  const header = formatRequestHeader(env);
  const headerBytes = textEncoder.encode(header);
  if (!(await verifyEd25519(fromPubBytes, headerBytes, sig))) {
    throw new E2EError("bad_signature", "Ed25519 signature verification failed");
  }

  if (options.replay) {
    options.replay.checkAndInsert(env.from, env.req_nonce, options.nowMs, maxSkewMs);
  }
  if (!responderX25519) {
    throw new E2EError("invalid_envelope", "missing responder X25519 key pair");
  }
  if (!constantTimeEqual(responderX25519.publicKeyBytes, toX25519Bytes)) {
    throw new E2EError("invalid_envelope", "responder X25519 prekey mismatch");
  }

  const sharedSecret = await x25519Agree(responderX25519.privateKey, ephBytes);
  const reqKey = await deriveRequestKey(sharedSecret, salt, header);
  const plaintext = await openAesGcm(reqKey, reqNonce, headerBytes, ct);
  return { plaintext, sharedSecret };
}

/** Encrypt a reply payload bound to `req` and its X25519 `sharedSecret`. */
export async function sealReply(
  req: RequestEnvelope,
  sharedSecret: Uint8Array,
  plaintext: Uint8Array,
  overrides: { replyNonce?: Uint8Array; replySalt?: Uint8Array } = {},
): Promise<ReplyEnvelope> {
  assertCleanTranscriptFields([req.from, req.to, req.channel_id, req.req_nonce], "reply");
  decodeBase64Url(req.req_nonce, 12);
  const replyNonce = overrides.replyNonce ?? randomBytes(12);
  const replySalt = overrides.replySalt ?? randomBytes(32);
  if (replyNonce.length !== 12 || replySalt.length !== 32) {
    throw new E2EError("invalid_envelope", "invalid reply_nonce or reply_salt length");
  }
  const reply: ReplyEnvelope = {
    v: REPLY_VERSION,
    from: req.to,
    to: req.from,
    channel_id: req.channel_id,
    req_nonce: req.req_nonce,
    reply_nonce: encodeBase64Url(replyNonce),
    reply_salt: encodeBase64Url(replySalt),
    ct: "",
  };
  const replyKey = await deriveReplyKey(sharedSecret, replySalt, req, reply.reply_nonce);
  const aad = textEncoder.encode(formatReplyAad(reply));
  const ct = await sealAesGcm(replyKey, replyNonce, aad, plaintext);
  reply.ct = encodeBase64Url(ct);
  return reply;
}

/** Decrypt a `spawn-e2e-reply-v2` envelope bound to `req` and `sharedSecret`. */
export async function openReply(
  req: RequestEnvelope,
  reply: ReplyEnvelope,
  sharedSecret: Uint8Array,
): Promise<Uint8Array> {
  if (reply.v !== REPLY_VERSION) {
    throw new E2EError("invalid_envelope", `unexpected reply version ${reply.v}`);
  }
  assertCleanTranscriptFields(
    [reply.from, reply.to, reply.channel_id, reply.req_nonce, reply.reply_nonce, reply.reply_salt],
    "reply",
  );
  if (
    reply.from !== req.to ||
    reply.to !== req.from ||
    reply.channel_id !== req.channel_id ||
    reply.req_nonce !== req.req_nonce
  ) {
    throw new E2EError("invalid_envelope", "reply transcript binding mismatch");
  }
  const replyNonce = decodeBase64Url(reply.reply_nonce, 12);
  const replySalt = decodeBase64Url(reply.reply_salt, 32);
  const ct = decodeBase64Url(reply.ct);
  const replyKey = await deriveReplyKey(sharedSecret, replySalt, req, reply.reply_nonce);
  const aad = textEncoder.encode(formatReplyAad(reply));
  return openAesGcm(replyKey, replyNonce, aad, ct);
}
