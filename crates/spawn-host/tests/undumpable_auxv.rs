//! Regression test for Linux < 6.4 (`5.4.143`) where `prctl(PR_SET_DUMPABLE, 0)`
//! makes `/proc/self/auxv` unreadable and `rustix::param::clock_ticks_per_second`
//! must use libc `getauxval` (`use-libc-auxv`) instead of panicking.

#[cfg(target_os = "linux")]
#[test]
fn clock_ticks_survives_undumpable_process() {
    spawn_host::undumpable().expect("set PR_SET_DUMPABLE=0");
    let hertz = rustix::param::clock_ticks_per_second();
    assert!(hertz > 0, "clock_ticks_per_second must be positive, got {hertz}");
}
