#!/usr/bin/env python3
"""Compare physoracle traces and report the first divergence.

    python trace_diff.py A.trace B.trace
    python trace_diff.py --all [X64_DIR] [X86_DIR]

Single-file mode: prints the first divergence as step / entity / field, for
entity lines (`e NAME p <3> q <4> v <3> w <3> sc <int> aw <int>`):

    first divergence: step 7 entity box field p[2]: 409d9791 != 409d9792

--all compares every trace in the 1.2 golden directory (default
`../tests/golden-12` relative to this script) with its x64 golden in
`../tests/golden`.  `<scenario>.fpu24.trace` / `.fpu53.trace` fall back to
`<scenario>.trace`; `<scenario>.trace` is used as is.  One line per file plus
counts.  Exit status: 0 identical, 1 different/missing, 2 unreadable input.
"""
import os
import sys


def fields(line):
    """Split an entity line into labelled hex words; returns (name, [(field, value)])."""
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


def load(path):
    with open(path, "rb") as f:
        return f.read().replace(b"\r\n", b"\n").decode(errors="replace").splitlines()


def first_divergence(a, b):
    """Return (identical, message) for two line lists."""
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
            db = dict(fb[1])
            for k, va in fa[1]:
                vb = db.get(k)
                if vb != va:
                    detail = f" entity {fa[0]} field {k}: {va} != {vb}"
                    break
        return False, f"line {i + 1} step {step}{detail}"
    return True, f"identical ({len(a)} lines)"


def golden_for(x86_path):
    """x64 golden path for a golden-12 trace name."""
    name = os.path.basename(x86_path)
    base = name
    for suffix in (".fpu24.trace", ".fpu53.trace"):
        if name.endswith(suffix):
            base = name[: -len(suffix)] + ".trace"
            break
    if base == "vars.trace":
        return "engine_tables.trace"  # vars.trace is the dumpvars line of engine_tables
    return base


def run_all(x64_dir, x86_dir):
    try:
        names = sorted(os.listdir(x86_dir))
    except OSError as e:
        print(f"cannot read {x86_dir}: {e}", file=sys.stderr)
        return 2
    counts = {"identical": 0, "divergent": 0, "missing": 0}
    rows = []
    for name in names:
        if not name.endswith(".trace"):
            continue
        x86 = os.path.join(x86_dir, name)
        base = golden_for(name)
        x64 = os.path.join(x64_dir, base)
        if not os.path.exists(x64):
            counts["missing"] += 1
            rows.append((name, f"no x64 golden ({base})"))
            continue
        try:
            a, b = load(x64), load(x86)
        except OSError as e:
            print(f"cannot read {name}: {e}", file=sys.stderr)
            return 2
        same, msg = first_divergence(a, b)
        counts["identical" if same else "divergent"] += 1
        rows.append((name, msg))
    for name, msg in rows:
        print(f"{name}: {msg}")
    print(
        f"summary: {counts['identical']} identical, {counts['divergent']} divergent, "
        f"{counts['missing']} without x64 golden"
    )
    return 0 if counts["divergent"] == 0 and counts["missing"] == 0 else 1


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    if len(sys.argv) >= 2 and sys.argv[1] == "--all":
        args = sys.argv[2:]
        x64 = args[0] if len(args) > 0 else os.path.join(here, "..", "tests", "golden")
        x86 = args[1] if len(args) > 1 else os.path.join(here, "..", "tests", "golden-12")
        return run_all(x64, x86)
    if len(sys.argv) != 3:
        print(__doc__.strip().splitlines()[0], file=sys.stderr)
        return 2
    try:
        a, b = load(sys.argv[1]), load(sys.argv[2])
    except OSError as e:
        print(f"cannot read: {e}", file=sys.stderr)
        return 2
    same, msg = first_divergence(a, b)
    if same:
        print(msg)
        return 0
    print(f"first divergence: {msg}")
    i = int(msg.split()[1]) - 1
    print(f"  A: {a[i] if i < len(a) else '<missing line>'}")
    print(f"  B: {b[i] if i < len(b) else '<missing line>'}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
