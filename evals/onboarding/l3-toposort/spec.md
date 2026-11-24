# l3-toposort

Write `toposort.sh`, a POSIX `sh` script (it may use `awk`, `sed`, `sort`)
that orders the nodes of a dependency graph read from standard input.

Input, one item per line; blank lines are ignored:

- `A B`: node `A` must come before node `B` (an edge; repeats are allowed,
  and `A A` is a cycle);
- `A`: node `A` exists.

Node names are `[A-Za-z0-9_]+`, compared as ASCII bytes (`LC_ALL=C`).

Output: every node exactly once, one per line, in the one order where each
node comes after every node with an edge to it and, whenever several nodes
could come next, the ASCII-smallest comes first. Exit 0.

When no such order exists (a cycle): first write the nodes that can be
ordered, as above, until none can; then a line `cycle`; then every node
left, ASCII-sorted, one per line; and exit 1.

Example: `b c` and `a c` give `a`, `b`, `c`. `x y`, `y x`, `x z`, `w`
give `w`, `cycle`, `x`, `y`, `z`.
