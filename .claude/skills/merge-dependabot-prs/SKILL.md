---
name: merge-dependabot-prs
description: Use when Dependabot pull requests are open on this repository and need triage or merging — the Monday batch, a request to clear the dependency queue, a stuck bump, or a single update waiting on review.
---

# Merging Dependabot pull requests

## Overview

**A bump is graded on the graph it produces, not on the version numbers in its title.**

This crate's premise is that its runtime dependency graph is pure Rust: no C library, no
`-sys` crate, no build-time toolchain. § Dependencies of the design document states that as a
requirement, because it is what makes the `i686` and `wasm32` lanes cheap enough to run on
every push — and those lanes are what keep the bit-exactness guarantee scoped to targets
somebody actually builds for. A version bump is the only change that can destroy it silently,
from a transitive package nobody named, and **no CI lane states the rule**.

`tools/check-pure-rust.sh` is where the rule is stated. Read its header once.

## Triage

```sh
gh pr list --author "app/dependabot" --state open \
  --json number,title,headRefName,mergeable,mergeStateStatus
```

Classify each by head branch — the three kinds get different gates:

| Head branch | What it can change | Gate |
| --- | --- | --- |
| `dependabot/cargo/…` | the library's own graph | purity gate, graph diff, CI |
| `dependabot/cargo/fuzz/…` | the fuzz harness alone | the fuzz workspace still resolves |
| `dependabot/github_actions/…` | workflows | CI |

## Per pull request

**1. Shape.** `gh pr diff N --name-only`. A cargo bump touches `Cargo.toml` and `Cargo.lock`
(or `fuzz/Cargo.lock`) and nothing else; an actions bump touches `.github/` and nothing else.
Dependabot does not edit source. Anything outside that list is the end of the run — report it.

Ask GitHub, not the local checkout. A `git diff origin/main...prN` reads whatever `origin/main`
was last fetched, and after a merge earlier in the same batch that ref is behind — the diff then
carries the *previous* pull request's lockfile and reads as a shape violation that is not one.

**2. Worktree.** Never grade a bump from the diff; grade it from a resolved graph.

```sh
git fetch origin pull/N/head:prN
git worktree add /path/to/scratch/prN prN
```

**3. Purity gate** — `dependabot/cargo/…` only. In the worktree:

```sh
tools/check-pure-rust.sh
```

Non-zero exit means the update is not pure Rust. **Stop. Touch nothing** — no comment, no
label, no close — and report the finding with the reach chain the script printed.

**4. Graph diff** — same PR class. What entered or moved is not in the PR title:

```sh
tools/check-pure-rust.sh --graph                    # on main
tools/check-pure-rust.sh --graph                    # in the worktree
diff main-graph.txt head-graph.txt
```

Report every line that moves. A transitive *minor* bump riding along under a patch-bump title
is the thing this catches — `flate2` 1.1.9 → 1.1.10 carries `miniz_oxide` 0.8.9 → 0.9.1.

**5. The fuzz workspace** — `dependabot/cargo/fuzz/…` only.

**Do not run the purity gate on it.** `libfuzzer-sys` builds libFuzzer through `cc`, so the
workspace fails the gate by design; it is a development harness rather than something the
crate ships, which is why `fuzz/Cargo.toml` is kept out of the parent workspace. **CI green
says nothing here either** — no lane builds the fuzz workspace on a pull request, `heavy.yml`
being scheduled and manual. The signal is a resolve on the PR head:

```sh
cargo check --locked --manifest-path fuzz/Cargo.toml
```

**6. The decode-path read.** `dependabot-automerge.yml` withholds automerge from `quick-xml`,
`lz4_flex`, `flate2`, `ruzstd` and `base64` because every byte those crates see comes from a
file this crate was asked to parse, and their bumps "want a human read whatever the semver
range says". **Running this skill is that read** — do it rather than inheriting the hold.

Read the upstream release notes across the version range and state in one line what changed.
Hold, and say why, if the notes describe a decoder change, an output change, or new `unsafe`:
that is a claim about decoded bits or about hardening, and settling it needs the corpus
differential rather than a merge.

**7. CI.** The single required check is named `CI`, and it is an aggregate: it `needs:` every
other job, so it posts no status at all until they finish. Watch the whole set, then confirm the
required one:

```sh
gh pr checks N --watch --fail-fast
gh pr checks N --required
```

**`--required --watch` together is a trap.** In the window before the aggregate reports —
which is exactly where a freshly updated branch sits — it exits 1 with `no required checks
reported on the branch`. That is "not yet", not "red", and grading it as red holds a mergeable
bump for no reason.

Red or pending is not a merge.

**8. Merge.** `gh pr merge N --squash --delete-branch`

**9. One at a time, and re-grade what the merge displaced.** Branch protection requires an
up-to-date branch, so the first merge in a batch leaves every other open pull request
`mergeStateStatus: BEHIND` — including ones touching a different lockfile, which cannot
conflict and are held anyway. `BEHIND` is not a merge.

Dependabot rebases on its own eventually. When it has not, move the branch:

```sh
gh pr update-branch N
```

**That changes the head commit, and a head commit that changed is a head commit that was not
graded.** Re-run this pull request's gates from step 1 against the moved head before merging
it — the shape check, the purity gate and the graph diff, or the fuzz resolve. CI re-runs
there too, so step 7 comes round again with it.

Expect `mergeable` and `mergeStateStatus` to read `UNKNOWN` for a few seconds after any merge
or branch update while GitHub recomputes. Poll until they settle rather than reading `UNKNOWN`
as either verdict.

## After the batch

- A pull request that edited **`Cargo.toml`** moved a version requirement in the published
  manifest. Record it for the next release's `CHANGELOG.md` entry, which must state whether
  decoded output moved and what was compared. One that edited **only a lockfile** reaches no
  consumer and needs no entry — a library's lockfile is not published.
- Do not bump `Cargo.toml`'s `version`. In this repository that *is* the release action, and
  it is a separate, deliberate one.

## Red flags — stop and report

| Thought | Reality |
| --- | --- |
| "It's a patch bump, the graph can't have changed" | A patch release can add a dependency and can bump a transitive one across a minor. The gate reads the graph; the title does not. |
| "CI is green and wasm32 built, so it's pure" | The `wasm32` lane is evidence, not the check — it exists for another reason and names no package when it goes red. There is no purity lane. Run the script. |
| "The gate wouldn't run, but the diff looks fine" | A gate that did not run is not a gate that passed. Report the blocker instead. |
| "The `-sys` crate is Windows-only" / "it's only a build dependency" | The rule is *every* target the toolchain supports, with *no* host toolchain. `--target all` and the build edges are deliberate. Both still fail. |
| "The automerge workflow held it, so a human already decided" | The hold means nobody has read it yet. Step 6 is the read. |
| "I'll merge the batch, then verify" | The gates grade one head commit. Merging invalidates the rest. |
| "Purity failed but the crate is obviously fine" | Then it is a change to § Dependencies, argued in the design document — not a merge. |

## Report

End with one row per pull request: number, verdict, and the evidence that produced it — gate
exit, graph lines that moved, the release-notes line, CI state. A verdict with no evidence
beside it is the failure this skill exists to prevent.
