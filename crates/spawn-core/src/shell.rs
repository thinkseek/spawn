//! Building shell command strings for external CLIs.

/// Quote one argv element for `sh -c`.
///
/// The result is a single shell word: wrapped in single quotes, with every
/// internal single quote rewritten as `'\''`. This is injective, so the
/// invoked program receives the exact bytes of the input and no part of it is
/// interpreted as shell syntax.
pub fn shell_quote(text: &str) -> String {
    let mut word = String::with_capacity(text.len() + 2);
    word.push('\'');
    for character in text.chars() {
        if character == '\'' {
            word.push_str("'\\''");
        } else {
            word.push(character);
        }
    }
    word.push('\'');
    word
}

/// Join argv into one shell command string.
pub fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|element| shell_quote(element))
        .collect::<Vec<String>>()
        .join(" ")
}
