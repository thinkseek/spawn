//! The request path (design §2; docs/node-protocol-v1.md), in order:
//!
//! 1. strip the public prefix; the target's grammar, the route and method,
//!    the five `Spawn-Node-*` headers (each once), `seq` 0 for enroll
//!    only, and the `Content-Length` (one, canonical, within the verb's
//!    cap) — all before any body byte is read: `400 bad_request` (`413
//!    too_large` for a result);
//! 2. pacing per source address and per `node_id`: `429 busy`;
//! 3. `|ts_ms − now| ≤ 120 000`: `409 clock_skew`;
//! 4. the signed body read (the whole body, or a result's metadata line)
//!    and checked as strict JSON;
//! 5. a key-cache hit verifies at once: a bad signature is `401
//!    bad_signature` with no database call;
//! 6. the `spawn_node_gw_*` call, verify-then-commit: the transaction
//!    commits only once the signature verifies against the key the ledger
//!    returned (and the answer could be built);
//! 7. every answer but an artifact's body is signed (seven lines,
//!    `Spawn-Primary-Key`, `Spawn-Primary-Sig`) and carries
//!    `server_time_ms`.

use std::io::{BufRead as _, Read as _};
use std::net::{IpAddr, TcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use spawn_node_proto::codec::{parse_decimal, sha256, sha256_hex};
use spawn_node_proto::enroll::{EnrollAnswer, EnrollRequest};
use spawn_node_proto::envelope::{REQUEST_HEADERS, is_clock_skewed, strip_public_prefix};
use spawn_node_proto::grant::{
    ArtifactRole, ArtifactUrl, Grant, GrantInputs, JobKind, SignedGrant,
};
use spawn_node_proto::heartbeat::{Ack, Beat, Fenced, LeaseRef};
use spawn_node_proto::json::{self, OrderedMap};
use spawn_node_proto::response::{
    HEADER_PRIMARY_KEY, HEADER_PRIMARY_SIG, RefusalBody, RequestId, sign_response,
};
use spawn_node_proto::result::ResultMeta;
use spawn_node_proto::spec::{Holds, NodeSpec, spec_sha256};
use spawn_node_proto::{
    Checked, Envelope, Limit, Method, PublicKey, Refusal, SigningKey, VERSION, Verb, check_request,
};
use spawn_postgres::{NodeEnroll, PostgresStore};

use crate::artifacts::{self, Miss, REGISTRY_MANIFEST};
use crate::cache::KeyCache;
use crate::http::{self, Request, Response};
use crate::limits::{Class, Limiter};
use crate::results::{self, InFlight, Stored};
use crate::span;

/// The gateway's clock: Unix milliseconds.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;
/// A new connection to the ledger as the gateway's login.
pub type Connector = Arc<dyn Fn() -> Result<PostgresStore, String> + Send + Sync>;

/// The system clock.
pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            })
    })
}

/// What a gateway is made of.
pub struct Settings {
    /// Primary Ed25519 signing key used to sign gateway responses.
    pub primary: SigningKey,
    /// Cell identifiers accepted by this gateway.
    pub cells: Vec<String>,
    /// Optional reverse-proxy URL prefix stripped before routing.
    pub public_prefix: String,
    /// Root directory containing artifact blobs and manifests.
    pub artifacts: PathBuf,
    /// Directory where job logs and metadata are persisted.
    pub results: PathBuf,
    /// Monotonic/wall clock source used for timestamps and signatures.
    pub clock: Clock,
    /// Factory opening a synchronous PostgreSQL store connection per worker.
    pub connector: Connector,
}

/// The gateway, shared by its workers.
pub struct Gateway {
    primary: SigningKey,
    cells: Vec<String>,
    public_prefix: String,
    artifacts: PathBuf,
    results: PathBuf,
    clock: Clock,
    connector: Connector,
    keys: KeyCache,
    limiter: Limiter,
    in_flight: InFlight,
    db_calls: AtomicU64,
}

/// The `(node_id, boot_id, seq)` a response binds: the request's headers,
/// each one only when it is well formed (empty or 0 otherwise).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rid {
    /// Lowercase 64-hex node identifier from the request header, or empty.
    pub node_id: String,
    /// Lowercase 32-hex boot identifier from the request header, or empty.
    pub boot_id: String,
    /// Parsed decimal sequence number from the request header, or `0`.
    pub seq: u64,
}

impl Rid {
    /// Borrows the response-binding request identity tuple for signing.
    pub fn id(&self) -> RequestId<'_> {
        RequestId {
            node_id: &self.node_id,
            boot_id: &self.boot_id,
            seq: self.seq,
        }
    }

    fn of(request: &Request<'_>) -> Self {
        let hex = |name: &str, len: usize| {
            request
                .header(name)
                .filter(|value| spawn_node_proto::codec::is_lower_hex(value, len))
                .unwrap_or_default()
                .to_string()
        };
        Self {
            node_id: hex(spawn_node_proto::envelope::HEADER_NODE_ID, 64),
            boot_id: hex(spawn_node_proto::envelope::HEADER_BOOT, 32),
            seq: request
                .header(spawn_node_proto::envelope::HEADER_SEQ)
                .and_then(|value| parse_decimal(value).ok())
                .unwrap_or(0),
        }
    }
}

/// Why a request gets no success answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Refuse(Refusal),
    /// No such artifact: a signed `404 not_found`.
    NotFound,
}

impl From<Refusal> for Answer {
    fn from(refusal: Refusal) -> Self {
        Self::Refuse(refusal)
    }
}

type Flow = Result<Response, Answer>;

/// A checked request, the time it is answered at and what the answer binds.
#[derive(Clone, Copy)]
struct Call<'c, 'a> {
    checked: &'c Checked<'a>,
    now: u64,
    rid: &'c Rid,
}

const BAD: Answer = Answer::Refuse(Refusal::BadRequest);

/// `POST /node/v1/spec`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecAnswer {
    /// Protocol version (`1`).
    pub v: u64,
    /// Server timestamp in Unix milliseconds.
    pub server_time_ms: u64,
    /// False when the ledger held this spec already.
    pub changed: bool,
}

/// `POST /node/v1/jobs/<job_id>/result`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultAnswer {
    /// Protocol version (`1`).
    pub v: u64,
    /// Server timestamp in Unix milliseconds.
    pub server_time_ms: u64,
    /// Identifier of the job whose result was recorded.
    pub job_id: String,
    /// Lease epoch associated with the job result.
    pub lease_epoch: u64,
}

/// A grant as `spawn_node_gw_beat` returns it.
#[derive(Debug, Deserialize)]
struct LedgerGrant {
    job_id: String,
    lease_epoch: u64,
    kind: String,
    #[serde(default)]
    cost: Option<OrderedMap<u64>>,
    #[serde(default)]
    attempt: Option<u64>,
    #[serde(default)]
    spec: Value,
    /// False when the node holds the job's base commit warm (it is among
    /// the beat's `warm_bases`): the baseline bundle is then left out of
    /// the grant (design §3.1). Absent: needed.
    #[serde(default)]
    needs_baseline: Option<bool>,
}

/// The grant fields a job's `spec` may carry.
#[derive(Debug, Default, Deserialize)]
struct JobSpec {
    #[serde(default)]
    timeout_s: Option<u64>,
    #[serde(default)]
    inputs: Option<GrantInputs>,
    #[serde(default)]
    env: Option<OrderedMap<String>>,
    #[serde(default)]
    requires: Option<Holds>,
    #[serde(default)]
    artifact_urls: Option<Vec<ArtifactUrl>>,
}

impl Gateway {
    /// Constructs a new [`Gateway`] instance from validated [`Settings`].
    pub fn new(settings: Settings) -> Self {
        Self {
            primary: settings.primary,
            cells: settings.cells,
            public_prefix: settings.public_prefix,
            artifacts: settings.artifacts,
            results: settings.results,
            clock: settings.clock,
            connector: settings.connector,
            keys: KeyCache::default(),
            limiter: Limiter::default(),
            in_flight: InFlight::default(),
            db_calls: AtomicU64::new(0),
        }
    }

    /// The primary's public key.
    pub fn primary_key(&self) -> PublicKey {
        self.primary.public_key()
    }

    /// Database calls made so far (connections opened and gateway functions
    /// called).
    pub fn db_calls(&self) -> u64 {
        self.db_calls.load(Ordering::SeqCst)
    }

    /// The key cache.
    pub fn keys(&self) -> &KeyCache {
        &self.keys
    }

    /// A response signed over the seven lines; `now` must be the
    /// `server_time_ms` the body carries.
    pub fn signed(&self, status: u16, rid: &Rid, now: u64, body: Vec<u8>) -> Response {
        let signature = sign_response(&self.primary, status, rid.id(), now, &body);
        Response::bytes(status, body)
            .header("Content-Type", "application/json")
            .header(HEADER_PRIMARY_KEY, signature.primary_key)
            .header(HEADER_PRIMARY_SIG, signature.sig)
    }

    /// A refusal of section 2.4, signed.
    pub fn refusal(&self, refusal: Refusal, rid: &Rid, now: u64) -> Response {
        self.signed(
            refusal.status(),
            rid,
            now,
            RefusalBody::new(refusal, now).to_bytes(),
        )
    }

    fn not_found(&self, rid: &Rid, now: u64) -> Response {
        let body = RefusalBody {
            v: VERSION,
            error: "not_found".to_string(),
            server_time_ms: now,
        };
        self.signed(404, rid, now, body.to_bytes())
    }

    /// The answer to one request; `store` is the worker's connection.
    pub fn handle(&self, store: &mut Option<PostgresStore>, request: &mut Request<'_>) -> Response {
        let now = (self.clock)();
        let rid = Rid::of(request);
        match self.route(store, request, now, &rid) {
            Ok(response) => response,
            Err(Answer::Refuse(refusal)) => self.refusal(refusal, &rid, now),
            Err(Answer::NotFound) => self.not_found(&rid, now),
        }
    }

    fn route(
        &self,
        store: &mut Option<PostgresStore>,
        request: &mut Request<'_>,
        now: u64,
        rid: &Rid,
    ) -> Flow {
        // 1. Structure, before any body byte.
        let target = strip_public_prefix(&request.target, &self.public_prefix)
            .ok_or(BAD)?
            .to_string();
        let method = Method::parse(&request.method).map_err(|_| BAD)?;
        let verb = Verb::route(method, &target).map_err(|_| BAD)?;
        if REQUEST_HEADERS
            .iter()
            .any(|name| request.header_count(name) > 1)
        {
            return Err(BAD);
        }
        let envelope = Envelope::from_headers(|name| request.header(name)).map_err(|_| BAD)?;
        if (verb == Verb::Enroll) != (envelope.seq == 0) {
            return Err(BAD);
        }
        let len = content_length(request, method)?;
        if len > verb.body_cap() as u64 {
            return Err(match verb {
                Verb::Result { .. } => Refusal::TooLarge.into(),
                _ => BAD,
            });
        }

        // 2. Pacing.
        let class = match verb {
            Verb::Artifact(_) | Verb::RegistryManifest => Class::Artifact,
            _ => Class::Api,
        };
        if !self
            .limiter
            .allow(class, source_address(request), &envelope.node_id)
        {
            return Err(Refusal::RateLimited.into());
        }

        // 3. The clock.
        if is_clock_skewed(envelope.ts_ms, now) {
            return Err(Refusal::ClockSkew.into());
        }

        // 4. The signed body, then every structural check of the protocol.
        let body = read_signed_body(request, &verb, len)?;
        let checked = check_request(&request.method, &target, |name| request.header(name), &body)
            .map_err(|error| Answer::Refuse(error.refusal()))?;

        match &verb {
            Verb::Enroll => self.enroll(store, &checked, now, rid),
            Verb::Heartbeat => self.beat(store, &checked, now, rid),
            Verb::Spec => self.spec(store, &checked, now, rid),
            Verb::Result { job_id } => {
                let call = Call {
                    checked: &checked,
                    now,
                    rid,
                };
                self.result(store, request, &call, len, job_id)
            }
            Verb::Artifact(_) => self.artifact(request, &checked),
            Verb::RegistryManifest => self.manifest(&checked, now, rid),
        }
    }

    /// Step 5: a cache hit verifies now; a miss is verified against the
    /// ledger's key (verify-then-commit).
    fn verify_cached(&self, checked: &Checked<'_>) -> Result<(), Answer> {
        match self.keys.get(&checked.envelope.node_id) {
            Some(cached) => checked
                .verify(&cached.key)
                .map_err(|_| Refusal::BadSignature.into()),
            None => Ok(()),
        }
    }

    /// The node's cached key for its current boot: what the verbs that
    /// never reach the ledger require (`409 beat_first` without it).
    fn require_cached(&self, checked: &Checked<'_>) -> Result<(), Answer> {
        let cached = self
            .keys
            .get(&checked.envelope.node_id)
            .filter(|cached| cached.boot_id == checked.envelope.boot_id)
            .ok_or(Answer::Refuse(Refusal::BeatFirst))?;
        checked
            .verify(&cached.key)
            .map_err(|_| Refusal::BadSignature.into())
    }

    fn serves(&self, cell_id: &str) -> bool {
        self.cells.iter().any(|cell| cell == cell_id)
    }

    /// One ledger call on the worker's connection (made when absent); an
    /// error drops the connection and is `503 busy`.
    fn ledger<T, E: std::fmt::Display>(
        &self,
        store: &mut Option<PostgresStore>,
        call: impl FnOnce(&PostgresStore) -> Result<T, E>,
    ) -> Result<T, Answer> {
        if store.is_none() {
            self.db_calls.fetch_add(1, Ordering::SeqCst);
            match (self.connector)() {
                Ok(connected) => *store = Some(connected),
                Err(error) => {
                    eprintln!("spawn-node-gateway: database: {error}");
                    return Err(Refusal::Unavailable.into());
                }
            }
        }
        let Some(connected) = store.as_ref() else {
            return Err(Refusal::Unavailable.into());
        };
        self.db_calls.fetch_add(1, Ordering::SeqCst);
        call(connected).map_err(|error| {
            eprintln!("spawn-node-gateway: database: {error}");
            *store = None;
            Answer::Refuse(Refusal::Unavailable)
        })
    }

    /// The refusal a ledger answer means. `verified`: the request's
    /// signature verified against the key the answer returned.
    fn refused(&self, answer: &Value, verified: bool, node_id: &str) -> Result<(), Answer> {
        if answer["ok"] == true {
            return if verified {
                Ok(())
            } else {
                Err(Refusal::BadSignature.into())
            };
        }
        let refusal = match answer["error"].as_str().unwrap_or_default() {
            "unknown_node" => {
                self.keys.evict(node_id);
                Refusal::UnknownNode
            }
            "revoked" => {
                self.keys.evict(node_id);
                if verified {
                    Refusal::Revoked
                } else {
                    Refusal::BadSignature
                }
            }
            _ if !verified => Refusal::BadSignature,
            "stale_boot" => Refusal::StaleBoot,
            "replay" => Refusal::Replay,
            "beat_first" => Refusal::BeatFirst,
            "bad_request" => Refusal::BadRequest,
            _ => Refusal::Unavailable,
        };
        Err(refusal.into())
    }

    /// `POST /node/v1/enroll`: proof of possession (the new key signs its
    /// own request, seq 0), then the token consumed and the key pinned.
    fn enroll(
        &self,
        store: &mut Option<PostgresStore>,
        checked: &Checked<'_>,
        now: u64,
        rid: &Rid,
    ) -> Flow {
        let body = checked.signed_body;
        let enroll: EnrollRequest = json::parse(body).map_err(|_| BAD)?;
        enroll.validate().map_err(|_| BAD)?;
        let pubkey = enroll.pubkey().map_err(|_| BAD)?;
        let token = enroll.token().map_err(|_| BAD)?;
        // A token for another primary can never be answered here: the
        // node would refuse this primary's key.
        if !token.pins(&self.primary.public_key()) || !self.serves(&enroll.spec.cell_id) {
            return Err(BAD);
        }
        checked
            .verify(&pubkey)
            .map_err(|_| Answer::Refuse(Refusal::BadSignature))?;
        let spec_bytes = &body[span::member(body, "spec").ok_or(BAD)?];
        NodeSpec::parse(spec_bytes).map_err(|_| BAD)?;
        let spec: Value = serde_json::from_slice(spec_bytes).map_err(|_| BAD)?;
        let spec_sha = spec_sha256(spec_bytes);
        let token_sha = sha256(token.as_str().as_bytes());
        let envelope = &checked.envelope;
        let call = NodeEnroll {
            token_sha256: &token_sha,
            pubkey: &pubkey.0,
            boot_id: &envelope.boot_id,
            spec_sha256: &spec_sha,
            spec: &spec,
        };
        let enrolled = self.ledger(store, |ledger| {
            ledger.node_gw_enroll(&call, |answer| answer["ok"] == true)
        })?;
        if !enrolled.committed {
            return Err(
                match enrolled.answer["error"].as_str().unwrap_or_default() {
                    "bad_request" => BAD,
                    // Not (or no longer) a node this token may enroll.
                    "bad_token" | "key_in_use" => Refusal::UnknownNode.into(),
                    _ => Refusal::Unavailable.into(),
                },
            );
        }
        self.keys
            .insert(&envelope.node_id, pubkey, &envelope.boot_id);
        let answer = EnrollAnswer {
            v: VERSION,
            server_time_ms: now,
            primary_pubkey: self.primary.public_key().to_b64u(),
        };
        Ok(self.signed(200, rid, now, json::to_vec(&answer)))
    }

    /// `POST /node/v1/heartbeat`.
    fn beat(
        &self,
        store: &mut Option<PostgresStore>,
        checked: &Checked<'_>,
        now: u64,
        rid: &Rid,
    ) -> Flow {
        let beat = Beat::parse(checked.signed_body).map_err(|_| BAD)?;
        if !self.serves(&beat.cell_id) {
            return Err(BAD);
        }
        self.verify_cached(checked)?;
        let value: Value = serde_json::from_slice(checked.signed_body).map_err(|_| BAD)?;
        let envelope = &checked.envelope;
        let seq = i64::try_from(envelope.seq).map_err(|_| BAD)?;
        let mut verified = None;
        let mut built = None;
        let called = self.ledger(store, |ledger| {
            ledger.node_gw_beat(
                &envelope.node_id,
                &envelope.boot_id,
                seq,
                &value,
                |answer| {
                    let Some(key) = ledger_key(answer).filter(|key| checked.verify(key).is_ok())
                    else {
                        return false;
                    };
                    verified = Some(key);
                    if answer["ok"] != true {
                        return false;
                    }
                    match self.ack(answer, &envelope.node_id, now) {
                        Ok(body) => {
                            built = Some(body);
                            true
                        }
                        Err(error) => {
                            eprintln!("spawn-node-gateway: beat answer: {error}");
                            false
                        }
                    }
                },
            )
        })?;
        self.refused(&called.answer, verified.is_some(), &envelope.node_id)?;
        let (Some(key), Some(body), true) = (verified, built, called.committed) else {
            return Err(Refusal::Unavailable.into());
        };
        self.keys.insert(&envelope.node_id, key, &envelope.boot_id);
        Ok(self.signed(200, rid, now, body))
    }

    /// The heartbeat's answer from the ledger's, its grants signed.
    fn ack(&self, answer: &Value, node_id: &str, now: u64) -> Result<Vec<u8>, String> {
        let field = |name: &str| answer[name].clone();
        let number = |name: &str| {
            answer[name]
                .as_u64()
                .ok_or_else(|| format!("no {name} in the ledger's answer"))
        };
        let lease_ttl_s = number("lease_ttl_s")?;
        let expires_ms = number("server_time_ms")? + lease_ttl_s * 1000;
        let grants = answer["grants"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|grant| match self.grant(grant, node_id, expires_ms) {
                Ok(signed) => Some(signed),
                Err(error) => {
                    // The node never holds it; its next beat gives the
                    // lease back (grant_lost).
                    eprintln!("spawn-node-gateway: grant dropped: {error}");
                    None
                }
            })
            .collect();
        let ack = Ack {
            v: VERSION,
            server_time_ms: now,
            next_beat_ms: number("next_beat_ms")?,
            lease_ttl_s,
            need_spec: answer["need_spec"] == true,
            need_sync: serde_json::from_value::<Holds>(field("need_sync"))
                .map_err(|error| format!("need_sync: {error}"))?,
            renewed: serde_json::from_value::<Vec<LeaseRef>>(field("renewed"))
                .map_err(|error| format!("renewed: {error}"))?,
            fenced: serde_json::from_value::<Vec<Fenced>>(field("fenced"))
                .map_err(|error| format!("fenced: {error}"))?,
            acked_results: serde_json::from_value::<Vec<LeaseRef>>(field("acked_results"))
                .map_err(|error| format!("acked_results: {error}"))?,
            grants,
            drain: answer["drain"] == true,
        };
        Ok(ack.to_bytes())
    }

    /// One grant's exact bytes, signed with the primary key over
    /// `spawn-node-grant-v1\n` + hex(SHA-256(bytes)) + `\n`.
    fn grant(&self, grant: &Value, node_id: &str, expires_ms: u64) -> Result<SignedGrant, String> {
        let bytes = build_grant(grant, node_id, expires_ms)?;
        Ok(SignedGrant::sign(&self.primary, &bytes))
    }

    /// `POST /node/v1/spec`.
    fn spec(
        &self,
        store: &mut Option<PostgresStore>,
        checked: &Checked<'_>,
        now: u64,
        rid: &Rid,
    ) -> Flow {
        let body = checked.signed_body;
        let spec = NodeSpec::parse(body).map_err(|_| BAD)?;
        if !self.serves(&spec.cell_id) {
            return Err(BAD);
        }
        self.verify_cached(checked)?;
        let value: Value = serde_json::from_slice(body).map_err(|_| BAD)?;
        let sha = spec_sha256(body);
        let envelope = &checked.envelope;
        let mut verified = None;
        let called = self.ledger(store, |ledger| {
            ledger.node_gw_spec(
                &envelope.node_id,
                &envelope.boot_id,
                &sha,
                &value,
                |answer| {
                    let Some(key) = ledger_key(answer).filter(|key| checked.verify(key).is_ok())
                    else {
                        return false;
                    };
                    verified = Some(key);
                    answer["ok"] == true
                },
            )
        })?;
        self.refused(&called.answer, verified.is_some(), &envelope.node_id)?;
        let (Some(key), true) = (verified, called.committed) else {
            return Err(Refusal::Unavailable.into());
        };
        self.keys.insert(&envelope.node_id, key, &envelope.boot_id);
        let answer = SpecAnswer {
            v: VERSION,
            server_time_ms: now,
            changed: called.answer["changed"] == true,
        };
        Ok(self.signed(200, rid, now, json::to_vec(&answer)))
    }

    /// `POST /node/v1/jobs/<job_id>/result`: the log streamed to disk and
    /// checked, the files in place, then the fenced completion.
    fn result(
        &self,
        store: &mut Option<PostgresStore>,
        request: &mut Request<'_>,
        call: &Call<'_, '_>,
        len: u64,
        job_id: &str,
    ) -> Flow {
        let Call { checked, now, rid } = *call;
        self.require_cached(checked)?;
        let meta_line = checked.signed_body;
        let meta = ResultMeta::parse(meta_line).map_err(|error| Answer::Refuse(error.refusal()))?;
        if meta.job_id != job_id {
            return Err(BAD);
        }
        let log_len = len - meta_line.len() as u64 - 1;
        if log_len > Limit::ResultLog.bytes() as u64 {
            return Err(Refusal::TooLarge.into());
        }
        if log_len != meta.log_bytes {
            return Err(BAD);
        }
        let epoch = i64::try_from(meta.lease_epoch).map_err(|_| BAD)?;
        let dir = results::result_dir(&self.results, job_id, meta.lease_epoch);
        let _claim = self
            .in_flight
            .claim(&format!("{job_id}.{}", meta.lease_epoch))
            .ok_or(Answer::Refuse(Refusal::RateLimited))?;
        let stored = results::store(
            &dir,
            meta_line,
            meta.log_bytes,
            &meta.log_sha256,
            request.body(),
        )
        .map_err(|error| match error {
            results::Refused::Mismatch => BAD,
            results::Refused::TooLarge => Refusal::TooLarge.into(),
            results::Refused::Conflict => Refusal::LeaseFenced.into(),
            results::Refused::Io(error) => {
                eprintln!("spawn-node-gateway: result {job_id}: {error}");
                Refusal::Unavailable.into()
            }
        })?;
        let mut value: Value = serde_json::from_slice(meta_line).map_err(|_| BAD)?;
        value["result_sha256"] = Value::String(sha256_hex(meta_line));
        let node_id = &checked.envelope.node_id;
        let done = self.ledger(store, |ledger| {
            ledger.node_gw_result(node_id, job_id, epoch, &value, |done| *done)
        })?;
        if !done.answer {
            if stored == Stored::Written {
                results::remove(&dir);
            }
            return Err(Refusal::LeaseFenced.into());
        }
        let answer = ResultAnswer {
            v: VERSION,
            server_time_ms: now,
            job_id: job_id.to_string(),
            lease_epoch: meta.lease_epoch,
        };
        Ok(self.signed(200, rid, now, json::to_vec(&answer)))
    }

    /// `GET /node/v1/artifacts/<algo:hex>`: no ledger call; the cached key
    /// required; the body unsigned (its address verifies it).
    fn artifact(&self, request: &Request<'_>, checked: &Checked<'_>) -> Flow {
        let miss = |miss: Miss| match miss {
            Miss::NotFound => Answer::NotFound,
        };
        let Verb::Artifact(digest) = &checked.verb else {
            return Err(BAD);
        };
        let path = artifacts::artifact_path(&self.artifacts, digest);
        self.require_cached(checked)?;
        artifacts::serve(&path, request.header("Range")).map_err(miss)
    }

    /// `GET /node/v1/registry-manifest`: no ledger call; the cached key
    /// required; a signed JSON answer like every other (design §2.3), the
    /// manifest file's members with `v` and `server_time_ms` set.
    fn manifest(&self, checked: &Checked<'_>, now: u64, rid: &Rid) -> Flow {
        self.require_cached(checked)?;
        let path = self.artifacts.join(REGISTRY_MANIFEST);
        let body = match artifacts::manifest_body(&path, now) {
            Ok(body) => body,
            Err(artifacts::ManifestError::Missing) => return Err(Answer::NotFound),
            Err(artifacts::ManifestError::Invalid(why)) => {
                eprintln!("spawn-node-gateway: {}: {why}", path.display());
                return Err(Refusal::Unavailable.into());
            }
        };
        Ok(self.signed(200, rid, now, body))
    }
}

/// A grant's exact bytes from a lease `spawn_node_gw_beat` returned: the
/// job's kind, epoch, attempt and cost, and the grant fields of its spec
/// (without the baseline bundle when the ledger says `needs_baseline:
/// false`); validated, within 8 KiB.
pub fn build_grant(grant: &Value, node_id: &str, expires_ms: u64) -> Result<Vec<u8>, String> {
    let ledger: LedgerGrant =
        serde_json::from_value(grant.clone()).map_err(|error| format!("ledger grant: {error}"))?;
    let job: JobSpec = if ledger.spec.is_object() {
        serde_json::from_value(ledger.spec.clone())
            .map_err(|error| format!("{}: job spec: {error}", ledger.job_id))?
    } else {
        JobSpec::default()
    };
    let kind = match ledger.kind.as_str() {
        // A shadow gate runs as a gate; the ledger alone knows it decides nothing.
        "gate" | "gate_shadow" => JobKind::Gate,
        "build" => JobKind::Build,
        "de_batch" => JobKind::DeBatch,
        other => return Err(format!("{}: kind {other}", ledger.job_id)),
    };
    let mut artifact_urls = job.artifact_urls;
    if ledger.needs_baseline == Some(false)
        && let Some(urls) = artifact_urls.as_mut()
    {
        urls.retain(|url| url.role != ArtifactRole::BaselineBundle);
    }
    let grant = Grant {
        v: VERSION,
        job_id: ledger.job_id,
        lease_epoch: ledger.lease_epoch,
        node_id: node_id.to_string(),
        kind,
        expires_ms,
        attempt: ledger.attempt,
        cost: ledger.cost.filter(|cost| !cost.is_empty()),
        timeout_s: job.timeout_s,
        inputs: job.inputs,
        env: job.env,
        requires: job.requires.filter(|requires| !requires.is_empty()),
        artifact_urls,
    };
    grant
        .validate()
        .map_err(|error| format!("{}: {error}", grant.job_id))?;
    let bytes = grant.to_bytes();
    if bytes.len() > Limit::Grant.bytes() {
        return Err(format!("{}: {} bytes", grant.job_id, bytes.len()));
    }
    Ok(bytes)
}

/// The node's key in a ledger answer (standard base64, PostgreSQL's
/// `encode(…, 'base64')`).
fn ledger_key(answer: &Value) -> Option<PublicKey> {
    let text: String = answer["pubkey"]
        .as_str()?
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text)
        .ok()?;
    Some(PublicKey(bytes.try_into().ok()?))
}

/// The body length from the one `Content-Length` (canonical decimal); a
/// GET has none or 0, a POST exactly one; no `Transfer-Encoding`.
fn content_length(request: &Request<'_>, method: Method) -> Result<u64, Answer> {
    if request.header_count("Transfer-Encoding") > 0 {
        return Err(BAD);
    }
    let len = match (request.header_count("Content-Length"), method) {
        (0, Method::Get) => 0,
        (1, _) => request
            .header("Content-Length")
            .and_then(|value| parse_decimal(value).ok())
            .ok_or(BAD)?,
        _ => return Err(BAD),
    };
    if method == Method::Get && len != 0 {
        return Err(BAD);
    }
    Ok(len)
}

/// The address a request is paced by: the TCP peer, or behind a reverse
/// proxy on this host (a loopback peer) the last `X-Forwarded-For` entry,
/// the one that proxy wrote.
fn source_address(request: &Request<'_>) -> IpAddr {
    if request.peer.is_loopback()
        && let Some(forwarded) = request
            .header_values("X-Forwarded-For")
            .last()
            .and_then(|value| value.rsplit(',').next())
            .and_then(|last| last.trim().parse().ok())
    {
        return forwarded;
    }
    request.peer
}

/// The bytes the signature covers: a result's metadata line (at most
/// 256 KiB, `413` past it) followed by its `\n`, or the whole body.
fn read_signed_body(request: &mut Request<'_>, verb: &Verb, len: u64) -> Result<Vec<u8>, Answer> {
    let mut body = Vec::new();
    if let Verb::Result { .. } = verb {
        let window = len.min(Limit::ResultMeta.bytes() as u64 + 1);
        request
            .body()
            .take(window)
            .read_until(b'\n', &mut body)
            .map_err(|_| BAD)?;
        if body.last() != Some(&b'\n') {
            return Err(if body.len() > Limit::ResultMeta.bytes() {
                Refusal::TooLarge.into()
            } else {
                BAD
            });
        }
    } else {
        request
            .body()
            .take(len)
            .read_to_end(&mut body)
            .map_err(|_| BAD)?;
        if body.len() as u64 != len {
            return Err(BAD);
        }
    }
    Ok(body)
}

/// A worker: the shared gateway and its own database connection.
pub struct Worker {
    gateway: Arc<Gateway>,
    store: Option<PostgresStore>,
}

impl http::Handler for Worker {
    fn handle(&mut self, request: &mut Request<'_>) -> Response {
        self.gateway.handle(&mut self.store, request)
    }

    fn malformed(&mut self, _peer: IpAddr) -> Response {
        let now = (self.gateway.clock)();
        self.gateway
            .refusal(Refusal::BadRequest, &Rid::default(), now)
    }
}

/// Serve `listener` with `workers` threads; returns the address.
pub fn spawn(
    listener: TcpListener,
    gateway: &Arc<Gateway>,
    workers: usize,
) -> std::io::Result<std::net::SocketAddr> {
    spawn_with(listener, gateway, workers, http::Timeouts::default())
}

/// [`spawn`] with other connection deadlines.
pub fn spawn_with(
    listener: TcpListener,
    gateway: &Arc<Gateway>,
    workers: usize,
    timeouts: http::Timeouts,
) -> std::io::Result<std::net::SocketAddr> {
    http::spawn(listener, workers, timeouts, || Worker {
        gateway: Arc::clone(gateway),
        store: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lease(needs_baseline: Option<bool>) -> Value {
        let url = |role: &str, byte: char| {
            let digest = format!("sha256:{}", byte.to_string().repeat(64));
            json!({"role": role, "digest": digest, "size": 10,
                   "url": format!("/node/v1/artifacts/{digest}")})
        };
        let mut lease = json!({
            "job_id": "gate-0123456789ab-k3x9q2ma", "lease_epoch": 2, "kind": "gate",
            "cost": {"gate_sandbox": 1}, "attempt": 1,
            "spec": {"timeout_s": 600, "artifact_urls": [
                url("delta_bundle", 'a'), url("baseline_bundle", 'b'), url("controller_pack", 'c')]}
        });
        if let Some(needed) = needs_baseline {
            lease["needs_baseline"] = needed.into();
        }
        lease
    }

    fn roles(bytes: &[u8]) -> Vec<ArtifactRole> {
        let grant: Grant = serde_json::from_slice(bytes).unwrap();
        grant
            .artifact_urls
            .unwrap()
            .into_iter()
            .map(|url| url.role)
            .collect()
    }

    #[test]
    fn the_baseline_bundle_is_granted_only_when_needed() {
        let node = "0".repeat(64);
        let all = [
            ArtifactRole::DeltaBundle,
            ArtifactRole::BaselineBundle,
            ArtifactRole::ControllerPack,
        ];
        let needed = build_grant(&lease(Some(true)), &node, 9).unwrap();
        assert_eq!(roles(&needed), all);
        let absent = build_grant(&lease(None), &node, 9).unwrap();
        assert_eq!(roles(&absent), all, "absent means needed");
        let warm = build_grant(&lease(Some(false)), &node, 9).unwrap();
        assert_eq!(
            roles(&warm),
            [ArtifactRole::DeltaBundle, ArtifactRole::ControllerPack]
        );
        let grant: Grant = serde_json::from_slice(&warm).unwrap();
        assert_eq!(
            (grant.lease_epoch, grant.expires_ms, grant.timeout_s),
            (2, 9, Some(600))
        );
        assert_eq!(grant.node_id, node);
    }

    #[test]
    fn a_lease_that_makes_no_valid_grant_is_refused() {
        let node = "0".repeat(64);
        let mut forbidden = lease(None);
        forbidden["spec"]["env"] = json!({"PATH": "/tmp"});
        assert!(build_grant(&forbidden, &node, 9).is_err());
        let mut odd = lease(None);
        odd["kind"] = "mystery".into();
        assert!(build_grant(&odd, &node, 9).is_err());
    }
}
