#!/usr/bin/env python3
"""Compare two physoracle traces and report the first divergence.

    python trace_diff.py A.trace B.trace

Line endings are normalised (the committed goldens are CRLF, the oracle writes LF).
For entity lines (`e NAME p <3> q <4> v <3> w <3> sc <int> aw <int>`) the first
differing field is named, with the step number and entity name, e.g.

    first divergence: step 7 entity box field p[2]: 409d9791 != 409d9792

Exit status: 0 identical, 1 different, 2 unreadable input.
"""
import sys


def fields(line):
    """Split an entity line into labelled hex words; returns (name, field, [values])."""
    t = line.split()
    if not t or t[0] != "e" or "p" not in t:
        return None
    name = t[1]
    out = []
    labels = {"p": 3, "q": 4, "v": 3, "w": 3, "sc": 1, "aw": 1}
    i = 2
    while i < len(t):
        key = t[i]
        n = labels.get(key)
        if n is None:
            i += 1
            continue
        vals = t[i + 1 : i + 1 + n]
        for k, v in enumerate(vals):
            out.append((f"{key}[{k}]", v))
        i += 1 + n
    return name, out


def main():
    if len(sys.argv) != 3:
        print(__doc__.strip().splitlines()[0], file=sys.stderr)
        return 2
    try:
        a = open(sys.argv[1], "rb").read().replace(b"\r\n", b"\n").decode(errors="replace").splitlines()
        b = open(sys.argv[2], "rb").read().replace(b"\r\n", b"\n").decode(errors="replace").splitlines()
    except OSError as e:
        print(f"cannot read: {e}", file=sys.stderr)
        return 2
    step = -1
    for i in range(max(len(a), len(b))):
        x = a[i] if i < len(a) else "<missing line>"
        y = b[i] if i < len(b) else "<missing line>"
        toks = x.split()
        if len(toks) >= 2 and toks[0] == "step":
            step = int(toks[1])
        if x == y:
            continue
        fa, fb = fields(x), fields(y)
        detail = ""
        if fa and fb and fa[0] == fb[0]:
            da, db = dict(fa[1]), dict(fb[1])
            for k, va in fa[1]:
                vb = db.get(k)
                if vb != va:
                    detail = f" entity {fa[0]} field {k}: {va} != {vb}"
                    break
        print(f"first divergence: line {i + 1} step {step}{detail}")
        print(f"  A: {x}")
        print(f"  B: {y}")
        return 1
    print(f"identical ({len(a)} lines)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
