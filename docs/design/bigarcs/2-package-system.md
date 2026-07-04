> **Progress:** slice 3 landed (it32) — dependency VERSION ASSERTIONS (K0401 on
> an exact-match mismatch) and a `kupl.lock` (via `kupl pkg lock`) recording each
> dep's path/version/content-hash, with `kupl pkg tree` reporting drift. The
> package arc's LOCAL surface is now complete: path deps + namespace isolation +
> versions + lockfile. A hosted REGISTRY (version-only deps fetched over the
> network) is deliberately future — it needs server infrastructure.
>
> Slice 2 landed (it31) — NAMESPACE ISOLATION via load-time name
> mangling (src/resolve.rs): each dependency package's definitions + internal
> references are renamed `pkg$name`, the root stays bare, and cross-package
> access is qualified `dep.name`. Two deps with the same name no longer collide.
> Frontend-only (loader + resolve pass) — engines untouched, invariant intact.
>
> Slice 1 landed (it30) — local **path** dependencies. A new
> zero-dep `src/manifest.rs` reads the `kupl.toml` `[dependencies]` subset, and
> the loader resolves `use <dep>` across packages (its manifest's `entry`), with
> a clear K0400 on a missing path. Namespace isolation (mangling), lockfile +
> versions, and a `kupl pkg` registry are later slices. Backward-compatible:
> bare `.kupl` files with no manifest load exactly as before.

# Big-arc design: Package / dependency system (ecosystem)

**Feasibility:** high · **Risk:** medium · **Estimated effort:** ~5 /loop iterations
_(Produced by a parallel design workflow, 2026-07-04. Grounded in the actual source.)_

## Summary
KUPL already has everything for local packages except two missing pieces: nobody reads kupl.toml (it is write-only from `kupl new`), and the loader resolves `use x` only as a file path relative to the entry dir. The least-invasive design adds a tiny zero-dep manifest reader plus a dependency-aware resolution step in the loader, then solves the flat-namespace collision by making the kupl.toml boundary a package boundary and load-time-mangling cross-package names entirely in the frontend — the four engines keep consuming one identical merged Program, so the interp==KVM invariant is untouched. Registry/versioning layer on top via kupl.lock (FNV hashes from encoding.rs) and a `kupl pkg` fetch over system curl.

## Key files
, , , , , , , 

## Byte-identical / determinism impact
Irrelevant-by-construction, which is the point. All changes live above the engines (manifest.rs, resolve.rs, loader.rs, run.rs, main.rs). The loader still produces one merged `Program` that `check`/`interp`/`vm`/`compile`(.kx)/`cgen` consume identically — none of them gain any package awareness, so the interp==KVM==.kx==native differential regression cannot be perturbed. Programs with no `[dependencies]` load byte-for-byte as today (single anonymous package, no mangling), so all 129 existing tests and every example block stay green. Iter 2's mangling only changes the STRING KEYS in the symbol maps, and it changes them the same way for every engine (they all key off the same merged Program), preserving byte-identity. Determinism is preserved by keeping loader traversal order stable and emitting kupl.lock entries in sorted order.

## Dependencies & ordering
Self-contained arc, independent of the four-engine arcs. Internal ordering is strict: Iter 1 (manifest reader + local dep resolution, flat merge) is the foundation; Iter 2 (package-boundary mangling in a new resolve.rs pass — the real namespacing fix) depends on Iter 1's package-id threading in the loader; Iter 3 (kupl.lock + version assertions, reusing encoding::hash_fnv) depends on Iter 1's manifest reader; Iter 4 (kupl pkg + curl registry fetch, reusing the ai.rs curl pattern) depends on Iters 1-3 and simply populates a local path that the earlier machinery already handles. Cross-arc conflict risk is low and confined to loader.rs — only an arc that also rewrites `use`/import resolution (e.g. a stdlib-module-import arc) would collide; coordinate if such an arc is scheduled concurrently.

## First iteration (shippable slice)
Iteration 1 — "local path dependencies" (smallest shippable, tested slice):

1. New module `src/manifest.rs` (zero-dep, self-contained like `json.rs`): parse the `kupl.toml` subset — `[section]` headers, `key = "string"`, and single-line inline tables `{ path = "...", version = "..." }`. Public API: `Manifest { name, version, entry, deps: Vec<Dep{name, path, version} } and `fn read(path: &Path) -> Result<Manifest, String>`. Include unit tests for a manifest with `[dependencies]`, the bare-string shorthand, and a malformed line.

2. `src/loader.rs`: seed loading from a manifest. When the entry arg has a `kupl.toml` in its dir/ancestors, read it. Change the work queue element to carry an owning package root: resolve a `use P` whose first segment is a declared dependency by reading THAT dependency's `kupl.toml` and enqueueing its `entry` file joined onto the dep's path root; otherwise resolve relative to the current package root exactly as today (`loader.rs:148-153` unchanged for the non-dep case). Keep flat `program.items.extend` merge for now. Keep canonical-path dedup and cycle safety.

3. Keep the flat namespace but improve the collision message: when `check.rs` would emit K0201/K0203 across two different files, the loader/SourceMap already knows each item's owning file — surface "defined in <fileA> and <fileB>" (can be done minimally by leaving check.rs alone and documenting the limitation; the real fix is iter 2). Do NOT attempt mangling in iter 1.

4. Tests (mirroring the existing `loader.rs` tempdir test at `loader.rs:170`): create two project dirs — `math/` (kupl.toml name=math entry=main.kupl, a `pub fun add`) and `app/` (kupl.toml with `[dependencies] math = { path = "../math" }`, `use math`, a `fun main` calling `add(...)`). Assert `loader::load(app/main.kupl)` merges both, `check` is clean, and `interp` runs `main`. Add a negative test: dependency path missing → clear K0400-style diagnostic pointing at the `use` span.

5. No engine files touched; `cargo test` + the all-examples differential regression stay green because programs without `[dependencies]` load byte-identically to today. Commit + push as one `/loop` iteration.

Deliberately deferred to later iterations: name mangling/isolation (iter 2), kupl.lock + version assertions (iter 3), `kupl pkg` + curl registry fetch (iter 4).

## Design
## Grounding: what exists today

- **`kupl.toml` is write-only.** It is produced by `scaffold_project` in `src/main.rs:208` (`[project]` with `name`/`version`/`entry`) and **never read anywhere** — `grep kupl.toml` hits only `main.rs`. There is no TOML parser in the tree.
- **`use` resolution is pure filesystem.** `src/loader.rs:148-153`: each `(use_path, span)` is split on `.` into path segments, joined onto `root` (the entry file's parent dir, `loader.rs:115`), and given a `.kupl` extension. So `use util` → `<entrydir>/util.kupl`, `use lib.math` → `<entrydir>/lib/math.kupl`. There is **no notion of a dependency**; every `use` must live under the entry tree.
- **One flat namespace, hard-merged.** `loader.rs:154` does `program.items.extend(file_program.items)`; all files collapse into a single `Program`. `check.rs::collect` keys every symbol by **bare name** in flat `HashMap`s (`funs`/`types`/`ctors`/`components`/`contracts`, `check.rs:16-24`) and emits `K0201`/`K0203` "defined more than once" (`check.rs:134,196`). `ProgramDb` (`interp.rs:26-49`) and the compile name-map do the same. **Two packages sharing an item name is a hard error today.**
- **`pub`/`is_pub` is not visibility.** It exists on `FunDecl` (`ast.rs:62`) but is consumed only by `effects.rs` (`must_declare`) and `fmt.rs` — it does **not** restrict access. There is no encapsulation; everything merged is globally visible.
- **`module` keyword is a no-op.** `parser.rs:286-292` accepts and discards it ("module identity is derived from the file path").
- **Reusable infra already present:** `encoding.rs::hash_fnv` (FNV-1a 64-bit) + `hex_encode` for lockfile hashing; the curl-subprocess pattern in `ai.rs:301-318` (`Command::new("curl")`, body on stdin) for registry fetch. Zero-dep, both.
- **Qualified access parses predictably:** `math.add(x)` becomes `Call{ callee: Field{ recv: Ident("math"), name:"add" } }` (or `MethodCall`) per `ast.rs:305-317`. A frontend pass can detect `recv == Ident(<known package alias>)` and rewrite it to a plain `Ident`.

This is engine-agnostic: nothing below the loader/frontend changes. All four engines keep receiving one merged `Program`; the only thing that varies is the string keys in the symbol maps, which every engine derives identically.

---

## (a) `kupl.toml` `[dependencies]` format

Extend the existing manifest. Local first, registry-ready later:

```toml
[project]
name = "myapp"
version = "0.1.0"
entry = "main.kupl"

[dependencies]
math   = { path = "../math" }            # local, iter 1
util2  = { path = "vendor/util2" }       # local, relative to THIS manifest
json2  = { version = "1.2.0" }           # registry, iter 4 (later)
mathx  = { path = "../math", version = "0.3.0" }  # local + version assertion, iter 3
```

A bare-string shorthand is accepted: `math = "../math"` ≡ `{ path = "../math" }` (path if it contains `/`, `.`, or exists on disk; else a registry version requirement). The `path` is resolved **relative to the directory of the manifest that declares it**, so dependency trees are relocatable.

**Manifest reader** (new self-contained module `src/manifest.rs`, sibling to `json.rs`/`csv.rs`, zero-dep, ~60 lines): a line-oriented parser for exactly this grammar — `[section]` headers, `key = "string"`, and single-line inline tables `key = { k = "v", k2 = "v2" }`. It is NOT a general TOML parser (that would be scope creep); it parses the subset `kupl.toml` uses. Returns a `Manifest { name, version, entry, deps: Vec<Dep> }` where `Dep { name, path: Option<String>, version: Option<String> }`. This same reader replaces the ad-hoc `format!` in `scaffold_project` for symmetry (optional).

## (b) Loader resolution of `use <dep>`

The loader gains a **package-aware root map**. Today `root` is a single `PathBuf` (`loader.rs:115`). Replace the queue element `(PathBuf, Option<Span>)` with `(PathBuf, PkgId, Option<Span>)` and carry a `packages: Vec<PackageCtx>` where `PackageCtx { name, root: PathBuf, deps: HashMap<String,PkgId> }`.

Resolution of a `use P` inside a file owned by package `K` (`loader.rs:148`):
1. If `P`'s first segment is a **declared dependency** of `K` (in `K.deps`): resolve to that dependency package `D`, load `D`'s `kupl.toml`, and enqueue **D's `entry` file** joined onto `D.root`, tagged with `PkgId(D)`. A dotted tail (`use math.internal`) maps to a subfile under `D.root` the same way relative `use` does today, still tagged `PkgId(D)`.
2. Otherwise (today's behavior, unchanged): resolve `P` as a file path relative to `K.root`, tagged with the **same** `PkgId(K)` — i.e. relative `use` stays intra-package.

Dedup by canonical path is kept (`loader.rs:119-121`). A dependency reachable by two paths loads once. Cycle-safety is unchanged (idempotent per file).

Entry discovery for `kupl run`: when the argument is inside a project (a `kupl.toml` exists in the arg's dir or an ancestor), the loader reads it to seed package `0` = the root package and its `deps`. When run on a bare `.kupl` file with no manifest, behavior is exactly today's (single anonymous package, entry dir = root, no deps) — **full backward compatibility**.

## (c) The namespacing problem — the core decision

**Rule: the `kupl.toml` boundary IS the namespace boundary.** Files reached by relative `use` (same package) keep the flat, hard-merged namespace they have today — so **every existing single-file and multi-file program is unaffected**. Only crossing into a *declared dependency* introduces isolation.

Mechanism — **load-time name mangling, done entirely in the frontend** (a new `src/resolve.rs` pass the loader calls after parsing, before `check`):

- Each package `K` (except the root package) has its every **defined** item renamed `K::name` (separator chosen to be non-lexable, e.g. `::` or `$` — these strings are never re-lexed, they flow straight into the `HashMap<String,_>` keys, so any sentinel works). The **root/entry package is NOT mangled**, so top-level user code keeps bare names.
- **Intra-package references are auto-qualified.** Within package `K`, an unqualified `Ident("add")` / `TyExprKind::Name`/`Generic` / constructor / `fulfills` / child-component / wire target that resolves to a `K`-defined name is rewritten to `K::add`. Names that don't resolve locally (builtins, prelude types like `Json`, root-provided names) are left bare.
- **Cross-package references are explicit and qualified.** In the root package, `use math` binds the alias `math` (or `use math as m`). The resolve pass rewrites `Field{recv:Ident("math"), name:"add"}` (and the `MethodCall` shape) into `Ident("math::add")`, then the enclosing `Call` proceeds normally. An optional selective form `use math (add, sub)` injects unmangled local aliases (`add` → `math::add`) into the root namespace for ergonomics.

After this pass the loader hands `check`/every engine a single merged `Program` whose symbol names are globally unique by construction. **`K0201`/`K0203` collisions between distinct packages become impossible**; a genuine same-package redefinition still errors as today.

**Does it break existing code?** No:
- Single-file program: one anonymous root package, no mangling, no deps → byte-identical to today.
- Multi-file via relative `use`: all one package → flat merge preserved → byte-identical to today.
- Only NEW code that declares `[dependencies]` and calls `dep.item` uses the new path.

**Why mangling and not runtime qualified-name lookup?** Runtime qualified names would force new resolution logic into `interp.rs`, `vm.rs`, `compile.rs`, AND `cgen.rs` — four engines, differential-test surface, exactly what the arc says to avoid. Mangling keeps all of it in the loader/resolve frontend; the engines never learn packages exist.

**Iteration staging of (c):** Iter 1 ships local deps with the **flat merge still in place** plus an improved collision diagnostic (name both owning files/packages). This is the smallest shippable diff and is honest — it works as long as public names don't clash, with the convention "prefix your public names." Iter 2 introduces the mangling pass and lifts the constraint entirely.

## (d) Versioning / lockfile sketch (iter 3)

- `version` on a dependency is an **assertion** first: after resolving `D` locally, compare `D.kupl.toml`'s `version` against the requirement; mismatch → warning (later error under a strict flag). No SemVer solver initially — local paths pin exactly what's on disk.
- **`kupl.lock`** (written next to `kupl.toml`, its own tiny section format read by the same `manifest.rs` reader):
  ```toml
  [[package]]
  name = "math"
  version = "0.3.0"
  source = "path:../math"
  hash = "fnv:9a3f...";   # FNV-1a over the concatenation of the package's source files, via encoding::hash_fnv + hex_encode
  ```
  On `kupl run`/`build`, if a lock exists, the loader recomputes each dependency's hash and **errors on drift** (reproducible builds); `kupl pkg update` rewrites the lock. The hash uses the already-present `encoding.rs` primitives — zero new dependencies. Determinism note: lock entries are emitted in a stable (sorted-by-name) order so the file is reproducible.

## (e) Registry story (iter 4, later — sketched only)

- **`kupl pkg` subcommands** in `main.rs` dispatch: `add <name>[@ver]` (edit `[dependencies]`), `fetch`/`install` (materialize registry deps into a cache), `vendor <name>` (copy a resolved dep into `vendor/` and rewrite the manifest to a path dep — the escape hatch that keeps everything local/offline), `publish` (later).
- **Transport = system curl**, mirroring `ai.rs:301`. A registry dep `json2 = { version = "1.2.0" }` resolves to `GET <registry>/json2/1.2.0/manifest` then the referenced source files, unpacked into a per-user cache `~/.kupl/cache/json2/1.2.0/`. The loader treats a fetched cache dir exactly like a `path` dep — **the registry is just a way to populate a local path**, so all of iters 1-3 are reused unchanged.
- Integrity: the fetched content's FNV hash is written to `kupl.lock`; re-fetch verifies against it. A real content-addressed/signed story is explicitly deferred.
- No tarball dependency (no external archiver): fetch a small JSON file listing member files + a single concatenated blob, or N individual `curl` calls. Decoding reuses `json.rs`.

---

## Per-engine impact

None. Every change is in `manifest.rs` (new), `resolve.rs` (new), `loader.rs`, `main.rs`, and `run.rs` (wiring the manifest into `load`). `check.rs`, `interp.rs`, `vm.rs`, `compile.rs`, `bytecode.rs`, `cgen.rs`, `kx.rs` are untouched — they consume the same merged `Program` they always did. The `interp==KVM==.kx==native` differential regression is therefore unaffected by construction.

## Honest risks / limitations

- The mangling pass (iter 2) must walk **every** AST reference site: `ExprKind::Ident`, `Call`/`MethodCall`/`Field` callees, `TyExprKind::Name`/`Generic`/`Fun`, constructor names, `component` `fulfills`, child `component` names, and `wire` endpoints. `run.rs`/`effects.rs` already contain reusable walkers (`walk_block`, `collect_ty_names`, `collect_expr_names`) to model it on, but it is the fiddly part and the main risk. Missing a site = an unresolved-name error, which is loud and testable, not silent corruption.
- Diagnostics: mangled names must be **de-mangled for display** in `SourceMap::render`/`to_json` (strip `pkg::`) so users never see internal symbols. Small but must not be forgotten.
- No SemVer resolver — local/path-pinned only until a real registry exists. Stated plainly, not hidden.
