//! `POST /node/v1/jobs/<job_id>/result` (section 3.5): one line of metadata
//! JSON, one `\n`, then the raw log bytes. The signed body is the metadata
//! line without its newline; the log is bound by `log_sha256` inside it.

use serde::{Deserialize, Serialize};

use super::codec::{is_lower_hex, sha256_hex};
use super::json::{self, OrderedMap};
use super::target::is_job_id;
use super::{Error, Limit, VERSION};

/// The signal a gate died of: a number, or a name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Signal {
    /// Numeric signal.
    Number(u64),
    /// Symbolic signal name.
    Name(String),
}

/// The inputs the node judged (compared field by field with the
/// primary's, as `shadow.rs` does).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultInputs {
    /// Base commit.
    pub base: Option<String>,
    /// Candidate commit.
    pub candidate: Option<String>,
    /// Controller commit.
    pub controller: Option<String>,
    /// Scenario tree digest.
    pub scenarios_sha256: Option<String>,
    /// Selected scenario name.
    pub scenario: Option<String>,
}

/// A result's metadata line. `exit_code`, `signal` and `verdict_raw` are
/// always written (null when absent); the fields after `log_sha256` are
/// written only when set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultMeta {
    /// Protocol version.
    pub v: u64,
    /// Job identifier.
    pub job_id: String,
    /// Monotonic lease epoch.
    pub lease_epoch: u64,
    /// The gate's own status; null when it died of a signal or never ran.
    pub exit_code: Option<i64>,
    /// Signal that terminated the job, if any.
    pub signal: Option<Signal>,
    /// Whether execution timed out.
    pub timed_out: bool,
    /// Whether the node fenced itself before running or reporting.
    pub self_fenced: bool,
    /// Elapsed runtime in whole seconds.
    pub seconds: u64,
    /// The verdict file's bytes (at most 64 KiB), or null.
    pub verdict_raw: Option<String>,
    /// Where stdout ends in the log (stderr follows).
    pub stdout_bytes: u64,
    /// Total log bytes.
    pub log_bytes: u64,
    /// Lowercase SHA-256 hex digest of the full log.
    pub log_sha256: String,
    /// Whether the log was truncated to fit [`Limit::ResultLog`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    /// Gate inputs recorded by the node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<ResultInputs>,
    /// Environment summary recorded by the node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<OrderedMap<String>>,
    /// Set when the gate never started: a node fault by definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrapper_error: Option<String>,
}

impl ResultMeta {
    /// A received metadata line: within 256 KiB, strict JSON, valid fields.
    pub fn parse(line: &[u8]) -> Result<Self, Error> {
        if line.len() > Limit::ResultMeta.bytes() {
            return Err(Error::TooLarge {
                limit: Limit::ResultMeta,
                size: line.len(),
            });
        }
        let meta: Self = json::parse(line)?;
        meta.validate()?;
        Ok(meta)
    }

    /// Validate version, identifiers, digests and size bounds.
    pub fn validate(&self) -> Result<(), Error> {
        if self.v != VERSION {
            return Err(Error::Body("v"));
        }
        if !is_job_id(&self.job_id) {
            return Err(Error::Body("job_id"));
        }
        if !is_lower_hex(&self.log_sha256, 64) {
            return Err(Error::Body("log_sha256"));
        }
        if self.stdout_bytes > self.log_bytes {
            return Err(Error::Body("stdout_bytes"));
        }
        if self.log_bytes > Limit::ResultLog.bytes() as u64 {
            return Err(Error::TooLarge {
                limit: Limit::ResultLog,
                size: usize::try_from(self.log_bytes).unwrap_or(usize::MAX),
            });
        }
        if self
            .verdict_raw
            .as_ref()
            .is_some_and(|raw| raw.len() > Limit::Verdict.bytes())
        {
            return Err(Error::TooLarge {
                limit: Limit::Verdict,
                size: self.verdict_raw.as_ref().map_or(0, String::len),
            });
        }
        Ok(())
    }

    /// The metadata line (without its newline): the signed body.
    pub fn to_bytes(&self) -> Vec<u8> {
        json::to_vec(self)
    }

    /// Checks the received log against `log_bytes` and `log_sha256`.
    pub fn check_log(&self, log: &[u8]) -> Result<(), Error> {
        if log.len() as u64 == self.log_bytes && sha256_hex(log) == self.log_sha256 {
            Ok(())
        } else {
            Err(Error::LogMismatch)
        }
    }

    /// The log split back into stdout and stderr.
    pub fn streams<'a>(&self, log: &'a [u8]) -> Result<(&'a [u8], &'a [u8]), Error> {
        let at = usize::try_from(self.stdout_bytes).map_err(|_| Error::LogMismatch)?;
        if at > log.len() {
            return Err(Error::LogMismatch);
        }
        Ok(log.split_at(at))
    }
}

/// A result's body: the metadata line, `\n`, the log.
pub fn build_body(meta_line: &[u8], log: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(meta_line.len() + 1 + log.len());
    body.extend_from_slice(meta_line);
    body.push(b'\n');
    body.extend_from_slice(log);
    body
}

/// Splits a result body into its metadata line (without the newline) and
/// its log, refusing a metadata line over 256 KiB or a log over 32 MiB
/// (`413`), and a body with no newline (`400`).
pub fn split_body(body: &[u8]) -> Result<(&[u8], &[u8]), Error> {
    let meta_cap = Limit::ResultMeta.bytes();
    let window = &body[..body.len().min(meta_cap + 1)];
    let Some(newline) = window.iter().position(|&b| b == b'\n') else {
        return Err(if body.len() > meta_cap {
            Error::TooLarge {
                limit: Limit::ResultMeta,
                size: body.len(),
            }
        } else {
            Error::Body("result has no metadata line")
        });
    };
    let (meta, log) = (&body[..newline], &body[newline + 1..]);
    if log.len() > Limit::ResultLog.bytes() {
        return Err(Error::TooLarge {
            limit: Limit::ResultLog,
            size: log.len(),
        });
    }
    Ok((meta, log))
}

#[cfg(test)]
mod tests {
    use super::super::Refusal;
    use super::*;

    #[test]
    fn the_body_splits_at_the_first_newline_and_enforces_both_caps() {
        let (meta, log) = split_body(b"{}\nab\ncd").unwrap();
        assert_eq!((meta, log), (&b"{}"[..], &b"ab\ncd"[..]));
        assert_eq!(
            split_body(b"{}"),
            Err(Error::Body("result has no metadata line"))
        );

        let long_meta = vec![b' '; Limit::ResultMeta.bytes() + 1];
        let err = split_body(&long_meta).unwrap_err();
        assert_eq!(err.refusal(), Refusal::TooLarge);
        let mut at_cap = vec![b' '; Limit::ResultMeta.bytes()];
        at_cap.push(b'\n');
        split_body(&at_cap).unwrap();

        let mut big_log = b"{}\n".to_vec();
        big_log.resize(3 + Limit::ResultLog.bytes() + 1, b'x');
        let err = split_body(&big_log).unwrap_err();
        assert!(matches!(
            err,
            Error::TooLarge {
                limit: Limit::ResultLog,
                ..
            }
        ));
        assert_eq!(err.refusal(), Refusal::TooLarge);
    }
}
