# Security policy

## Reporting a vulnerability

Email **security@macourteau.dev**. Please do not open a public issue, pull request or discussion
for a suspected vulnerability.

A useful report names the version or commit, the feature set (`fits`, `xisf`, `checksum`), the
entry point, and what happened against what was expected. Build the reproducer in Rust source,
byte by byte, the way this repository's own tests build their fixtures, rather than attaching a
frame: real FITS and XISF files carry observatory coordinates and timestamps, and a report should
not have to publish them.

## Scope

astroframe treats every byte it is given as untrusted, in every format and through every entry
point. What counts as a vulnerability, which parts of the repository are in scope, and how
severity is rated are set out in the
[threat model](https://github.com/macourteau/astroframe-rs/blob/main/.oss-scanner/threat_model.md).

## Supported versions

Fixes are released on top of the latest published version. A fix that changes decoded output for
an input a previous release decoded is a breaking change under this crate's versioning (see
`CHANGELOG.md`), so below `1.0` it ships as a minor release rather than a patch.
