//! The mask (objective 117; sync/design-secret-injection.md 3.4 and 5.A):
//! known secret values replaced by `[SECRET:NAME]` in every form a program
//! prints them, before output reaches an agent, and so a transcript.
//!
//! The values file holds `NAME=value` lines (`#` comments and blank lines
//! ignored; NAME upper-case letters, digits and `_`; one NAME on several
//! lines gives its current and retired values). It is read by path, at mode
//! 600, and a value is never on an argv. A value under 8 bytes is refused:
//! masking a common short string would break every output.
//!
//! The mask itself (its forms and how a match is replaced) is
//! [`spawn_core::mask`], which the transcript archive uses too (objective
//! 175). Each value is masked plain, JSON-escaped, URL-encoded (upper- and
//! lower-case hex: also the password of a `user:password@` URL), as base64
//! at each of the three alignments in the standard and URL-safe alphabets
//! (the characters that do not depend on the neighbouring bytes), and as hex
//! in both cases. A form under 6 bytes is not used; at one place the
//! longest form wins, and every form that starts inside a match and runs
//! past its end joins it, until none does: the union is one
//! `[SECRET:NAME]`, named by the form that matched first. A value
//! transformed another way (base64 wrapped across lines as MIME does,
//! reversed, split, encrypted) is not caught (design section 4).
//!
//! - `spawn mask --values <file>`: stdin to stdout through the mask as it
//!   streams, the last bytes of each read (as many as the longest form,
//!   less one, or a whole span still being joined) held for the next, so a
//!   value split across reads is caught and the output does not depend on
//!   where the input is cut.
//! - `spawn mask-hook --values <file>`: a Claude Code PostToolUse hook. When
//!   `tool_response` holds a form of a value, it prints the response with
//!   every string masked as `updatedToolOutput`; otherwise nothing.
//!
//! Both fail closed: a values file that cannot be read, has group or world
//! access, holds a line that is not NAME=value or a short value, or names
//! no value makes `spawn mask` exit 2 having printed nothing of its input,
//! and makes the hook withhold the whole response, as does an event it
//! cannot read or parse. Errors name the file and the line, never a value.
//! Nothing here installs the hook anywhere.

use std::io::Read;
use std::path::Path;

use serde_json::{Value, json};
use spawn_core::mask::Mask;

/// The shortest value the file may hold.
const SHORTEST_VALUE: usize = 8;

/// The values of the file at `path`, `(name, value)` in its order.
///
/// # Errors
///
/// The file and the reason, with the line when it is one line's: never a
/// value.
pub(crate) fn read_values(path: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let shown = path.display();
    let meta = std::fs::metadata(path).map_err(|error| format!("{shown}: cannot read: {error}"))?;
    if !meta.is_file() {
        return Err(format!("{shown}: not a file"));
    }
    {
        use std::os::unix::fs::PermissionsExt as _;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(format!("{shown}: has group or world access (chmod 600)"));
        }
    }
    let bytes = std::fs::read(path).map_err(|error| format!("{shown}: cannot read: {error}"))?;
    let mut values = Vec::new();
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        let number = index + 1;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        let Some(equals) = line.iter().position(|byte| *byte == b'=') else {
            return Err(format!("{shown}: line {number}: not NAME=value"));
        };
        let (name, value) = (&line[..equals], &line[equals + 1..]);
        if name.is_empty()
            || !name
                .iter()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
        {
            return Err(format!("{shown}: line {number}: not NAME=value"));
        }
        if value.len() < SHORTEST_VALUE {
            return Err(format!(
                "{shown}: line {number}: a value shorter than {SHORTEST_VALUE} bytes"
            ));
        }
        values.push((String::from_utf8_lossy(name).into_owned(), value.to_vec()));
    }
    if values.is_empty() {
        return Err(format!("{shown}: names no value"));
    }
    Ok(values)
}

/// What a match of a value becomes: `[SECRET:NAME]`.
const MARKER: &str = "SECRET";

/// The values file named by `--values <file>`, the only argument.
fn values_flag(arguments: &[String], usage: &str) -> Result<String, String> {
    match arguments {
        [flag, path] if flag == "--values" => Ok(path.clone()),
        _ => Err(usage.to_string()),
    }
}

/// `spawn mask`'s usage, with what it does not catch.
const MASK_USAGE: &str = "usage: spawn mask --values <file>\n\
    stdin to stdout with every known form of each value (plain, JSON-escaped, URL-encoded,\n\
    base64, hex) replaced by [SECRET:NAME]. Not masked: a value transformed another way,\n\
    such as base64 wrapped across lines (MIME, 76 columns), reversed, split or encrypted.";

/// `spawn mask --values <file>`: exit 2, having printed nothing, when the
/// values file cannot be used.
pub(crate) fn mask_command(arguments: &[String]) -> Result<i32, String> {
    let path = values_flag(arguments, MASK_USAGE)?;
    let mask = match read_values(Path::new(&path)) {
        Ok(values) => Mask::new(&values, MARKER),
        Err(error) => {
            eprintln!("spawn mask: {error}");
            return Ok(2);
        }
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    if let Err(error) = mask.stream(&mut stdin.lock(), &mut stdout.lock()) {
        eprintln!("spawn mask: {error}");
        return Ok(2);
    }
    Ok(0)
}

/// A PostToolUse hook's answer: `updated` replaces the tool's output.
fn hook_answer(updated: &Value) -> String {
    json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "updatedToolOutput": updated}})
        .to_string()
}

/// The answer that withholds the whole response, saying why in words that
/// hold nothing of the input.
fn withheld(why: &str) -> String {
    hook_answer(&json!(format!(
        "[the tool's output was withheld: spawn mask-hook could not check it ({why})]"
    )))
}

/// Every string of `value` masked, object keys included; every other value
/// as it was.
fn mask_value(mask: &Mask, value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(mask.text(text)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| mask_value(mask, item)).collect())
        }
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, field)| (mask.text(key), mask_value(mask, field)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The hook's answer to the event `input` (read, or why not), with the
/// values file at `path`: `None` to print nothing.
fn hook_reply(path: &Path, input: std::io::Result<Vec<u8>>) -> Option<String> {
    let values = match read_values(path) {
        Ok(values) => values,
        Err(error) => {
            eprintln!("spawn mask-hook: {error}");
            return Some(withheld("its values file cannot be used"));
        }
    };
    let Ok(input) = input else {
        return Some(withheld("its event could not be read"));
    };
    let Ok(event) = serde_json::from_slice::<Value>(&input) else {
        return Some(withheld("its event is not JSON"));
    };
    let response = event.get("tool_response")?;
    let masked = mask_value(&Mask::new(&values, MARKER), response);
    (masked != *response).then(|| hook_answer(&masked))
}

/// `spawn mask-hook --values <file>`: always exits 0 once its arguments are
/// right; what it prints is the hook's answer.
pub(crate) fn hook_command(arguments: &[String]) -> Result<i32, String> {
    let path = values_flag(arguments, "usage: spawn mask-hook --values <file>")?;
    let mut input = Vec::new();
    let read = std::io::stdin()
        .lock()
        .read_to_end(&mut input)
        .map(|_| input);
    if let Some(answer) = hook_reply(Path::new(&path), read) {
        println!("{answer}");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::{hook_reply, read_values};
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};

    /// The values file in a scratch directory, removed with it.
    struct File(PathBuf);

    impl File {
        fn new(text: &str, mode: u32) -> Self {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = crate::storage::tests::scratch("mask");
            let path = dir.join("values");
            std::fs::write(&path, text).expect("write");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
            Self(path)
        }
    }

    impl Drop for File {
        fn drop(&mut self) {
            if let Some(dir) = self.0.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    #[test]
    fn the_values_file_fails_closed_naming_the_file_and_line() {
        let ok = File::new(
            "# made up\n\nBB_ONE=made-up-value-1\nBB_ONE=made-up-value-2\n",
            0o600,
        );
        assert_eq!(read_values(&ok.0).expect("read").len(), 2);
        for (text, mode, says) in [
            ("BB_ONE=made-up-value-1\n", 0o644, "group or world access"),
            (
                "BB_ONE=made-up-value-1\nnot a line\n",
                0o600,
                "line 2: not NAME=value",
            ),
            ("lower=made-up-value-1\n", 0o600, "line 1: not NAME=value"),
            (
                "BB_SHORT=seven77\n",
                0o600,
                "line 1: a value shorter than 8",
            ),
            ("# nothing\n", 0o600, "names no value"),
        ] {
            let file = File::new(text, mode);
            let error = read_values(&file.0).expect_err(says);
            assert!(
                error.contains(&file.0.display().to_string()) && error.contains(says),
                "{error}"
            );
            assert!(
                !error.contains("made-up-value") && !error.contains("seven77"),
                "{error}"
            );
        }
        let missing = read_values(Path::new("/nonexistent/values")).expect_err("missing");
        assert!(missing.contains("/nonexistent/values"), "{missing}");
    }

    fn updated(answer: &str) -> Value {
        serde_json::from_str::<Value>(answer).expect("JSON")["hookSpecificOutput"]["updatedToolOutput"].clone()
    }

    #[test]
    fn the_hook_replaces_only_a_response_that_holds_a_value() {
        let file = File::new("BB_ONE=made-up:abc/one+xyz\n", 0o600);
        let event = |stdout: &str| {
            json!({"hook_event_name": "PostToolUse", "tool_name": "Bash",
                   "tool_input": {"command": "cat config"},
                   "tool_response": {"stdout": stdout, "stderr": "", "interrupted": false}})
            .to_string()
            .into_bytes()
        };
        let answer =
            hook_reply(&file.0, Ok(event("url=made-up:abc/one+xyz ok"))).expect("an answer");
        assert_eq!(
            updated(&answer),
            json!({"stdout": "url=[SECRET:BB_ONE] ok", "stderr": "", "interrupted": false})
        );
        assert_eq!(
            serde_json::from_str::<Value>(&answer).expect("JSON")["hookSpecificOutput"]["hookEventName"],
            "PostToolUse"
        );
        assert_eq!(hook_reply(&file.0, Ok(event("nothing to see"))), None);
    }

    #[test]
    fn the_hook_withholds_on_a_values_error_and_an_event_it_cannot_read() {
        let good = File::new("BB_ONE=made-up:abc/one+xyz\n", 0o600);
        let open = File::new("BB_ONE=made-up:abc/one+xyz\n", 0o644);
        let event = json!({"tool_response": {"stdout": "made-up:abc/one+xyz"}}).to_string();
        let cut = &event[..event.find("made-up").expect("the value") + 9];
        let replies = [
            hook_reply(&open.0, Ok(event.clone().into_bytes())),
            hook_reply(&good.0, Ok(b"cat config\nmade-up:abc/one+xyz\n".to_vec())),
            hook_reply(&good.0, Ok(cut.as_bytes().to_vec())),
            hook_reply(&good.0, Ok(Vec::new())),
            hook_reply(&good.0, Err(std::io::Error::other("a directory"))),
        ];
        for reply in replies {
            let reply = reply.expect("withheld");
            let said = updated(&reply);
            assert!(
                said.as_str().is_some_and(|text| text.contains("withheld")),
                "{reply}"
            );
            assert!(
                !reply.contains("made-up") && !reply.contains("cat config"),
                "{reply}"
            );
        }
    }

    #[test]
    fn the_hook_masks_an_overlap_whole() {
        let file = File::new(
            "BB_A=made-up-AAAA-0123456789abcdefghij\nBB_C=PREFIX-made-up-AAAA\n",
            0o600,
        );
        let event = json!({"hook_event_name": "PostToolUse", "tool_name": "Bash",
                           "tool_response": {"stdout": "PREFIX-made-up-AAAA-0123456789abcdefghij",
                                             "stderr": "", "interrupted": false}})
        .to_string();
        let answer = hook_reply(&file.0, Ok(event.into_bytes())).expect("an answer");
        assert_eq!(
            updated(&answer),
            json!({"stdout": "[SECRET:BB_C]", "stderr": "", "interrupted": false})
        );
        assert!(!answer.contains("0123456789"), "{answer}");
    }

    #[test]
    fn a_directory_is_not_a_file() {
        let dir = crate::storage::tests::scratch("mask-dir");
        let values = dir.join("values");
        std::fs::create_dir(&values).expect("mkdir");
        let error = read_values(&values).expect_err("a directory");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(error, format!("{}: not a file", values.display()));
    }
}
