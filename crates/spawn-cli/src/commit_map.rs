//! The commit map of a history rewrite (objective 173): `spawn commit-map
//! load <file> --source <git dir>` loads git filter-repo's commit-map once,
//! owner only, through `spawn_commit_map_load`. The event log keeps the old
//! ids; every reader that compares a recorded id with git reads it through
//! the map (`spawn_commit_now`), and the integrator refuses an owner's
//! request that would bring a commit from before the rewrite onto main
//! (`integrate/owner.rs`, `pre_rewrite`).
//!
//! filter-repo's file (`.git/filter-repo/commit-map`) is a header line
//! (`old new`, optional here) and then one `old new` pair per line, each 40
//! hex characters; a new of forty zeros is a commit the rewrite dropped.
//! Every new commit must be a commit of the source before anything is
//! loaded: a map of another repository is refused whole.

use std::io::Write as _;
use std::process::Stdio;

use crate::Flags;

/// The usage of `spawn commit-map`, the only copy (main's help reads it).
pub(crate) const USAGE: &str = "usage: spawn commit-map load <file> --source <git dir>";

/// The new commit filter-repo writes for a commit the rewrite dropped.
const DROPPED: &str = "0000000000000000000000000000000000000000";

/// One pair of the map: the old commit and the new one (`None`: dropped).
pub(crate) type Pair = (String, Option<String>);

/// `spawn commit-map load <file> --source <git dir>`.
pub(crate) fn command(rest: &[String]) -> Result<i32, String> {
    let (Some(verb), Some(file)) = (rest.first(), rest.get(1)) else {
        return Err(USAGE.to_string());
    };
    if verb != "load" || file.starts_with("--") {
        return Err(USAGE.to_string());
    }
    let flags = Flags::parse(&rest[2..])?;
    let source = flags.required("source")?;
    let text = std::fs::read_to_string(file)
        .map_err(|error| format!("read the commit map {file}: {error}"))?;
    let pairs = read(&text).map_err(|error| format!("the commit map {file}: {error}"))?;
    let missing = not_commits(&source, &pairs)?;
    if let Some(first) = missing.first() {
        return Err(format!(
            "{} new commit(s) of the map are not commits of {source} (the first: {first}): nothing \
             loaded",
            missing.len()
        ));
    }
    let store = crate::open_store()?;
    let (count, loaded) = store
        .commit_map_load(&pairs)
        .map_err(|error| error.to_string())?;
    if loaded {
        let dropped = pairs.iter().filter(|(_, new)| new.is_none()).count();
        println!("commit map: {count} pair(s) loaded ({dropped} dropped commit(s))");
    } else {
        println!("commit map: the same {count} pair(s) are loaded already; nothing changed");
    }
    Ok(0)
}

/// Whether `word` is a full commit id: 40 hex characters.
fn full(word: &str) -> bool {
    word.len() == 40 && word.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// filter-repo's commit-map as pairs, lower case: the header (`old new`)
/// first when there is one, then one `old new` pair per line (blank lines
/// skipped), a new of forty zeros read as `None` (dropped). An error names
/// the line (from 1).
pub(crate) fn read(text: &str) -> Result<Vec<Pair>, String> {
    let mut pairs = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.is_empty() {
            continue;
        }
        if index == 0 && fields == ["old", "new"] {
            continue;
        }
        let [old, new] = fields[..] else {
            return Err(format!(
                "line {number} is not an `old new` pair: {:?}",
                line.chars().take(120).collect::<String>()
            ));
        };
        if !full(old) || !full(new) {
            return Err(format!(
                "line {number}: each commit is 40 hex characters: {:?}",
                line.chars().take(120).collect::<String>()
            ));
        }
        let new = new.to_ascii_lowercase();
        pairs.push((old.to_ascii_lowercase(), (new != DROPPED).then_some(new)));
    }
    if pairs.is_empty() {
        return Err("it holds no pair".to_string());
    }
    Ok(pairs)
}

/// The new commits of `pairs` that are not commits of `source`, in the
/// map's order, read with one `git cat-file --batch-check`.
fn not_commits(source: &str, pairs: &[Pair]) -> Result<Vec<String>, String> {
    let news: Vec<&str> = pairs.iter().filter_map(|(_, new)| new.as_deref()).collect();
    if news.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = crate::sources::controller_git()
        .args([
            "--git-dir",
            source,
            "cat-file",
            "--batch-check=%(objecttype)",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("run git cat-file: {error}"))?;
    let mut input = news.join("\n");
    input.push('\n');
    let mut stdin = child.stdin.take().ok_or("git cat-file has no stdin")?;
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child
        .wait_with_output()
        .map_err(|error| format!("run git cat-file: {error}"))?;
    let _ = writer.join();
    if !output.status.success() {
        return Err(format!(
            "git cat-file in {source}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let answers = String::from_utf8_lossy(&output.stdout);
    let answers: Vec<&str> = answers.lines().collect();
    if answers.len() != news.len() {
        return Err(format!(
            "git cat-file in {source} answered {} of {} commits",
            answers.len(),
            news.len()
        ));
    }
    Ok(news
        .iter()
        .zip(answers)
        .filter(|(_, answer)| *answer != "commit")
        .map(|(new, _)| (*new).to_string())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::read;

    #[test]
    fn a_filter_repo_commit_map_is_read_as_pairs_and_its_dropped_commits_as_none() {
        let a = "a".repeat(40);
        let b = "B".repeat(40);
        let c = "c".repeat(40);
        let zeros = "0".repeat(40);
        let text =
            format!("old                                      new\n{a} {b}\n\n{c} {zeros}\n");
        assert_eq!(
            read(&text),
            Ok(vec![(a.clone(), Some("b".repeat(40))), (c.clone(), None),])
        );
        // The header is optional.
        assert_eq!(
            read(&format!("{a} {a}\n")),
            Ok(vec![(a.clone(), Some(a.clone()))])
        );
        // An error names the line.
        let error = read(&format!("old new\n{a} {b}\n{a}\n")).unwrap_err();
        assert!(error.contains("line 3"), "{error}");
        let error = read(&format!("{a} xyz\n")).unwrap_err();
        assert!(error.contains("line 1"), "{error}");
        // A header anywhere but the first line is no pair.
        let error = read(&format!("{a} {b}\nold new\n")).unwrap_err();
        assert!(error.contains("line 2"), "{error}");
        assert!(read("old new\n").is_err(), "no pair");
    }
}
