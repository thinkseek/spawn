# l2-semver-sort

Write `semver_sort.sh`, a POSIX `sh` script (it may use `awk`, `sed`,
`sort`) that reads versions from standard input, one per line, and writes
them sorted in ascending Semantic Versioning 2.0.0 precedence, one per
line, duplicates kept.

Every input line is `MAJOR.MINOR.PATCH` or `MAJOR.MINOR.PATCH-PRERELEASE`:
`MAJOR`, `MINOR`, `PATCH` are numbers without leading zeros (they may be
longer than any shell integer: compare them as numbers, not as text, and
not by converting to a float); `PRERELEASE` is dot-separated identifiers
of `[0-9A-Za-z-]`. Precedence:

1. `MAJOR`, then `MINOR`, then `PATCH`, numerically.
2. With those equal, a version with a pre-release is lower than one without.
3. Pre-releases compare identifier by identifier, left to right: two
   numeric identifiers numerically; two others by ASCII order; a numeric
   identifier is lower than a non-numeric one; when every identifier of
   one is equal to the start of the other, the shorter is lower.

A version given more than once is written as many times as given. The
script exits 0.
