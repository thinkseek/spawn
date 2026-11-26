# End-to-End Encrypted Browser & Node Channel (`spawn-e2e-v2`) & PKCS#8 Ed25519 Key Format Matrix

> **Status**: Phase 0 Frozen Contract (Master Issue [#113](https://github.com/thinkseek/spawn/issues/113) Phase 0 Deliverable 3C & 20 Safeguards #2).
> **Golden Fixtures**:
> - [`crates/spawn-node-proto/testdata/spawn-e2e-v2/`](../crates/spawn-node-proto/testdata/spawn-e2e-v2/)
> - [`crates/spawn-node-proto/testdata/pkcs8-ed25519-v2/`](../crates/spawn-node-proto/testdata/pkcs8-ed25519-v2/)
> **Verification Tests**:
> - [`crates/spawn-node-proto/tests/spawn_e2e_v2.rs`](../crates/spawn-node-proto/tests/spawn_e2e_v2.rs)
> - [`crates/spawn-node-proto/tests/pkcs8_ed25519_v2.rs`](../crates/spawn-node-proto/tests/pkcs8_ed25519_v2.rs)
> - [`crates/spawn-node-gateway/tests/pkcs8_fixture.rs`](../crates/spawn-node-gateway/tests/pkcs8_fixture.rs)

---

## 1. Cryptographic Primitives (WebCrypto + `ring` + Go Stdlib Parity)

`spawn-e2e-v2` replaces non-WebCrypto symmetric algorithms so browser clients (`window.crypto.subtle` without third-party polyfills), Rust (`ring`), and Go execute identical wire cryptography:

| Role | Primitive | Parameters |
| --- | --- | --- |
| Ephemeral Key Agreement | `X25519` (RFC 7748) | Ephemeral 32-byte scalar × responder prekey public key; low-order / all-zero shared secrets rejected |
| Identity Authentication | `Ed25519` (RFC 8032) | `key_id = lowercase_hex(SHA-256(raw_pubkey_32))` (`64` hex chars) |
| Key Derivation | `HKDF-SHA256` (RFC 5869) | Explicit 32-byte random `salt` + domain-separated UTF-8 `info` -> `32`-byte CEK |
| Authenticated Encryption | `AES-256-GCM` | `12`-byte random nonce, UTF-8 transcript `aad`, `16`-byte tag appended (`ct \|\| tag`) |
| Binary Field Encoding | Strict unpadded `base64url` (`b64u`) | No `=`, URL/fragment safe; zero custom JSON canonicalization needed for signing |

---

## 2. Wire Envelopes & Exact Transcript Bindings

### 2.1 Request Envelope (`spawn-e2e-v2`)

Every binary field uses strict unpadded `b64u`:

```json
{
  "v": "spawn-e2e-v2",
  "from": "<64-hex sender key_id>",
  "from_pub": "<b64u 32-byte sender Ed25519 pubkey>",
  "to": "<64-hex responder key_id>",
  "epoch": 1795600000,
  "to_x25519": "<b64u 32-byte responder X25519 prekey>",
  "channel_id": "<channel identifier>",
  "at_ms": 1795600000120,
  "eph": "<b64u 32-byte ephemeral X25519 pubkey>",
  "req_nonce": "<b64u 12-byte AES-GCM nonce>",
  "salt": "<b64u 32-byte HKDF salt>",
  "ct": "<b64u AES-256-GCM ciphertext || 16-byte tag>",
  "sig": "<b64u 64-byte Ed25519 signature>"
}
```

#### Header Signing Input & AEAD AAD (`req_header`)

Joined by ASCII `0x0a` (`\n`), with **no trailing newline**:

```text
spawn-e2e-req-v2
<from>
<to>
<epoch>
<to_x25519>
<channel_id>
<at_ms>
<eph>
<req_nonce>
<salt>
```

1. **Pre-open authentication**:
   - Responder checks `SHA-256(b64u_decode(from_pub)) == from` and sender trust/allowlist.
   - Responder checks `|now_ms - at_ms| <= 120_000` (`MAX_SKEW_MS`).
   - Responder verifies `Ed25519(from_pub, req_header, sig)` **before** ECDH or AEAD decryption.
   - Responder checks `(from, req_nonce)` replay cache (`MAX_SKEW_MS` window) and rejects low-order X25519 inputs (all-zero shared secret).
2. **Request Key Derivation (`HKDF-SHA256`)**:
   - `ikm = X25519(responder_x25519_priv, eph)` (`32` bytes; reject if all `0x00`).
   - `salt = b64u_decode(salt)` (`32` bytes).
   - `info = "spawn-e2e-v2:request\n" || req_header`.
   - `request_key = HKDF-SHA256(ikm, salt, info, 32)`.
3. **Request AEAD (`AES-256-GCM`)**:
   - `key = request_key`, `nonce = b64u_decode(req_nonce)` (`12` bytes), `aad = req_header`.

### 2.2 Reply Envelope (`spawn-e2e-reply-v2`)

Each reply derives an independent key from a fresh `reply_salt` and `reply_nonce`, bound to the request identity and `req_nonce`:

```json
{
  "v": "spawn-e2e-reply-v2",
  "from": "<64-hex responder key_id>",
  "to": "<64-hex requester key_id>",
  "channel_id": "<channel identifier>",
  "req_nonce": "<b64u 12-byte req_nonce echoed>",
  "reply_nonce": "<b64u 12-byte fresh AES-GCM nonce>",
  "reply_salt": "<b64u 32-byte fresh HKDF salt>",
  "ct": "<b64u AES-256-GCM ciphertext || 16-byte tag>"
}
```

- **Reply Key Derivation (`HKDF-SHA256`)**:
  - `ikm`: exact `32`-byte X25519 shared secret from the corresponding request.
  - `salt = b64u_decode(reply_salt)` (`32` bytes).
  - `info = "spawn-e2e-v2:reply\n" || <req.from> || "\n" || <req.to> || "\n" || <req.channel_id> || "\n" || <req_nonce> || "\n" || <reply_nonce>`.
  - `reply_key = HKDF-SHA256(ikm, salt, info, 32)`.
- **Reply AEAD (`AES-256-GCM`)**:
  - `key = reply_key`, `nonce = b64u_decode(reply_nonce)` (`12` bytes).
  - `aad` (`\n`-joined, no trailing newline):
    ```text
    spawn-e2e-reply-v2
    <from>
    <to>
    <channel_id>
    <req_nonce>
    <reply_nonce>
    <reply_salt>
    ```

---

## 3. Browser Key Provisioning & Distribution Contract (20 Safeguards #2)

1. **Fragment-Only Delivery (`#e2e=`)**:
   - Enrollment/pairing links pass client key material exclusively in the URL fragment `#e2e=<b64u-payload>` (never in query parameters `?e2e=`, headers, cookies, or `GET /api/*` JSON). URL fragments do not traverse HTTP requests or proxy logs.
   - Immediately upon boot/import, the browser calls `window.history.replaceState(null, "", window.location.pathname + window.location.search)` before rendering or issuing network calls so `#e2e=` is scrubbed from session history and referrer exposure.
2. **Non-Extractable WebCrypto Storage in `IndexedDB`**:
   - Client identity/agreement keys are imported into `window.crypto.subtle` with **`extractable: false`** (`importKey("pkcs8", ..., false, ["sign"])` / `["deriveBits"]`) and persisted solely in an origin-scoped **`IndexedDB`** object store.
   - Private keys and raw seeds **must never** be written to `localStorage`, `sessionStorage`, `document.cookie`, or JS heap globals/React state after import completes.
3. **Relay / Proxy Opacity**:
   - Edge reverse proxies (`spawn-node-edge`) and static UI bundles inspect only `v`, `from`, `to`, `epoch`, `channel_id`, and `at_ms` for routing; plaintext commands and outputs never exist outside browser WebCrypto memory and the target node/gateway.

---

## 4. Cross-Language PKCS#8 Ed25519 Key Format Matrix (`pkcs8-ed25519-v2`)

Three DER-encoded PKCS#8 `PrivateKeyInfo` layouts appear across Go, Rust `ring`, OpenSSL, and WebCrypto (`OID 1.3.101.112`, `06 03 2b 65 70`). Readers such as `spawn-node-gateway/src/primary.rs::from_pkcs8` **must accept all three**:

| Variant | Length (bytes) | ASN.1 / DER Structure | Primary Producer |
| --- | --- | --- | --- |
| `v1` | **`48`** | `30 2e 02 01 00 30 05 06 03 2b 65 70 04 22 04 20` \|\| `seed32` | Go `crypto/x509.MarshalPKCS8PrivateKey`, WebCrypto `exportKey("pkcs8")` |
| `v2_ring` | **`83`** | `30 51` \|\| `02 01 01 30 05 06 03 2b 65 70 04 22 04 20` \|\| `seed32` \|\| `81 21 00` \|\| `pub32` | Rust `ring::signature::Ed25519KeyPair::generate_pkcs8` |
| `v2_rfc8410` | **`85`** | `30 53` \|\| `02 01 01 30 05 06 03 2b 65 70 04 22 04 20` \|\| `seed32` \|\| `a1 23 03 21 00` \|\| `pub32` | OpenSSL CLI / RFC 8410 Section 7 PKCS#8 v2 |

### Mandatory Cross-Checks

1. Parsing `v1` (48 B), `v2_ring` (83 B), or `v2_rfc8410` (85 B) generated from the same 32-byte RFC 8032 seed yields identical `pubkey_raw` (32 B), `key_id`, and deterministic Ed25519 signatures.
2. Node.js / WebCrypto `crypto.subtle.importKey("pkcs8", ...)` and `node:crypto.createPrivateKey(...)` import all three encodings and derive the same SPKI (`44` bytes) and `Ed25519` signature bytes verified by Rust `ring` (`spawn-node-proto` and `spawn-node-gateway`).
3. Malformed prefixes, truncated lengths (`47`, `84`, `86`), and foreign curve OIDs (`1.3.101.113` Ed448) must be rejected (`reject.json`).
