# KUPL Platform Reach & Packaging

Proposal v0.1 — 2026-07-03.
Status: PROPOSAL — how "platform-independent" becomes true in practice, per target.

**Why this doc exists:** cross-compilation + WASM (TOOLCHAIN §9) covers CPUs, not
*platforms*. A platform is an OS surface (windows, sensors, notifications), a
lifecycle (suspend/resume, background limits), and a delivery mechanism (packages,
stores, signing). Each needs a designed answer or "universal" quietly means
"servers and CLIs."

---

## Principle: the platform is a capability provider

The std library defines **portable capability contracts** — `cap.Ui`, `cap.Fs`,
`cap.Net`, `cap.HttpServer`, `cap.Camera`, `cap.Push`, `cap.Audio`, `cap.Location`,
`cap.Sensors`, `cap.Clipboard`, … Each **target profile** ships a platform runtime
adapter (part of libkrt) implementing the subset that platform honestly supports.

The compiler knows the target profile. If the composition root requires
`cap.Camera` and the target is `server-linux`, that is a **compile error**, not a
runtime surprise:

```
error[K0742]: `cap.Camera` is not provided by target profile `server-linux`
  --> shop/main.kupl:4
  note: provided by: ios, android, macos, windows, web(getUserMedia)
```

This is universality with honesty: one language everywhere, but each program states
its platform demands in its one composition root, and the toolchain verifies them
per target. (It's the same audit story as `cap.unsafe` — grep one file.)

## Lifecycle: platform events are just ports

The `app` component gains platform lifecycle events, delivered like every other
message: `on start`, `on stop`, plus `on suspend` / `on resume` / `on low_memory`
where the profile defines them. Handler model unchanged; a server app simply never
receives `suspend`. No callbacks, no delegates, no second event system.

## Target profiles & packaging (`kupl pkg --target <profile>`)

| Profile | Runtime shape | Packaging output | Notes |
|---|---|---|---|
| `server-{linux,…}` | static native binary (libkrt embedded) | binary / OCI image | Phase 4; first-class from day one |
| `desktop-{macos,windows,linux}` | native binary + wgpu renderer window | .app / .msix / AppImage | signing hooks in toolchain config, not in code |
| `web` | WASM(+WASI) module | static site bundle | DOM or WebGPU renderer adapter (UI.md); also how visual tools preview client-side |
| `ios`, `android` | **libkrt as embedded library inside a generated native shell project** | Xcode / Gradle project → store artifact | the Flutter-proven pattern: thin Swift/Kotlin shell owns lifecycle + store compliance, delegates everything to KUPL. `kupl pkg --target ios` generates/refreshes the shell; it is a build artifact, not code you maintain |
| `embedded-*` | libkrt-min: no GC config? — system-tier only, static dispatch, `#![no_std]`-style runtime subset | firmware image | later; the three-tier design makes it *possible*, profile makes it *honest* |

Mobile is deliberately the shell pattern rather than "native compilation solves it":
app review, entitlements, push registration, and lifecycle quirks live in the shell
where platform tooling expects them; KUPL owns 100% of the application. Store
signing/identity is toolchain configuration (`kupl.toml [target.ios]`), never
language surface.

## Std portability contract

Every std module is annotated with the profiles it supports; `kupl doc` and the
manifest carry it; `kupl context` includes it — so an AI generating code for a
stated target *cannot accidentally* reach for an unavailable capability without a
structured diagnostic pushing back with the fix.

## Sequencing

- Phase 4 (native backend): `server-*`, `desktop-*` (headless), `web` (WASI).
- Phase 5 (device runtime + wgpu renderer): desktop windowed UI, WebGPU web UI.
- Phase 6: `ios` / `android` shell generation.
- Embedded: after self-hosting maturity (Phase 7+); design constraint today is only
  "libkrt must be factorable" — which the C-ABI libkrt split (TOOLCHAIN §9) already
  ensures.
