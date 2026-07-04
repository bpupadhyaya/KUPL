# KUPL Gap Audit & Enrichment Roadmap

Audited 2026-07-04 against: `docs/design/LANGUAGE.md` (incl. §12 open
questions), the `[design]` markers in `docs/reference/LANGUAGE-REFERENCE.md`,
and known limitations called out in commit messages. Checked off as landed.

## Tier 1 — language ergonomics (active)

- [x] **Record update `with`** — `user with age: 36` (design §10 uses it; today K0223)
- [x] **Std lib depth** — List: fold/any/all/sort/take/drop/get/index_of;
      Str: ends_with/replace/chars/repeat/parse_int/parse_float;
      Int: min/max; Float: floor/ceil/round/min/max/pow
- [x] **Component-private functions callable** from handlers/exposes (declared
      but unreachable today)
- [x] **User-code generics** — `fun sort_by[T](xs: List[T], key: fn(T) -> Int)`
      (checker-level instantiation; engines are ready)
- [ ] **Map[K, V] and Set[T]** collections (design §3)

## Tier 2 — component model completion

- [ ] **Contract-typed requires** — `prop repo: TodoRepo` accepting any
      fulfilling component (dynamic dispatch through the contract)
- [ ] **`forall` in laws** — property testing with generated values (design §1)
- [ ] **Timers** — `on every 5s`, `after 2s` (design §4); needs a virtual-time
      story for deterministic tests
- [ ] **Hot-swap state migration** (design open Q4; Builder live-editing hook)

## Tier 3 — hardware & systems tiers (next arc)

- [ ] KIR (typed SSA) + `kernel fun` + `at(gpu)` placement; Metal lowering first
- [ ] Components + per-component GC in the native backend
- [ ] Sized numerics (i8…u64, f32), Byte/Char, BigInt/Decimal
- [ ] System tier: ownership, `low`/`asm` (design §6)
- [ ] Capabilities as attenuable values (`cap.Http.limited_to(…)`)

## Tier 4 — ecosystem

- [ ] Package registry + `kupl pkg publish` with enforced API compat
- [ ] LSP: hover, completion, go-to-definition (diagnostics ship today)
- [ ] `kupl patch` (component-granular edits); conformance suite numbering
- [ ] WASM target; cross-compilation story

## Resolved design open questions (LANGUAGE.md §12)

1. UI trees → `docs/design/UI.md` (render = component construction). **Designed.**
2. Int default → **decided & shipped:** i64 checked, overflow panics.
3. Effect granularity → shipped hierarchical effects (`db` covers `db.read`).
4. Hot-swap state migration → supervision restart hook shipped; migration TBD.
5. Package identity → `kupl.toml` shipped; registry governance TBD.
