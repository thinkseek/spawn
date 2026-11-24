#!/bin/sh
# The reference: a stable insertion sort in awk with SemVer 2.0.0 precedence.
LC_ALL=C
export LC_ALL
awk '
function numcmp(a, b) {
    sub(/^0+/, "", a); sub(/^0+/, "", b)
    if (length(a) != length(b)) return length(a) < length(b) ? -1 : 1
    # Compared as text (equal lengths): awk would compare numeric-looking
    # strings as floating point.
    if ((a "") == (b "")) return 0
    return (a "") < (b "") ? -1 : 1
}
function strcmp(a, b) { return (a "") == (b "") ? 0 : ((a "") < (b "") ? -1 : 1) }
function isnum(s) { return s ~ /^[0-9]+$/ }
function precmp(a, b,    x, y, n, m, i, c) {
    if (a == "" && b == "") return 0
    if (a == "") return 1
    if (b == "") return -1
    n = split(a, x, "."); m = split(b, y, ".")
    for (i = 1; i <= n && i <= m; i++) {
        if (isnum(x[i]) && isnum(y[i])) c = numcmp(x[i], y[i])
        else if (isnum(x[i])) c = -1
        else if (isnum(y[i])) c = 1
        else c = strcmp(x[i], y[i])
        if (c != 0) return c
    }
    return n == m ? 0 : (n < m ? -1 : 1)
}
function vercmp(a, b,    pa, pb, ca, cb, x, y, i, c) {
    pa = ""; pb = ""
    i = index(a, "-"); if (i) { pa = substr(a, i + 1); a = substr(a, 1, i - 1) }
    i = index(b, "-"); if (i) { pb = substr(b, i + 1); b = substr(b, 1, i - 1) }
    split(a, x, "."); split(b, y, ".")
    for (i = 1; i <= 3; i++) { c = numcmp(x[i], y[i]); if (c != 0) return c }
    return precmp(pa, pb)
}
{ v[NR] = $0 }
END {
    for (i = 2; i <= NR; i++) {
        t = v[i]; j = i - 1
        while (j >= 1 && vercmp(v[j], t) > 0) { v[j + 1] = v[j]; j-- }
        v[j + 1] = t
    }
    for (i = 1; i <= NR; i++) print v[i]
}'
