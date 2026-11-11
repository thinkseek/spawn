#!/usr/bin/env python3
"""Prove that a commit only moves code out of one Rust file into new files.

    scripts/move-check.py [-C <repo>] [--allow <path>]... <base> <head> <source>

This is the check for splitting a large file by concern: rev-e's proposal
(sync/proposal-integrate-split-rev-e.md) and M1 in rev-g's review
(sync/review-integrate-split.md). A step that only moves code changes no
behaviour, and this script checks that claim from the two commits instead
of taking it on trust.

The comparison is by Rust tokens, not by lines. rustfmt rewraps code that
a move dedents (an inline module's body, now in its own file) or that a
visibility token lengthens, so a line-by-line check would fail every real
move. Whitespace outside literals means nothing to Rust. Every token that
does mean something has to match, in order: identifiers, punctuation,
numbers, string and char literals byte for byte (their inner indentation
included), and comments (with leading space dropped on each line of a
block comment).

One normalisation is applied, because rustfmt adds and drops these when it
rewraps: a trailing comma before `}`, before `]`, and before the `)` of a
call is ignored. A one-element tuple's comma, `(x,)`, is kept.

What passes:
- <source> loses lines, and gains only whole `mod` and `use` statements
  (rustfmt may wrap one over lines), each with the attributes and `///`
  docs directly above it, blank lines and `//!` module docs.
- <source> may lose a top-level `use` statement outright (the code that
  needed it moved; the build proves no name changed meaning). Each is
  flagged.
- Each contiguous run of lines removed from <source> (a run that starts
  with a lone `}` the next kept line repeats is read one line down: the
  same removal, as the moved code reads) appears as a
  contiguous run of tokens in exactly one file that <head> adds. One
  difference is allowed there: a visibility (`pub(super)` or `pub(crate)`)
  inserted before an item keyword that had none.
- An inline module, `mod <name> { ... }`, may become `mod <name>;` in
  <source>, with its body in <name>.rs in <source>'s module directory.
  The body is compared as rustfmt lays it out as a file of its own (with
  <base>'s rustfmt.toml), because a dedent lets rustfmt do more than
  rewrap: it drops a closure's braces when the closure's body now fits on
  one line, for one. So rustfmt is trusted to keep meaning; nothing else
  is.
- Every token of an added file is covered by exactly one moved run, or
  belongs to what <source> may gain (as above): `use` and `mod` statements,
  their attributes, and `//!` docs.
- No other file changes, except paths given with --allow.

Flagged for a look, without failing:
- `super::` or `self::` in a run moved out of an item list and one module
  down, where its meaning shifts. A module body moved into its own file
  stays at the same depth, so it is not flagged.
- A glob `use ...::*;` added to <source>. Prefer explicit re-export lists:
  a later local item silently shadows a glob-imported namesake.

Exit status: 0 the commit is a pure move; 1 it is not, with what differs
said; 2 a usage or git error.
"""

from __future__ import annotations

import argparse
import difflib
import os
import re
import subprocess
import sys
import tempfile

ITEM_KEYWORDS = {
    "fn", "struct", "enum", "union", "const", "static", "type", "trait", "mod", "use",
    "async", "unsafe", "extern", "impl", "macro_rules",
}
VISIBILITY_SCOPES = {"super", "crate"}
INLINE_MOD = re.compile(r"^(\s*)(?:pub(?:\((?:crate|super)\))? )?mod ([A-Za-z_][A-Za-z0-9_]*) \{\s*$")
IDENT = re.compile(r"r#[A-Za-z_][A-Za-z0-9_]*|[A-Za-z_][A-Za-z0-9_]*")
NUMBER = re.compile(r"[0-9][0-9_]*(?:\.[0-9][0-9_]*)?(?:[eE][+-]?[0-9_]+)?[A-Za-z0-9_]*")
CHAR = re.compile(r"b?'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]+\}|.)|[^\\'\n])'")
RAW = re.compile(r'b?r(#*)"')


class Token:
    __slots__ = ("text", "line")

    def __init__(self, text: str, line: int) -> None:
        self.text = text
        self.line = line

    def __repr__(self) -> str:
        return f"{self.text!r}@{self.line + 1}"


def tokenize(text: str) -> list[Token]:
    """Rust tokens, with the 0-based line each starts on."""
    tokens: list[Token] = []
    i, line, n = 0, 0, len(text)
    while i < n:
        c = text[i]
        if c == "\n":
            line += 1
            i += 1
        elif c in " \t\r":
            i += 1
        elif text.startswith("//", i):
            end = text.find("\n", i)
            end = n if end < 0 else end
            tokens.append(Token(text[i:end].rstrip(), line))
            i = end
        elif text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            body = "\n".join(part.strip() for part in text[i:j].split("\n"))
            tokens.append(Token(body, line))
            line += text.count("\n", i, j)
            i = j
        elif (m := RAW.match(text, i)) and not (i and (text[i - 1].isalnum() or text[i - 1] == "_")):
            close = '"' + m.group(1)
            j = text.find(close, m.end())
            j = n if j < 0 else j + len(close)
            tokens.append(Token(text[i:j], line))
            line += text.count("\n", i, j)
            i = j
        elif c == '"' or (c == "b" and text.startswith('b"', i)):
            j = i + (2 if c == "b" else 1)
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            j = min(j + 1, n)
            tokens.append(Token(text[i:j], line))
            line += text.count("\n", i, j)
            i = j
        elif (c == "'" or text.startswith("b'", i)) and (m := CHAR.match(text, i)):
            tokens.append(Token(m.group(0), line))
            i = m.end()
        elif c == "'" and (m := IDENT.match(text, i + 1)):
            tokens.append(Token("'" + m.group(0), line))
            i = m.end()
        elif (m := IDENT.match(text, i)):
            tokens.append(Token(m.group(0), line))
            i = m.end()
        elif c.isdigit() and (m := NUMBER.match(text, i)):
            tokens.append(Token(m.group(0), line))
            i = m.end()
        else:
            tokens.append(Token(c, line))
            i += 1
    return tokens


def normalize(tokens: list[Token]) -> list[Token]:
    """Drop the trailing commas rustfmt adds or removes (see the header)."""
    opener_before: dict[int, str] = {}
    stack: list[int] = []
    for k, token in enumerate(tokens):
        if token.text in "([{" and len(token.text) == 1:
            stack.append(k)
            opener_before[k] = tokens[k - 1].text if k else ""
        elif token.text in ")]}" and len(token.text) == 1 and stack:
            opener_before[k] = opener_before[stack.pop()]
    out: list[Token] = []
    for k, token in enumerate(tokens):
        if token.text == "," and k + 1 < len(tokens):
            nxt = tokens[k + 1].text
            if nxt in ("}", "]"):
                continue
            if nxt == ")":
                before = opener_before.get(k + 1, "")
                if IDENT.fullmatch(before) or before in (")", "]", ">", "!"):
                    continue
        out.append(token)
    return out


def git(repo: str, *args: str) -> str:
    done = subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True, check=False)
    if done.returncode != 0:
        print(f"move-check: git {' '.join(args)}: {done.stderr.strip()}", file=sys.stderr)
        sys.exit(2)
    return done.stdout


def visibility_at(tokens: list[Token], k: int) -> int:
    """The length of a `pub(super)`/`pub(crate)` starting at k, or 0."""
    if (
        k + 3 < len(tokens)
        and tokens[k].text == "pub"
        and tokens[k + 1].text == "("
        and tokens[k + 2].text in VISIBILITY_SCOPES
        and tokens[k + 3].text == ")"
    ):
        return 4
    return 0


def match_at(run: list[Token], file: list[Token], start: int) -> int | None:
    """Where `run` ends in `file` when it starts at `start`, allowing a
    visibility inserted before an item keyword; None when it does not."""
    i, j = 0, start
    while i < len(run):
        if j < len(file) and file[j].text == run[i].text:
            i, j = i + 1, j + 1
            continue
        width = visibility_at(file, j)
        prior = run[i - 1].text if i else ""
        if width and run[i].text in ITEM_KEYWORDS and prior not in ("pub", ")"):
            if j + width < len(file) and file[j + width].text == run[i].text:
                j += width
                continue
        return None
    return j


def statement_end(tokens: list[Token], k: int) -> int | None:
    """The end of a `use ...;` / `mod x;` (with a visibility) at k, or None."""
    k += visibility_at(tokens, k) or (1 if k < len(tokens) and tokens[k].text == "pub" else 0)
    if k >= len(tokens) or tokens[k].text not in ("use", "mod"):
        return None
    depth = 0
    for j in range(k + 1, len(tokens)):
        text = tokens[j].text
        if text in ("{", "(", "["):
            depth += 1
        elif text in ("}", ")", "]"):
            depth -= 1
            if depth < 0:
                return None
        elif text == ";" and depth == 0:
            return j + 1
        elif text == "{" and tokens[k].text == "mod":
            return None
    return None


def attribute_end(tokens: list[Token], k: int) -> int | None:
    """The end of an attribute `#[...]` / `#![...]` at k, or None."""
    j = k + 1
    if j < len(tokens) and tokens[j].text == "!":
        j += 1
    if not (tokens[k].text == "#" and j < len(tokens) and tokens[j].text == "["):
        return None
    depth = 0
    for m in range(j, len(tokens)):
        if tokens[m].text == "[":
            depth += 1
        elif tokens[m].text == "]":
            depth -= 1
            if depth == 0:
                return m + 1
    return None


def rustfmt(repo: str, base: str, text: str) -> str | None:
    """`text` as rustfmt formats it as a file of its own, with <base>'s
    rustfmt.toml; None when rustfmt refuses it."""
    with tempfile.TemporaryDirectory(prefix="move-check-") as scratch:
        config = subprocess.run(
            ["git", "-C", repo, "show", f"{base}:rustfmt.toml"], capture_output=True, text=True, check=False
        ).stdout
        with open(os.path.join(scratch, "rustfmt.toml"), "w") as out:
            out.write(config)
        done = subprocess.run(
            ["rustfmt", "--emit", "stdout", "--config-path", scratch],
            input=text, capture_output=True, text=True, check=False,
        )
    if done.returncode != 0:
        print(f"move-check: rustfmt: {done.stderr.strip()[:400]}", file=sys.stderr)
        return None
    return done.stdout


def statements_only(tokens: list[Token], covered: list[bool] | None = None) -> int | None:
    """The first token that is not part of a `use`/`mod` statement (with
    the attributes and `///` docs directly above it) or a `//!` doc,
    skipping covered tokens; None when there is none."""
    covered = covered or [False] * len(tokens)
    k = 0
    while k < len(tokens):
        if covered[k] or tokens[k].text.startswith("//!"):
            k += 1
            continue
        j = k
        while j < len(tokens) and not covered[j]:
            if tokens[j].text.startswith("///"):
                j += 1
            elif (end := attribute_end(tokens, j)) is not None:
                j = end
            else:
                break
        end = statement_end(tokens, j) if j < len(tokens) and not covered[j] else None
        if end is None or any(covered[j:end]):
            return k
        k = end
    return None


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("-C", dest="repo", default=".")
    parser.add_argument("--allow", action="append", default=[])
    parser.add_argument("base")
    parser.add_argument("head")
    parser.add_argument("source")
    args = parser.parse_args()
    repo, source = args.repo, args.source
    failures: list[str] = []
    flags: list[str] = []

    added: list[str] = []
    for row in git(repo, "diff", "--no-renames", "--name-status", args.base, args.head).splitlines():
        kind, path = row.split("\t", 1)
        if path == source and kind == "M":
            continue
        if kind == "A" and path.endswith(".rs"):
            added.append(path)
        elif path not in args.allow:
            failures.append(f"{path}: changed ({kind}); only {source} and new .rs files may")

    base_text = git(repo, "show", f"{args.base}:{source}")
    base = base_text.split("\n")
    head = git(repo, "show", f"{args.head}:{source}").split("\n")
    base_tokens = normalize(tokenize(base_text))
    news = {path: normalize(tokenize(git(repo, "show", f"{args.head}:{path}"))) for path in added}
    covered = {path: [False] * len(tokens) for path, tokens in news.items()}
    stem = source[: -len(".rs")]
    module_dir = os.path.dirname(source) if os.path.basename(source) in ("mod.rs", "main.rs", "lib.rs") else stem

    # What <source> gained: whole use/mod statements (rustfmt may wrap a
    # long one over several lines), each with its attributes and docs, and
    # //! docs.
    matcher = difflib.SequenceMatcher(a=base, b=head, autojunk=False)
    opcodes = matcher.get_opcodes()
    for tag, _a0, _a1, b0, b1 in opcodes:
        if tag not in ("insert", "replace"):
            continue
        gained = normalize(tokenize("\n".join(head[b0:b1])))
        bad = statements_only(gained)
        if bad is not None:
            failures.append(
                f"{source}:{b0 + gained[bad].line + 1}: added what is not a mod/use statement, its attributes"
                f" or docs, or a module doc: {head[b0 + gained[bad].line].strip()[:100]}"
            )
        for k in range(len(gained) - 1):
            if gained[k].text == ":" and gained[k + 1].text == "*" and k and gained[k - 1].text == ":":
                flags.append(f"{source}:{b0 + gained[k].line + 1}: glob import added (prefer an explicit list)")

    def tokens_in(first: int, last: int) -> list[int]:
        """Indexes of base tokens that start on lines first..last-1."""
        return [k for k, token in enumerate(base_tokens) if first <= token.line < last]

    def place(indexes: list[int], files: list[str], what: str, run: list[Token] | None = None) -> str | None:
        run = [base_tokens[k] for k in indexes] if run is None else run
        if not run:
            return ""
        found = []
        for path in files:
            tokens = news[path]
            for start in range(len(tokens)):
                if tokens[start].text != run[0].text and not visibility_at(tokens, start):
                    continue
                if start >= 4 and visibility_at(tokens, start - 4) and run[0].text in ITEM_KEYWORDS:
                    continue  # the same placement, found from its inserted visibility
                end = match_at(run, tokens, start)
                if end is not None and not any(covered[path][start:end]):
                    found.append((path, start, end))
        if not found:
            failures.append(f"{what}: appears in no added file as it was (first token {run[0]!r})")
            return None
        trivial = all(token.text in "{}()[];," for token in run)
        if len(found) > 1 and not trivial:
            places = ", ".join(f"{p} at line {news[p][s].line + 1}" for p, s, _ in found[:4])
            failures.append(f"{what}: appears in more than one place: {places}")
            return None
        path, start, end = found[0]
        for k in range(start, end):
            covered[path][k] = True
        return path

    # Runs of removed lines. A blank line difflib kept between two removed
    # stretches (it matched a blank line elsewhere) joins them.
    removed = sorted(d for tag, a0, a1, _b0, _b1 in opcodes if tag in ("delete", "replace") for d in range(a0, a1))
    runs: list[tuple[int, int]] = []
    for d in removed:
        if runs and all(not base[g].strip() for g in range(runs[-1][1], d)):
            runs[-1] = (runs[-1][0], d + 1)
        else:
            runs.append((d, d + 1))

    # Slide a run down past a closing line it shares with what follows
    # (`}` then the next item, against the same item then its `}`): the
    # same removal, said the way the moved code reads.
    removed_set = set(removed)
    for r, (a0, a1) in enumerate(runs):
        while a1 < len(base) and a1 not in removed_set and base[a0] == base[a1] and base[a0].strip() in ("}", "};", ""):
            a0, a1 = a0 + 1, a1 + 1
        runs[r] = (a0, a1)

    # A top-level use statement <source> no longer needs may go: the code
    # that needed it moved. The build says no name changed meaning.
    depth_of: list[int] = []
    depth = 0
    for token in base_tokens:
        if token.text in ("}", ")", "]"):
            depth -= 1
        depth_of.append(depth)
        if token.text in ("{", "(", "["):
            depth += 1

    def without_imports(indexes: list[int]) -> list[list[int]]:
        """`indexes` split around the top-level use statements in it."""
        parts: list[list[int]] = [[]]
        inside = set(indexes)
        k = 0
        while k < len(indexes):
            at = indexes[k]
            end = statement_end(base_tokens, at) if depth_of[at] == 0 else None
            keyword = at + (visibility_at(base_tokens, at) or int(base_tokens[at].text == "pub"))
            if end is not None and base_tokens[keyword].text == "use" and all(i in inside for i in range(at, end)):
                text = " ".join(t.text for t in base_tokens[at:end])
                flags.append(f"{source}:{base_tokens[at].line + 1}: import removed: {text[:100]}")
                parts.append([])
                k += end - at
                continue
            parts[-1].append(at)
            k += 1
        return [part for part in parts if part]

    for a0, a1 in runs:
        what = f"{source}:{a0 + 1}-{a1}"
        indexes = tokens_in(a0, a1)
        opener = next((d for d in range(a0, a1) if INLINE_MOD.match(base[d])), None)
        if opener is not None:
            m = INLINE_MOD.match(base[opener])
            name = m.group(2)
            target = f"{module_dir}/{name}.rs"
            stub = re.compile(rf"^{re.escape(m.group(1))}(?:pub(?:\((?:crate|super)\))? )?mod {name};\s*$")
            if target in news and any(stub.match(line) for line in head):
                # The tokens from `mod` to its `{`, and the matching `}`.
                brace = next(k for k in indexes if base_tokens[k].line == opener and base_tokens[k].text == "{")
                depth, close = 0, None
                for k in range(brace, len(base_tokens)):
                    if base_tokens[k].text == "{":
                        depth += 1
                    elif base_tokens[k].text == "}":
                        depth -= 1
                        if depth == 0:
                            close = k
                            break
                if close is None or close not in indexes:
                    failures.append(f"{what}: mod {name}'s closing brace is not among the removed lines")
                    continue
                before = [k for k in indexes if k < brace and base_tokens[k].line < opener]
                after = [k for k in indexes if k > close]
                close_line = base_tokens[close].line
                if base[close_line].strip() != "}" or any(base_tokens[k].line == opener for k in range(brace + 1, close)):
                    failures.append(f"{what}: mod {name} must open on a line of its own and close with a lone `}}`")
                    continue
                body_text = "\n".join(base[opener + 1 : close_line]) + "\n"
                formatted = rustfmt(repo, args.base, body_text)
                if formatted is None:
                    failures.append(f"{what}: rustfmt could not format mod {name}'s body on its own")
                    continue
                place([], [target], f"{what} (mod {name}'s body, as rustfmt lays it out at top level)",
                      normalize(tokenize(formatted)))
                for part, label in ((before, "before"), (after, "after")):
                    for piece in without_imports(part):
                        place(piece, [p for p in news if p != target], f"{what} ({label} mod {name})")
                continue
        wheres = [place(part, list(news), what) for part in without_imports(indexes)]
        where = next((w for w in wheres if w), None)
        if where:
            for d in range(a0, a1):
                if re.search(r"\b(?:super|self)::", base[d]) and not base[d].lstrip().startswith("//"):
                    flags.append(f"{source}:{d + 1}: moved one module down into {where}: {base[d].strip()[:100]}")

    # What else an added file may hold: use/mod statements, their
    # attributes and docs, and //! docs.
    for path, tokens in news.items():
        k = 0
        while (bad := statements_only(tokens[k:], covered[path][k:])) is not None:
            k += bad
            failures.append(f"{path}:{tokens[k].line + 1}: not moved from {source}: {tokens[k].text[:80]!r}")
            while k < len(tokens) and not covered[path][k]:
                k += 1

    for flag in flags:
        print(f"move-check: look: {flag}")
    if failures:
        for failure in failures:
            print(f"move-check: FAIL {failure}")
        return 1
    moved = sum(sum(c) for c in covered.values())
    print(f"move-check: pass: {moved} tokens moved from {source} into {len(news)} new file(s): {' '.join(sorted(news))}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
