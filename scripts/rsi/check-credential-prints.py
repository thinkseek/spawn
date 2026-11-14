#!/usr/bin/env python3
"""check-credential-prints: no script prints a credential (objective 174, D8).

Usage: check-credential-prints.py ROOT...   (each ROOT a directory or a file)

Reads every shell script under the roots (*.sh, *.bash, or a file whose first
line is a sh/bash #!) and fails when a command that shows bytes is applied to a
known credential:

- a reader (cat, head, tail, less, sed without -i, awk, jq, xxd, od, base64,
  strings, cut, sort, diff, grep without -c/-l/-L/-q, ...) given a known
  credential file, as an argument or on `<`;
- a reader given a credential variable or a $( ) that read a credential file
  on a here-string (`cat <<<"$API_TOKEN"`) or in an unquoted heredoc's body
  (`cat <<EOF` ... `$API_TOKEN` ... `EOF`; a quoted `<<'EOF'` body is text);
- echo, printf, logger or wall of a credential variable or of a $( ) that read a
  credential file. A credential variable: a credential's name in upper case
  ($SPAWN_DATABASE_URL, ${R2_SECRET}), or any name ending in PASSWORD, PASSWD,
  APIKEY, SECRET or TOKEN ($pg_url_password); not a lower-case local such as
  $session or $gate_key (a cache key), nor the shell's $PWD and $OLDPWD;
- printenv NAME, and env, printenv, set, declare -p or export -p piped
  (through sort, uniq, cat, head or tail) to a grep (not -v), sed or awk for a
  credential name;

when what it shows reaches the output: the pipeline's last stage writes to the
terminal or the log (stdout not sent to a file; >&2, >/dev/stderr, >/dev/stdout,
>/dev/tty, /dev/fd/N and /proc/self/fd/N still count as shown, and so does a tee
to one of them), and
no later stage only counts or hashes (wc, sha*sum, md5sum, b2sum, cksum, grep
-c/-l/-L/-q). A $( ) whose output is kept in a variable or compared is not
shown: VAR=$(cat file) is how a script reads a credential.

The known credentials are not listed here. They come from the value
collector, the one list every value scan reads (secret-values.py; design
sync/design-store-scan-rule.md): its credential files (KV, MIXED, WHOLE,
WHOLE_ABSOLUTE, JSON_LEAVES, RETIRED, RENAMED) and older copies of them beside
them under a longer name, and its rule for a credential's name
(credential_name: TOKEN, KEY, SECRET, PASSWORD, ... as a whole word, and a
name ending in DATABASE_URL, objective 175). Nothing is added here. The collector is
read from $SPAWN_SECRET_VALUES, else /usr/local/lib/spawn/redact/secret-values.py.
A missing or unreadable collector refuses (exit 2): a check with no list must
not be able to say clean.

A line the check takes wrongly is accepted by a comment on it or on the line
before: `# credential-print: ok <why>` (the why is required). Accepted lines
are counted in the summary.

Heredocs are taken only from a real `<<` operator (not in a comment, in quotes,
in a here-string `<<<` or in `(( ))` arithmetic), and a heredoc whose delimiter
line never comes refuses (exit 2, naming path:line): the check never skips the
rest of a file silently. The summary counts the lines read and the heredoc body
lines taken as text.

Output: one line per finding, `path:line: <command>: <what it shows>`, naming
the command and the credential file or name, never the line itself (a script
line could hold a value). A word from the script that is not an environment
name (printenv's argument, an env | grep pattern) is shown as <a credential
name>. Exit 0 clean, 1 findings, 2 refused.

Not seen (said, not checked): a credential path held in a variable set
elsewhere ($f); a value held in a variable (v=$(cat file); echo "$v"); Python
and other languages; xtrace (set -x); a full environment dump not filtered for
a name (env alone); systemd-cat and other commands that log a command's
output; heredocs inside a quoted "$( )".
"""
import importlib.util
import os
import re
import shlex
import sys

COLLECTOR_DEFAULT = "/usr/local/lib/spawn/redact/secret-values.py"
ACCEPT = re.compile(r"#\s*credential-print:\s*ok\s+\S")

READERS = {"cat", "tac", "nl", "head", "tail", "less", "more", "most", "bat", "xxd", "od",
           "hexdump", "strings", "base64", "base32", "sed", "awk", "gawk", "mawk", "jq", "cut",
           "sort", "uniq", "paste", "column", "diff", "tee", "grep", "egrep", "fgrep", "rg",
           "zgrep", "zcat", "fold", "fmt", "rev"}
GREPS = {"grep", "egrep", "fgrep", "rg", "zgrep"}
CONCEALING = {"wc", "sha256sum", "sha1sum", "sha224sum", "sha384sum", "sha512sum", "md5sum",
              "b2sum", "cksum", "sum"}
PRINTERS = {"echo", "printf", "logger", "wall"}
ENV_PASSING = {"sort", "uniq", "cat", "tac", "head", "tail"}
SHOWN_TARGET = re.compile(r"^(/dev/(stdout|stderr|tty)|/dev/fd/\d+|/proc/self/fd/\d+)$")
# The shell's own variables whose names hold a credential word (PWD): never a credential.
SHELL_VARIABLES = {"PWD", "OLDPWD"}
ENV_NAME = re.compile(r"[A-Z_][A-Z0-9_]*")
ENV_DUMPS = {"env", "printenv", "export", "declare", "set"}
WRAPPERS = {"sudo", "command", "exec", "nohup", "builtin", "time"}
WRAPPERS_WITH_ARG = {"nice": {"-n"}, "ionice": {"-c", "-n"}, "timeout": set(), "stdbuf": set()}
COUNT_ONLY = {"-c", "--count", "-l", "--files-with-matches", "-L", "--files-without-match",
              "-q", "--quiet", "--silent"}
SCRIPT_SUFFIX = re.compile(r"\.(sh|bash|py|md|rs|ts|js|service|timer|txt|sql|json)$")
ASSIGN = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
VAR_REF = re.compile(r"\$\{?([A-Za-z_][A-Za-z0-9_]*)")
HEREDOC_WORD = re.compile(r"(['\"])([^'\"]+)\1|(\\?)([A-Za-z0-9_][A-Za-z0-9_.-]*)")


def refuse(message):
    print(f"check-credential-prints: {message}: refused", file=sys.stderr)
    sys.exit(2)


def load_collector():
    path = os.environ.get("SPAWN_SECRET_VALUES") or COLLECTOR_DEFAULT
    if not os.path.isfile(path):
        refuse(f"no value collector at {path}")
    try:
        spec = importlib.util.spec_from_file_location("secret_values", path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
    except (Exception, SystemExit) as error:  # named by its type only: its text could hold a value
        refuse(f"the value collector {path} cannot be read ({type(error).__name__})")
    for name in ("KV", "MIXED", "WHOLE", "WHOLE_ABSOLUTE", "JSON_LEAVES", "RETIRED", "RENAMED",
                 "credential_name"):
        if not hasattr(module, name):
            refuse(f"the value collector {path} has no {name}")
    return module


def credential_files(collector):
    """Each credential file as the tail that names it in a script: its name, or its
    directory and name when the name alone is too plain (.env, url, pw)."""
    paths = (list(collector.KV) + list(collector.MIXED) + list(collector.WHOLE)
             + list(collector.WHOLE_ABSOLUTE) + list(collector.JSON_LEAVES)
             + [collector.RETIRED] + list(collector.RENAMED.values()))
    tails = set()
    for path in paths:
        parts = [p for p in path.split("/") if p]
        name = parts[-1]
        if (name.startswith(".") or len(name) < 6) and len(parts) > 1:
            tails.add(parts[-2] + "/" + name)
        else:
            tails.add(name)
    return sorted(tails)


class Rules:
    def __init__(self, collector):
        self.files = credential_files(collector)
        self.file_res = [(t, re.compile(r"(?:^|[/\s\"'=])" + re.escape(t) + r"([.~-][\w.~-]*)?(?![\w.~-])"))
                         for t in self.files]
        self.is_credential_name = collector.credential_name
        self.endings = getattr(collector, "CREDENTIAL_ENDINGS", ())

    def file_in(self, word):
        for tail, pattern in self.file_res:
            for match in pattern.finditer(word):
                suffix = match.group(1) or ""
                if not SCRIPT_SUFFIX.search(suffix):
                    return tail
        return None

    def name(self, name):
        return self.is_credential_name(name)

    def variable(self, name):
        """A variable that holds a credential: a credential's name in the
        environment's upper case, or any name ending in one of the collector's
        endings (pg_url_password). A lower-case local named for a word (session,
        gate_key: a cache key) is not one."""
        if name in SHELL_VARIABLES:
            return False
        if name.isupper():
            return self.name(name)
        return name.upper().endswith(tuple(self.endings))

    def name_in(self, text):
        for word in re.findall(r"[A-Za-z_][A-Za-z0-9_]*", text):
            if self.name(word):
                return word
        return None


def substitutions(text):
    """The bodies of $( ) and ` ` in text, outermost first, and text with each
    replaced by a marker \x00N\x00."""
    bodies, out, i = [], [], 0
    while i < len(text):
        if text.startswith("$(", i) and not text.startswith("$((", i):
            depth, j = 1, i + 2
            while j < len(text) and depth:
                if text.startswith("$(", j):
                    depth += 1
                    j += 1
                elif text[j] == ")":
                    depth -= 1
                j += 1
            bodies.append(text[i + 2:j - 1])
            out.append(f"\x00{len(bodies) - 1}\x00")
            i = j
        elif text[i] == "`":
            j = text.find("`", i + 1)
            if j < 0:
                j = len(text)
            bodies.append(text[i + 1:j])
            out.append(f"\x00{len(bodies) - 1}\x00")
            i = j + 1
        else:
            out.append(text[i])
            i += 1
    return bodies, "".join(out)


def tokens(text):
    lexer = shlex.shlex(text, posix=True, punctuation_chars=";&|<>")
    lexer.whitespace_split = True
    lexer.commenters = "#"
    try:
        return list(lexer)
    except ValueError:  # an open quote: a string that goes on over lines
        return text.split()


def pipelines(words):
    """[[stage words, ...], ...] split at ; && || & and at |."""
    lines, stages, stage = [], [], []
    for word in words:
        if word in (";", "&&", "||", "&", ";;"):
            if stage:
                stages.append(stage)
            if stages:
                lines.append(stages)
            stages, stage = [], []
        elif word in ("|", "|&"):
            stages.append(stage)
            stage = []
        else:
            stage.append(word)
    if stage:
        stages.append(stage)
    if stages:
        lines.append(stages)
    return lines


def command_of(stage):
    """(command, arguments, input files, stdout sent to a file, here-string words,
    heredocs) of one stage."""
    words, inputs, to_file, here, heredocs, i = [], [], False, [], 0, 0
    while i < len(stage):
        word = stage[i]
        nxt = stage[i + 1] if i + 1 < len(stage) else ""
        if word == "<":
            inputs.append(nxt)
            i += 2
            continue
        if word == "<<<":
            here.append(nxt)
            i += 2
            continue
        if word in ("<<", "<<-"):
            heredocs += 1
            i += 2
            continue
        if word in (">", ">>", "&>", "&>>", ">|"):
            prev = words[-1] if words else ""
            shown = bool(SHOWN_TARGET.match(nxt))  # >/dev/stderr: still shown
            if prev.isdigit():  # 2> file: stderr; 1> file: stdout
                words.pop()
                if prev == "1":
                    to_file = not shown
            else:
                to_file = not shown
            i += 2
            continue
        if word == ">&":  # >&2 and >&1: still shown; >& file: both to the file
            if not nxt.isdigit() and nxt != "-":
                to_file = not SHOWN_TARGET.match(nxt)
            i += 2
            continue
        words.append(word)
        i += 1
    while words and ASSIGN.match(words[0]):
        words.pop(0)
    while words:
        head = os.path.basename(words[0])
        if head in WRAPPERS:
            words.pop(0)
        elif head == "env" and len(words) > 1 and (ASSIGN.match(words[1]) or words[1].startswith("-")):
            words.pop(0)
            while words and (ASSIGN.match(words[0]) or words[0].startswith("-")):
                words.pop(0)
        elif head in WRAPPERS_WITH_ARG:
            words.pop(0)
            while words and words[0].startswith("-"):
                flag = words.pop(0)
                if flag in WRAPPERS_WITH_ARG[head] and words:
                    words.pop(0)
            if head == "timeout" and words:
                words.pop(0)
        else:
            break
    if not words:
        return "", [], inputs, to_file, here, heredocs
    return os.path.basename(words[0]), words[1:], inputs, to_file, here, heredocs


def count_only(arguments):
    for word in arguments:
        if word in COUNT_ONLY:
            return True
        if re.fullmatch(r"-[A-Za-z]+", word) and set(word[1:]) & set("clLq"):
            return True
    return False


class Checker:
    def __init__(self, rules):
        self.rules = rules

    def value_in(self, word, subs):
        """The credential a word shows when expanded: a credential variable, or a
        $( ) (marker \x00N\x00 into subs) that shows one."""
        for ref in VAR_REF.findall(word):
            if self.rules.variable(ref):
                return "$" + ref
        for index in re.findall(r"\x00(\d+)\x00", word):
            inner = self.text(subs[int(index)])
            if inner:
                return inner[1]
        return None

    def shown_name(self, word):
        return word if ENV_NAME.fullmatch(word) else "<a credential name>"

    def pipeline(self, stages, subs, heres=None):
        """What this pipeline shows: (shown, why) for its output, before its last
        stage's redirection is judged; why names the credential. heres: the
        line's heredoc bodies, (quoted, text), taken in order."""
        tainted = None
        env_stream = False
        last_to_file = False
        shown_anyway = None
        for stage in stages:
            command, arguments, inputs, to_file, here, heredocs = command_of(stage)
            last_to_file = to_file
            why = None
            files = [w for w in arguments if not w.startswith("-")] + inputs
            bodies = [next(heres, (True, "")) if heres is not None else (True, "")
                      for _ in range(heredocs)]
            if command in READERS:
                for word in here:
                    found = self.value_in(word, subs)
                    if found:
                        why = (f"{command} <<<", found)
                for quoted, body in bodies:
                    if not quoted:
                        body_subs, flat = substitutions(re.sub(r"\\[$`]", "", body))  # \$X: text
                        found = self.value_in(flat, body_subs)
                        if found:
                            why = (f"{command} <<", found)
                for word in files:
                    found = self.rules.file_in(word)
                    if found:
                        why = (command, found)
                        break
                if command == "sed" and any(w.startswith("--in-place") or
                                            re.fullmatch(r"-[A-Za-z]*i.*", w) for w in arguments):
                    why = None  # in place: writes the file, shows nothing
                if command in GREPS and count_only(arguments):
                    why = None
                    tainted = None
                inverted = command in GREPS and any(w in ("-v", "--invert-match") or
                                                    re.fullmatch(r"-[A-Za-z]*v[A-Za-z]*", w)
                                                    for w in arguments)
                if (env_stream and command in GREPS | {"sed", "awk"} and not count_only(arguments)
                        and not inverted):
                    found = self.rules.name_in(" ".join(arguments))
                    if found:
                        why = (f"env | {command}", self.shown_name(found))
            if command in PRINTERS:
                for word in arguments:
                    found = self.value_in(word, subs)
                    if found:
                        marked = "\x00" in word and not any(
                            self.rules.variable(r) for r in VAR_REF.findall(word))
                        why = (f"{command} $( )" if marked else command, found)
            if command == "printenv" and arguments:
                for word in arguments:
                    if self.rules.name(word):
                        why = (command, self.shown_name(word))
            env_stream = ((command in ENV_DUMPS and
                           (not arguments or (command in ("declare", "export") and arguments == ["-p"])))
                          or (env_stream and command in ENV_PASSING))
            if why:
                tainted = why
            elif tainted and (command in CONCEALING or (command in GREPS and count_only(arguments))):
                tainted = None
            if tainted and command == "tee" and any(SHOWN_TARGET.match(w) for w in arguments):
                shown_anyway = tainted
        if shown_anyway and (last_to_file or not tainted):
            return shown_anyway, False
        return tainted, last_to_file

    def text(self, text):
        """The first credential this text shows when its output is used: a $( )'s body."""
        bodies, flat = substitutions(text)
        for stages in pipelines(tokens(flat)):
            tainted, to_file = self.pipeline(stages, bodies)
            if tainted and not to_file:
                return tainted
        return None

    def line(self, text, heres=()):
        """Findings of one logical line: [(command, what)]."""
        bodies, flat = substitutions(text)
        found = []
        heres = iter(heres)
        for stages in pipelines(tokens(flat)):
            tainted, to_file = self.pipeline(stages, bodies, heres)
            if tainted and not to_file:
                found.append(tainted)
        # A $( ) on its own: shown only when the command around it shows it (echo,
        # printf: judged above); kept in a variable or compared, it is not.
        return found


class OpenHeredoc(Exception):
    """A heredoc whose delimiter line never comes (its first line's number)."""


def heredocs(line):
    """(dash, quoted, delimiter) of each real << operator in a logical line: not
    in a comment, not in quotes, not a here-string <<<, not inside (( ))."""
    out, i, n, quote, arith = [], 0, len(line), None, 0
    while i < n:
        c = line[i]
        if quote:
            if c == "\\" and quote == '"':
                i += 2
                continue
            if c == quote:
                quote = None
            i += 1
            continue
        if c == "\\":
            i += 2
            continue
        if c in "'\"":
            quote = c
        elif c == "#" and (i == 0 or line[i - 1] in " \t;|&("):
            break
        elif line.startswith("((", i):
            arith += 1
            i += 2
            continue
        elif arith and line.startswith("))", i):
            arith -= 1
            i += 2
            continue
        elif not arith and line.startswith("<<<", i):
            i += 3
            continue
        elif not arith and line.startswith("<<", i):
            j = i + 2
            dash = j < n and line[j] == "-"
            j += dash
            while j < n and line[j] in " \t":
                j += 1
            match = HEREDOC_WORD.match(line, j)
            if match:
                quoted = bool(match.group(1) or match.group(3))
                out.append((dash, quoted, match.group(2) or match.group(4)))
                i = match.end()
                continue
            i = j
            continue
        i += 1
    return out


def logical_lines(text):
    """(first line number, logical line, accepted, heredoc bodies) with backslash
    continuations joined and each heredoc's body, (quoted, text), kept apart from
    the lines. Also the number of body lines. A heredoc with no delimiter line
    raises OpenHeredoc."""
    out, raw, i, body_lines = [], text.split("\n"), 0, 0
    while i < len(raw):
        start, line = i, raw[i]
        accepted = bool(ACCEPT.search(line)) or (i > 0 and bool(ACCEPT.search(raw[i - 1]))
                                                 and raw[i - 1].lstrip().startswith("#"))
        while line.endswith("\\") and i + 1 < len(raw):
            i += 1
            line = line[:-1] + " " + raw[i]
            accepted = accepted or bool(ACCEPT.search(raw[i]))
        i += 1
        if line.lstrip().startswith("#"):
            continue
        bodies = []
        for dash, quoted, delimiter in heredocs(line):
            first = i
            while i < len(raw) and (raw[i].lstrip("\t") if dash else raw[i]) != delimiter:
                i += 1
            if i >= len(raw):
                raise OpenHeredoc(start + 1)
            bodies.append((quoted, "\n".join(raw[first:i])))
            body_lines += i - first
            i += 1
        out.append((start + 1, line, accepted, bodies))
    return out, body_lines


def is_script(path):
    if path.endswith((".sh", ".bash")):
        return True
    try:
        with open(path, "rb") as handle:
            first = handle.readline(200)
    except OSError:
        return False
    return first.startswith(b"#!") and re.search(rb"\b(ba|da|k|z)?sh\b", first) is not None


def scripts(root):
    if os.path.isfile(root):
        yield root
        return
    for directory, subdirs, names in os.walk(root):
        subdirs[:] = sorted(d for d in subdirs if d not in (".git", "node_modules", ".work", "target"))
        for name in sorted(names):
            path = os.path.join(directory, name)
            if os.path.isfile(path) and not os.path.islink(path) and is_script(path):
                yield path


def main(argv):
    if not argv:
        print(__doc__.split("\n\n")[1], file=sys.stderr)
        return 2
    for root in argv:
        if not os.path.exists(root):
            refuse(f"no {root}")
    checker = Checker(Rules(load_collector()))
    findings = accepted = files = lines = text_lines = 0
    for root in argv:
        for path in scripts(root):
            files += 1
            try:
                with open(path, encoding="utf-8", errors="replace") as handle:
                    text = handle.read()
            except OSError as error:
                refuse(f"{path} cannot be read ({error.strerror})")
            try:
                logical, body_lines = logical_lines(text)
            except OpenHeredoc as error:
                refuse(f"{path}:{error.args[0]}: a heredoc with no delimiter line")
            lines += text.count("\n") + (not text.endswith("\n") and bool(text))
            text_lines += body_lines
            for number, line, ok, bodies in logical:
                for command, what in checker.line(line, bodies):
                    if ok:
                        accepted += 1
                    else:
                        findings += 1
                        print(f"{path}:{number}: {command}: shows {what}")
    print(f"check-credential-prints: {files} script(s), {findings} finding(s), {accepted} accepted"
          f" ({lines} line(s), {text_lines} of them heredoc bodies)", file=sys.stderr)
    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
