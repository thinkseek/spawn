#!/usr/bin/env python3
"""Tests for scripts/move-check.py: a pure move passes, and each way of
changing code while moving it fails.

    scripts/move-check-test.py

Each case builds a commit in a scratch git repository and runs the check
on it. The repository, and HOME for git, live in one scratch directory
under $TMPDIR, made here and removed at the end; nothing outside it is
written or removed (cc-rules (a): the run refuses a HOME that is not its
own scratch). Needs git, rustfmt and python3.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile

CHECK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "move-check.py")
SOURCE = "src/big.rs"

BASE = '''\
//! A big file.

use std::fmt;

/// Joins two names.
fn join(first: &str, second: &str) -> String {
    let joined = format!("{first} and {second}");
    let width = joined.len();
    assert!(width > 0, "never empty");
    joined
}

/// Says hello.
pub fn hello(name: &str) -> String {
    join("hello", name)
}

struct Shown(u32);

impl fmt::Display for Shown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The tests.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_joins() {
        let count = || {
            hello("a").len()
        };
        assert_eq!(count(), 11);
        let text = "two
    lines";
        // A comment kept as it was.
        assert!(text.contains("lines"));
    }
}
'''

MOD_STUB = '''\
/// The tests.
#[cfg(test)]
mod tests;
'''

TESTS_FILE = '''\
use super::*;

#[test]
fn hello_joins() {
    let count = || hello("a").len();
    assert_eq!(count(), 11);
    let text = "two
    lines";
    // A comment kept as it was.
    assert!(text.contains("lines"));
}
'''

JOIN = '''\
/// Joins two names.
fn join(first: &str, second: &str) -> String {
    let joined = format!("{first} and {second}");
    let width = joined.len();
    assert!(width > 0, "never empty");
    joined
}
'''

JOIN_FILE = '''\
//! Joining names.

/// Joins two names.
pub(super) fn join(first: &str, second: &str) -> String {
    let joined = format!("{first} and {second}");
    let width = joined.len();
    assert!(width > 0, "never empty");
    joined
}
'''


SHOWN = '''\
struct Shown(u32);

impl fmt::Display for Shown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
'''

SHOWN_FILE = '''\
use std::fmt;

pub(super) struct Shown(u32);

impl fmt::Display for Shown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
'''

SHOWN_MOVED = BASE.replace(SHOWN + "\n", "").replace("use std::fmt;\n", "use std::fmt;\n\nmod shown;\n")

SLIDE_MIDDLE = '''\
fn middle() -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    write!(text, "{}", 2).unwrap();
    text
}

'''

SLIDE_BASE = '''\
fn first() -> u32 {
    1
}

''' + SLIDE_MIDDLE + '''\
fn last() -> u32 {
    3
}
'''

SLIDE_MIDDLE_FILE = '''\
pub(super) fn middle() -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    write!(text, "{}", 2).unwrap();
    text
}
'''


def moved_tests(source: str) -> str:
    start = source.index("/// The tests.")
    return source[:start] + MOD_STUB


def moved_join(source: str, use_line: str = "use join::join;\n") -> str:
    source = source.replace(JOIN + "\n", "")
    return source.replace("use std::fmt;\n", f"use std::fmt;\n\nmod join;\n{use_line}")


def tree(files: dict[str, str]) -> dict[str, str]:
    return files


CASES: list[tuple[str, int, dict[str, str], str, str]] = []


def case(name: str, expect: int, files: dict[str, str], says: str = "", base: str = BASE) -> None:
    CASES.append((name, expect, files, says, base))


# Passes.
case("a module's body moved to its own file, as rustfmt lays it out", 0,
     {SOURCE: moved_tests(BASE), "src/big/tests.rs": TESTS_FILE})
case("an item moved to a new file with pub(super) and a use", 0,
     {SOURCE: moved_join(BASE), "src/big/join.rs": JOIN_FILE})
case("both at once", 0,
     {SOURCE: moved_join(moved_tests(BASE)), "src/big/tests.rs": TESTS_FILE, "src/big/join.rs": JOIN_FILE})
case("a glob re-export passes but is flagged", 0,
     {SOURCE: moved_join(BASE, "use join::*;\n"), "src/big/join.rs": JOIN_FILE}, "glob import added")

case("a re-export rustfmt wraps over lines, one under #[cfg(test)]", 0,
     {SOURCE: moved_join(BASE, "#[cfg(test)]\nuse join::{\n    join,\n};\n"), "src/big/join.rs": JOIN_FILE})
case("an import the source no longer needs, removed and flagged", 0,
     {SOURCE: moved_join(SHOWN_MOVED, "use join::join;\n").replace("use std::fmt;\n", ""),
      "src/big/join.rs": JOIN_FILE, "src/big/shown.rs": SHOWN_FILE}, "import removed: use std")
case("a run diffed as the previous item's } plus the item", 0,
     {SOURCE: SLIDE_BASE.replace(SLIDE_MIDDLE, ""), "src/big/middle.rs": SLIDE_MIDDLE_FILE}, "", SLIDE_BASE)

case("an #[allow] on an added use passes, flagged", 0,
     {SOURCE: moved_join(BASE, "#[allow(warnings)]\nuse join::join;\n"), "src/big/join.rs": JOIN_FILE},
     "attribute other than #[cfg(test)] added: #[allow(warnings)]")
case("a #[cfg] on a new file's use passes, flagged", 0,
     {SOURCE: moved_join(BASE), "src/big/join.rs": JOIN_FILE.replace("//! Joining names.\n", "//! Joining names.\n\n#[cfg(any())]\nuse std::fmt;\n")},
     "attribute other than #[cfg(test)] added: #[cfg(any())]")

# Fails: what M1 asks to be shown.
case("two moved lines swapped", 1,
     {SOURCE: moved_join(BASE), "src/big/join.rs": JOIN_FILE.replace(
         '    let joined = format!("{first} and {second}");\n    let width = joined.len();\n',
         '    let width = joined.len();\n    let joined = format!("{first} and {second}");\n')})
case("one word changed on a widened line", 1,
     {SOURCE: moved_join(BASE), "src/big/join.rs": JOIN_FILE.replace("second: &str)", "other: &str)")})

# Fails: other changes.
case("two lines swapped in a moved module body", 1,
     {SOURCE: moved_tests(BASE), "src/big/tests.rs": TESTS_FILE.replace(
         "    let count = || hello(\"a\").len();\n    assert_eq!(count(), 11);\n",
         "    assert_eq!(count(), 11);\n    let count = || hello(\"a\").len();\n")}, "")
case("a string literal's inner indentation changed", 1,
     {SOURCE: moved_tests(BASE), "src/big/tests.rs": TESTS_FILE.replace("\n    lines\"", "\nlines\"")})
case("a comment changed", 1,
     {SOURCE: moved_tests(BASE), "src/big/tests.rs": TESTS_FILE.replace("kept as it was", "kept as is")})
case("a line added to a new file", 1,
     {SOURCE: moved_join(BASE), "src/big/join.rs": JOIN_FILE + "\nconst EXTRA: u32 = 1;\n"}, "not moved from")
case("a removed line found nowhere", 1,
     {SOURCE: BASE.replace('    join("hello", name)\n', '    join("hi", name)\n')})
case("a line added to the source that is not mod/use", 1,
     {SOURCE: moved_join(BASE).replace("use join::join;\n", "use join::join;\nconst EXTRA: u32 = 1;\n"),
      "src/big/join.rs": JOIN_FILE}, "added what is not")
case("an attribute added above no mod/use", 1,
     {SOURCE: moved_join(BASE).replace("struct Shown", "#[derive(Debug)]\nstruct Shown"),
      "src/big/join.rs": JOIN_FILE}, "")
case("another file changed", 1,
     {SOURCE: moved_join(BASE), "src/big/join.rs": JOIN_FILE, "src/other.rs": "fn other() {}\n// changed\n"},
     "only")
case("a moved item kept a copy in the source", 1,
     {SOURCE: BASE.replace("use std::fmt;\n", "use std::fmt;\n\nmod join;\n"), "src/big/join.rs": JOIN_FILE},
     "not moved from")
case("an item moved twice", 1,
     {SOURCE: moved_join(BASE), "src/big/join.rs": JOIN_FILE,
      "src/big/again.rs": JOIN_FILE.replace("//! Joining names.\n\n", "")}, "")
case("a visibility other than pub(super)/pub(crate)", 1,
     {SOURCE: moved_join(BASE), "src/big/join.rs": JOIN_FILE.replace("pub(super) fn", "pub fn")})
case("an inline module renamed", 1,
     {SOURCE: BASE.replace("/// The tests.\n#[cfg(test)]\nmod tests {", "/// The tests.\n#[cfg(test)]\nmod testz {")
      .replace("mod testz {", "mod testz {", 1)}, "")
case("a use inside a moved fn's body dropped", 1,
     {SOURCE: SLIDE_BASE.replace(SLIDE_MIDDLE, ""),
      "src/big/middle.rs": SLIDE_MIDDLE_FILE.replace("    use std::fmt::Write as _;\n", "")}, "", SLIDE_BASE)
case("the module stub left out", 1,
     {SOURCE: moved_tests(BASE).replace(MOD_STUB, ""), "src/big/tests.rs": TESTS_FILE})


def run(argv: list[str], cwd: str, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True, check=False)


def main() -> int:
    parent = os.environ.get("TMPDIR") or tempfile.gettempdir()
    scratch = tempfile.mkdtemp(prefix="move-check-test-", dir=parent)
    home = os.path.join(scratch, "home")
    os.mkdir(home)
    real_home = os.path.expanduser("~")
    if os.path.realpath(home) == os.path.realpath(real_home) or not home.startswith(scratch + os.sep):
        print(f"move-check-test: refusing: HOME {home} is not this run's scratch", file=sys.stderr)
        return 2
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    # rustup's rustfmt finds its toolchain from these, which default to HOME.
    env.setdefault("RUSTUP_HOME", os.path.join(real_home, ".rustup"))
    env.setdefault("CARGO_HOME", os.path.join(real_home, ".cargo"))
    env.update(HOME=home, TMPDIR=scratch, GIT_CONFIG_NOSYSTEM="1",
               GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t", GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@t")
    repo = os.path.join(scratch, "repo")
    failed = 0
    try:
        os.mkdir(repo)
        run(["git", "init", "-q", "-b", "main"], repo, env)
        os.makedirs(os.path.join(repo, "src"))
        for path, text in {SOURCE: BASE, "src/other.rs": "fn other() {}\n",
                           "rustfmt.toml": 'edition = "2024"\n'}.items():
            with open(os.path.join(repo, path), "w") as out:
                out.write(text)
        run(["git", "add", "-A"], repo, env)
        run(["git", "commit", "-q", "-m", "base"], repo, env)
        for name, expect, files, says, base in CASES:
            run(["git", "checkout", "-q", "--detach", "main"], repo, env)
            run(["git", "clean", "-qfd"], repo, env)
            if base != BASE:
                with open(os.path.join(repo, SOURCE), "w") as out:
                    out.write(base)
                run(["git", "commit", "-qam", "another base"], repo, env)
            start = run(["git", "rev-parse", "HEAD"], repo, env).stdout.strip()
            for path, text in files.items():
                os.makedirs(os.path.dirname(os.path.join(repo, path)), exist_ok=True)
                with open(os.path.join(repo, path), "w") as out:
                    out.write(text)
            run(["git", "add", "-A"], repo, env)
            run(["git", "commit", "-q", "-m", name], repo, env)
            done = run([sys.executable, "-I", CHECK, "-C", repo, start, "HEAD", SOURCE], repo, env)
            output = done.stdout + done.stderr
            ok = done.returncode == expect and says in output
            failed += not ok
            print(f"{'ok' if ok else 'FAILED'}: {name} (exit {done.returncode}, expected {expect})")
            if not ok:
                print("    " + output.strip().replace("\n", "\n    "))
    finally:
        if scratch.startswith(os.path.realpath(parent) + os.sep) or scratch.startswith(parent + os.sep):
            shutil.rmtree(scratch)
    print(f"move-check-test: {len(CASES) - failed} of {len(CASES)} cases pass")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
