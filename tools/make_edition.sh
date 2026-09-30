#!/usr/bin/env bash
# make_edition.sh — cut a REFERENCE EDITION of cobweb: a sanitised export of
# one commit, built and tested, for readers who get the tree without history.
#
#   tools/make_edition.sh <commit> [--date YYYY-MM-DD] [--out DIR]
#                         [--prev DIR] [--rules FILE] [--no-build]
#   make edition COMMIT=<commit> [DATE=...] [OUT=...] [RULES=...]
#
# What to remove is NOT in this repository: it comes from a private rules
# file (--rules, else $COBWEB_EDITION_RULES, else ~/.cobweb-edition-rules.py),
# a Python module defining
#   RULES      [R(path, why, old, new, optional=False)]  passage replacements;
#              `old` must occur exactly once or the run stops naming the rule
#   REMOVE     [(path, why)]           files dropped from the edition
#   RENAMES    [(old, new, why)]       files renamed in the edition
#   DRAFTS     default parent directory of --out
#   PREV       the previous edition (the scan's and the diff's baseline)
#   PROTECTED  [dir]                   directories this tool never writes
#   SCAN       {family: regex}         scanned in every text file and in the
#              strings of every binary; HARVEST names families whose matches
#              in the removed text become extra terms; REGISTRY an optional
#              tab/comma-separated file of more terms
#   EDITION_TITLE first line of bin/VERSIONS.txt, with {date}
#   RELEASE_HINT  printed at the end, formatted with {out} and {date}
#
# Steps: git archive <commit> (no history; sha256 recorded) -> rules ->
# cargo build --release + cargo test --release on the cleaned tree, and the
# tests on the uncleaned tree for a baseline -> bin/ + bin/VERSIONS.txt.
# Next to the edition, never inside it:
#   <out>.removal-report.md   everything the rules removed or changed
#   <out>.scan-report.md      every scan hit with file and line; NEW = not in PREV
#   <out>.edition-delta.diff  PREV -> this edition (bin/ excluded)
#   <out>.logs/               build and test logs
# Exit 0: edition written. Exit 1: error; no partial edition is left at <out>.
set -euo pipefail
export PYTHONDONTWRITEBYTECODE=1

REPO=$(cd "$(dirname "$0")/.." && pwd)
COMMIT= DATE=$(date +%F) OUT= PREV= BUILD=1
RULES=${COBWEB_EDITION_RULES:-$HOME/.cobweb-edition-rules.py}

die() { echo "make_edition: $*" >&2; exit 1; }
while [ $# -gt 0 ]; do
    case $1 in
        --date) DATE=$2; shift 2 ;;
        --out) OUT=$2; shift 2 ;;
        --prev) PREV=$2; shift 2 ;;
        --rules) RULES=$2; shift 2 ;;
        --no-build) BUILD=0; shift ;;
        -h|--help) sed -n '2,34p' "$0"; exit 0 ;;
        -*) die "unknown option $1" ;;
        *) [ -z "$COMMIT" ] || die "one commit only"; COMMIT=$1; shift ;;
    esac
done
[ -n "$COMMIT" ] || die "usage: tools/make_edition.sh <commit> [--date YYYY-MM-DD] [--out DIR] [--rules FILE]"
[[ $DATE =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] || die "bad --date $DATE"
[ -r "$RULES" ] || die "no rules file ($RULES): pass --rules or set COBWEB_EDITION_RULES"
RULES=$(realpath "$RULES")

# settings from the rules file: DRAFTS PREV PROTECTED (newline-separated)
cfg() {
    python3 - "$RULES" "$1" <<'PY'
import importlib.util, sys
s = importlib.util.spec_from_file_location("rules", sys.argv[1]); m = importlib.util.module_from_spec(s); s.loader.exec_module(m)
v = getattr(m, sys.argv[2], "")
print("\n".join(v) if isinstance(v, list) else v)
PY
}
OUT=$(realpath -m "${OUT:-$(cfg DRAFTS)/cobweb-$DATE}")
PREV=${PREV:-$(cfg PREV)}
while IFS= read -r p; do
    [ -z "$p" ] || case $OUT/ in "$(realpath -m "$p")"/*) die "$OUT is inside protected $p";; esac
done < <(cfg PROTECTED)
[ ! -e "$OUT" ] || die "$OUT exists; an edition is never overwritten"
[ -d "$PREV" ] || die "no previous edition at '$PREV' (--prev)"

SHA=$(git -C "$REPO" rev-parse --verify "$COMMIT^{commit}") || die "no such commit $COMMIT"
SHORT=${SHA:0:7}
WORK=$(mktemp -d "$(dirname "$OUT")/.edition-work.XXXXXX")
LOGS=$OUT.logs
trap 'rm -rf "$WORK"' EXIT
fail() { rm -rf "$OUT"; die "$*"; }
rm -rf "$LOGS" "$OUT".removal-report.md "$OUT".scan-report.md "$OUT".edition-delta.diff
mkdir -p "$LOGS"

# ── 1. export ────────────────────────────────────────────────────────────────
git -C "$REPO" archive "$SHA" > "$WORK/src.tar"
SRC_SUM=$(sha256sum "$WORK/src.tar" | cut -c1-64)
mkdir "$WORK/src" "$OUT"
tar -x -C "$WORK/src" -f "$WORK/src.tar"
tar -x -C "$OUT" -f "$WORK/src.tar"
echo "make_edition: $SHORT exported (archive sha256 $SRC_SUM)"

# ── 2. rules ─────────────────────────────────────────────────────────────────
python3 - "$RULES" "$OUT" "$OUT.removal-report.md" "$SHORT" "$SRC_SUM" "$DATE" <<'PY' || fail "cleaning rules failed"
import difflib, importlib.util, os, sys
rules_path, out, report, short, src_sum, date = sys.argv[1:]
s = importlib.util.spec_from_file_location("rules", rules_path); R = importlib.util.module_from_spec(s); s.loader.exec_module(R)

errors, rep = [], []
w = rep.append
w(f"# Removal report: cobweb edition {date} (from {short})\n\n"
  "PRIVATE: quotes what was removed. Keep it next to the edition, never inside it.\n\n"
  f"- Source: cobweb `{short}`, `git archive` sha256 `{src_sum}` (before cleaning)\n"
  f"- Rules: `{rules_path}` ({len(R.REMOVE)} removals, {len(R.RENAMES)} renames, {len(R.RULES)} passage rules)\n\n")

w("## Files removed\n\n")
for path, why in R.REMOVE:
    p = os.path.join(out, path)
    if os.path.lexists(p):
        os.remove(p)
        w(f"- `{path}`: {why}\n")
    else:
        w(f"- `{path}`: not in this commit (rule inert)\n")
for d, dirs, fs in os.walk(out, topdown=False):   # directories the removals emptied
    if not os.listdir(d) and d != out:
        os.rmdir(d)

w("\n## Files renamed\n\n")
for old, new, why in R.RENAMES:
    po, pn = os.path.join(out, old), os.path.join(out, new)
    if os.path.exists(po) and not os.path.exists(pn):
        os.rename(po, pn)
        w(f"- `{old}` -> `{new}`: {why}\n")
    else:
        errors.append(f"rename {old} -> {new}: expected {old} and no {new}")

w("\n## Passages replaced\n\n")
skipped = []
for n, r in enumerate(R.RULES, 1):
    p = os.path.join(out, r.path)
    text = open(p, encoding="utf-8").read() if os.path.exists(p) else None
    count = text.count(r.old) if text is not None else 0
    if count != 1:
        if count == 0 and r.optional:
            skipped.append((n, r))
            continue
        errors.append(f"rule {n} ({r.path}): passage found {count} times, need exactly 1")
        continue
    with open(p, "w", encoding="utf-8") as f:
        f.write(text.replace(r.old, r.new))
    d = difflib.unified_diff(r.old.splitlines(keepends=True), r.new.splitlines(keepends=True), "before", "after", n=0)
    w(f"### {n}. `{r.path}`\n\n{r.why}\n\n```diff\n{''.join(l for l in d if not l.startswith(('---', '+++')))}```\n\n")
if skipped:
    w("## Optional rules not applied (passage absent in this commit)\n\n")
    for n, r in skipped:
        w(f"- {n}. `{r.path}`: {r.why}\n")
open(report, "w", encoding="utf-8").write("".join(rep))
if errors:
    print("\n".join("  " + e for e in errors), file=sys.stderr)
    sys.exit(1)
print(f"make_edition: {len(R.RULES) - len(skipped)} passage rules applied, {len(skipped)} optional skipped")
PY

# ── 3. build, test, VERSIONS.txt ─────────────────────────────────────────────
tests_passed() { grep -E '^test result:' "$1" | awk '{p+=$4} END {print p+0}'; }
tests_failed() { grep -E '^test result:' "$1" | awk '{f+=$6} END {print f+0}'; }
if [ "$BUILD" = 1 ]; then
    before=$(cd "$OUT" && find . -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum)
    export CARGO_TERM_COLOR=never
    # no local path may end up in a binary
    export RUSTFLAGS="--remap-path-prefix=$OUT=/cobweb --remap-path-prefix=$WORK=/build"
    echo "make_edition: building the cleaned tree"
    (cd "$OUT" && CARGO_TARGET_DIR=$WORK/target-clean cargo build --release --manifest-path sim/Cargo.toml) \
        > "$LOGS/build-clean.log" 2>&1 || fail "cleaned tree does not build (see $LOGS/build-clean.log)"
    echo "make_edition: testing the cleaned tree"
    (cd "$OUT" && CARGO_TARGET_DIR=$WORK/target-clean cargo test --release --manifest-path sim/Cargo.toml) \
        > "$LOGS/test-clean.log" 2>&1 || fail "cleaned tree fails its tests (see $LOGS/test-clean.log)"
    echo "make_edition: testing the uncleaned tree (baseline)"
    (cd "$WORK/src" && CARGO_TARGET_DIR=$WORK/target-src cargo test --release --manifest-path sim/Cargo.toml) \
        > "$LOGS/test-baseline.log" 2>&1 || true
    after=$(cd "$OUT" && find . -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum)
    [ "$before" = "$after" ] || fail "the build changed files in the edition tree"
    CP=$(tests_passed "$LOGS/test-clean.log") CF=$(tests_failed "$LOGS/test-clean.log")
    BP=$(tests_passed "$LOGS/test-baseline.log") BF=$(tests_failed "$LOGS/test-baseline.log")
    [ "$CF" = 0 ] || fail "$CF test(s) failed on the cleaned tree"

    mkdir "$OUT/bin"
    bins=$(cd "$OUT" && cargo metadata --no-deps --format-version 1 --manifest-path sim/Cargo.toml \
        | python3 -c 'import json,sys; print(" ".join(sorted({t["name"] for p in json.load(sys.stdin)["packages"] for t in p["targets"] if "bin" in t["kind"]})))')
    for b in $bins; do install -m 0755 "$WORK/target-clean/release/$b" "$OUT/bin/$b"; done
    base_note="baseline $SHORT: $BP passed"
    [ "$BF" = 0 ] || base_note="$base_note, $BF FAILED"
    {
        title=$(cfg EDITION_TITLE); title=${title:-cobweb edition {date\} — release binaries}
        echo "${title//\{date\}/$DATE}"
        echo "source: cobweb $SHORT (history omitted); built from this clean tree"
        echo "source tree git archive sha256 ($SHORT, before cleaning): $SRC_SUM"
        echo "toolchain: $(rustc --version); $(cargo --version)"
        echo "target: x86_64-unknown-linux-gnu, cargo build --release --manifest-path sim/Cargo.toml"
        echo "tests: cargo test --release => $CP passed, $CF failed ($base_note)"
        echo
        echo "sha256  binary"
        (cd "$OUT/bin" && for b in $bins; do sha256sum "$b"; done)
    } > "$OUT/bin/VERSIONS.txt"
    echo "make_edition: $CP tests passed on the cleaned tree (baseline $BP); $(echo $bins | wc -w) binaries"
fi

# ── 4. scan + the edition-to-edition diff ────────────────────────────────────
python3 - "$RULES" "$OUT" "$PREV" "$OUT.scan-report.md" <<'PY' || fail "scan failed"
import importlib.util, os, re, subprocess, sys
rules_path, out, prev, report = sys.argv[1:]
s = importlib.util.spec_from_file_location("rules", rules_path); R = importlib.util.module_from_spec(s); s.loader.exec_module(R)
fam = dict(R.SCAN)
harvest = set()
for r in R.RULES:
    removed = "".join(l for l in r.old.splitlines() if l not in r.new)
    for k in getattr(R, "HARVEST", []):
        harvest |= {m.group(0).lower() for m in re.finditer(fam[k], removed, re.I)}
extra = sorted(h for h in harvest if not any(re.search(rx, h, re.I) for rx in fam.values()))
if extra:
    fam["harvested"] = "|".join(map(re.escape, extra))
reg = getattr(R, "REGISTRY", "")
if reg and os.access(reg, os.R_OK):
    terms = {re.escape(t) for row in open(reg) for t in re.split(r"[\t,]", row.strip()) if len(t) >= 4}
    if terms:
        fam["registry"] = "|".join(sorted(terms))
PATS = [(k, re.compile(v, re.I)) for k, v in fam.items()]

def scan(root):
    hits = []
    for d, dirs, fs in os.walk(root):
        dirs.sort()
        for f in sorted(fs):
            p = os.path.join(d, f)
            rel = os.path.relpath(p, root)
            b = open(p, "rb").read()
            text = b"\0" not in b[:65536]
            if not text and rel.startswith("bin/"):
                lines, where = subprocess.run(["strings", "-n", "6", p], capture_output=True, text=True).stdout.splitlines(), "strings"
            elif text:
                lines, where = b.decode("utf-8", "replace").splitlines(), "line"
            else:
                continue
            for i, ln in enumerate([rel] + lines):
                for k, rx in PATS:
                    m = rx.search(ln)
                    if m:
                        loc = f"{rel} (path)" if i == 0 else (f"{rel}:{i}" if where == "line" else f"{rel} (strings)")
                        hits.append((k, loc, rel, m.group(0), ln.strip()[:200]))
    return hits

now = scan(out)
old = {(k, rel, t) for k, _, rel, _, t in scan(prev)}
new = [h for h in now if (h[0], h[2], h[4]) not in old]
w = ["# Scan report\n\nPRIVATE: names what it searched for. Keep it next to the edition, never inside it.\n\n",
     f"- Edition: `{out}`\n- Baseline for NEW: `{prev}`\n",
     f"- Hits: {len(now)} total, **{len(new)} NEW** (not in the baseline with the same file and text)\n\n",
     "| family | hits | new |\n|---|---|---|\n"]
w += [f"| {k} | {sum(h[0] == k for h in now)} | {sum(h[0] == k for h in new)} |\n" for k, _ in PATS]
w.append("\n## NEW hits\n\n")
w += [f"- `{loc}` [{k}] `{m}`: {t}\n" for k, loc, _, m, t in new] or ["none\n"]
w.append("\n## All hits\n\n")
w += [f"- `{loc}` [{k}] `{m}`: {t}\n" for k, loc, _, m, t in now]
w.append("\n## Patterns\n\n")
w += [f"- {k}: `{v}`\n" for k, v in fam.items()]
open(report, "w").write("".join(w))
print(f"make_edition: scan {len(now)} hits, {len(new)} new -> {report}")
PY

(cd "$(dirname "$OUT")" && diff -ruN -x bin "$PREV" "$(basename "$OUT")") > "$OUT.edition-delta.diff" || true
echo "make_edition: edition     $OUT"
echo "make_edition: review      $OUT.edition-delta.diff ($(grep -c '' "$OUT.edition-delta.diff") lines)"
echo "make_edition: removals    $OUT.removal-report.md"
echo "make_edition: scan        $OUT.scan-report.md"
hint=$(cfg RELEASE_HINT)
[ -z "$hint" ] || echo "make_edition: release (after review): ${hint//\{out\}/$OUT}" | sed "s/{date}/$DATE/g"
