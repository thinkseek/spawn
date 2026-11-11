use super::host::{self, Class};
use super::{GateFailure, before_the_candidate, red_as_host_fault, red_believed};

const REFUSED: &str = "connect to the test database: error connecting to server: Connection \
                           refused (os error 111)";

/// A red is main's own only when some shared failure is the work's and
/// none is a connection or database error (§10.3 "Main is red"): a
/// shared {assertion, connection refused} is the host's, as a broken
/// test server fails main and candidate alike.
#[test]
fn a_red_with_a_connection_failure_among_its_shared_failures_is_the_hosts() {
    let assertion = host::classify("assertion `left == right` failed");
    assert_eq!(assertion, Class::Work);
    assert_eq!(host::classify(REFUSED), Class::Connection);
    assert!(red_believed(&[assertion]));
    assert!(red_believed(&[assertion, assertion]));
    assert!(
        !red_believed(&[assertion, host::classify(REFUSED)]),
        "mixed: the host's"
    );
    assert!(!red_believed(&[Class::Database, assertion]));
    assert!(!red_believed(&[Class::Connection]));
    assert!(!red_believed(&[]));
}

/// A red the gate's base check found (before the candidate) that is not
/// believed is a fault of the base alone: `on` `base`, which the cap
/// never counts, and no word of a candidate that never ran.
#[test]
fn a_base_check_red_not_believed_is_the_bases_host_fault() {
    let runs = "1, then 1 on the base, before the candidate";
    assert!(before_the_candidate(runs));
    let GateFailure::HostFault { on, reason, .. } =
        red_as_host_fault("check-tests.sh".into(), runs.into(), &[], String::new())
    else {
        panic!("a host fault");
    };
    assert_eq!(on, "base");
    assert!(
        reason.contains("twice on the base, before the candidate"),
        "{reason}"
    );
    assert!(!reason.contains("on the candidate and"), "{reason}");
    let runs = "1, then 1 on the candidate; 1, then 1 on the base";
    assert!(!before_the_candidate(runs));
    let GateFailure::HostFault { on, .. } =
        red_as_host_fault("check-tests.sh".into(), runs.into(), &[], String::new())
    else {
        panic!("a host fault");
    };
    assert_eq!(on, "candidate and base");
}
