#!/bin/sh
# The reference: Kahn's algorithm, the ASCII-smallest ready node first.
LC_ALL=C
export LC_ALL
awk '
NF == 0 { next }
{
    node[$1] = 1
    if (!($1 in indeg)) indeg[$1] = 0
    if (NF >= 2) {
        node[$2] = 1
        if (!($2 in indeg)) indeg[$2] = 0
        edges[$1] = edges[$1] " " $2
        indeg[$2]++
    }
}
END {
    left = 0
    for (n in node) left++
    while (left > 0) {
        best = ""
        for (n in node) if (!(n in done) && indeg[n] == 0 && (best == "" || (n "") < (best ""))) best = n
        if (best == "") break
        print best; done[best] = 1; left--
        k = split(edges[best], out, " ")
        for (i = 1; i <= k; i++) indeg[out[i]]--
    }
    if (left == 0) exit 0
    print "cycle"
    m = 0
    for (n in node) if (!(n in done)) rest[++m] = n
    for (i = 2; i <= m; i++) { t = rest[i]; j = i - 1
        while (j >= 1 && (rest[j] "") > (t "")) { rest[j + 1] = rest[j]; j-- }
        rest[j + 1] = t }
    for (i = 1; i <= m; i++) print rest[i]
    exit 1
}'
