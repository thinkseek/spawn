#!/bin/sh
# The reference slug: lowercase, runs of other bytes to one '-', trimmed.
LC_ALL=C
export LC_ALL
tr 'A-Z' 'a-z' | sed -e 's/[^a-z0-9][^a-z0-9]*/-/g' -e 's/^-//' -e 's/-$//'
