#!/usr/bin/env bash
#
# The crate's pure-Rust rule, checked against the resolved dependency graph.
#
# § Dependencies of the design document states the rule as a requirement rather than an
# observation: a graph with no C in it builds for any target the Rust toolchain supports with
# no host toolchain, no `cc`, and no build script that shells out. That is what makes the
# `i686` and `wasm32` lanes cheap enough to run on every push, and those lanes are in turn
# what keeps the bit-exactness guarantee scoped to targets somebody actually builds for.
#
# Nothing in CI states that rule, which is why this script exists. The `wasm32-wasip1` lane
# is *evidence* for it — a graph that compiled C would fail there — but evidence read off a
# lane that exists for another reason is not the same as the check, and it names no package
# when it goes red.
#
# What is checked, over the non-dev graph:
#
#   1. No package declares `links`. That key is the definitive marker of native library
#      linkage; `libz-sys`, reached by `flate2`'s C backends, carries `links = "z"`.
#   2. No package is a C-toolchain crate — `cc`, `cmake`, `bindgen`, `pkg-config` and the
#      rest of the family below. Reaching one of those means something compiles or locates C
#      at build time, whether or not the result carries a `links` key.
#   3. No package name ends in `-sys` or `_sys`, the convention for a binding to a native
#      library.
#
# Build edges are in scope deliberately: a `cc` that only ever runs at build time still
# demands a host C toolchain, which is the cost the rule exists to avoid. So is every
# platform — `--target all` is what makes a dependency gated to
# `[target.'cfg(windows)'.dependencies]` a finding here rather than a surprise for somebody
# else's target.
#
# **Membership comes from `cargo tree`, not from `cargo metadata`'s resolve.** The two
# disagree, and the disagreement is not academic: `cargo metadata` reports a package that
# feature resolution never activates, so `flate2` 1.1.10 reads there as pulling `zlib-rs`
# — which no build of this crate compiles. Grading that view would fail the gate on a
# package that is in the lockfile and nowhere else. `cargo metadata` is used here only to
# read attributes off packages `cargo tree` has already placed in the graph.
#
# **A build script is not itself a violation and is not reported as one.** Five packages in
# the current graph carry one — `crc32fast`, `libc`, `proc-macro2`, `quote`, `thiserror` —
# and every one of them is pure Rust emitting `cargo:rustc-cfg`. The residual hole is a build
# script that shells out to a compiler by hand without going through `cc`; that one is caught
# by the `wasm32-wasip1` lane rather than here, and the summary line names the count so the
# hole is visible rather than implied.
#
# Usage:
#   tools/check-pure-rust.sh [--manifest-path PATH]   grade the graph, explain the verdict
#   tools/check-pure-rust.sh --graph [...]            print `name version` per line, sorted
#
# `--graph` exists so two revisions can be diffed: a version bump that pulls a package the
# graph did not carry before is the interesting case, and the bump's own diff does not say so.
#
# The fuzz workspace is **not** in scope and fails this check by design: `libfuzzer-sys`
# builds libFuzzer through `cc`. It is a development harness rather than something the crate
# ships, and `fuzz/Cargo.toml` is kept out of the parent workspace for the same reason.

set -euo pipefail

manifest="Cargo.toml"
mode="check"

while [ $# -gt 0 ]; do
    case "$1" in
        --manifest-path) manifest="${2:?--manifest-path needs a value}"; shift 2 ;;
        --graph)         mode="graph"; shift ;;
        -h|--help)       sed -n '/^# Usage:/,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)               echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

# `--locked` rather than `--offline`: the question is what the committed lockfile resolves to,
# and a run that silently re-resolved would grade a graph no build ever sees.
tree="$(mktemp)"
meta="$(mktemp)"
trap 'rm -f "$tree" "$meta"' EXIT

cargo tree --locked --all-features --target all --edges normal,build \
    --prefix depth --no-dedupe --manifest-path "$manifest" > "$tree"
cargo metadata --format-version 1 --all-features --locked --manifest-path "$manifest" > "$meta"

python3 - "$tree" "$meta" "$mode" <<'PYTHON'
import json
import re
import sys

tree_path, meta_path, mode = sys.argv[1], sys.argv[2], sys.argv[3]

# The C-toolchain family. Every one of these means a C or C++ compiler, or a search for a
# native library, at build time. `autocfg` is deliberately absent: it probes with rustc alone.
C_TOOLCHAIN = {
    "cc", "cmake", "bindgen", "pkg-config", "vcpkg", "nasm-rs",
    "system-deps", "metadeps", "autotools", "make-cmd", "meson-next",
}

# `--prefix depth` writes the depth with no separator: `0astroframe v0.2.2`, `1flate2 v1.1.10`.
# The trailing ` (*)` marks a subtree printed elsewhere and ` (proc-macro)` a kind.
LINE = re.compile(r"^(\d+)([A-Za-z0-9_.+-]+) v([^ ]+)")

# The depth stack turns the flat listing back into paths, so a finding is reported with the
# chain that reaches it rather than as a bare name.
chains, stack = {}, []
for raw in open(tree_path):
    match = LINE.match(raw.rstrip("\n"))
    if not match:
        continue
    depth, name, version = int(match.group(1)), match.group(2), match.group(3)
    del stack[depth:]
    stack.append("%s %s" % (name, version))
    chains.setdefault((name, version), list(stack))

if mode == "graph":
    for entry in sorted("%s %s" % key for key in chains):
        print(entry)
    sys.exit(0)

# A graph of one is what a failed resolve looks like, and grading an empty set passes
# silently. Refuse instead — the same reason `deps-greps` refuses an empty vendor directory.
if len(chains) < 2:
    sys.exit("resolved %d package(s) -- refusing to pass on a graph that small" % len(chains))

attributes = {(p["name"], p["version"]): p for p in json.load(open(meta_path))["packages"]}
root = chains and min(chains, key=lambda key: len(chains[key]))

findings, build_scripts = [], []
for key, chain in sorted(chains.items()):
    name, version = key
    package = attributes.get(key, {})
    where = "    " + " -> ".join(chain)

    if package.get("links"):
        findings.append("%s %s declares links = %s -- native library linkage\n%s"
                        % (name, version, package["links"], where))
    if key != root and name in C_TOOLCHAIN:
        findings.append("%s %s is a C-toolchain crate -- the build needs a C compiler or pkg-config\n%s"
                        % (name, version, where))
    if name.endswith("-sys") or name.endswith("_sys"):
        findings.append("%s %s is a -sys crate -- bindings to a native library\n%s"
                        % (name, version, where))

    if any("custom-build" in t["kind"] for t in package.get("targets", [])):
        build_scripts.append("%s %s" % (name, version))

print("non-dev graph: %d packages (normal and build edges, every target)" % len(chains))
print("build scripts: %d -- %s" % (len(build_scripts), ", ".join(build_scripts) or "none"))
print("  a build script is not a violation; that it compiles no C is what wasm32-wasip1 proves")

if findings:
    print()
    for finding in findings:
        print("FAIL: " + finding)
    sys.exit("\nthe dependency graph is not pure Rust: %d finding(s)" % len(findings))

print("verdict: pure Rust -- no links key, no C-toolchain crate, no -sys crate")
PYTHON
