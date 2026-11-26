// Pure WebCrypto verification of `docs/spawn-e2e-v2.md` against the frozen
// golden & reject fixtures in `crates/spawn-node-proto/testdata/`.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  DEFAULT_MAX_SKEW_MS,
  E2EError,
  ReplayCache,
  bytesToHex,
  decodeBase64Url,
  deriveReplyKey,
  deriveRequestKey,
  encodeBase64Url,
  encodeEd25519Pkcs8V1,
  encodeEd25519Pkcs8V2Rfc8410,
  encodeEd25519Pkcs8V2Ring,
  encodeEd25519Spki,
  formatReplyAad,
  formatReplyInfo,
  formatRequestHeader,
  hexToBytes,
  importEd25519Pkcs8,
  importX25519PrivateKey,
  keyId,
  openReply,
  openRequest,
  parseEd25519Spki,
  sealReply,
  sealRequest,
  sha256,
  signEd25519,
  verifyEd25519,
  x25519Agree,
  type ReplyEnvelope,
  type RequestEnvelope,
} from "../src/e2e.ts";

const pkcs8Dir = new URL("../../crates/spawn-node-proto/testdata/pkcs8-ed25519-v2/", import.meta.url);
const e2eDir = new URL("../../crates/spawn-node-proto/testdata/spawn-e2e-v2/", import.meta.url);

const readBytes = (dir: URL, name: string): Uint8Array =>
  new Uint8Array(readFileSync(new URL(name, dir)));

const readJson = <T>(dir: URL, name: string): T =>
  JSON.parse(readFileSync(new URL(name, dir), "utf8")) as T;

interface Pkcs8Vectors {
  schema: string;
  seed_hex: string;
  pubkey_b64u: string;
  key_id: string;
  spki_der_b64u: string;
  message: string;
  signature_b64u: string;
  formats: Record<"v1" | "v2_ring" | "v2_rfc8410", {
    version: number;
    len: number;
    file: string;
    der_b64u: string;
    sha256_hex: string;
  }>;
}

interface Pkcs8Reject {
  case: string;
  der_b64u: string;
}

interface E2EVectors {
  v: string;
  sender_ed25519_seed_hex: string;
  sender_ed25519_pub_b64u: string;
  sender_id: string;
  responder_ed25519_seed_hex: string;
  responder_ed25519_pub_b64u: string;
  responder_id: string;
  sender_eph_x25519_seed_hex: string;
  sender_eph_x25519_pub_b64u: string;
  responder_x25519_seed_hex: string;
  responder_x25519_pub_b64u: string;
  shared_secret_hex: string;
  request_signing_input: string;
  request_key_hex: string;
  request_plaintext: string;
  request_envelope: RequestEnvelope;
  reply_info: string;
  reply_aad: string;
  reply_key_hex: string;
  reply_plaintext: string;
  reply_envelope: ReplyEnvelope;
}

type E2EReject =
  | { case: string; x25519_pub_b64u: string }
  | { case: string; envelope: RequestEnvelope; reason: string };

test("web/src/e2e.ts uses pure WebCrypto with zero Node-specific globals or imports", () => {
  const source = readFileSync(new URL("../src/e2e.ts", import.meta.url), "utf8");
  assert.doesNotMatch(source, /\bnode:/);
  assert.doesNotMatch(source, /\bBuffer\b/);
  assert.doesNotMatch(source, /\brequire\s*\(/);
});

test("pkcs8-ed25519-v2 golden fixtures (v1, v2_ring, v2_rfc8410, SPKI) import and sign identically", async () => {
  const vec = readJson<Pkcs8Vectors>(pkcs8Dir, "vectors.json");
  assert.equal(vec.schema, "pkcs8-ed25519-v2");

  const seed = hexToBytes(vec.seed_hex);
  const message = new TextEncoder().encode(vec.message);
  const spkiFile = readBytes(pkcs8Dir, "key.spki.der");
  assert.equal(encodeBase64Url(spkiFile), vec.spki_der_b64u);
  const rawPubFromSpki = parseEd25519Spki(spkiFile);
  assert.equal(encodeBase64Url(rawPubFromSpki), vec.pubkey_b64u);
  assert.equal(await keyId(rawPubFromSpki), vec.key_id);

  for (const format of ["v1", "v2_ring", "v2_rfc8410"] as const) {
    const spec = vec.formats[format];
    const der = readBytes(pkcs8Dir, spec.file);
    assert.equal(der.length, spec.len, `${format} DER length`);
    assert.equal(encodeBase64Url(der), spec.der_b64u, `${format} b64u`);
    assert.equal(bytesToHex(await sha256(der)), spec.sha256_hex, `${format} sha256`);

    const identity = await importEd25519Pkcs8(der);
    assert.equal(identity.format, format);
    assert.equal(identity.version, spec.version);
    assert.deepEqual(identity.seed, seed);
    assert.equal(encodeBase64Url(identity.publicKeyBytes), vec.pubkey_b64u);
    assert.equal(identity.keyId, vec.key_id);
    assert.equal(encodeBase64Url(identity.spkiDer), vec.spki_der_b64u);

    const sig = await signEd25519(identity.privateKey, message);
    assert.equal(encodeBase64Url(sig), vec.signature_b64u, `${format} Ed25519 signature`);
    assert.equal(await verifyEd25519(identity.publicKeyBytes, message, sig), true);
  }

  assert.deepEqual(encodeEd25519Pkcs8V1(seed), readBytes(pkcs8Dir, "key.v1.der"));
  assert.deepEqual(encodeEd25519Pkcs8V2Ring(seed, rawPubFromSpki), readBytes(pkcs8Dir, "key.v2-ring.der"));
  assert.deepEqual(encodeEd25519Pkcs8V2Rfc8410(seed, rawPubFromSpki), readBytes(pkcs8Dir, "key.v2-rfc8410.der"));
  assert.deepEqual(encodeEd25519Spki(rawPubFromSpki), spkiFile);

  // Non-extractable import verifies & signs while keeping key non-extractable.
  const nonExtractable = await importEd25519Pkcs8(readBytes(pkcs8Dir, "key.v2-ring.der"), {
    extractable: false,
  });
  assert.equal(nonExtractable.privateKey.extractable, false);
  const sig = await signEd25519(nonExtractable.privateKey, message);
  assert.equal(encodeBase64Url(sig), vec.signature_b64u);
});

test("pkcs8-ed25519-v2 rejects all malformed DER fixtures and mismatched embedded public keys", async () => {
  const rejects = readJson<Pkcs8Reject[]>(pkcs8Dir, "reject.json");
  assert.ok(rejects.length >= 4);
  for (const item of rejects) {
    const der = decodeBase64Url(item.der_b64u);
    await assert.rejects(
      () => importEd25519Pkcs8(der),
      (err: unknown) => err instanceof E2EError && err.code === "invalid_pkcs8",
      `expected invalid_pkcs8 rejection for ${item.case}`,
    );
  }

  for (const name of ["key.v2-ring.der", "key.v2-rfc8410.der"]) {
    const mutated = readBytes(pkcs8Dir, name);
    mutated[mutated.length - 1]! ^= 0x01;
    await assert.rejects(
      () => importEd25519Pkcs8(mutated),
      (err: unknown) => err instanceof E2EError && err.code === "invalid_pkcs8",
      `expected embedded pubkey mismatch rejection for ${name}`,
    );
  }
});

test("spawn-e2e-v2 golden request and reply vectors match WebCrypto byte-for-byte", async () => {
  const vec = readJson<E2EVectors>(e2eDir, "vectors.json");
  assert.equal(vec.v, "spawn-e2e-v2");

  const sender = await importEd25519Pkcs8(encodeEd25519Pkcs8V1(hexToBytes(vec.sender_ed25519_seed_hex)));
  const responderIdentity = await importEd25519Pkcs8(
    encodeEd25519Pkcs8V1(hexToBytes(vec.responder_ed25519_seed_hex)),
  );
  assert.equal(encodeBase64Url(sender.publicKeyBytes), vec.sender_ed25519_pub_b64u);
  assert.equal(sender.keyId, vec.sender_id);
  assert.equal(encodeBase64Url(responderIdentity.publicKeyBytes), vec.responder_ed25519_pub_b64u);
  assert.equal(responderIdentity.keyId, vec.responder_id);

  const ephSeed = hexToBytes(vec.sender_eph_x25519_seed_hex);
  const senderEph = await importX25519PrivateKey(ephSeed);
  const responderX25519 = await importX25519PrivateKey(hexToBytes(vec.responder_x25519_seed_hex));
  assert.equal(encodeBase64Url(senderEph.publicKeyBytes), vec.sender_eph_x25519_pub_b64u);
  assert.equal(encodeBase64Url(responderX25519.publicKeyBytes), vec.responder_x25519_pub_b64u);

  const shared = await x25519Agree(senderEph.privateKey, responderX25519.publicKeyBytes);
  assert.equal(bytesToHex(shared), vec.shared_secret_hex);

  const header = formatRequestHeader(vec.request_envelope);
  assert.equal(header, vec.request_signing_input);
  const reqKey = await deriveRequestKey(
    shared,
    decodeBase64Url(vec.request_envelope.salt, 32),
    header,
  );
  assert.equal(bytesToHex(reqKey), vec.request_key_hex);

  const sealedReq = await sealRequest({
    sender,
    responderId: vec.responder_id,
    responderX25519Pub: responderX25519.publicKeyBytes,
    epoch: vec.request_envelope.epoch,
    channelId: vec.request_envelope.channel_id,
    atMs: vec.request_envelope.at_ms,
    plaintext: new TextEncoder().encode(vec.request_plaintext),
    ephemeralSeed: ephSeed,
    reqNonce: decodeBase64Url(vec.request_envelope.req_nonce, 12),
    salt: decodeBase64Url(vec.request_envelope.salt, 32),
  });
  assert.deepEqual(sealedReq.envelope, vec.request_envelope);
  assert.equal(bytesToHex(sealedReq.sharedSecret), vec.shared_secret_hex);

  const openedReq = await openRequest(vec.request_envelope, responderX25519, {
    nowMs: vec.request_envelope.at_ms,
    responderId: vec.responder_id,
  });
  assert.equal(new TextDecoder().decode(openedReq.plaintext), vec.request_plaintext);
  assert.equal(bytesToHex(openedReq.sharedSecret), vec.shared_secret_hex);

  assert.equal(
    formatReplyInfo(vec.request_envelope, vec.reply_envelope.reply_nonce),
    vec.reply_info,
  );
  assert.equal(formatReplyAad(vec.reply_envelope), vec.reply_aad);
  const replyKey = await deriveReplyKey(
    shared,
    decodeBase64Url(vec.reply_envelope.reply_salt, 32),
    vec.request_envelope,
    vec.reply_envelope.reply_nonce,
  );
  assert.equal(bytesToHex(replyKey), vec.reply_key_hex);

  const sealedReply = await sealReply(
    vec.request_envelope,
    openedReq.sharedSecret,
    new TextEncoder().encode(vec.reply_plaintext),
    {
      replyNonce: decodeBase64Url(vec.reply_envelope.reply_nonce, 12),
      replySalt: decodeBase64Url(vec.reply_envelope.reply_salt, 32),
    },
  );
  assert.deepEqual(sealedReply, vec.reply_envelope);

  const openedReply = await openReply(
    sealedReq.envelope,
    sealedReply,
    sealedReq.sharedSecret,
  );
  assert.equal(new TextDecoder().decode(openedReply), vec.reply_plaintext);
});

test("spawn-e2e-v2 rejects 100% of reject.json vectors (low-order, tampered ct/sig/aad, impostor)", async () => {
  const vec = readJson<E2EVectors>(e2eDir, "vectors.json");
  const rejects = readJson<E2EReject[]>(e2eDir, "reject.json");
  const responderX25519 = await importX25519PrivateKey(hexToBytes(vec.responder_x25519_seed_hex));
  assert.equal(rejects.length, 7);

  for (const item of rejects) {
    if ("x25519_pub_b64u" in item) {
      const lowOrderPub = decodeBase64Url(item.x25519_pub_b64u, 32);
      await assert.rejects(
        () => x25519Agree(responderX25519.privateKey, lowOrderPub),
        (err: unknown) => err instanceof E2EError && err.code === "low_order_point",
        `expected low_order_point for ${item.case}`,
      );
      continue;
    }

    const expectedCode =
      item.case === "tampered_ct_tag"
        ? "decrypt_failed"
        : item.case === "impostor_from_pub"
          ? "untrusted_sender"
          : "bad_signature";

    await assert.rejects(
      () => openRequest(item.envelope, responderX25519, { nowMs: item.envelope.at_ms }),
      (err: unknown) => err instanceof E2EError && err.code === expectedCode,
      `expected ${expectedCode} for ${item.case}`,
    );
  }
});

test("spawn-e2e-v2 enforces strict unpadded base64url, clock skew, replay cache, allowlist, and round-trip", async () => {
  // 1. Strict base64url rejections (empty, padded, standard b64, non-zero trailing bits)
  for (const bad of ["", "AQ==", "AQ=", "A+B/", "a", "AB", "AAB"]) {
    assert.throws(
      () => decodeBase64Url(bad),
      (err: unknown) => err instanceof E2EError && err.code === "invalid_envelope",
      `expected rejection for base64url ${JSON.stringify(bad)}`,
    );
  }

  const vec = readJson<E2EVectors>(e2eDir, "vectors.json");
  const responderX25519 = await importX25519PrivateKey(hexToBytes(vec.responder_x25519_seed_hex));
  const atMs = vec.request_envelope.at_ms;

  // 2. Clock skew bounds (±120_000 ms accepted; ±120_001 ms rejected)
  await openRequest(vec.request_envelope, responderX25519, {
    nowMs: atMs + DEFAULT_MAX_SKEW_MS,
  });
  await openRequest(vec.request_envelope, responderX25519, {
    nowMs: atMs - DEFAULT_MAX_SKEW_MS,
  });
  for (const delta of [DEFAULT_MAX_SKEW_MS + 1, -(DEFAULT_MAX_SKEW_MS + 1)]) {
    await assert.rejects(
      () => openRequest(vec.request_envelope, responderX25519, { nowMs: atMs + delta }),
      (err: unknown) => err instanceof E2EError && err.code === "clock_skew",
    );
  }

  // 3. Replay cache rejects duplicates inside window and prunes expired entries
  const replay = new ReplayCache();
  await openRequest(vec.request_envelope, responderX25519, { nowMs: atMs, replay });
  await assert.rejects(
    () => openRequest(vec.request_envelope, responderX25519, { nowMs: atMs + 1000, replay }),
    (err: unknown) => err instanceof E2EError && err.code === "replay",
  );
  // Expiry prunes previous entry once `nowMs - DEFAULT_MAX_SKEW_MS > seenAt`
  const cache = new ReplayCache();
  cache.checkAndInsert("sender-a", "nonce-1", 1000, 5000);
  assert.throws(
    () => cache.checkAndInsert("sender-a", "nonce-1", 6000, 5000),
    (err: unknown) => err instanceof E2EError && err.code === "replay",
  );
  cache.checkAndInsert("sender-a", "nonce-1", 6001, 5000);
  assert.equal(cache.size, 1);

  // 4. Sender allowlist refusal
  await assert.rejects(
    () =>
      openRequest(vec.request_envelope, responderX25519, {
        nowMs: atMs,
        allowSender: () => false,
      }),
    (err: unknown) => err instanceof E2EError && err.code === "untrusted_sender",
  );

  // 5. Full randomized round-trip + reply tampering checks
  const sender = await importEd25519Pkcs8(readBytes(pkcs8Dir, "key.v2-rfc8410.der"));
  const freshResponder = await importX25519PrivateKey(hexToBytes(vec.responder_x25519_seed_hex), {
    extractable: false,
  });
  const now = 1_795_600_500_000;
  const reqOut = await sealRequest({
    sender,
    responderId: vec.responder_id,
    responderX25519Pub: freshResponder.publicKeyBytes,
    epoch: 42,
    channelId: "chan-live",
    atMs: now,
    plaintext: new TextEncoder().encode("{\"op\":\"ping\"}"),
  });
  const opened = await openRequest(reqOut.envelope, freshResponder, {
    nowMs: now + 50,
    responderId: vec.responder_id,
    replay,
  });
  assert.equal(new TextDecoder().decode(opened.plaintext), "{\"op\":\"ping\"}");
  const replyOut = await sealReply(
    reqOut.envelope,
    opened.sharedSecret,
    new TextEncoder().encode("{\"pong\":42}"),
  );
  const replyPlain = await openReply(reqOut.envelope, replyOut, reqOut.sharedSecret);
  assert.equal(new TextDecoder().decode(replyPlain), "{\"pong\":42}");

  // Mutated channel binding on reply must fail
  await assert.rejects(
    () =>
      openReply(
        reqOut.envelope,
        { ...replyOut, channel_id: "chan-other" },
        reqOut.sharedSecret,
      ),
    (err: unknown) => err instanceof E2EError && err.code === "invalid_envelope",
  );
});
