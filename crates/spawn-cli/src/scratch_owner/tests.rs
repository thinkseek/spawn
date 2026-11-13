use super::{Claim, build_path_ok};

#[test]
fn placeholder_compiles() { let _ = (Claim::parse(""), build_path_ok("target")); }
