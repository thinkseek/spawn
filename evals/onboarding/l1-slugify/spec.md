# l1-slugify

Write `slugify.sh`, a POSIX `sh` script (it may use `tr`, `sed` and `awk`)
that reads lines from standard input and writes, for each input line, one
output line: its slug.

The slug of a line, byte by byte with `LC_ALL=C`:

1. ASCII `A`-`Z` become `a`-`z`.
2. Every maximal run of bytes that are not `a`-`z` or `0`-`9` becomes one `-`.
3. A leading and a trailing `-` are removed.

A line with no letter or digit has the empty slug (an empty output line).
The script exits 0.

Example: `  Hello, World!` gives `hello-world`; `Café au lait` gives
`caf-au-lait`.
