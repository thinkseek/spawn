//! Stateless HMAC-SHA256 read-only viewer tokens (`sv1.<exp>.<sig>`) and
//! closed [`AuthPrincipal`] authorization for `spawn serve`.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::hmac;
use zeroize::Zeroizing;

/// Wire prefix identifying a short-lived read-only viewer token.
pub(super) const VIEWER_TOKEN_PREFIX: &str = "sv1";

/// Domain separation label binding viewer subkeys strictly to `ReadOnlyViewer`.
const VIEWER_SUBKEY_DOMAIN: &[u8] = b"spawn.serve.readonly-viewer.hmac-sha256.v1";

/// Domain separation label for constant-time admin bearer comparison.
const ADMIN_COMPARE_DOMAIN: &[u8] = b"spawn.serve.admin-token.compare.v1";

/// Default lifetime when `spawn serve viewer-token` omits `--ttl` (2 hours).
pub(super) const DEFAULT_VIEWER_TTL_SECS: u64 = 2 * 60 * 60;

/// Maximum permitted lifetime for any minted viewer token (24 hours).
pub(super) const MAX_VIEWER_TTL_SECS: u64 = 24 * 60 * 60;

/// Error body message returned when a `ReadOnlyViewer` attempts any mutation.
pub(super) const READONLY_MUTATION_ERROR: &str = "read-only viewer token cannot mutate state";

/// Closed authorization principal resolved for every HTTP/SSE request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AuthPrincipal {
    /// Unauthenticated owner-authorized request from loopback (`SPAWN_SERVE_LOCAL_OPEN=1`),
    /// an allowed CIDR (`SPAWN_SERVE_OPEN_NETWORKS`), or a token-less loopback server.
    LocalOwner,
    /// Caller presenting the master admin `SPAWN_SERVE_TOKEN`.
    AdminToken,
    /// Caller presenting a verified, unexpired read-only `sv1` viewer token.
    ReadOnlyViewer { expires_at_ms: i64 },
}

impl AuthPrincipal {
    /// Only [`Self::LocalOwner`] and [`Self::AdminToken`] may issue state mutations.
    pub(super) const fn can_mutate(self) -> bool {
        matches!(self, Self::LocalOwner | Self::AdminToken)
    }

    /// Wall-clock Unix deadline in milliseconds beyond which an active `/api/stream`
    /// connection for this principal must close (`None` for unbounded principals).
    pub(super) const fn stream_expires_at_ms(self) -> Option<i64> {
        match self {
            Self::LocalOwner | Self::AdminToken => None,
            Self::ReadOnlyViewer { expires_at_ms } => Some(expires_at_ms),
        }
    }
}

/// Current Unix timestamp in milliseconds, returning `None` if the system
/// clock precedes the Unix epoch or overflows `i64` (fails closed).
pub(super) fn current_epoch_ms() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
}

/// Closed reason why a `sv1` viewer token failed verification (every variant maps
/// fail-closed to `401 Unauthorized`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ViewerTokenError {
    EmptyMasterSecret,
    MalformedFormat,
    InvalidSignatureEncoding,
    InvalidSignature,
    ClockOverflow,
    Expired,
}

/// Constant-time byte-slice comparison backed by `ring::hmac::verify`.
pub(super) fn constant_time_eq(given: &[u8], expected: &[u8]) -> bool {
    let key = hmac::Key::new(hmac::HMAC_SHA256, ADMIN_COMPARE_DOMAIN);
    let expected_mac = hmac::sign(&key, expected);
    hmac::verify(&key, given, expected_mac.as_ref()).is_ok()
}

/// Current Unix timestamp in whole seconds (fails closed if the system clock
/// precedes the Unix epoch).
pub(super) fn current_epoch_secs() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|error| format!("system clock precedes Unix epoch: {error}"))
}

/// Parse a compact duration string (`30s`, `15m`, `2h`, `1h30m`, `1d`) into
/// seconds, enforcing `1s <= ttl <= 24h`.
pub(super) fn parse_ttl_secs(input: &str) -> Result<u64, String> {
    let bytes = input.as_bytes();
    if bytes.is_empty() {
        return Err("TTL must not be empty (expected e.g. 30m, 2h, 24h)".to_string());
    }
    let mut i = 0_usize;
    let mut total = 0_u64;
    while i < bytes.len() {
        let num_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == num_start || i >= bytes.len() {
            return Err(format!(
                "invalid TTL {input:?}: expected <digits><s|m|h|d> segments (e.g. 2h, 30m)"
            ));
        }
        let amount: u64 = input[num_start..i]
            .parse()
            .map_err(|_| format!("invalid TTL {input:?}: number overflow"))?;
        let unit_secs: u64 = match bytes[i] {
            b's' => 1,
            b'm' => 60,
            b'h' => 3_600,
            b'd' => 86_400,
            _ => {
                return Err(format!(
                    "invalid TTL unit in {input:?}: expected one of s, m, h, d"
                ));
            }
        };
        i += 1;
        let segment = amount
            .checked_mul(unit_secs)
            .ok_or_else(|| format!("TTL {input:?} overflows u64 seconds"))?;
        total = total
            .checked_add(segment)
            .ok_or_else(|| format!("TTL {input:?} overflows u64 seconds"))?;
    }
    if total == 0 {
        return Err("TTL must be greater than zero".to_string());
    }
    if total > MAX_VIEWER_TTL_SECS {
        return Err(format!(
            "TTL {input:?} ({total}s) exceeds maximum allowed viewer lifetime of 24h ({MAX_VIEWER_TTL_SECS}s)"
        ));
    }
    Ok(total)
}

fn derive_viewer_subkey(master_token: &str) -> Zeroizing<[u8; 32]> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, master_token.as_bytes());
    let digest = hmac::sign(&key, VIEWER_SUBKEY_DOMAIN);
    let mut out = Zeroizing::new([0_u8; 32]);
    out.copy_from_slice(digest.as_ref());
    out
}

/// Mint a deterministic stateless `sv1.<exp_epoch_secs>.<b64url_hmac>` token.
/// Never logs or embeds `master_token` in returned values or error messages.
pub(super) fn mint_viewer_token(
    master_token: &str,
    now_secs: u64,
    ttl_secs: u64,
) -> Result<(String, i64), String> {
    if master_token.is_empty() {
        return Err("master serve token must not be empty".to_string());
    }
    if ttl_secs == 0 || ttl_secs > MAX_VIEWER_TTL_SECS {
        return Err(format!(
            "viewer token TTL {ttl_secs}s out of allowed range 1..={MAX_VIEWER_TTL_SECS}s"
        ));
    }
    let exp_secs = now_secs
        .checked_add(ttl_secs)
        .ok_or_else(|| "viewer token expiration overflows u64".to_string())?;
    let expires_at_ms = i64::try_from(exp_secs)
        .ok()
        .and_then(|secs| secs.checked_mul(1_000))
        .ok_or_else(|| "viewer token expiration overflows i64 milliseconds".to_string())?;
    let subkey = derive_viewer_subkey(master_token);
    let signing_key = hmac::Key::new(hmac::HMAC_SHA256, &subkey[..]);
    let signing_input = format!("{VIEWER_TOKEN_PREFIX}.{exp_secs}");
    let tag = hmac::sign(&signing_key, signing_input.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(tag.as_ref());
    Ok((format!("{signing_input}.{sig_b64}"), expires_at_ms))
}

/// Verify a `sv1.<exp_epoch_secs>.<b64url_hmac>` viewer token fail-closed.
pub(super) fn verify_viewer_token(
    candidate: &str,
    master_token: &str,
    now_secs: u64,
) -> Result<AuthPrincipal, ViewerTokenError> {
    if master_token.is_empty() {
        return Err(ViewerTokenError::EmptyMasterSecret);
    }
    let mut parts = candidate.split('.');
    let (Some(prefix), Some(exp_part), Some(sig_part), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ViewerTokenError::MalformedFormat);
    };
    if prefix != VIEWER_TOKEN_PREFIX
        || exp_part.is_empty()
        || exp_part.starts_with('0')
        || !exp_part.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(ViewerTokenError::MalformedFormat);
    }
    let exp_secs: u64 = exp_part
        .parse()
        .map_err(|_| ViewerTokenError::ClockOverflow)?;
    let expires_at_ms = i64::try_from(exp_secs)
        .ok()
        .and_then(|secs| secs.checked_mul(1_000))
        .ok_or(ViewerTokenError::ClockOverflow)?;

    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_part)
        .map_err(|_| ViewerTokenError::InvalidSignatureEncoding)?;
    if sig_bytes.len() != 32 || URL_SAFE_NO_PAD.encode(&sig_bytes) != sig_part {
        return Err(ViewerTokenError::InvalidSignatureEncoding);
    }

    let subkey = derive_viewer_subkey(master_token);
    let signing_key = hmac::Key::new(hmac::HMAC_SHA256, &subkey[..]);
    let signing_input = format!("{prefix}.{exp_part}");
    hmac::verify(&signing_key, signing_input.as_bytes(), &sig_bytes)
        .map_err(|_| ViewerTokenError::InvalidSignature)?;

    if now_secs >= exp_secs {
        return Err(ViewerTokenError::Expired);
    }
    Ok(AuthPrincipal::ReadOnlyViewer { expires_at_ms })
}

/// Authenticate an explicit bearer or `?token=` credential against `master_token`.
/// Rejects everything except the exact `AdminToken` or a valid unexpired `sv1` token.
pub(super) fn authenticate_bearer(
    candidate: &str,
    master_token: &str,
    now_secs: Option<u64>,
) -> Option<AuthPrincipal> {
    if !master_token.is_empty() && constant_time_eq(candidate.as_bytes(), master_token.as_bytes()) {
        return Some(AuthPrincipal::AdminToken);
    }
    let now = now_secs?;
    verify_viewer_token(candidate, master_token, now).ok()
}

#[cfg(test)]
mod tests {
    use super::{
        AuthPrincipal, DEFAULT_VIEWER_TTL_SECS, MAX_VIEWER_TTL_SECS, VIEWER_TOKEN_PREFIX,
        ViewerTokenError, authenticate_bearer, constant_time_eq, derive_viewer_subkey,
        mint_viewer_token, parse_ttl_secs, verify_viewer_token,
    };
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use ring::hmac;

    const MASTER: &str = "super-secret-master-token-for-tests";
    const NOW: u64 = 1_795_838_000;

    #[test]
    fn ttl_parser_enforces_units_bounds_and_max_24h() {
        assert_eq!(parse_ttl_secs("1s"), Ok(1));
        assert_eq!(parse_ttl_secs("30m"), Ok(1_800));
        assert_eq!(parse_ttl_secs("2h"), Ok(DEFAULT_VIEWER_TTL_SECS));
        assert_eq!(parse_ttl_secs("1h30m"), Ok(5_400));
        assert_eq!(parse_ttl_secs("24h"), Ok(MAX_VIEWER_TTL_SECS));
        assert_eq!(parse_ttl_secs("1d"), Ok(MAX_VIEWER_TTL_SECS));

        for invalid in [
            "",
            "0s",
            "0h",
            "120",
            "24h1s",
            "25h",
            "2d",
            "-1h",
            "2h ",
            " 2h",
            "2x",
            "h",
            "99999999999999999999h",
        ] {
            assert!(parse_ttl_secs(invalid).is_err(), "{invalid:?} must fail");
        }
    }

    #[test]
    fn minted_token_is_header_safe_domain_separated_and_deterministic() {
        let (tok, exp_ms) = mint_viewer_token(MASTER, NOW, 7_200).expect("mint");
        assert_eq!(exp_ms, i64::try_from(NOW + 7_200).expect("i64") * 1_000);
        assert!(tok.starts_with(&format!("{VIEWER_TOKEN_PREFIX}.{}.", NOW + 7_200)));
        assert!(!tok.contains(MASTER));
        assert!(tok.bytes().all(|b| b.is_ascii_graphic()));
        assert!(!tok.contains(['+', '/', '=']));

        let (tok2, exp_ms2) = mint_viewer_token(MASTER, NOW, 7_200).expect("deterministic");
        assert_eq!(tok, tok2);
        assert_eq!(exp_ms, exp_ms2);

        // Domain-separated subkey differs from signing directly with the raw master secret.
        let raw_key = hmac::Key::new(hmac::HMAC_SHA256, MASTER.as_bytes());
        let raw_sig = URL_SAFE_NO_PAD.encode(hmac::sign(
            &raw_key,
            format!("sv1.{}", NOW + 7_200).as_bytes(),
        ));
        assert_ne!(tok.rsplit('.').next(), Some(raw_sig.as_str()));
        assert_ne!(&derive_viewer_subkey(MASTER)[..], MASTER.as_bytes());
    }

    #[test]
    fn verify_and_authenticate_enforce_expiry_tamper_and_format_fail_closed() {
        let (tok, exp_ms) = mint_viewer_token(MASTER, NOW, 3_600).expect("mint");
        assert_eq!(
            verify_viewer_token(&tok, MASTER, NOW),
            Ok(AuthPrincipal::ReadOnlyViewer {
                expires_at_ms: exp_ms
            })
        );
        assert_eq!(
            verify_viewer_token(&tok, MASTER, NOW + 3_599),
            Ok(AuthPrincipal::ReadOnlyViewer {
                expires_at_ms: exp_ms
            })
        );
        assert_eq!(
            verify_viewer_token(&tok, MASTER, NOW + 3_600),
            Err(ViewerTokenError::Expired)
        );
        assert_eq!(
            verify_viewer_token(&tok, MASTER, NOW + 5_000),
            Err(ViewerTokenError::Expired)
        );

        // Wrong master secret -> InvalidSignature.
        assert_eq!(
            verify_viewer_token(&tok, "wrong-secret", NOW),
            Err(ViewerTokenError::InvalidSignature)
        );
        assert_eq!(
            verify_viewer_token(&tok, "", NOW),
            Err(ViewerTokenError::EmptyMasterSecret)
        );

        // Signature tamper & expiration tamper -> InvalidSignature.
        let mut flipped = tok.clone().into_bytes();
        let last = flipped.len() - 1;
        flipped[last] = if flipped[last] == b'A' { b'B' } else { b'A' };
        let tampered_sig = String::from_utf8(flipped).expect("ascii");
        let sig_res = verify_viewer_token(&tampered_sig, MASTER, NOW);
        assert!(
            matches!(
                sig_res,
                Err(ViewerTokenError::InvalidSignature | ViewerTokenError::InvalidSignatureEncoding)
            ),
            "tampered sig got {sig_res:?}"
        );

        let sig_only = tok.rsplit('.').next().expect("sig");
        let tampered_exp = format!("sv1.{}.{sig_only}", NOW + 9_999);
        assert_eq!(
            verify_viewer_token(&tampered_exp, MASTER, NOW),
            Err(ViewerTokenError::InvalidSignature)
        );

        // Malformed formats.
        for bad in [
            "",
            MASTER,
            "sv1.100",
            "sv1.100.abc.def",
            "sv2.9999999999.abc",
            "sv1.01795838000.abc",
            "sv1.0.abc",
            "sv1.-1.abc",
            "sv1.1795838000.",
            "sv1.1795838000.not_32_bytes",
        ] {
            assert!(
                verify_viewer_token(bad, MASTER, NOW).is_err(),
                "{bad:?} must fail closed"
            );
        }

        // authenticate_bearer principal separation & RBAC.
        let admin = authenticate_bearer(MASTER, MASTER, Some(NOW));
        assert_eq!(admin, Some(AuthPrincipal::AdminToken));
        assert!(admin.is_some_and(AuthPrincipal::can_mutate));
        assert_eq!(AuthPrincipal::LocalOwner.stream_expires_at_ms(), None);
        assert_eq!(AuthPrincipal::AdminToken.stream_expires_at_ms(), None);

        let viewer = authenticate_bearer(&tok, MASTER, Some(NOW));
        assert_eq!(
            viewer,
            Some(AuthPrincipal::ReadOnlyViewer {
                expires_at_ms: exp_ms
            })
        );
        assert!(!viewer.is_some_and(AuthPrincipal::can_mutate));
        assert_eq!(
            viewer.and_then(AuthPrincipal::stream_expires_at_ms),
            Some(exp_ms)
        );

        assert_eq!(authenticate_bearer(&tok, MASTER, Some(NOW + 3_600)), None);
        assert_eq!(authenticate_bearer(&tok, MASTER, None), None);
        assert_eq!(authenticate_bearer(&tampered_exp, MASTER, Some(NOW)), None);
    }

    #[test]
    fn constant_time_eq_compares_bytes_accurately() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abd", b"abc"));
        assert!(!constant_time_eq(b"ab", b"abc"));
        assert!(!constant_time_eq(b"", b"abc"));
    }

    #[test]
    fn mint_rejects_empty_secret_and_overlong_or_overflowing_ttls() {
        assert!(mint_viewer_token("", NOW, 60).is_err());
        assert!(mint_viewer_token(MASTER, NOW, 0).is_err());
        assert!(mint_viewer_token(MASTER, NOW, MAX_VIEWER_TTL_SECS + 1).is_err());
        assert!(mint_viewer_token(MASTER, u64::MAX - 10, 60).is_err());
    }
}
