use spawn_types::clock::{Clock, DeterministicClock};
use spawn_types::identifiers::{IdGenerator, validate_opaque_label};
use spawn_types::observation::Confidence;

#[test]
fn rejects_empty_and_oversized_labels() {
    assert!(validate_opaque_label("").is_err());
    let long = "a".repeat(257);
    assert!(validate_opaque_label(&long).is_err());
    assert!(validate_opaque_label("valid-label_1").is_ok());
}

#[test]
fn rejects_control_characters() {
    assert!(validate_opaque_label("bad\nname").is_err());
}

#[test]
fn identifier_generator_is_ordered_and_unique() {
    let clock = DeterministicClock::starting_at(1_000);
    let generator = IdGenerator::new(&clock, 7);
    let first = generator.new_session_id();
    clock.advance_millis(1);
    let second = generator.new_session_id();
    assert_ne!(first, second);
    assert!(first.as_str().starts_with("session_"));
    assert!(second.as_str().starts_with("session_"));
}

#[test]
fn confidence_enforces_closed_interval() {
    assert!(Confidence::new(0.0).is_ok());
    assert!(Confidence::new(1.0).is_ok());
    assert!(Confidence::new(-0.01).is_err());
    assert!(Confidence::new(1.01).is_err());
    assert!(Confidence::new(f64::NAN).is_err());
}

#[test]
fn deterministic_clock_advances_explicitly() {
    let clock = DeterministicClock::starting_at(10);
    assert_eq!(clock.now().unix_millis(), 10);
    assert_eq!(clock.advance_millis(5).unix_millis(), 15);
}
