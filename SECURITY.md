# Security Policy

## Supported Versions

KUPL is pre-1.0 (`1.0.0-alpha`). Only the latest commit on `main` and the most
recent tagged release are supported with security fixes — there are no
long-term-support branches yet. See [`docs/VERSIONING.md`](docs/VERSIONING.md)
for the versioning policy and the path to `1.0.0` stable.

## Reporting a Vulnerability

Please report security vulnerabilities using GitHub's private vulnerability
reporting: open the **Security** tab on this repository and click **"Report a
vulnerability."** This opens a private channel with the maintainer and avoids
disclosing the issue publicly before a fix is available.

Do not open a public issue for a security vulnerability.

This is a solo-maintained open source project — response times are
best-effort. Expect an initial acknowledgment within about a week.

## Threat Model

**KUPL is not a sandbox.** The effect system (`uses io`, `uses ai`, etc.) is a
*compile-time* discipline for documenting and reasoning about side effects —
it is not a runtime confinement mechanism. A program that declares `uses io`
can perform arbitrary I/O, including spawning subprocesses and network access,
with no syscall filtering, filesystem jail, or capability revocation at
runtime.

**Do not run untrusted KUPL programs as a way to sandbox them.** If you need
to execute untrusted code, run it inside an OS-level sandbox (container, VM,
seccomp profile, cgroup memory/CPU limits) — the same as you would for any
other general-purpose language.

The full threat model, including exactly which resource limits *are* enforced
(recursion depth, tensor size, JSON/expression nesting, regex backtracking
budget, BigInt magnitude, message size caps) and which are not (total memory,
CPU/wall-clock time, file descriptors, output volume, by default), is
documented in
[`docs/PRODUCTION.md` § Threat model](docs/PRODUCTION.md#threat-model--read-this-before-running-untrusted-code).

## Known Non-Guarantees

- **`kupl native`'s generated executable has no sandboxing of its own.** It is
  a standalone C-compiled binary with the full privileges of the user running
  it — the same OS-level sandboxing advice above applies to it as well.
- **Cryptographic primitives exposed by the standard library** (e.g.
  `sha256`/`hmac_sha256`) are implemented from scratch, in-tree, matching this
  project's zero-dependency design. They are covered by known-answer test
  vectors (FIPS 180-4 / RFC 4231) for correctness, but have not undergone an
  independent, formal cryptographic audit. `hash_fnv` remains
  non-cryptographic and is documented as such — do not use it for anything
  security-sensitive.
- **No hosted package registry yet.** There is no live server for
  version-pinned (non-`path`) dependencies, so there is currently no
  third-party supply chain to audit. See `docs/PRODUCTION.md` § Known
  limitations.
