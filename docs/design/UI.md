# KUPL UI & Rendering Model

Proposal v0.1 — 2026-07-03. Resolves LANGUAGE.md open question #1.
Status: PROPOSAL — the highest-priority open design; visual tools cannot design
their canvases against an undefined rendering model.

---

## Position

**No template language.** UI is expressed in a `render` block that is nothing but
component construction — same syntax, same semantics, same type checker. "Components
all the way down" survives contact with pixels.

**Two layers, cleanly split:**

1. **The view protocol** (language + std, platform-independent): `render` blocks
   produce a typed **view tree**; layout, style, and semantic/accessibility
   attributes are std types on its nodes.
2. **Renderer adapters** (capability providers, per platform): a renderer is just a
   component that `fulfills cap.Ui` and consumes view trees. Adapters are swappable;
   the view protocol is the contract.

## The `render` block

```kupl
component TodoPage {
    intent "Renders the todo list; emits toggles and search input."

    in  todos:  List[Todo]
    out toggle: TodoId
    out search: Str

    render {
        Column(gap: 8.px, pad: 16.px) {
            Header(prop title: "My Todos")
            SearchBox(prop placeholder: "filter…") -> search
            for t in todos {
                Row(key: t.id, gap: 6.px) {
                    Checkbox(checked: t.done) -> toggle(t.id)
                    Text(t.title, style: if t.done { .strikethrough } else { .body })
                }
            }
        }
    }
}
```

Semantics:

- `render` is a **pure projection** of props, state, and `in`-port values into a view
  tree. It declares no `uses`, cannot mutate `state`, cannot `send`/`emit` directly —
  enforced by the effect checker. All behavior stays in handlers.
- The runtime re-evaluates `render` when any input it read changes, and **reconciles**
  the new tree against the old one by key (`key:` where identity matters, positional
  otherwise). Reconciliation granularity is the component instance — child instances
  with unchanged identity keep their state and mailbox.
- `render` **desugars to child instantiation + wiring**. `Checkbox(checked: t.done)
  -> toggle(t.id)` is sugar for constructing a `Checkbox`, binding its `checked`
  prop, and wiring its output port into this component's `toggle` out-port with the
  mapping applied. A visual tool sees ordinary components and ordinary wires.
- Control flow inside `render` is ordinary KUPL (`if`, `for`, `match`) — no parallel
  template dialect to learn or for models to hallucinate.

## Layout & style (std, typed)

- Layout components: `Column`, `Row`, `Stack`, `Grid`, `Scroll`, `Spacer` — a
  flexbox-class constraint model (main/cross axis, `gap`, `pad`, `grow`, `align`).
- `Style` is a typed record (`.body`, `.strikethrough` are std presets; user themes
  are values passed as props/capabilities). **No string-typed CSS anywhere** — style
  typos are compile errors, and models can't emit unparseable styling.
- Units are typed: `8.px`, `1.fr`, `50.pct`.
- Every view node carries optional **semantic attributes** (`role`, `label`,
  `hint`) — the accessibility tree is part of the protocol from day one, mapped by
  each adapter to the platform's a11y API (not bolted on later).

## Renderer architecture

- **Reference renderer: GPU-drawn retained scene graph on wgpu** (Metal / Vulkan /
  DX12 / WebGPU from one codebase). One renderer covers desktop and browser, pixels
  are identical everywhere, and it dogfoods KUPL's own device runtime — the same
  stream abstraction kernels use (TOOLCHAIN §8). Honest note: text shaping is the
  hard 20% (grapheme/bidi/shaping); v1 binds a proven shaping library through the
  system-tier FFI rather than reimplementing it.
- **DOM adapter** (WASM target): optional alternative for web-native feel, form
  controls, and SEO-relevant surfaces. Same view protocol.
- **Native toolkit adapters** (SwiftUI/UIKit, Android Views/Compose): possible
  later for platform-faithful mobile UI; the protocol is deliberately small enough
  to make this feasible. See PLATFORMS.md.
- The window/surface, input events, frame clock, and a11y bridge arrive through
  `cap.Ui` — a headless test adapter renders view trees to data structures, which
  is also how `example` blocks assert on UI and how visual tools diff "what
  would change visually."

## What visual tools get from this

- A WYSIWYG canvas = the live view tree from the running renderer (not a
  re-implementation of rendering — what you see is the actual renderer).
- Canvas manipulation = AST edits inside `render` blocks (move node, set prop,
  wrap in `Row`, add `key:`) — ordinary, formatter-stable source edits.
- Placeholder/sketch components = dashed view nodes; interaction recording = the
  event stream `cap.Ui` already delivers, captured.

## Sequencing

- The **view protocol + headless adapter** need only toolchain Phase 1–2 (they're
  data structures + reconciliation in the interpreter) — spec them with the language.
- The wgpu reference renderer lands with the device runtime (Phase 5); an interim
  DOM/WASM adapter at Phase 4 unblocks visible UI earlier if needed.

## Open questions (this doc's own)

1. Animation model: timeline components vs. transition props on view nodes
   (leaning: `transition` props for the 90% case; animation *components* for
   orchestration — keeps purity of `render`).
2. Theming: theme-as-capability (`requires theme: cap.Theme`) vs. theme-as-prop
   drilling vs. ambient — capability is the KUPL-consistent answer, needs ergonomics
   check.
3. Responsive layout: constraint queries on layout components (`when width < 600.px`)
   vs. separate render arms — needs a worked phone/desktop example.
4. Rich text / inline markup model.
