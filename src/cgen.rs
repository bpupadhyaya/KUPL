//! Native backend v0: KVM bytecode -> C -> machine code via the system cc.
//!
//! Each chunk becomes a C function (registers are a local array, jumps are
//! gotos); a small embedded runtime provides the value model and builtins,
//! sharing display/operator semantics with the interpreter and KVM (asserted
//! by differential tests). v0 memory model: arena-style — allocations are not
//! freed (fine for batch programs; the per-component GC arrives with KIR).
//! Components are not compiled natively yet — use `kupl bundle` for apps.

use std::fmt::Write as _;

use crate::bytecode::*;
use crate::value::Value;

pub fn emit_c(module: &Module) -> Result<String, String> {
    let Some(&main_idx) = module.funs.get("main") else {
        return Err("`kupl native` needs a `fun main()` (component apps: use `kupl bundle`)".into());
    };
    if !module.ai_funs.is_empty() {
        return Err(format!(
            "`ai fun {}` is not supported by the native backend yet — use `kupl run`, `kupl run --vm`, or `kupl bundle`",
            module.ai_funs[0].name
        ));
    }

    let mut out = String::new();
    out.push_str(RUNTIME);

    // forward declarations
    for (i, c) in module.chunks.iter().enumerate() {
        let _ = writeln!(out, "static KValue fun_{i}(KValue* caps, KValue* args); /* {} */", c.name);
    }
    let _ = writeln!(out, "\nKValue (*CHUNKS[])(KValue*, KValue*) = {{");
    for i in 0..module.chunks.len() {
        let _ = writeln!(out, "    fun_{i},");
    }
    let _ = writeln!(out, "}};\n");

    // ctor metadata tables
    let _ = writeln!(out, "const KCtorMeta CTORS[] = {{");
    for ct in &module.ctors {
        let empty = Vec::new();
        let fields = module.ctor_field_names.get(&ct.variant).unwrap_or(&empty);
        let names: Vec<String> = fields.iter().map(|f| format!("\"{}\"", c_escape(f))).collect();
        let _ = writeln!(
            out,
            "    {{ \"{}\", \"{}\", {}, (const char*[]){{ {} }} }},",
            c_escape(&ct.type_name),
            c_escape(&ct.variant),
            ct.arity,
            if names.is_empty() { "0".to_string() } else { names.join(", ") }
        );
    }
    let _ = writeln!(out, "}};\n#define N_CTORS {}\n", module.ctors.len());

    for (i, chunk) in module.chunks.iter().enumerate() {
        emit_chunk(&mut out, module, i, chunk)?;
    }

    let _ = writeln!(
        out,
        "\nint main(void) {{\n    fun_{main_idx}(0, 0);\n    return 0;\n}}"
    );
    Ok(out)
}

fn emit_chunk(out: &mut String, module: &Module, idx: usize, chunk: &Chunk) -> Result<(), String> {
    let _ = writeln!(out, "/* {} */", chunk.name);
    let _ = writeln!(out, "static KValue fun_{idx}(KValue* caps, KValue* args) {{");
    let nregs = chunk.nregs.max(1);
    let _ = writeln!(out, "    KValue regs[{nregs}];");
    let _ = writeln!(out, "    for (int i = 0; i < {nregs}; i++) regs[i] = k_unit();");
    if chunk.ncaps > 0 {
        let _ = writeln!(out, "    for (int i = 0; i < {}; i++) regs[i] = caps[i];", chunk.ncaps);
    }
    if chunk.nparams > 0 {
        let _ = writeln!(
            out,
            "    for (int i = 0; i < {}; i++) regs[{} + i] = args[i];",
            chunk.nparams, chunk.ncaps
        );
    }
    let _ = writeln!(out, "    (void)caps; (void)args;");

    for (pc, op) in chunk.code.iter().enumerate() {
        let _ = write!(out, "L{pc}: ");
        emit_op(out, module, chunk, op)?;
    }
    // safety net: falling off the end returns unit
    let _ = writeln!(out, "L{}: return k_unit();", chunk.code.len());
    let _ = writeln!(out, "}}\n");
    Ok(())
}

fn const_expr(v: &Value, module: &Module) -> Result<String, String> {
    Ok(match v {
        Value::Int(x) => format!("k_int({x}LL)"),
        Value::Float(x) => {
            if x.fract() == 0.0 && x.is_finite() {
                format!("k_float({x:.1})")
            } else {
                format!("k_float({x:?})")
            }
        }
        Value::Bool(x) => format!("k_bool({})", *x as i32),
        Value::Unit => "k_unit()".to_string(),
        Value::Str(s) => format!("k_str(\"{}\")", c_escape(s)),
        Value::Fun(name) => {
            let idx = module
                .funs
                .get(name.as_str())
                .ok_or_else(|| format!("unknown function `{name}` in constant"))?;
            format!("k_fun({idx})")
        }
        other => return Err(format!("non-serializable constant {other}")),
    })
}

fn str_const<'a>(chunk: &'a Chunk, idx: u16) -> Result<&'a str, String> {
    match &chunk.consts[idx as usize] {
        Value::Str(s) => Ok(s.as_str()),
        _ => Err("expected string constant".into()),
    }
}

fn emit_op(out: &mut String, module: &Module, chunk: &Chunk, op: &Op) -> Result<(), String> {
    use Op::*;
    let line = match op {
        Const(d, idx) => format!("regs[{d}] = {};", const_expr(&chunk.consts[*idx as usize], module)?),
        Move(d, s) => format!("regs[{d}] = regs[{s}];"),
        Add(d, a, b) => format!("regs[{d}] = k_add(regs[{a}], regs[{b}]);"),
        Sub(d, a, b) => format!("regs[{d}] = k_sub(regs[{a}], regs[{b}]);"),
        Mul(d, a, b) => format!("regs[{d}] = k_mul(regs[{a}], regs[{b}]);"),
        Div(d, a, b) => format!("regs[{d}] = k_div(regs[{a}], regs[{b}]);"),
        Rem(d, a, b) => format!("regs[{d}] = k_rem(regs[{a}], regs[{b}]);"),
        Eq(d, a, b) => format!("regs[{d}] = k_bool(k_eq(regs[{a}], regs[{b}]));"),
        Ne(d, a, b) => format!("regs[{d}] = k_bool(!k_eq(regs[{a}], regs[{b}]));"),
        Lt(d, a, b) => format!("regs[{d}] = k_cmp(regs[{a}], regs[{b}], 0);"),
        Le(d, a, b) => format!("regs[{d}] = k_cmp(regs[{a}], regs[{b}], 1);"),
        Gt(d, a, b) => format!("regs[{d}] = k_cmp(regs[{a}], regs[{b}], 2);"),
        Ge(d, a, b) => format!("regs[{d}] = k_cmp(regs[{a}], regs[{b}], 3);"),
        Neg(d, a) => format!("regs[{d}] = k_neg(regs[{a}]);"),
        Not(d, a) => format!("regs[{d}] = k_not(regs[{a}]);"),
        Jump(t) => format!("goto L{t};"),
        JumpIfFalse(r, t) => format!("if (!k_truthy(regs[{r}])) goto L{t};"),
        JumpIfTrue(r, t) => format!("if (k_truthy(regs[{r}])) goto L{t};"),
        Call { dst, fun, start, argc } => {
            format!("regs[{dst}] = fun_{fun}(0, &regs[{start}]); (void){argc};")
        }
        CallBuiltin { dst, which, start, argc } => match *which {
            BUILTIN_PRINT => format!("k_print(regs[{start}]); regs[{dst}] = k_unit(); (void){argc};"),
            BUILTIN_TO_STR => format!("regs[{dst}] = k_to_str(regs[{start}]); (void){argc};"),
            BUILTIN_PANIC => format!("k_panic_v(regs[{start}]); (void){argc}; (void){dst};"),
            BUILTIN_MAP_NEW => format!("regs[{dst}] = k_map_new(); (void){start}; (void){argc};"),
            BUILTIN_SET_NEW => format!("regs[{dst}] = k_set_new(); (void){start}; (void){argc};"),
            BUILTIN_SET_FROM => format!("regs[{dst}] = k_set_from(regs[{start}]); (void){argc};"),
            BUILTIN_TENSOR => format!("regs[{dst}] = k_bt_tensor(regs[{start}]); (void){argc};"),
            BUILTIN_ZEROS => format!("regs[{dst}] = k_bt_zeros(regs[{start}]); (void){argc};"),
            BUILTIN_ARANGE => format!("regs[{dst}] = k_bt_arange(regs[{start}]); (void){argc};"),
            _ => return Err("unknown builtin".into()),
        },
        CallValue { dst, f, start, argc } => {
            format!("regs[{dst}] = k_call(regs[{f}], &regs[{start}], {argc});")
        }
        Method { dst, recv, name, start, argc } => {
            let m = str_const(chunk, *name)?;
            format!(
                "regs[{dst}] = k_method(regs[{recv}], \"{}\", &regs[{start}], {argc});",
                c_escape(m)
            )
        }
        Ret(r) => format!("return regs[{r}];"),
        MakeList { dst, start, len } => format!("regs[{dst}] = k_list(&regs[{start}], {len});"),
        MakeCtor { dst, ctor, start, len } => {
            format!("regs[{dst}] = k_ctor({ctor}, &regs[{start}], {len});")
        }
        GetField { dst, obj, idx } => format!("regs[{dst}] = k_field(regs[{obj}], {idx});"),
        GetFieldNamed { dst, obj, name } => {
            let f = str_const(chunk, *name)?;
            format!("regs[{dst}] = k_field_named(regs[{obj}], \"{}\");", c_escape(f))
        }
        WithField { dst, obj, name, value } => {
            let f = str_const(chunk, *name)?;
            format!(
                "regs[{dst}] = k_with_field(regs[{obj}], \"{}\", regs[{value}]);",
                c_escape(f)
            )
        }
        TagIs { dst, obj, ctor } => format!("regs[{dst}] = k_bool(k_tag_is(regs[{obj}], {ctor}));"),
        MakeClosure { dst, proto, start, ncaps } => {
            format!("regs[{dst}] = k_closure({proto}, &regs[{start}], {ncaps});")
        }
        MakeRange { dst, lo, hi, inclusive } => {
            format!("regs[{dst}] = k_range(regs[{lo}], regs[{hi}], {});", *inclusive as i32)
        }
        IterLen(d, x) => format!("regs[{d}] = k_iter_len(regs[{x}]);"),
        IterGet { dst, iter, idx } => {
            format!("regs[{dst}] = k_iter_get(regs[{iter}], regs[{idx}]);")
        }
        ToStr(d, s) => format!("regs[{d}] = k_to_str(regs[{s}]);"),
        Concat(d, a, b) => format!("regs[{d}] = k_concat(regs[{a}], regs[{b}]);"),
        StateGet(..) | StateSet(..) | MakeInstance { .. } | WireOp { .. } | EmitOp { .. }
        | CallComp { .. } => {
            "k_panic(\"components are not supported by the native backend v0 (use kupl bundle)\");"
                .to_string()
        }
        // emit_c rejects modules with ai funs before reaching here
        CallAi { .. } => {
            "k_panic(\"ai funs are not supported by the native backend yet\");".to_string()
        }
        Panic(idx) => {
            let m = str_const(chunk, *idx)?;
            format!("k_panic(\"{}\");", c_escape(m))
        }
    };
    let _ = writeln!(out, "{line}");
    Ok(())
}

fn c_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// The embedded C runtime. Semantics mirror value.rs / interp.rs exactly —
/// the differential test compares native stdout against the interpreter.
const RUNTIME: &str = r#"/* KUPL native runtime v0 (generated — do not edit) */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <math.h>

typedef struct KValue KValue;
typedef struct { int64_t len; KValue* items; } KList;
typedef struct { int32_t ctor; KValue* fields; int32_t nfields; } KCtor;
typedef struct { int32_t proto; int32_t ncaps; KValue* caps; } KClosure;
typedef struct { int64_t len; double* data; } KTensor;
typedef struct { int64_t len; KValue* keys; KValue* vals; } KMap;
typedef struct { int64_t len; KValue* items; } KSet;
typedef struct { const char* type_name; const char* variant; int arity; const char** fields; } KCtorMeta;

struct KValue {
    enum { K_INT, K_FLOAT, K_BOOL, K_UNIT, K_STR, K_LIST, K_CTOR, K_CLOSURE, K_FUN, K_RANGE, K_TENSOR, K_MAP, K_SET } tag;
    union {
        int64_t i; double f; int b;
        const char* s;
        KList* list; KCtor* ctor; KClosure* clo; KTensor* ten; KMap* map; KSet* set;
        int32_t fun;
        struct { int64_t lo, hi; int incl; } range;
    } as;
};

static void k_panic(const char* msg) {
    fprintf(stderr, "panic: %s\n", msg);
    exit(101);
}

static void* k_alloc(size_t n) {
    void* p = malloc(n < 1 ? 1 : n);
    if (!p) k_panic("out of memory");
    return p;
}

static KValue k_int(int64_t v)   { KValue x; x.tag = K_INT;   x.as.i = v; return x; }
static KValue k_float(double v)  { KValue x; x.tag = K_FLOAT; x.as.f = v; return x; }
static KValue k_bool(int v)      { KValue x; x.tag = K_BOOL;  x.as.b = !!v; return x; }
static KValue k_unit(void)       { KValue x; x.tag = K_UNIT;  x.as.i = 0; return x; }
static KValue k_str(const char* s) { KValue x; x.tag = K_STR; x.as.s = s; return x; }
static KValue k_fun(int32_t idx) { KValue x; x.tag = K_FUN;   x.as.fun = idx; return x; }

static KValue k_range(KValue lo, KValue hi, int incl) {
    if (lo.tag != K_INT || hi.tag != K_INT) k_panic("range bounds must be Int");
    KValue x; x.tag = K_RANGE; x.as.range.lo = lo.as.i; x.as.range.hi = hi.as.i; x.as.range.incl = incl;
    return x;
}

static KValue k_list(KValue* items, int n) {
    KList* l = k_alloc(sizeof(KList));
    l->len = n;
    l->items = k_alloc(sizeof(KValue) * (n < 1 ? 1 : n));
    memcpy(l->items, items, sizeof(KValue) * n);
    KValue x; x.tag = K_LIST; x.as.list = l; return x;
}

extern const KCtorMeta CTORS[];

static KValue k_ctor(int idx, KValue* fields, int n) {
    KCtor* c = k_alloc(sizeof(KCtor));
    c->ctor = idx; c->nfields = n;
    c->fields = k_alloc(sizeof(KValue) * (n < 1 ? 1 : n));
    memcpy(c->fields, fields, sizeof(KValue) * n);
    KValue x; x.tag = K_CTOR; x.as.ctor = c; return x;
}

static KValue k_closure(int proto, KValue* caps, int n) {
    KClosure* c = k_alloc(sizeof(KClosure));
    c->proto = proto; c->ncaps = n;
    c->caps = k_alloc(sizeof(KValue) * (n < 1 ? 1 : n));
    memcpy(c->caps, caps, sizeof(KValue) * n);
    KValue x; x.tag = K_CLOSURE; x.as.clo = c; return x;
}

static KValue k_tensor_new(double* data, int64_t n) {
    KTensor* t = k_alloc(sizeof(KTensor));
    t->len = n; t->data = data;
    KValue x; x.tag = K_TENSOR; x.as.ten = t; return x;
}

static int k_eq(KValue a, KValue b);

static KValue k_map_new(void) {
    KMap* m = k_alloc(sizeof(KMap));
    m->len = 0; m->keys = 0; m->vals = 0;
    KValue x; x.tag = K_MAP; x.as.map = m; return x;
}
static KValue k_map_make(KValue* keys, KValue* vals, int64_t n) {
    KMap* m = k_alloc(sizeof(KMap));
    m->len = n;
    m->keys = k_alloc(sizeof(KValue) * (n < 1 ? 1 : n));
    m->vals = k_alloc(sizeof(KValue) * (n < 1 ? 1 : n));
    memcpy(m->keys, keys, sizeof(KValue) * n);
    memcpy(m->vals, vals, sizeof(KValue) * n);
    KValue x; x.tag = K_MAP; x.as.map = m; return x;
}
static KValue k_set_new(void) {
    KSet* s = k_alloc(sizeof(KSet));
    s->len = 0; s->items = 0;
    KValue x; x.tag = K_SET; x.as.set = s; return x;
}
static KValue k_set_make(KValue* items, int64_t n) {
    KSet* s = k_alloc(sizeof(KSet));
    s->len = n;
    s->items = k_alloc(sizeof(KValue) * (n < 1 ? 1 : n));
    memcpy(s->items, items, sizeof(KValue) * n);
    KValue x; x.tag = K_SET; x.as.set = s; return x;
}
static KValue k_set_from(KValue v) {
    if (v.tag != K_LIST) k_panic("Set(...) needs a List");
    KList* l = v.as.list;
    KValue* out = k_alloc(sizeof(KValue) * (l->len < 1 ? 1 : l->len));
    int64_t n = 0;
    for (int64_t i = 0; i < l->len; i++) {
        int dup = 0;
        for (int64_t j = 0; j < n; j++)
            if (k_eq(out[j], l->items[i])) { dup = 1; break; }
        if (!dup) out[n++] = l->items[i];
    }
    return k_set_make(out, n);
}

static KValue k_bt_tensor(KValue v) {
    if (v.tag != K_LIST) k_panic("tensor() needs a List[Float]");
    KList* l = v.as.list;
    double* d = k_alloc(sizeof(double) * (l->len < 1 ? 1 : l->len));
    for (int64_t i = 0; i < l->len; i++) {
        KValue it = l->items[i];
        if (it.tag == K_FLOAT) d[i] = it.as.f;
        else if (it.tag == K_INT) d[i] = (double)it.as.i;
        else k_panic("tensor() needs Float elements");
    }
    return k_tensor_new(d, l->len);
}
static KValue k_bt_zeros(KValue v) {
    if (v.tag != K_INT || v.as.i < 0) k_panic("zeros() needs a non-negative size");
    double* d = k_alloc(sizeof(double) * (v.as.i < 1 ? 1 : v.as.i));
    for (int64_t i = 0; i < v.as.i; i++) d[i] = 0.0;
    return k_tensor_new(d, v.as.i);
}
static KValue k_bt_arange(KValue v) {
    if (v.tag != K_INT || v.as.i < 0) k_panic("arange() needs a non-negative size");
    double* d = k_alloc(sizeof(double) * (v.as.i < 1 ? 1 : v.as.i));
    for (int64_t i = 0; i < v.as.i; i++) d[i] = (double)i;
    return k_tensor_new(d, v.as.i);
}

static int k_truthy(KValue v) {
    if (v.tag != K_BOOL) k_panic("condition must be Bool");
    return v.as.b;
}

/* ---- display (mirrors value.rs) ---- */

typedef struct { char* buf; size_t len, cap; } KBuf;
static void kb_grow(KBuf* b, size_t need) {
    if (b->len + need + 1 > b->cap) {
        b->cap = (b->cap ? b->cap * 2 : 64) + need;
        b->buf = realloc(b->buf, b->cap);
        if (!b->buf) k_panic("out of memory");
    }
}
static void kb_puts(KBuf* b, const char* s) {
    size_t n = strlen(s);
    kb_grow(b, n);
    memcpy(b->buf + b->len, s, n);
    b->len += n; b->buf[b->len] = 0;
}

static void k_fmt_float(KBuf* b, double f) {
    char tmp[64];
    if (isfinite(f) && f == floor(f)) {
        snprintf(tmp, sizeof tmp, "%.1f", f);
    } else {
        /* shortest representation that round-trips (matches Rust Display) */
        for (int prec = 1; prec <= 17; prec++) {
            snprintf(tmp, sizeof tmp, "%.*g", prec, f);
            if (strtod(tmp, 0) == f) break;
        }
    }
    kb_puts(b, tmp);
}

static void k_display(KBuf* b, KValue v, int quote_str);
static void k_display_inner(KBuf* b, KValue v) { k_display(b, v, 1); }

static void k_display(KBuf* b, KValue v, int quote_str) {
    char tmp[64];
    switch (v.tag) {
        case K_INT: snprintf(tmp, sizeof tmp, "%lld", (long long)v.as.i); kb_puts(b, tmp); break;
        case K_FLOAT: k_fmt_float(b, v.as.f); break;
        case K_BOOL: kb_puts(b, v.as.b ? "true" : "false"); break;
        case K_UNIT: kb_puts(b, "()"); break;
        case K_STR:
            if (quote_str) { kb_puts(b, "\""); kb_puts(b, v.as.s); kb_puts(b, "\""); }
            else kb_puts(b, v.as.s);
            break;
        case K_LIST: {
            kb_puts(b, "[");
            for (int64_t i = 0; i < v.as.list->len; i++) {
                if (i) kb_puts(b, ", ");
                k_display_inner(b, v.as.list->items[i]);
            }
            kb_puts(b, "]");
            break;
        }
        case K_CTOR: {
            kb_puts(b, CTORS[v.as.ctor->ctor].variant);
            if (v.as.ctor->nfields > 0) {
                kb_puts(b, "(");
                for (int i = 0; i < v.as.ctor->nfields; i++) {
                    if (i) kb_puts(b, ", ");
                    k_display_inner(b, v.as.ctor->fields[i]);
                }
                kb_puts(b, ")");
            }
            break;
        }
        case K_CLOSURE: kb_puts(b, "<fn>"); break;
        case K_FUN: kb_puts(b, "<fn>"); break;
        case K_RANGE:
            snprintf(tmp, sizeof tmp, "%lld..%s%lld", (long long)v.as.range.lo,
                     v.as.range.incl ? "=" : "", (long long)v.as.range.hi);
            kb_puts(b, tmp);
            break;
        case K_MAP: {
            kb_puts(b, "Map{");
            for (int64_t i = 0; i < v.as.map->len; i++) {
                if (i) kb_puts(b, ", ");
                k_display_inner(b, v.as.map->keys[i]);
                kb_puts(b, ": ");
                k_display_inner(b, v.as.map->vals[i]);
            }
            kb_puts(b, "}");
            break;
        }
        case K_SET: {
            kb_puts(b, "Set{");
            for (int64_t i = 0; i < v.as.set->len; i++) {
                if (i) kb_puts(b, ", ");
                k_display_inner(b, v.as.set->items[i]);
            }
            kb_puts(b, "}");
            break;
        }
        case K_TENSOR: {
            kb_puts(b, "Tensor([");
            for (int64_t i = 0; i < v.as.ten->len; i++) {
                if (i) kb_puts(b, ", ");
                k_fmt_float(b, v.as.ten->data[i]);
            }
            kb_puts(b, "])");
            break;
        }
    }
}

static const char* k_show(KValue v) {
    KBuf b = {0};
    k_display(&b, v, 0);
    return b.buf ? b.buf : "";
}

static void k_print(KValue v) { printf("%s\n", k_show(v)); }
static KValue k_to_str(KValue v) { return k_str(k_show(v)); }
static void k_panic_v(KValue v) { k_panic(k_show(v)); }

static KValue k_concat(KValue a, KValue b) {
    const char* x = k_show(a); const char* y = k_show(b);
    char* out = k_alloc(strlen(x) + strlen(y) + 1);
    strcpy(out, x); strcat(out, y);
    return k_str(out);
}

/* ---- operators (mirror interp raw_binary_op) ---- */

static int k_eq(KValue a, KValue b) {
    if (a.tag == K_MAP && b.tag == K_MAP) {
        if (a.as.map->len != b.as.map->len) return 0;
        for (int64_t i = 0; i < a.as.map->len; i++) {
            int found = 0;
            for (int64_t j = 0; j < b.as.map->len; j++)
                if (k_eq(a.as.map->keys[i], b.as.map->keys[j])
                    && k_eq(a.as.map->vals[i], b.as.map->vals[j])) { found = 1; break; }
            if (!found) return 0;
        }
        return 1;
    }
    if (a.tag == K_SET && b.tag == K_SET) {
        if (a.as.set->len != b.as.set->len) return 0;
        for (int64_t i = 0; i < a.as.set->len; i++) {
            int found = 0;
            for (int64_t j = 0; j < b.as.set->len; j++)
                if (k_eq(a.as.set->items[i], b.as.set->items[j])) { found = 1; break; }
            if (!found) return 0;
        }
        return 1;
    }
    if (a.tag != b.tag) return 0;
    switch (a.tag) {
        case K_INT: return a.as.i == b.as.i;
        case K_FLOAT: return a.as.f == b.as.f;
        case K_BOOL: return a.as.b == b.as.b;
        case K_UNIT: return 1;
        case K_STR: return strcmp(a.as.s, b.as.s) == 0;
        case K_LIST:
            if (a.as.list->len != b.as.list->len) return 0;
            for (int64_t i = 0; i < a.as.list->len; i++)
                if (!k_eq(a.as.list->items[i], b.as.list->items[i])) return 0;
            return 1;
        case K_CTOR: {
            if (strcmp(CTORS[a.as.ctor->ctor].variant, CTORS[b.as.ctor->ctor].variant)) return 0;
            if (a.as.ctor->nfields != b.as.ctor->nfields) return 0;
            for (int i = 0; i < a.as.ctor->nfields; i++)
                if (!k_eq(a.as.ctor->fields[i], b.as.ctor->fields[i])) return 0;
            return 1;
        }
        case K_RANGE:
            return a.as.range.lo == b.as.range.lo && a.as.range.hi == b.as.range.hi
                && a.as.range.incl == b.as.range.incl;
        case K_TENSOR:
            if (a.as.ten->len != b.as.ten->len) return 0;
            for (int64_t i = 0; i < a.as.ten->len; i++)
                if (a.as.ten->data[i] != b.as.ten->data[i]) return 0;
            return 1;
        default: return 0;
    }
}

static KValue k_tensor_binop(KValue a, KValue b, int op) { /* 0:+ 1:- 2:* 3:/ */
    KTensor *x = a.as.ten, *y = b.as.ten;
    if (x->len != y->len) k_panic("tensor length mismatch");
    double* d = k_alloc(sizeof(double) * (x->len < 1 ? 1 : x->len));
    for (int64_t i = 0; i < x->len; i++) {
        switch (op) {
            case 0: d[i] = x->data[i] + y->data[i]; break;
            case 1: d[i] = x->data[i] - y->data[i]; break;
            case 2: d[i] = x->data[i] * y->data[i]; break;
            default: d[i] = x->data[i] / y->data[i]; break;
        }
    }
    return k_tensor_new(d, x->len);
}

static KValue k_add(KValue a, KValue b) {
    if (a.tag == K_INT && b.tag == K_INT) {
        int64_t r;
        if (__builtin_add_overflow(a.as.i, b.as.i, &r)) k_panic("integer overflow in addition");
        return k_int(r);
    }
    if (a.tag == K_FLOAT && b.tag == K_FLOAT) return k_float(a.as.f + b.as.f);
    if (a.tag == K_STR && b.tag == K_STR) return k_concat(a, b);
    if (a.tag == K_TENSOR && b.tag == K_TENSOR) return k_tensor_binop(a, b, 0);
    k_panic("invalid operand types"); return k_unit();
}
static KValue k_sub(KValue a, KValue b) {
    if (a.tag == K_INT && b.tag == K_INT) {
        int64_t r;
        if (__builtin_sub_overflow(a.as.i, b.as.i, &r)) k_panic("integer overflow in subtraction");
        return k_int(r);
    }
    if (a.tag == K_FLOAT && b.tag == K_FLOAT) return k_float(a.as.f - b.as.f);
    if (a.tag == K_TENSOR && b.tag == K_TENSOR) return k_tensor_binop(a, b, 1);
    k_panic("invalid operand types"); return k_unit();
}
static KValue k_mul(KValue a, KValue b) {
    if (a.tag == K_INT && b.tag == K_INT) {
        int64_t r;
        if (__builtin_mul_overflow(a.as.i, b.as.i, &r)) k_panic("integer overflow in multiplication");
        return k_int(r);
    }
    if (a.tag == K_FLOAT && b.tag == K_FLOAT) return k_float(a.as.f * b.as.f);
    if (a.tag == K_TENSOR && b.tag == K_TENSOR) return k_tensor_binop(a, b, 2);
    k_panic("invalid operand types"); return k_unit();
}
static KValue k_div(KValue a, KValue b) {
    if (a.tag == K_INT && b.tag == K_INT) {
        if (b.as.i == 0) k_panic("division by zero");
        if (a.as.i == INT64_MIN && b.as.i == -1) k_panic("integer overflow in division");
        return k_int(a.as.i / b.as.i);
    }
    if (a.tag == K_FLOAT && b.tag == K_FLOAT) return k_float(a.as.f / b.as.f);
    if (a.tag == K_TENSOR && b.tag == K_TENSOR) return k_tensor_binop(a, b, 3);
    k_panic("invalid operand types"); return k_unit();
}
static KValue k_rem(KValue a, KValue b) {
    if (a.tag == K_INT && b.tag == K_INT) {
        if (b.as.i == 0) k_panic("remainder by zero");
        return k_int(a.as.i % b.as.i);
    }
    if (a.tag == K_FLOAT && b.tag == K_FLOAT) return k_float(fmod(a.as.f, b.as.f));
    k_panic("invalid operand types"); return k_unit();
}
static KValue k_cmp(KValue a, KValue b, int op) { /* 0:< 1:<= 2:> 3:>= */
    double x, y; int is_str = 0; int c = 0;
    if (a.tag == K_INT && b.tag == K_INT) { x = 0; y = 0; c = (a.as.i < b.as.i) ? -1 : (a.as.i > b.as.i); }
    else if (a.tag == K_FLOAT && b.tag == K_FLOAT) { x = a.as.f; y = b.as.f; c = (x < y) ? -1 : (x > y); }
    else if (a.tag == K_STR && b.tag == K_STR) { is_str = 1; int r = strcmp(a.as.s, b.as.s); c = (r < 0) ? -1 : (r > 0); }
    else { k_panic("invalid operand types"); }
    (void)is_str;
    switch (op) {
        case 0: return k_bool(c < 0);
        case 1: return k_bool(c <= 0);
        case 2: return k_bool(c > 0);
        default: return k_bool(c >= 0);
    }
}
static KValue k_neg(KValue a) {
    if (a.tag == K_INT) {
        if (a.as.i == INT64_MIN) k_panic("integer overflow in negation");
        return k_int(-a.as.i);
    }
    if (a.tag == K_FLOAT) return k_float(-a.as.f);
    k_panic("cannot negate"); return k_unit();
}
static KValue k_not(KValue a) {
    if (a.tag != K_BOOL) k_panic("cannot `!` non-Bool");
    return k_bool(!a.as.b);
}

/* ---- calls, fields, iteration ---- */

extern KValue (*CHUNKS[])(KValue*, KValue*);

static KValue k_call(KValue f, KValue* args, int argc) {
    (void)argc;
    if (f.tag == K_FUN) return CHUNKS[f.as.fun](0, args);
    if (f.tag == K_CLOSURE) return CHUNKS[f.as.clo->proto](f.as.clo->caps, args);
    k_panic("value is not callable"); return k_unit();
}

static KValue k_field(KValue v, int idx) {
    if (v.tag != K_CTOR || idx >= v.as.ctor->nfields) k_panic("field index out of range");
    return v.as.ctor->fields[idx];
}

static KValue k_field_named(KValue v, const char* name) {
    if (v.tag != K_CTOR) k_panic("value has no fields");
    const KCtorMeta* m = &CTORS[v.as.ctor->ctor];
    for (int i = 0; i < m->arity; i++)
        if (strcmp(m->fields[i], name) == 0) return v.as.ctor->fields[i];
    k_panic("no such field"); return k_unit();
}

static KValue k_with_field(KValue v, const char* name, KValue newval) {
    if (v.tag != K_CTOR) k_panic("value has no fields to update");
    const KCtorMeta* m = &CTORS[v.as.ctor->ctor];
    for (int i = 0; i < m->arity; i++) {
        if (strcmp(m->fields[i], name) == 0) {
            KValue* fields = k_alloc(sizeof(KValue) * (v.as.ctor->nfields < 1 ? 1 : v.as.ctor->nfields));
            memcpy(fields, v.as.ctor->fields, sizeof(KValue) * v.as.ctor->nfields);
            fields[i] = newval;
            KCtor* c = k_alloc(sizeof(KCtor));
            c->ctor = v.as.ctor->ctor; c->nfields = v.as.ctor->nfields; c->fields = fields;
            KValue out; out.tag = K_CTOR; out.as.ctor = c; return out;
        }
    }
    k_panic("no such field"); return k_unit();
}

static int k_tag_is(KValue v, int ctor) {
    return v.tag == K_CTOR && strcmp(CTORS[v.as.ctor->ctor].variant, CTORS[ctor].variant) == 0;
}

static KValue k_iter_len(KValue v) {
    if (v.tag == K_RANGE) {
        int64_t hi = v.as.range.incl ? v.as.range.hi + 1 : v.as.range.hi;
        int64_t n = hi - v.as.range.lo;
        return k_int(n > 0 ? n : 0);
    }
    if (v.tag == K_LIST) return k_int(v.as.list->len);
    k_panic("`for` needs a Range or List"); return k_unit();
}

static KValue k_iter_get(KValue v, KValue idx) {
    if (idx.tag != K_INT) k_panic("iterator index must be Int");
    if (v.tag == K_RANGE) return k_int(v.as.range.lo + idx.as.i);
    if (v.tag == K_LIST) {
        if (idx.as.i < 0 || idx.as.i >= v.as.list->len) k_panic("list index out of range");
        return v.as.list->items[idx.as.i];
    }
    k_panic("cannot iterate"); return k_unit();
}

/* ---- builtin methods (mirror interp shared_method) ---- */

static int k_ctor_variant_is(KValue v, const char* name) {
    return v.tag == K_CTOR && strcmp(CTORS[v.as.ctor->ctor].variant, name) == 0;
}
static KValue k_some(KValue v) { return k_ctor(0, &v, 1); }        /* ctor table order: Some, None, Ok, Err */
static KValue k_none(void) { return k_ctor(1, 0, 0); }

static KValue k_method(KValue recv, const char* name, KValue* args, int argc) {
    (void)argc;
    if (recv.tag == K_LIST) {
        KList* l = recv.as.list;
        if (!strcmp(name, "len")) return k_int(l->len);
        if (!strcmp(name, "map")) {
            KValue* out = k_alloc(sizeof(KValue) * (l->len < 1 ? 1 : l->len));
            for (int64_t i = 0; i < l->len; i++) out[i] = k_call(args[0], &l->items[i], 1);
            KValue r = k_list(out, (int)l->len);
            return r;
        }
        if (!strcmp(name, "filter")) {
            KValue* out = k_alloc(sizeof(KValue) * (l->len < 1 ? 1 : l->len));
            int n = 0;
            for (int64_t i = 0; i < l->len; i++)
                if (k_truthy(k_call(args[0], &l->items[i], 1))) out[n++] = l->items[i];
            return k_list(out, n);
        }
        if (!strcmp(name, "find")) {
            for (int64_t i = 0; i < l->len; i++)
                if (k_truthy(k_call(args[0], &l->items[i], 1))) return k_some(l->items[i]);
            return k_none();
        }
        if (!strcmp(name, "sum")) {
            int64_t si = 0; double sf = 0; int isf = 0;
            for (int64_t i = 0; i < l->len; i++) {
                KValue it = l->items[i];
                if (it.tag == K_INT) {
                    if (__builtin_add_overflow(si, it.as.i, &si)) k_panic("integer overflow in sum");
                } else if (it.tag == K_FLOAT) { isf = 1; sf += it.as.f; }
                else k_panic("cannot sum non-numeric");
            }
            return isf ? k_float(sf + (double)si) : k_int(si);
        }
        if (!strcmp(name, "contains")) {
            for (int64_t i = 0; i < l->len; i++)
                if (k_eq(l->items[i], args[0])) return k_bool(1);
            return k_bool(0);
        }
        if (!strcmp(name, "push")) {
            KValue* out = k_alloc(sizeof(KValue) * (l->len + 1));
            memcpy(out, l->items, sizeof(KValue) * l->len);
            out[l->len] = args[0];
            return k_list(out, (int)l->len + 1);
        }
        if (!strcmp(name, "fold")) {
            KValue acc = args[0];
            for (int64_t i = 0; i < l->len; i++) {
                KValue cb[2]; cb[0] = acc; cb[1] = l->items[i];
                acc = k_call(args[1], cb, 2);
            }
            return acc;
        }
        if (!strcmp(name, "any")) {
            for (int64_t i = 0; i < l->len; i++)
                if (k_truthy(k_call(args[0], &l->items[i], 1))) return k_bool(1);
            return k_bool(0);
        }
        if (!strcmp(name, "all")) {
            for (int64_t i = 0; i < l->len; i++)
                if (!k_truthy(k_call(args[0], &l->items[i], 1))) return k_bool(0);
            return k_bool(1);
        }
        if (!strcmp(name, "sort")) {
            KValue* out = k_alloc(sizeof(KValue) * (l->len < 1 ? 1 : l->len));
            memcpy(out, l->items, sizeof(KValue) * l->len);
            /* insertion sort: stable, no globals needed for the comparator */
            for (int64_t i = 1; i < l->len; i++) {
                KValue key = out[i];
                int64_t j = i - 1;
                while (j >= 0) {
                    int gt;
                    if (out[j].tag == K_INT && key.tag == K_INT) gt = out[j].as.i > key.as.i;
                    else if (out[j].tag == K_FLOAT && key.tag == K_FLOAT) gt = out[j].as.f > key.as.f;
                    else if (out[j].tag == K_STR && key.tag == K_STR) gt = strcmp(out[j].as.s, key.as.s) > 0;
                    else { k_panic("`sort` needs Int, Float, or Str elements"); gt = 0; }
                    if (!gt) break;
                    out[j + 1] = out[j];
                    j--;
                }
                out[j + 1] = key;
            }
            return k_list(out, (int)l->len);
        }
        if (!strcmp(name, "take") || !strcmp(name, "drop")) {
            if (args[0].tag != K_INT) k_panic("`take`/`drop` needs an Int");
            int64_t n = args[0].as.i;
            if (n < 0) n = 0;
            if (n > l->len) n = l->len;
            if (name[0] == 't') return k_list(l->items, (int)n);
            return k_list(l->items + n, (int)(l->len - n));
        }
        if (!strcmp(name, "get")) {
            if (args[0].tag != K_INT) k_panic("`get` needs an Int");
            int64_t i = args[0].as.i;
            return (i >= 0 && i < l->len) ? k_some(l->items[i]) : k_none();
        }
        if (!strcmp(name, "index_of")) {
            for (int64_t i = 0; i < l->len; i++)
                if (k_eq(l->items[i], args[0])) return k_some(k_int(i));
            return k_none();
        }
        if (!strcmp(name, "first")) return l->len ? k_some(l->items[0]) : k_none();
        if (!strcmp(name, "last")) return l->len ? k_some(l->items[l->len - 1]) : k_none();
        if (!strcmp(name, "reverse")) {
            KValue* out = k_alloc(sizeof(KValue) * (l->len < 1 ? 1 : l->len));
            for (int64_t i = 0; i < l->len; i++) out[i] = l->items[l->len - 1 - i];
            return k_list(out, (int)l->len);
        }
        if (!strcmp(name, "join")) {
            KBuf b = {0};
            for (int64_t i = 0; i < l->len; i++) {
                if (i) kb_puts(&b, k_show(args[0]));
                kb_puts(&b, k_show(l->items[i]));
            }
            return k_str(b.buf ? b.buf : "");
        }
    }
    if (recv.tag == K_STR) {
        const char* s = recv.as.s;
        if (!strcmp(name, "len")) {
            int64_t n = 0;
            for (const char* p = s; *p; p++) if ((*p & 0xC0) != 0x80) n++;
            return k_int(n);
        }
        if (!strcmp(name, "contains")) return k_bool(strstr(s, args[0].as.s) != 0);
        if (!strcmp(name, "starts_with")) return k_bool(strncmp(s, args[0].as.s, strlen(args[0].as.s)) == 0);
        if (!strcmp(name, "to_upper") || !strcmp(name, "to_lower")) {
            char* out = k_alloc(strlen(s) + 1);
            int up = name[3] == 'u';
            for (size_t i = 0; s[i]; i++) {
                char c = s[i];
                if (up && c >= 'a' && c <= 'z') c -= 32;
                if (!up && c >= 'A' && c <= 'Z') c += 32;
                out[i] = c;
            }
            out[strlen(s)] = 0;
            return k_str(out);
        }
        if (!strcmp(name, "trim")) {
            const char* a = s;
            while (*a == ' ' || *a == '\t' || *a == '\n' || *a == '\r') a++;
            const char* z = a + strlen(a);
            while (z > a && (z[-1] == ' ' || z[-1] == '\t' || z[-1] == '\n' || z[-1] == '\r')) z--;
            char* out = k_alloc((size_t)(z - a) + 1);
            memcpy(out, a, (size_t)(z - a));
            out[z - a] = 0;
            return k_str(out);
        }
        if (!strcmp(name, "ends_with")) {
            size_t sl = strlen(s), nl = strlen(args[0].as.s);
            return k_bool(nl <= sl && strcmp(s + sl - nl, args[0].as.s) == 0);
        }
        if (!strcmp(name, "replace")) {
            const char* from = args[0].as.s; const char* to = args[1].as.s;
            size_t fl = strlen(from);
            if (fl == 0) return k_str(s);
            KBuf b = {0};
            const char* p = s;
            for (;;) {
                const char* q = strstr(p, from);
                if (!q) { kb_puts(&b, p); break; }
                char* piece = k_alloc((size_t)(q - p) + 1);
                memcpy(piece, p, (size_t)(q - p)); piece[q - p] = 0;
                kb_puts(&b, piece); kb_puts(&b, to);
                p = q + fl;
            }
            return k_str(b.buf ? b.buf : "");
        }
        if (!strcmp(name, "chars")) {
            KValue tmp_items[4096];
            int n = 0;
            const char* p = s;
            while (*p && n < 4096) {
                int len = 1;
                if ((*p & 0xF8) == 0xF0) len = 4;
                else if ((*p & 0xF0) == 0xE0) len = 3;
                else if ((*p & 0xE0) == 0xC0) len = 2;
                char* c = k_alloc((size_t)len + 1);
                memcpy(c, p, (size_t)len); c[len] = 0;
                tmp_items[n++] = k_str(c);
                p += len;
            }
            return k_list(tmp_items, n);
        }
        if (!strcmp(name, "repeat")) {
            if (args[0].tag != K_INT || args[0].as.i < 0) k_panic("`repeat` needs a non-negative Int");
            int64_t n = args[0].as.i;
            size_t sl = strlen(s);
            if (sl * (size_t)n > 100000000) k_panic("`repeat` result too large");
            char* out = k_alloc(sl * (size_t)n + 1);
            out[0] = 0;
            for (int64_t i = 0; i < n; i++) memcpy(out + i * sl, s, sl);
            out[sl * (size_t)n] = 0;
            return k_str(out);
        }
        if (!strcmp(name, "parse_int")) {
            char* end;
            long long v = strtoll(s, &end, 10);
            if (end == s || *end != 0) return k_none();
            return k_some(k_int((int64_t)v));
        }
        if (!strcmp(name, "parse_float")) {
            char* end;
            double v = strtod(s, &end);
            if (end == s || *end != 0) return k_none();
            return k_some(k_float(v));
        }
        if (!strcmp(name, "split")) {
            const char* sep = args[0].as.s;
            size_t seplen = strlen(sep);
            KValue parts[1024];
            int n = 0;
            const char* p = s;
            if (seplen == 0) k_panic("`split` needs a non-empty separator");
            for (;;) {
                const char* q = strstr(p, sep);
                if (!q || n >= 1023) {
                    parts[n++] = k_str(p);
                    break;
                }
                char* piece = k_alloc((size_t)(q - p) + 1);
                memcpy(piece, p, (size_t)(q - p));
                piece[q - p] = 0;
                parts[n++] = k_str(piece);
                p = q + seplen;
            }
            return k_list(parts, n);
        }
    }
    if (recv.tag == K_INT) {
        if (!strcmp(name, "to_str")) return k_to_str(recv);
        if (!strcmp(name, "to_float")) return k_float((double)recv.as.i);
        if (!strcmp(name, "abs")) {
            if (recv.as.i == INT64_MIN) k_panic("integer overflow in abs");
            return k_int(recv.as.i < 0 ? -recv.as.i : recv.as.i);
        }
        if (!strcmp(name, "min")) return k_int(recv.as.i < args[0].as.i ? recv.as.i : args[0].as.i);
        if (!strcmp(name, "max")) return k_int(recv.as.i > args[0].as.i ? recv.as.i : args[0].as.i);
    }
    if (recv.tag == K_FLOAT) {
        if (!strcmp(name, "to_str")) return k_to_str(recv);
        if (!strcmp(name, "to_int")) return k_int((int64_t)recv.as.f);
        if (!strcmp(name, "abs")) return k_float(fabs(recv.as.f));
        if (!strcmp(name, "sqrt")) return k_float(sqrt(recv.as.f));
        if (!strcmp(name, "floor")) return k_float(floor(recv.as.f));
        if (!strcmp(name, "ceil")) return k_float(ceil(recv.as.f));
        if (!strcmp(name, "round")) return k_float(round(recv.as.f));
        if (!strcmp(name, "min")) return k_float(recv.as.f < args[0].as.f ? recv.as.f : args[0].as.f);
        if (!strcmp(name, "max")) return k_float(recv.as.f > args[0].as.f ? recv.as.f : args[0].as.f);
        if (!strcmp(name, "pow")) return k_float(pow(recv.as.f, args[0].as.f));
    }
    if (recv.tag == K_MAP) {
        KMap* m = recv.as.map;
        if (!strcmp(name, "len")) return k_int(m->len);
        if (!strcmp(name, "get")) {
            for (int64_t i = 0; i < m->len; i++)
                if (k_eq(m->keys[i], args[0])) return k_some(m->vals[i]);
            return k_none();
        }
        if (!strcmp(name, "contains_key")) {
            for (int64_t i = 0; i < m->len; i++)
                if (k_eq(m->keys[i], args[0])) return k_bool(1);
            return k_bool(0);
        }
        if (!strcmp(name, "insert")) {
            KValue* ks = k_alloc(sizeof(KValue) * (m->len + 1));
            KValue* vs = k_alloc(sizeof(KValue) * (m->len + 1));
            memcpy(ks, m->keys, sizeof(KValue) * m->len);
            memcpy(vs, m->vals, sizeof(KValue) * m->len);
            for (int64_t i = 0; i < m->len; i++)
                if (k_eq(ks[i], args[0])) { vs[i] = args[1]; return k_map_make(ks, vs, m->len); }
            ks[m->len] = args[0]; vs[m->len] = args[1];
            return k_map_make(ks, vs, m->len + 1);
        }
        if (!strcmp(name, "remove")) {
            KValue* ks = k_alloc(sizeof(KValue) * (m->len < 1 ? 1 : m->len));
            KValue* vs = k_alloc(sizeof(KValue) * (m->len < 1 ? 1 : m->len));
            int64_t n = 0;
            for (int64_t i = 0; i < m->len; i++)
                if (!k_eq(m->keys[i], args[0])) { ks[n] = m->keys[i]; vs[n] = m->vals[i]; n++; }
            return k_map_make(ks, vs, n);
        }
        if (!strcmp(name, "keys")) return k_list(m->keys, (int)m->len);
        if (!strcmp(name, "values")) return k_list(m->vals, (int)m->len);
    }
    if (recv.tag == K_SET) {
        KSet* st = recv.as.set;
        if (!strcmp(name, "len")) return k_int(st->len);
        if (!strcmp(name, "contains")) {
            for (int64_t i = 0; i < st->len; i++)
                if (k_eq(st->items[i], args[0])) return k_bool(1);
            return k_bool(0);
        }
        if (!strcmp(name, "insert")) {
            for (int64_t i = 0; i < st->len; i++)
                if (k_eq(st->items[i], args[0])) return recv;
            KValue* out = k_alloc(sizeof(KValue) * (st->len + 1));
            memcpy(out, st->items, sizeof(KValue) * st->len);
            out[st->len] = args[0];
            return k_set_make(out, st->len + 1);
        }
        if (!strcmp(name, "remove")) {
            KValue* out = k_alloc(sizeof(KValue) * (st->len < 1 ? 1 : st->len));
            int64_t n = 0;
            for (int64_t i = 0; i < st->len; i++)
                if (!k_eq(st->items[i], args[0])) out[n++] = st->items[i];
            return k_set_make(out, n);
        }
        if (!strcmp(name, "union")) {
            KSet* o = args[0].as.set;
            KValue* out = k_alloc(sizeof(KValue) * (st->len + o->len < 1 ? 1 : st->len + o->len));
            memcpy(out, st->items, sizeof(KValue) * st->len);
            int64_t n = st->len;
            for (int64_t i = 0; i < o->len; i++) {
                int dup = 0;
                for (int64_t j = 0; j < n; j++)
                    if (k_eq(out[j], o->items[i])) { dup = 1; break; }
                if (!dup) out[n++] = o->items[i];
            }
            return k_set_make(out, n);
        }
        if (!strcmp(name, "intersect") || !strcmp(name, "difference")) {
            KSet* o = args[0].as.set;
            int want = name[0] == 'i';
            KValue* out = k_alloc(sizeof(KValue) * (st->len < 1 ? 1 : st->len));
            int64_t n = 0;
            for (int64_t i = 0; i < st->len; i++) {
                int found = 0;
                for (int64_t j = 0; j < o->len; j++)
                    if (k_eq(st->items[i], o->items[j])) { found = 1; break; }
                if (found == want) out[n++] = st->items[i];
            }
            return k_set_make(out, n);
        }
        if (!strcmp(name, "to_list")) return k_list(st->items, (int)st->len);
    }
    if (recv.tag == K_TENSOR) {
        KTensor* t = recv.as.ten;
        if (!strcmp(name, "len")) return k_int(t->len);
        if (!strcmp(name, "get")) {
            if (args[0].tag != K_INT || args[0].as.i < 0 || args[0].as.i >= t->len)
                k_panic("tensor index out of range");
            return k_float(t->data[args[0].as.i]);
        }
        if (!strcmp(name, "sum") || !strcmp(name, "mean")) {
            double s = 0;
            for (int64_t i = 0; i < t->len; i++) s += t->data[i];
            if (name[0] == 's') return k_float(s);
            if (t->len == 0) k_panic("mean of an empty tensor");
            return k_float(s / (double)t->len);
        }
        if (!strcmp(name, "max") || !strcmp(name, "min")) {
            if (t->len == 0) k_panic("max/min of an empty tensor");
            double m = t->data[0];
            for (int64_t i = 1; i < t->len; i++) {
                if (name[1] == 'a' ? t->data[i] > m : t->data[i] < m) m = t->data[i];
            }
            return k_float(m);
        }
        if (!strcmp(name, "dot")) {
            if (args[0].tag != K_TENSOR || args[0].as.ten->len != t->len)
                k_panic("dot: length mismatch");
            double s = 0;
            for (int64_t i = 0; i < t->len; i++) s += t->data[i] * args[0].as.ten->data[i];
            return k_float(s);
        }
        if (!strcmp(name, "scale")) {
            if (args[0].tag != K_FLOAT) k_panic("`scale` needs a Float");
            double* d = k_alloc(sizeof(double) * (t->len < 1 ? 1 : t->len));
            for (int64_t i = 0; i < t->len; i++) d[i] = t->data[i] * args[0].as.f;
            return k_tensor_new(d, t->len);
        }
        if (!strcmp(name, "map")) {
            double* d = k_alloc(sizeof(double) * (t->len < 1 ? 1 : t->len));
            for (int64_t i = 0; i < t->len; i++) {
                KValue x = k_float(t->data[i]);
                KValue y = k_call(args[0], &x, 1);
                if (y.tag != K_FLOAT) k_panic("tensor map must return Float");
                d[i] = y.as.f;
            }
            return k_tensor_new(d, t->len);
        }
        if (!strcmp(name, "to_list")) {
            KValue* out = k_alloc(sizeof(KValue) * (t->len < 1 ? 1 : t->len));
            for (int64_t i = 0; i < t->len; i++) out[i] = k_float(t->data[i]);
            return k_list(out, (int)t->len);
        }
    }
    if (recv.tag == K_CTOR) {
        if (!strcmp(name, "is_some")) return k_bool(k_ctor_variant_is(recv, "Some"));
        if (!strcmp(name, "is_none")) return k_bool(k_ctor_variant_is(recv, "None"));
        if (!strcmp(name, "is_ok")) return k_bool(k_ctor_variant_is(recv, "Ok"));
        if (!strcmp(name, "is_err")) return k_bool(k_ctor_variant_is(recv, "Err"));
        if (!strcmp(name, "unwrap_or")) {
            if (k_ctor_variant_is(recv, "Some") || k_ctor_variant_is(recv, "Ok"))
                return recv.as.ctor->nfields ? recv.as.ctor->fields[0] : k_unit();
            return args[0];
        }
    }
    k_panic("no such method");
    return k_unit();
}
"#;
