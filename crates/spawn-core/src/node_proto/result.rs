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
    /// Set when the gate never started, or the node stopped it for a reason
    /// of its own (its watchdog): a node fault by definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrapper_error: Option<String>,
}

/// How a job ended, read from a [`ResultMeta`] in the order of section 6.2
/// (the order the primary judges a stored result in): the first that
/// applies.
///
/// The wire keeps five loose fields (`exit_code`, `signal`, `timed_out`,
/// `self_fenced`, `wrapper_error`) so the bytes stay the Go node's; this
/// is the one reading of them. A field a variant does not carry says
/// nothing more: a self-fenced or timed-out run's status, and the verdict
/// of a run that did not exit, are never judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome<'a> {
    /// The node fenced itself (its lease epoch moved): nothing is judged.
    SelfFenced,
    /// The gate never started, or the node stopped it for a reason of its
    /// own: a node fault, with the node's reason.
    WrapperError(&'a str),
    /// The node stopped the gate at its wall clock.
    TimedOut,
    /// The gate died of a signal.
    Signalled(&'a Signal),
    /// The gate exited with this status, having written this verdict file
    /// (or none).
    Exited {
        /// The gate's exit status.
        code: u8,
        /// The verdict file's bytes, when it wrote one.
        verdict: Option<&'a str>,
    },
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
        self.outcome().map(|_| ())
    }

    /// How the job ended ([`RunOutcome`]). Refuses a line whose ending
    /// cannot be read, none of which the Go node writes: both an exit code
    /// and a signal, an exit code outside `0..=255`, both `timed_out` and
    /// `self_fenced`, an empty `wrapper_error`, or no ending at all (no
    /// status, no wrapper error, neither flag).
    pub fn outcome(&self) -> Result<RunOutcome<'_>, Error> {
        /// The gate's own ending, when it ran to one.
        enum Status<'a> {
            Exited(u8),
            Signalled(&'a Signal),
        }
        let status = match (self.exit_code, &self.signal) {
            (Some(_), Some(_)) => return Err(Error::Body("exit_code and signal")),
            (Some(code), None) => Some(Status::Exited(
                u8::try_from(code).map_err(|_| Error::Body("exit_code"))?,
            )),
            (None, Some(signal)) => Some(Status::Signalled(signal)),
            (None, None) => None,
        };
        if self.timed_out && self.self_fenced {
            return Err(Error::Body("timed_out and self_fenced"));
        }
        let wrapper = match self.wrapper_error.as_deref() {
            Some("") => return Err(Error::Body("wrapper_error")),
            wrapper => wrapper,
        };
        Ok(match (self.self_fenced, wrapper, self.timed_out, status) {
            (true, _, _, _) => RunOutcome::SelfFenced,
            (false, Some(error), _, _) => RunOutcome::WrapperError(error),
            (false, None, true, _) => RunOutcome::TimedOut,
            (false, None, false, Some(Status::Signalled(signal))) => RunOutcome::Signalled(signal),
            (false, None, false, Some(Status::Exited(code))) => RunOutcome::Exited {
                code,
                verdict: self.verdict_raw.as_deref(),
            },
            (false, None, false, None) => return Err(Error::Body("no outcome")),
        })
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

    /// The 23 metadata lines of spawn-node's golden gate outcomes
    /// (`testdata/node-protocol-v1/gate-outcomes/*.json`, the `meta` of
    /// each, byte for byte, at thinkseek/spawn-node `dd32af3`), with the
    /// outcome each reads as.
    const GO_GATE_OUTCOMES: [(&str, &str, &str); 23] = [
        (
            "exit0-accepted",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":1,"exit_code":0,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":"{\"v\":1,\"verdict\":\"accepted\",\"base\":\"bd70ebd2b117b619585190e5bd186053135c3f88\",\"candidate\":\"39dc69147e038543a740ea68857b3dc0b3d743d2\"}\n","stdout_bytes":319,"log_bytes":348,"log_sha256":"94230991c423cf9119c40b2a7ede74b7ca8e022630ea0da863abcc5f4689a3c1","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "exited 0 with a verdict",
        ),
        (
            "exit0-no-verdict",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":3,"exit_code":0,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":null,"stdout_bytes":7,"log_bytes":7,"log_sha256":"5bde10ab52ac4959c5ec33346cd53ed10ce5c47894d29efe63837e11803c8854","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "exited 0",
        ),
        (
            "exit0-rejected",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":2,"exit_code":0,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":"{\"v\":1,\"verdict\":\"rejected\",\"reason\":\"012.sh: unsolved on the candidate, solved on the base\",\"base\":\"bd70ebd2b117b619585190e5bd186053135c3f88\",\"candidate\":\"39dc69147e038543a740ea68857b3dc0b3d743d2\"}\n","stdout_bytes":407,"log_bytes":407,"log_sha256":"1ad06de389f6fb0817050dd577499d472c0a01c40ac478f5e811f80105be7414","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "exited 0 with a verdict",
        ),
        (
            "exit137",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":17,"exit_code":137,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":null,"stdout_bytes":0,"log_bytes":7,"log_sha256":"0089bb674406997eb0e6850cf01c46d23d8689610788516cebf2a31b9276d691","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "exited 137",
        ),
        (
            "exit2",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":4,"exit_code":2,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":null,"stdout_bytes":0,"log_bytes":41,"log_sha256":"9cc55ec1f9ea13b78cdc8f4055bb1cfc34b445139cb9da6492974ae1b198aaa9","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "exited 2",
        ),
        (
            "exit3",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":5,"exit_code":3,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":null,"stdout_bytes":0,"log_bytes":46,"log_sha256":"c700797cbd0d6b796b3fa5afd54a713062e60892c6ca5c45898a0368b34b0648","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "exited 3",
        ),
        (
            "exit4-base-disagrees",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":6,"exit_code":4,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":null,"stdout_bytes":398,"log_bytes":398,"log_sha256":"2392fd630b9853382e13616fcbb7d80010f8358ad07d7fa6ef32f9544008874c","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "exited 4",
        ),
        (
            "exit4-no-pair",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":7,"exit_code":4,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":null,"stdout_bytes":213,"log_bytes":213,"log_sha256":"7b4bb601c4b2bff0bfb0469dbaf50959fde21d95b4fd615f54fdfc32d2c94620","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "exited 4",
        ),
        (
            "exit5-no-fact",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":10,"exit_code":5,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":null,"stdout_bytes":0,"log_bytes":0,"log_sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "exited 5",
        ),
        (
            "exit5-no-room",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":8,"exit_code":5,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":"{\"v\":1,\"no_room\":true,\"free_kb\":6291456,\"min_gb\":8,\"tmpdir\":\"/data01/spawn-node/w/g00/t\"}\n","stdout_bytes":90,"log_bytes":90,"log_sha256":"10667cc5b90f4d7abe7dee78b85e79f405af046752f23fb3c48fb5243878be8c","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "exited 5 with a verdict",
        ),
        (
            "exit5-slot-wait",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":9,"exit_code":5,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":"{\"v\":1,\"no_room\":true,\"check\":\"host scenario slot\",\"waited_ms\":600000,\"budget\":4,\"scenario\":\"011.sh\",\"base\":\"bd70ebd2b117b619585190e5bd186053135c3f88\",\"candidate\":\"39dc69147e038543a740ea68857b3dc0b3d743d2\"}\n","stdout_bytes":506,"log_bytes":506,"log_sha256":"1e1e52f5c7ca0c185c63c48018e3f075f66a545dbcc9986a55f43d192bc14157","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "exited 5 with a verdict",
        ),
        (
            "exit6-cannot-judge",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":14,"exit_code":6,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":"{\"v\":1,\"cannot_judge\":true,\"scenario\":\"021-web-e2e.sh\",\"runs\":\"none: a prerequisite is missing\",\"prerequisite\":\"x86_64\",\"base\":\"bd70ebd2b117b619585190e5bd186053135c3f88\",\"candidate\":\"39dc69147e038543a740ea68857b3dc0b3d743d2\"}\n","stdout_bytes":413,"log_bytes":413,"log_sha256":"f141cd3a3d9693698ea152d7a2fd16a8b5596c5989d60044894f6f3a6b43e444","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "exited 6 with a verdict",
        ),
        (
            "exit6-host-fault-base",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":12,"exit_code":6,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":"{\"v\":1,\"host_fault\":true,\"scenario\":\"011.sh\",\"on\":\"base\",\"runs\":\"2\",\"reason\":\"011.sh exited 2 on the base, before any candidate code ran: a host or harness fault, not a verdict on the candidate\",\"base\":\"bd70ebd2b117b619585190e5bd186053135c3f88\",\"candidate\":\"39dc69147e038543a740ea68857b3dc0b3d743d2\"}\n","stdout_bytes":487,"log_bytes":487,"log_sha256":"a85ffe665b92f39819232cc9fbbdb53e553312215ea8a85329f6e9126db76597","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "exited 6 with a verdict",
        ),
        (
            "exit6-host-fault-sandbox",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":11,"exit_code":6,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":"{\"v\":1,\"host_fault\":true,\"scenario\":\"011.sh\",\"on\":\"sandbox\",\"runs\":\"never started\",\"reason\":\"bwrap: Creating new namespace failed: No space left on device: a host fault, not a verdict on the candidate\",\"evidence\":\"sandbox_never_started\",\"tree\":\"candidate\",\"base\":\"bd70ebd2b117b619585190e5bd186053135c3f88\",\"candidate\":\"39dc69147e038543a740ea68857b3dc0b3d743d2\"}\n","stdout_bytes":547,"log_bytes":547,"log_sha256":"fd2cfe8a68f409d3c9cf5c8f2e31e8da245aaecf9be2675c91c884e9c345cd40","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "exited 6 with a verdict",
        ),
        (
            "exit6-main-red",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":13,"exit_code":6,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":"{\"v\":1,\"host_fault\":true,\"scenario\":\"check-tests.sh\",\"on\":\"main\",\"runs\":\"1, then 1\",\"reason\":\"check-tests.sh fails on the base twice (1, then 1), with the same failing tests: main is red on it, not a verdict on the candidate\",\"failures\":[{\"test\":\"spawn-core::fold::refold_matches\",\"message\":\"assertion failed: left == right\"}],\"base\":\"bd70ebd2b117b619585190e5bd186053135c3f88\",\"candidate\":\"39dc69147e038543a740ea68857b3dc0b3d743d2\"}\n","stdout_bytes":620,"log_bytes":620,"log_sha256":"804c8bc0be0ddbf0033082de5955fc5f92ac68c54feab7f8e12291c57940c8d6","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "exited 6 with a verdict",
        ),
        (
            "exit6-no-fact",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":15,"exit_code":6,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":null,"stdout_bytes":0,"log_bytes":0,"log_sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "exited 6",
        ),
        (
            "fact-unread-version",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":16,"exit_code":6,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":"{\"v\":2,\"host_fault\":true,\"scenario\":\"011.sh\",\"on\":\"sandbox\",\"runs\":\"never started\",\"reason\":\"bwrap: Creating new namespace failed: No space left on device: a host fault, not a verdict on the candidate\",\"evidence\":\"sandbox_never_started\",\"tree\":\"candidate\",\"base\":\"bd70ebd2b117b619585190e5bd186053135c3f88\",\"candidate\":\"39dc69147e038543a740ea68857b3dc0b3d743d2\"}\n","stdout_bytes":547,"log_bytes":547,"log_sha256":"4fe10ee817f78e1f52db4cca81f9d4de9768665cf60df471b8efec270cbd8d85","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "exited 6 with a verdict",
        ),
        (
            "self-fenced",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":23,"exit_code":null,"signal":15,"timed_out":false,"self_fenced":true,"seconds":0,"verdict_raw":null,"stdout_bytes":11,"log_bytes":11,"log_sha256":"adcec70f76a3a97a6357637fc0956d70a53cd514e9051a3173fed429c0024bcc","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "self_fenced",
        ),
        (
            "signal9",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":18,"exit_code":null,"signal":9,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":null,"stdout_bytes":11,"log_bytes":11,"log_sha256":"adcec70f76a3a97a6357637fc0956d70a53cd514e9051a3173fed429c0024bcc","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "signalled 9",
        ),
        (
            "timed-out",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":19,"exit_code":null,"signal":15,"timed_out":true,"self_fenced":false,"seconds":0,"verdict_raw":null,"stdout_bytes":15,"log_bytes":15,"log_sha256":"d3a73f9faa2370bf0585f5ded9bab13a801a220be60a4231b92e13e2ee2bb1a1","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"}}"#,
            "timed_out",
        ),
        (
            "wrapper-digest-mismatch",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":20,"exit_code":null,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":null,"stdout_bytes":0,"log_bytes":0,"log_sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"},"wrapper_error":"digest_mismatch"}"#,
            "wrapper_error digest_mismatch",
        ),
        (
            "wrapper-requires-not-held",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":21,"exit_code":null,"signal":null,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":null,"stdout_bytes":0,"log_bytes":0,"log_sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"},"wrapper_error":"requires_not_held"}"#,
            "wrapper_error requires_not_held",
        ),
        (
            "wrapper-tmpfs-budget",
            r#"{"v":1,"job_id":"gate-0123456789ab-k3x9q2ma","lease_epoch":22,"exit_code":null,"signal":15,"timed_out":false,"self_fenced":false,"seconds":0,"verdict_raw":null,"stdout_bytes":11,"log_bytes":11,"log_sha256":"adcec70f76a3a97a6357637fc0956d70a53cd514e9051a3173fed429c0024bcc","inputs":{"base":"bd70ebd2b117b619585190e5bd186053135c3f88","candidate":"39dc69147e038543a740ea68857b3dc0b3d743d2","controller":"cccccccccccccccccccccccccccccccccccccccc","scenarios_sha256":"191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707","scenario":null},"env":{"host":"n252","toolchain":"1.95.0","node":"n252","tier":"tmpfs-bwrap"},"wrapper_error":"tmpfs_budget_exceeded"}"#,
            "wrapper_error tmpfs_budget_exceeded",
        ),
    ];

    /// A short, stable name for an outcome, to compare against the table.
    fn named(outcome: RunOutcome<'_>) -> String {
        match outcome {
            RunOutcome::SelfFenced => "self_fenced".to_string(),
            RunOutcome::WrapperError(error) => format!("wrapper_error {error}"),
            RunOutcome::TimedOut => "timed_out".to_string(),
            RunOutcome::Signalled(Signal::Number(number)) => format!("signalled {number}"),
            RunOutcome::Signalled(Signal::Name(name)) => format!("signalled {name}"),
            RunOutcome::Exited { code, verdict } => match verdict {
                Some(_) => format!("exited {code} with a verdict"),
                None => format!("exited {code}"),
            },
        }
    }

    #[test]
    fn every_go_golden_outcome_parses_round_trips_and_reads_as_its_row() {
        for (name, line, want) in GO_GATE_OUTCOMES {
            let meta = ResultMeta::parse(line.as_bytes()).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(meta.to_bytes(), line.as_bytes(), "{name}: bytes round trip");
            let outcome = meta.outcome().unwrap();
            assert_eq!(named(outcome), want, "{name}");
            if let RunOutcome::Exited { verdict, .. } = outcome {
                assert_eq!(verdict, meta.verdict_raw.as_deref(), "{name}");
            }
        }
    }

    /// The accepted gate outcome, to change one field at a time.
    fn accepted() -> ResultMeta {
        ResultMeta::parse(GO_GATE_OUTCOMES[0].1.as_bytes()).unwrap()
    }

    /// Every combination of the five fields that reads as an outcome reads
    /// as the first of self_fenced, wrapper_error, timed_out, then the
    /// gate's own status (section 6.2's order, the primary's).
    #[test]
    fn every_readable_combination_reads_in_section_6_2_order() {
        let statuses = [
            (Some(0), None),
            (Some(255), None),
            (None, Some(Signal::Number(9))),
            (None, Some(Signal::Name("SIGTERM".to_string()))),
            (None, None),
        ];
        let mut read = 0;
        for self_fenced in [false, true] {
            for timed_out in [false, true] {
                for wrapper in [None, Some("sandbox_failed")] {
                    for verdict in [None, Some("{}\n")] {
                        for (exit_code, signal) in statuses.clone() {
                            let meta = ResultMeta {
                                exit_code,
                                signal: signal.clone(),
                                timed_out,
                                self_fenced,
                                verdict_raw: verdict.map(str::to_string),
                                wrapper_error: wrapper.map(str::to_string),
                                ..accepted()
                            };
                            let got = meta.outcome();
                            if self_fenced && timed_out {
                                assert_eq!(got, Err(Error::Body("timed_out and self_fenced")));
                                continue;
                            }
                            let want = if self_fenced {
                                Some(RunOutcome::SelfFenced)
                            } else if let Some(error) = wrapper {
                                Some(RunOutcome::WrapperError(error))
                            } else if timed_out {
                                Some(RunOutcome::TimedOut)
                            } else if let Some(signal) = &signal {
                                Some(RunOutcome::Signalled(signal))
                            } else {
                                exit_code.map(|code| RunOutcome::Exited {
                                    code: u8::try_from(code).unwrap(),
                                    verdict,
                                })
                            };
                            match want {
                                Some(want) => {
                                    assert_eq!(got, Ok(want), "{meta:?}");
                                    assert_eq!(meta.validate(), Ok(()));
                                    read += 1;
                                }
                                None => assert_eq!(got, Err(Error::Body("no outcome")), "{meta:?}"),
                            }
                        }
                    }
                }
            }
        }
        // 3 flag pairs (both set is refused) x 2 wrappers x 2 verdicts x 5
        // statuses, less the 2 lines with no ending (no flag, no wrapper, no
        // status; with or without a verdict).
        assert_eq!(read, 3 * 2 * 2 * 5 - 2);
    }

    /// The shorthands the primary's own tests write (a self-fenced exit 0
    /// with a verdict; a timed-out line with no status) stay readable.
    #[test]
    fn the_primarys_test_shorthands_stay_readable() {
        let fenced = ResultMeta {
            self_fenced: true,
            ..accepted()
        };
        assert_eq!(fenced.outcome(), Ok(RunOutcome::SelfFenced));
        let timed_out = ResultMeta {
            exit_code: None,
            verdict_raw: None,
            timed_out: true,
            ..accepted()
        };
        assert_eq!(timed_out.outcome(), Ok(RunOutcome::TimedOut));
    }

    /// The lines whose ending cannot be read are refused by `outcome`, by
    /// `validate` and by `parse` alike: one rule, read in one place.
    #[test]
    fn unreadable_endings_are_refused_everywhere() {
        let cases = [
            (
                ResultMeta {
                    signal: Some(Signal::Number(9)),
                    ..accepted()
                },
                Error::Body("exit_code and signal"),
            ),
            (
                ResultMeta {
                    exit_code: Some(256),
                    ..accepted()
                },
                Error::Body("exit_code"),
            ),
            (
                ResultMeta {
                    exit_code: Some(-1),
                    ..accepted()
                },
                Error::Body("exit_code"),
            ),
            (
                ResultMeta {
                    timed_out: true,
                    self_fenced: true,
                    ..accepted()
                },
                Error::Body("timed_out and self_fenced"),
            ),
            (
                ResultMeta {
                    wrapper_error: Some(String::new()),
                    ..accepted()
                },
                Error::Body("wrapper_error"),
            ),
            (
                ResultMeta {
                    exit_code: None,
                    verdict_raw: None,
                    ..accepted()
                },
                Error::Body("no outcome"),
            ),
        ];
        for (meta, error) in cases {
            assert_eq!(meta.outcome(), Err(error.clone()), "{meta:?}");
            assert_eq!(meta.validate(), Err(error.clone()), "{meta:?}");
            assert_eq!(ResultMeta::parse(&meta.to_bytes()), Err(error), "{meta:?}");
        }
    }
}
