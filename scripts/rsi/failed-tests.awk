# failed-tests.awk: the failing tests of a `cargo test` log, as
# rsi/scenarios/check-tests.sh prints them (the gate and the integrator read
# these lines: gate.sh shared_failed and new_failures, integrate/host.rs
# check_tests_failures):
# - `  in <test>: <line>` for each of a test's first five lines that say
#   what failed (`FAIL: `, `ERROR: `, `Error: `, `Error(`), all of them
#   printed as they are read, so before the FAILED lines;
# - then one `FAILED <test>: <where> <message>` per failing test, <where> the
#   panic's location and <message> its first 200 characters.
# The message (objective 208 (d)): the panic's own message, the first line
# after `panicked at`, not the test's first line of output (a test that
# prints before it panics named its output). A test that runs a Python
# unittest file (the relays' self-tests: their panic message is the
# script's output) names the first failing Python test and its error
# instead: `FAIL: <test> (<class>): AssertionError: <message>`. A test with
# no panic line keeps its first line of output.
# A test binary that ended without naming its failure (cargo's `process
# didn't exit successfully: ... (signal: ...)`, or an exit status other than
# a failed test's 101: an abort, a kill, a crash) names no test: it gets
# `CRASHED <target>: <status>`, <target> as cargo's `to rerun pass` names it,
# printed after the FAILED lines. With --no-fail-fast a run can crash one
# binary and name a failing test in another; the gate's flake rule test by
# test (gate.sh, objective 208 (b)) needs to know (rev-g's F1 on 208 R1).
# A Python FAIL:/ERROR: line counts only after unittest's `=====` rule, so a
# Rust test that prints `ERROR: ...` keeps its panic's message (rev-g n1).
function flush() {
    if (name != "") {
        m = msg
        if (pyfail != "") m = pyerr != "" ? pyfail ": " pyerr : pyfail
        failed = failed sprintf("FAILED %s: %s%s\n", name, at, substr(m, 1, 200))
    }
    name = ""; at = ""; msg = ""; pyfail = ""; pyerr = ""; panicked = 0; rule = 0
}
/to rerun pass `/ { target = $0; sub(/^.*to rerun pass `/, "", target); sub(/`.*$/, "", target) }
/process didn't exit successfully: / {
    status = $0
    sub(/^.*\(/, "", status); sub(/\)[^)]*$/, "", status)
    if (status !~ /^exit status: 101$/) crashed = crashed sprintf("CRASHED %s: %s\n", (target != "" ? target : "(a test binary)"), status)
}
/^---- .* stdout ----$/ { flush(); name = $2; next }
/^(failures:|test result:)/ { flush(); next }
name != "" && /^==========/ { rule = 1 }
name != "" && rule && pyfail == "" && /^(FAIL|ERROR): / { pyfail = $0 }
name != "" && pyfail != "" && pyerr == "" && /^[A-Za-z_][A-Za-z0-9_.]*(Error|Exception): / { pyerr = $0 }
name != "" && at == "" && /panicked at/ { sub(/^thread .* panicked at /, ""); at = $0 " "; panicked = 1; next }
name != "" && panicked == 1 && NF { msg = $0; panicked = 2; next }
name != "" && msg == "" && NF { msg = $0; next }
name != "" && /^(FAIL|ERROR): |Error: |Error\(/ && kept[name]++ < 5 { printf "  in %s: %s\n", name, substr($0, 1, 200) }
END { flush(); printf "%s%s", failed, crashed }
