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

/// How a native binary starts: a plain `fun main`, or a single-component `app`.
enum Entry {
    Main(usize),
    App(usize),
}

pub fn emit_c(module: &Module) -> Result<String, String> {
    // ai funs compile via the deterministic mock path (KUPL_AI_MOCK*); a
    // tool-using ai fun defers at runtime (see k_ai_call).
    let entry = if let Some(&main_idx) = module.funs.get("main") {
        Entry::Main(main_idx as usize)
    } else if let Some(app_idx) = module.components.iter().position(|c| c.is_app) {
        // slice 1: single-component apps only — children/wires/emit/timers defer
        check_native_component(module, app_idx)?;
        Entry::App(app_idx)
    } else {
        return Err("`kupl native` needs a `fun main()` or a single-component `app` (multi-component apps: use `kupl bundle`)".into());
    };

    let mut out = String::new();
    out.push_str(RUNTIME);
    out.push_str(COMPONENT_RUNTIME);

    // forward declarations — the depth-guarding wrapper `fun_i` and its body `fun_i_impl`
    for (i, c) in module.chunks.iter().enumerate() {
        let _ = writeln!(out, "static KValue fun_{i}(KValue* caps, KValue* args); /* {} */", c.name);
        let _ = writeln!(out, "static KValue fun_{i}_impl(KValue* caps, KValue* args);");
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
    let _ = writeln!(out, "}};\n#define N_CTORS {}", module.ctors.len());
    // runtime-visible count for k_ctor_by_name (the #define is out of scope in
    // the RUNTIME text, which comes earlier in the output)
    let _ = writeln!(out, "const int K_NCTORS = {};\n", module.ctors.len());

    // component metadata: per-component handler + timer tables, then COMPS[]
    for (ci, c) in module.components.iter().enumerate() {
        if !c.handlers.is_empty() {
            let _ = writeln!(out, "static const KHandler COMP{ci}_H[] = {{");
            for (port, chunk, has_param) in &c.handlers {
                let _ = writeln!(out, "    {{ \"{}\", {}, {} }},", c_escape(port), chunk, *has_param as i32);
            }
            let _ = writeln!(out, "}};");
        }
        if !c.timers.is_empty() {
            let _ = writeln!(out, "static const KTimerMeta COMP{ci}_T[] = {{");
            for t in &c.timers {
                let _ = writeln!(out, "    {{ {}, {}, {}LL }},", t.chunk, t.every as i32, t.interval_ms);
            }
            let _ = writeln!(out, "}};");
        }
        if !c.exposes.is_empty() {
            // deterministic order: sort by expose name (the map's iteration order
            // is not stable, and codegen output must be reproducible)
            let mut exposes: Vec<(&String, &u16)> = c.exposes.iter().collect();
            exposes.sort_by(|a, b| a.0.cmp(b.0));
            let _ = writeln!(out, "static const KExpose COMP{ci}_E[] = {{");
            for (name, chunk) in exposes {
                let _ = writeln!(out, "    {{ \"{}\", {} }},", c_escape(name), chunk);
            }
            let _ = writeln!(out, "}};");
        }
    }
    let _ = writeln!(out, "const KCompMeta COMPS[] = {{");
    for (ci, c) in module.components.iter().enumerate() {
        let handlers = if c.handlers.is_empty() { "0".to_string() } else { format!("COMP{ci}_H") };
        let timers = if c.timers.is_empty() { "0".to_string() } else { format!("COMP{ci}_T") };
        let exposes = if c.exposes.is_empty() { "0".to_string() } else { format!("COMP{ci}_E") };
        let _ = writeln!(
            out,
            "    {{ \"{}\", {}, {}, {}, {}, {}, {}, {}, {}, {}, {} }},",
            c_escape(&c.name),
            c.is_app as i32,
            c.nslots,
            c.init_chunk,
            c.restart_chunk,
            handlers,
            c.handlers.len(),
            timers,
            c.timers.len(),
            exposes,
            c.exposes.len(),
        );
    }
    let _ = writeln!(out, "}};\n#define N_COMPS {}\n", module.components.len());

    // ai-fun metadata: per-function return-type shape trees, then the AI_FUNS
    // table the C mock path reads. Always emit the table (a dummy entry when
    // there are none) so the `extern const KAiFun AI_FUNS[]` symbol resolves.
    let mut ai_ctr = 0usize;
    let shape_addrs: Vec<String> =
        module.ai_funs.iter().map(|f| emit_ai_shape(&mut out, &f.shape, &mut ai_ctr)).collect();
    // per-ai-fun tool tables (name, compiled-fn id, param names + shapes) so the
    // C mock tool loop can convert each round's JSON input and invoke the tool.
    let mut tools_expr: Vec<(String, usize)> = Vec::with_capacity(module.ai_funs.len());
    for (i, f) in module.ai_funs.iter().enumerate() {
        if f.tools.is_empty() {
            tools_expr.push(("0".to_string(), 0));
            continue;
        }
        let mut entries = Vec::with_capacity(f.tools.len());
        for (j, t) in f.tools.iter().enumerate() {
            let pshape_addrs: Vec<String> =
                t.params.iter().map(|(_, s)| emit_ai_shape(&mut out, s, &mut ai_ctr)).collect();
            let pnames: Vec<String> =
                t.params.iter().map(|(n, _)| format!("\"{}\"", c_escape(n))).collect();
            let pn = if pnames.is_empty() { "0".to_string() } else { pnames.join(", ") };
            let ps = if pshape_addrs.is_empty() { "0".to_string() } else { pshape_addrs.join(", ") };
            let _ = writeln!(out, "static const char* const AITOOL_{i}_{j}_PN[] = {{ {pn} }};");
            let _ = writeln!(out, "static const KAiShape* const AITOOL_{i}_{j}_PS[] = {{ {ps} }};");
            let fnid = *module.funs.get(&t.name).unwrap_or(&0);
            entries.push(format!(
                "{{ \"{}\", {}, AITOOL_{i}_{j}_PN, AITOOL_{i}_{j}_PS, {} }}",
                c_escape(&t.name),
                fnid,
                t.params.len()
            ));
        }
        let _ = writeln!(out, "static const KAiTool AITOOLS_{i}[] = {{ {} }};", entries.join(", "));
        tools_expr.push((format!("AITOOLS_{i}"), f.tools.len()));
    }
    // UFCS table: every top-level function, reachable via `x.f(args)` when no
    // built-in method matches. (Built-in methods are checked first in k_method.)
    let mut ufcs: Vec<(&String, u16)> = module.funs.iter().map(|(n, &i)| (n, i)).collect();
    ufcs.sort_by(|a, b| a.0.cmp(b.0));
    let _ = writeln!(out, "const KUfcs UFCS_FUNS[] = {{");
    if ufcs.is_empty() {
        let _ = writeln!(out, "    {{ 0, 0 }}");
    } else {
        for (name, idx) in &ufcs {
            let _ = writeln!(out, "    {{ \"{}\", {} }},", c_escape(name), idx);
        }
    }
    let _ = writeln!(out, "}};\nconst int K_NUFCS = {};\n", ufcs.len());

    let _ = writeln!(out, "const KAiFun AI_FUNS[] = {{");
    if module.ai_funs.is_empty() {
        let _ = writeln!(out, "    {{ 0, 0, 0, 0, 0, 0, 0 }}");
    } else {
        for (i, f) in module.ai_funs.iter().enumerate() {
            let key = format!("KUPL_AI_MOCK_{}", f.name.to_uppercase());
            let _ = writeln!(
                out,
                "    {{ \"{}\", \"{}\", {}, {}, {}, {}, {} }},",
                c_escape(&f.name),
                c_escape(&key),
                shape_addrs[i],
                f.wraps_result as i32,
                (!f.tools.is_empty()) as i32,
                tools_expr[i].0,
                tools_expr[i].1,
            );
        }
    }
    let _ = writeln!(out, "}};\n");

    for (i, chunk) in module.chunks.iter().enumerate() {
        emit_chunk(&mut out, module, i, chunk)?;
    }

    match entry {
        Entry::Main(main_idx) => {
            let _ = writeln!(
                out,
                "\nint main(int argc, char** argv) {{\n    k_argc = argc; k_argv = argv;\n    fun_{main_idx}(0, 0);\n    return 0;\n}}"
            );
        }
        Entry::App(app_idx) => {
            // instantiate the app (which creates children in creation order),
            // run @start for every instance (parents before children), then
            // drain the message queue to quiescence — mirrors vm.rs::run_app.
            let _ = writeln!(
                out,
                "\nint main(int argc, char** argv) {{\n    k_argc = argc; k_argv = argv;\n    \
                 k_print_unwired = 1;\n    \
                 k_instantiate({app_idx}, 0, 0);\n    \
                 for (int id = 0; id < k_ninsts; id++) {{ k_run_lifecycle(id, \"@start\"); k_arm_timers(id); }}\n    \
                 k_drain();\n    \
                 k_run_timers(100);\n    \
                 return 0;\n}}"
            );
        }
    }
    Ok(out)
}

/// Validate an app for the native backend. As of it39 every component construct
/// — state, handlers, children, wires, `emit`, timers, supervision, and
/// cross-component expose calls — compiles, so there is nothing to defer at the
/// component level. (Effectful builtins like `ai fun`/json still defer, but
/// those are handled in `emit_c`/`emit_op`, not here.) Kept as the hook for any
/// future component construct that needs a clear compile-time refusal.
fn check_native_component(_module: &Module, _app_idx: usize) -> Result<(), String> {
    Ok(())
}

/// Emit a `KAiShape` tree for an `ai fun` return type, returning the C address
/// expression (`&AISH_n`). Children are emitted before their parent so all
/// referenced statics are defined first.
fn emit_ai_shape(out: &mut String, shape: &crate::ai::AiShape, ctr: &mut usize) -> String {
    use crate::ai::AiShape;
    let id = *ctr;
    *ctr += 1;
    match shape {
        AiShape::Str => { let _ = writeln!(out, "static const KAiShape AISH_{id} = {{ 0, 0, 0, 0, 0, 0 }};"); }
        AiShape::Int => { let _ = writeln!(out, "static const KAiShape AISH_{id} = {{ 1, 0, 0, 0, 0, 0 }};"); }
        AiShape::Float => { let _ = writeln!(out, "static const KAiShape AISH_{id} = {{ 2, 0, 0, 0, 0, 0 }};"); }
        AiShape::Bool => { let _ = writeln!(out, "static const KAiShape AISH_{id} = {{ 3, 0, 0, 0, 0, 0 }};"); }
        AiShape::List(inner) => {
            let ia = emit_ai_shape(out, inner, ctr);
            let _ = writeln!(out, "static const KAiShape AISH_{id} = {{ 4, {ia}, 0, 0, 0, 0 }};");
        }
        AiShape::Option(inner) => {
            let ia = emit_ai_shape(out, inner, ctr);
            let _ = writeln!(out, "static const KAiShape AISH_{id} = {{ 5, {ia}, 0, 0, 0, 0 }};");
        }
        AiShape::Record { variant, fields, .. } => {
            let field_addrs: Vec<String> =
                fields.iter().map(|(_, s)| emit_ai_shape(out, s, ctr)).collect();
            let names: Vec<String> =
                fields.iter().map(|(n, _)| format!("\"{}\"", c_escape(n))).collect();
            let n_expr = if names.is_empty() { "0".to_string() } else { names.join(", ") };
            let s_expr = if field_addrs.is_empty() { "0".to_string() } else { field_addrs.join(", ") };
            let _ = writeln!(out, "static const char* const AISH_{id}_N[] = {{ {n_expr} }};");
            let _ = writeln!(out, "static const KAiShape* const AISH_{id}_S[] = {{ {s_expr} }};");
            let _ = writeln!(
                out,
                "static const KAiShape AISH_{id} = {{ 6, 0, \"{}\", AISH_{id}_N, AISH_{id}_S, {} }};",
                c_escape(variant),
                fields.len()
            );
        }
    }
    format!("&AISH_{id}")
}

fn emit_chunk(out: &mut String, module: &Module, idx: usize, chunk: &Chunk) -> Result<(), String> {
    let _ = writeln!(out, "/* {} */", chunk.name);
    // Depth-guard wrapper: matches the interpreter/KVM 10 000-frame recursion cap
    // so deep recursion panics cleanly rather than overflowing the C stack. cc -O2
    // inlines the one-line body call, so the overhead is a single inc/dec per call.
    let _ = writeln!(out, "static KValue fun_{idx}(KValue* caps, KValue* args) {{");
    let _ = writeln!(out, "    if (++k_depth > 10000) k_panic(\"stack overflow (10000 frames)\");");
    let _ = writeln!(out, "    KValue r = fun_{idx}_impl(caps, args);");
    let _ = writeln!(out, "    --k_depth;");
    let _ = writeln!(out, "    return r;");
    let _ = writeln!(out, "}}");
    let _ = writeln!(out, "static KValue fun_{idx}_impl(KValue* caps, KValue* args) {{");
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
        Value::SizedInt(b) => {
            // value fits its width (≤64 bits); build the __int128 from an i64/u64
            let (v, w) = (b.0, b.1);
            if w.is_signed() {
                format!("k_sized((__int128)(long long){}LL, {})", v, w.tag())
            } else {
                format!("k_sized((__int128)(unsigned long long){}ULL, {})", v, w.tag())
            }
        }
        Value::F32(v) => {
            // reconstruct from the exact 32-bit pattern (never lossy)
            format!("k_f32_bits({}u)", v.to_bits())
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
            BUILTIN_READ_FILE => format!("regs[{dst}] = k_read_file(regs[{start}]); (void){argc};"),
            BUILTIN_WRITE_FILE => format!("regs[{dst}] = k_write_file(regs[{start}], regs[{start}+1], 0); (void){argc};"),
            BUILTIN_APPEND_FILE => format!("regs[{dst}] = k_write_file(regs[{start}], regs[{start}+1], 1); (void){argc};"),
            BUILTIN_DELETE_FILE => format!("regs[{dst}] = k_delete_file(regs[{start}]); (void){argc};"),
            BUILTIN_FILE_EXISTS => format!("regs[{dst}] = k_file_exists(regs[{start}]); (void){argc};"),
            BUILTIN_JSON_PARSE => format!("regs[{dst}] = k_json_parse(regs[{start}]); (void){argc};"),
            BUILTIN_JSON_STRINGIFY => {
                format!("regs[{dst}] = k_json_stringify(regs[{start}]); (void){argc};")
            }
            BUILTIN_EXEC => format!("regs[{dst}] = k_exec(regs[{start}], regs[{start}+1]); (void){argc};"),
            BUILTIN_PATH_JOIN => format!("regs[{dst}] = k_path_join(regs[{start}], regs[{start}+1]); (void){argc};"),
            BUILTIN_PATH_BASE => format!("regs[{dst}] = k_path_base(regs[{start}]); (void){argc};"),
            BUILTIN_PATH_DIR => format!("regs[{dst}] = k_path_dir(regs[{start}]); (void){argc};"),
            BUILTIN_PATH_EXT => format!("regs[{dst}] = k_path_ext(regs[{start}]); (void){argc};"),
            BUILTIN_LIST_DIR => format!("regs[{dst}] = k_list_dir(regs[{start}]); (void){argc};"),
            BUILTIN_MAKE_DIR => format!("regs[{dst}] = k_make_dir(regs[{start}]); (void){argc};"),
            BUILTIN_REMOVE_DIR => format!("regs[{dst}] = k_remove_dir(regs[{start}]); (void){argc};"),
            BUILTIN_HTTP_GET => format!("regs[{dst}] = k_http_get(regs[{start}]); (void){argc};"),
            BUILTIN_HTTP_POST => format!("regs[{dst}] = k_http_post(regs[{start}], regs[{start}+1]); (void){argc};"),
            BUILTIN_RE_MATCH => format!("regs[{dst}] = k_re_match(regs[{start}], regs[{start}+1]); (void){argc};"),
            BUILTIN_RE_FIND => format!("regs[{dst}] = k_re_find(regs[{start}], regs[{start}+1]); (void){argc};"),
            BUILTIN_RE_FIND_ALL => format!("regs[{dst}] = k_re_find_all(regs[{start}], regs[{start}+1]); (void){argc};"),
            BUILTIN_RE_REPLACE => format!("regs[{dst}] = k_re_replace(regs[{start}], regs[{start}+1], regs[{start}+2]); (void){argc};"),
            BUILTIN_FORMAT_TIME => format!("regs[{dst}] = k_format_time(regs[{start}]); (void){argc};"),
            BUILTIN_YEAR_OF => format!("regs[{dst}] = k_year_of(regs[{start}]); (void){argc};"),
            BUILTIN_MONTH_OF => format!("regs[{dst}] = k_month_of(regs[{start}]); (void){argc};"),
            BUILTIN_DAY_OF => format!("regs[{dst}] = k_day_of(regs[{start}]); (void){argc};"),
            BUILTIN_HOUR_OF => format!("regs[{dst}] = k_hour_of(regs[{start}]); (void){argc};"),
            BUILTIN_MINUTE_OF => format!("regs[{dst}] = k_minute_of(regs[{start}]); (void){argc};"),
            BUILTIN_SECOND_OF => format!("regs[{dst}] = k_second_of(regs[{start}]); (void){argc};"),
            BUILTIN_WEEKDAY_OF => format!("regs[{dst}] = k_weekday_of(regs[{start}]); (void){argc};"),
            BUILTIN_YEARDAY_OF => format!("regs[{dst}] = k_yearday_of(regs[{start}]); (void){argc};"),
            BUILTIN_DATE_ISO => format!("regs[{dst}] = k_date_iso(regs[{start}]); (void){argc};"),
            BUILTIN_PARSE_ISO => format!("regs[{dst}] = k_parse_iso(regs[{start}]); (void){argc};"),
            BUILTIN_DATE_MAKE => format!("regs[{dst}] = k_date_make(regs[{start}], regs[{start}+1], regs[{start}+2], regs[{start}+3], regs[{start}+4], regs[{start}+5]); (void){argc};"),
            BUILTIN_NOW => format!("regs[{dst}] = k_now(); (void){start}; (void){argc};"),
            BUILTIN_READ_LINE => format!("regs[{dst}] = k_read_line(); (void){start}; (void){argc};"),
            BUILTIN_READ_ALL => format!("regs[{dst}] = k_read_all(); (void){start}; (void){argc};"),
            BUILTIN_BASE64_ENCODE => format!("regs[{dst}] = k_base64_encode(regs[{start}]); (void){argc};"),
            BUILTIN_BASE64_DECODE => format!("regs[{dst}] = k_base64_decode(regs[{start}]); (void){argc};"),
            BUILTIN_HEX_ENCODE => format!("regs[{dst}] = k_hex_encode(regs[{start}]); (void){argc};"),
            BUILTIN_HEX_DECODE => format!("regs[{dst}] = k_hex_decode(regs[{start}]); (void){argc};"),
            BUILTIN_HASH_FNV => format!("regs[{dst}] = k_hash_fnv(regs[{start}]); (void){argc};"),
            BUILTIN_CSV_PARSE => format!("regs[{dst}] = k_csv_parse(regs[{start}]); (void){argc};"),
            BUILTIN_CSV_STRINGIFY => format!("regs[{dst}] = k_csv_stringify(regs[{start}]); (void){argc};"),
            BUILTIN_URL_ENCODE => format!("regs[{dst}] = k_url_encode(regs[{start}]); (void){argc};"),
            BUILTIN_URL_DECODE => format!("regs[{dst}] = k_url_decode(regs[{start}]); (void){argc};"),
            BUILTIN_QUERY_PARSE => format!("regs[{dst}] = k_query_parse(regs[{start}]); (void){argc};"),
            BUILTIN_QUERY_BUILD => format!("regs[{dst}] = k_query_build(regs[{start}]); (void){argc};"),
            BUILTIN_ENV_VAR => format!("regs[{dst}] = k_env_var(regs[{start}]); (void){argc};"),
            BUILTIN_ARGS => format!("regs[{dst}] = k_args(); (void){start}; (void){argc};"),
            BUILTIN_EPRINT => format!("regs[{dst}] = k_eprint(regs[{start}]); (void){argc};"),
            BUILTIN_EXIT => format!("fflush(stdout); exit((int)regs[{start}].as.i); (void){argc}; (void){dst};"),
            BUILTIN_RANDOM_INTS => format!("regs[{dst}] = k_random_ints(regs[{start}], regs[{start}+1]); (void){argc};"),
            BUILTIN_RANDOM_FLOATS => format!("regs[{dst}] = k_random_floats(regs[{start}], regs[{start}+1]); (void){argc};"),
            BUILTIN_SHUFFLE => format!("regs[{dst}] = k_shuffle(regs[{start}], regs[{start}+1]); (void){argc};"),
            BUILTIN_BIG => format!("regs[{dst}] = k_big_builtin(regs[{start}]); (void){argc};"),
            BUILTIN_HTTP_SERVE => format!("regs[{dst}] = k_http_serve(regs[{start}], regs[{start}+1]); (void){argc};"),
            BUILTIN_RAT => format!("regs[{dst}] = k_rat_builtin(regs[{start}], regs[{start}+1]); (void){argc};"),
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
        StateGet(dst, slot) => format!("regs[{dst}] = k_state_get({slot});"),
        StateSet(slot, src) => format!("k_state_set({slot}, regs[{src}]);"),
        MakeInstance { dst, comp, start, argc, policy } => {
            // props are argc consecutive registers from `start`
            format!(
                "{{ int _id = k_instantiate({comp}, &regs[{start}], {argc}); k_insts[_id].restart_on_failure = ({policy} == 1); regs[{dst}] = k_component(_id); }}"
            )
        }
        WireOp { from, out_port, to, in_port } => {
            let out = str_const(chunk, *out_port)?;
            let inn = str_const(chunk, *in_port)?;
            format!(
                "k_wire((int)regs[{from}].as.i, \"{}\", (int)regs[{to}].as.i, \"{}\");",
                c_escape(out),
                c_escape(inn)
            )
        }
        EmitOp { port, payload } => {
            let p = str_const(chunk, *port)?;
            let val = match payload {
                Some(r) => format!("regs[{r}]"),
                None => "k_unit()".to_string(),
            };
            format!("k_emit(\"{}\", {});", c_escape(p), val)
        }
        CallComp { dst, fun, start, argc } => {
            // resolved cross-component call: run chunk `fun` with the CURRENT
            // instance (vm.rs threads cur_inst through push_frame). argc silences
            // an unused-var warning when zero.
            format!("(void){argc}; regs[{dst}] = CHUNKS[{fun}](0, &regs[{start}]);")
        }
        // emit_c rejects modules with ai funs before reaching here
        CallAi { dst, info, intent } => {
            // mock/deterministic path; the resolved intent + args are unused (the
            // mock ignores the prompt). Real providers/tools defer in k_ai_call.
            format!("regs[{dst}] = k_ai_call({info}); (void)regs[{intent}];")
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
            // `\?` neutralizes C trigraphs (`??x`); harmless in every C string.
            '?' => out.push_str("\\?"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            // Control bytes: fixed-width 3-digit OCTAL, not `\xNN`. A C `\x` escape
            // is greedy (consumes all following hex digits), so `\x00` + '5' would
            // merge into one byte `\x005` — a miscompile. `\NNN` takes at most 3
            // octal digits, so a following digit can never merge. (ch < 0x20.)
            c if (c as u32) < 0x20 => out.push_str(&format!("\\{:03o}", c as u32)),
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
#include <errno.h>
#include <unistd.h>
#include <time.h>
#include <setjmp.h>
#include <sys/wait.h>
#include <dirent.h>
#include <sys/stat.h>
#include <errno.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>

typedef struct KValue KValue;
typedef struct { int64_t len; KValue* items; } KList;
typedef struct { int32_t ctor; KValue* fields; int32_t nfields; } KCtor;
typedef struct { int32_t proto; int32_t ncaps; KValue* caps; } KClosure;
typedef struct { int64_t len; double* data; } KTensor;
typedef struct { int64_t len; KValue* keys; KValue* vals; } KMap;
typedef struct { int64_t len; KValue* items; } KSet;
typedef struct { const char* type_name; const char* variant; int arity; const char** fields; } KCtorMeta;
/* a fixed-width integer: the value is boxed (like the interpreter's i128 box) so
   KValue stays small; width is the IntW tag 0..7 (i8,i16,i32,i64,u8,u16,u32,u64) */
typedef struct { __int128 v; int width; } KSized;
typedef struct KBig KBig;
typedef struct KRat KRat;

struct KValue {
    enum { K_INT, K_FLOAT, K_BOOL, K_UNIT, K_STR, K_LIST, K_CTOR, K_CLOSURE, K_FUN, K_RANGE, K_TENSOR, K_MAP, K_SET, K_COMPONENT, K_SIZEDINT, K_F32, K_BIGINT, K_RATIONAL } tag;
    union {
        int64_t i; double f; int b; float f32v;
        const char* s;
        KList* list; KCtor* ctor; KClosure* clo; KTensor* ten; KMap* map; KSet* set;
        int32_t fun; KSized* sized; KBig* big; KRat* rat;
        struct { int64_t lo, hi; int incl; } range;
    } as;
};

/* Supervision landing pad: when a supervised dispatch is active, k_panic saves
   the message and longjmps to the pad instead of exiting (mirrors the VM's
   call_chunk_nested returning Err, caught by the restart-on-failure branch). */
static jmp_buf* k_pad = 0;
static char k_panic_buf[1024];
/* User-function call depth. Guards against unbounded recursion so deep recursion
   yields the same clean panic as the interpreter/KVM (which cap at 10 000 frames)
   instead of segfaulting on the C stack. __thread-local for safety if a future
   backend runs generated functions on multiple threads. */
static __thread int64_t k_depth = 0;
static void k_panic(const char* msg) {
    if (k_pad) {
        strncpy(k_panic_buf, msg, sizeof(k_panic_buf) - 1);
        k_panic_buf[sizeof(k_panic_buf) - 1] = 0;
        longjmp(*k_pad, 1);
    }
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
static KValue k_f32(float v)     { KValue x; x.tag = K_F32;   x.as.f32v = v; return x; }
static KValue k_f32_bits(uint32_t bits) { float v; memcpy(&v, &bits, 4); return k_f32(v); }

/* ---- fixed-width integers (mirror value.rs IntW + interp raw_binary_op) ---- */
static int k_iw_bits(int w) { switch (w % 4) { case 0: return 8; case 1: return 16; case 2: return 32; default: return 64; } }
static int k_iw_signed(int w) { return w < 4; }
static __int128 k_iw_max(int w) {
    int bits = k_iw_bits(w);
    return k_iw_signed(w) ? (((__int128)1 << (bits - 1)) - 1) : (((__int128)1 << bits) - 1);
}
static __int128 k_iw_min(int w) {
    if (!k_iw_signed(w)) return 0;
    return -((__int128)1 << (k_iw_bits(w) - 1));
}
static const char* k_iw_name(int w) {
    static const char* n[] = { "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64" };
    return n[w & 7];
}
static __int128 k_iw_wrap(int w, __int128 v) {
    int bits = k_iw_bits(w);
    __int128 m = (__int128)1 << bits;
    __int128 r = v % m; if (r < 0) r += m;     /* rem_euclid */
    if (k_iw_signed(w) && r > k_iw_max(w)) r -= m;
    return r;
}
static __int128 k_iw_sat(int w, __int128 v) {
    __int128 lo = k_iw_min(w), hi = k_iw_max(w);
    return v < lo ? lo : (v > hi ? hi : v);
}
static KValue k_sized(__int128 v, int w) {
    KValue x; x.tag = K_SIZEDINT;
    x.as.sized = (KSized*)k_alloc(sizeof(KSized));
    x.as.sized->v = v; x.as.sized->width = w;
    return x;
}
/* checked same-width arithmetic (op: 0+ 1- 2* 3/ 4%) — mirrors interp exactly */
static KValue k_sized_arith(KValue a, KValue b, int op) {
    if (a.as.sized->width != b.as.sized->width) k_panic("mismatched sized-int widths");
    int w = a.as.sized->width;
    __int128 x = a.as.sized->v, y = b.as.sized->v, r; const char* what;
    switch (op) {
        case 0: r = x + y; what = "addition"; break;
        case 1: r = x - y; what = "subtraction"; break;
        case 2: r = x * y; what = "multiplication"; break;
        case 3: if (y == 0) k_panic("division by zero"); r = x / y; what = "division"; break;
        default: if (y == 0) k_panic("remainder by zero"); r = x % y; what = "remainder"; break;
    }
    if (r < k_iw_min(w) || r > k_iw_max(w)) {
        char buf[64]; snprintf(buf, sizeof buf, "integer overflow in %s", what); k_panic(buf);
    }
    return k_sized(r, w);
}
/* width tag (0..7) from a to_iN/to_uN method name, or -1 */
static int k_width_of(const char* name) {
    static const char* n[] = { "to_i8","to_i16","to_i32","to_i64","to_u8","to_u16","to_u32","to_u64" };
    for (int i = 0; i < 8; i++) if (!strcmp(name, n[i])) return i;
    return -1;
}
/* print a ≤64-bit __int128 into buf (signed or unsigned by value) */
static void k_i128_print(char* buf, size_t n, __int128 x) {
    if (x > (__int128)INT64_MAX) snprintf(buf, n, "%llu", (unsigned long long)x);
    else snprintf(buf, n, "%lld", (long long)x);
}
static KValue k_bool(int v)      { KValue x; x.tag = K_BOOL;  x.as.b = !!v; return x; }
static KValue k_unit(void)       { KValue x; x.tag = K_UNIT;  x.as.i = 0; return x; }
/* Borrows `s` — does NOT copy. The pointer must outlive the KValue: pass a string
   literal, a k_strdup/k_alloc'd heap buffer, or a buffer owned by a live structure.
   NEVER a local stack buffer (dangles after return) or a shared/volatile static like
   strerror()'s or k_ai_err (a later call clobbers it). Wrap those in k_strdup(). */
static KValue k_str(const char* s) { KValue x; x.tag = K_STR; x.as.s = s; return x; }
static char* k_strdup(const char* s) { size_t n = strlen(s) + 1; char* c = (char*)k_alloc(n); memcpy(c, s, n); return c; }
/* An `Err` whose message mirrors Rust's io::Error Display for a raw OS error:
   "<strerror(errno)> (os error <errno>)" — so IO error VALUES are byte-identical to
   the interpreter. Reads errno first; the message is heap-owned (k_str borrows). */
static KValue k_err(KValue);
static KValue k_os_error(void) {
    int e = errno;
    const char* m = strerror(e);
    size_t cap = strlen(m) + 32;
    char* buf = (char*)k_alloc(cap);
    snprintf(buf, cap, "%s (os error %d)", m, e);
    return k_err(k_str(buf));
}

/* fixed-precision decimal formatting — a byte-for-byte mirror of Rust's
   interp::format_float (round half away from zero; no platform %.*f). */
static KValue k_format_float(double x, int64_t decimals) {
    char buf[64];
    if (isnan(x)) return k_str(k_strdup("nan"));
    if (isinf(x)) return k_str(k_strdup(x < 0 ? "-inf" : "inf"));
    int d = decimals < 0 ? 0 : (decimals > 18 ? 18 : (int)decimals);
    uint64_t scale = 1;
    for (int i = 0; i < d; i++) scale *= 10;
    uint64_t scaled = (uint64_t)floor(fabs(x) * (double)scale + 0.5);
    const char* sign = (x < 0 && scaled != 0) ? "-" : "";
    if (d == 0) {
        snprintf(buf, sizeof buf, "%s%llu", sign, (unsigned long long)scaled);
    } else {
        uint64_t ip = scaled / scale, fp = scaled % scale;
        snprintf(buf, sizeof buf, "%s%llu.%0*llu", sign, (unsigned long long)ip, d, (unsigned long long)fp);
    }
    return k_str(k_strdup(buf));
}

/* ---- BigInt: a C mirror of src/bigint.rs (sign-magnitude, base-1e9 limbs).
   The base is chosen so to_decimal matches the Rust engine byte-for-byte. ---- */
struct KBig { int neg; int n; uint32_t* limbs; };
#define KBIG_BASE 1000000000u
static KValue k_big_v(KBig* b) { KValue x; x.tag = K_BIGINT; x.as.big = b; return x; }
static KBig* k_big_norm(int neg, const uint32_t* limbs, int n) {
    while (n > 0 && limbs[n - 1] == 0) n--;
    KBig* b = (KBig*)k_alloc(sizeof(KBig));
    b->neg = (n == 0) ? 0 : (neg != 0);
    b->n = n;
    if (n > 0) { b->limbs = (uint32_t*)k_alloc(sizeof(uint32_t) * n); memcpy(b->limbs, limbs, sizeof(uint32_t) * n); }
    else b->limbs = 0;
    return b;
}
static KBig* k_big_from_i64(int64_t v) {
    if (v == 0) return k_big_norm(0, 0, 0);
    int neg = v < 0;
    uint64_t m = neg ? (~(uint64_t)v + 1) : (uint64_t)v;
    uint32_t tmp[3]; int n = 0;
    while (m > 0) { tmp[n++] = (uint32_t)(m % KBIG_BASE); m /= KBIG_BASE; }
    return k_big_norm(neg, tmp, n);
}
static KBig* k_big_from_str(const char* s) {
    while (*s == ' ' || *s == '\t' || *s == '\n' || *s == '\r') s++;
    int neg = 0;
    if (*s == '-') { neg = 1; s++; } else if (*s == '+') s++;
    if (!*s) return 0;
    for (const char* p = s; *p; p++) if (*p < '0' || *p > '9') return 0;
    int len = (int)strlen(s);
    int cap = (len + 8) / 9; if (cap < 1) cap = 1;
    uint32_t* limbs = (uint32_t*)k_alloc(sizeof(uint32_t) * cap);
    int li = 0, i = len;
    while (i > 0) {
        int st = i - 9; if (st < 0) st = 0;
        uint32_t v = 0;
        for (int j = st; j < i; j++) v = v * 10 + (uint32_t)(s[j] - '0');
        limbs[li++] = v;
        i = st;
    }
    return k_big_norm(neg, limbs, li);
}
static char* k_big_to_decimal(KBig* b) {
    if (b->n == 0) return k_strdup("0");
    int cap = b->n * 9 + 2;
    char* out = (char*)k_alloc(cap);
    int pos = 0;
    if (b->neg) out[pos++] = '-';
    pos += snprintf(out + pos, cap - pos, "%u", b->limbs[b->n - 1]);
    for (int i = b->n - 2; i >= 0; i--) pos += snprintf(out + pos, cap - pos, "%09u", b->limbs[i]);
    out[pos] = 0;
    return out;
}
static int k_big_cmp_mag(const uint32_t* a, int an, const uint32_t* bb, int bn) {
    if (an != bn) return an < bn ? -1 : 1;
    for (int i = an - 1; i >= 0; i--) if (a[i] != bb[i]) return a[i] < bb[i] ? -1 : 1;
    return 0;
}
static int k_big_add_mag(const uint32_t* a, int an, const uint32_t* bb, int bn, uint32_t* out) {
    uint64_t carry = 0; int m = an > bn ? an : bn, i;
    for (i = 0; i < m; i++) { uint64_t av = i < an ? a[i] : 0, bv = i < bn ? bb[i] : 0, s = av + bv + carry; out[i] = (uint32_t)(s % KBIG_BASE); carry = s / KBIG_BASE; }
    if (carry) out[i++] = (uint32_t)carry;
    return i;
}
static int k_big_sub_mag(const uint32_t* a, int an, const uint32_t* bb, int bn, uint32_t* out) {
    int64_t borrow = 0; int i;
    for (i = 0; i < an; i++) { int64_t av = a[i], bv = i < bn ? bb[i] : 0, d = av - bv - borrow; if (d < 0) { d += KBIG_BASE; borrow = 1; } else borrow = 0; out[i] = (uint32_t)d; }
    while (i > 0 && out[i - 1] == 0) i--;
    return i;
}
static int k_big_mul_small(const uint32_t* a, int an, uint64_t k, uint32_t* out) {
    if (k == 0 || an == 0) return 0;
    uint64_t carry = 0; int i;
    for (i = 0; i < an; i++) { uint64_t cur = (uint64_t)a[i] * k + carry; out[i] = (uint32_t)(cur % KBIG_BASE); carry = cur / KBIG_BASE; }
    while (carry) { out[i++] = (uint32_t)(carry % KBIG_BASE); carry /= KBIG_BASE; }
    return i;
}
static KBig* k_big_add(KValue av, KValue bv) {
    KBig* a = av.as.big; KBig* b = bv.as.big;
    if (a->neg == b->neg) {
        uint32_t* out = (uint32_t*)k_alloc(sizeof(uint32_t) * (a->n + b->n + 2));
        int n = k_big_add_mag(a->limbs, a->n, b->limbs, b->n, out);
        return k_big_norm(a->neg, out, n);
    }
    int c = k_big_cmp_mag(a->limbs, a->n, b->limbs, b->n);
    if (c == 0) return k_big_norm(0, 0, 0);
    int cap = (a->n > b->n ? a->n : b->n) + 1;
    uint32_t* out = (uint32_t*)k_alloc(sizeof(uint32_t) * cap);
    if (c > 0) { int n = k_big_sub_mag(a->limbs, a->n, b->limbs, b->n, out); return k_big_norm(a->neg, out, n); }
    int n = k_big_sub_mag(b->limbs, b->n, a->limbs, a->n, out);
    return k_big_norm(b->neg, out, n);
}
static KBig* k_big_negate(KBig* a) {
    KBig* r = (KBig*)k_alloc(sizeof(KBig));
    r->neg = a->n ? !a->neg : 0; r->n = a->n; r->limbs = a->limbs;
    return r;
}
static KBig* k_big_sub(KValue av, KValue bv) { return k_big_add(av, k_big_v(k_big_negate(bv.as.big))); }
static KBig* k_big_mul(KValue av, KValue bv) {
    KBig* a = av.as.big; KBig* b = bv.as.big;
    if (a->n == 0 || b->n == 0) return k_big_norm(0, 0, 0);
    int n = a->n + b->n;
    uint64_t* acc = (uint64_t*)k_alloc(sizeof(uint64_t) * n);
    for (int i = 0; i < n; i++) acc[i] = 0;
    for (int i = 0; i < a->n; i++) {
        uint64_t carry = 0;
        for (int j = 0; j < b->n; j++) { uint64_t cur = acc[i + j] + (uint64_t)a->limbs[i] * b->limbs[j] + carry; acc[i + j] = cur % KBIG_BASE; carry = cur / KBIG_BASE; }
        acc[i + b->n] += carry;
    }
    uint32_t* out = (uint32_t*)k_alloc(sizeof(uint32_t) * n);
    for (int i = 0; i < n; i++) out[i] = (uint32_t)acc[i];
    return k_big_norm(a->neg != b->neg, out, n);
}
/* returns quotient (want_rem=0) or remainder (want_rem=1) with the truncated
   sign convention (quotient sign = a^b, remainder sign = dividend). */
static KBig* k_big_divmod(KValue av, KValue bv, int want_rem) {
    KBig* a = av.as.big; KBig* b = bv.as.big;
    if (b->n == 0) k_panic(want_rem ? "remainder by zero" : "division by zero");
    if (k_big_cmp_mag(a->limbs, a->n, b->limbs, b->n) < 0)
        return want_rem ? k_big_norm(a->neg, a->limbs, a->n) : k_big_norm(0, 0, 0);
    int an = a->n, bn = b->n;
    uint32_t* quo = (uint32_t*)k_alloc(sizeof(uint32_t) * an);
    for (int i = 0; i < an; i++) quo[i] = 0;
    uint32_t* rem = (uint32_t*)k_alloc(sizeof(uint32_t) * (an + 2));
    uint32_t* nxt = (uint32_t*)k_alloc(sizeof(uint32_t) * (an + 2));
    uint32_t* tmp = (uint32_t*)k_alloc(sizeof(uint32_t) * (bn + 2));
    int remn = 0;
    for (int i = an - 1; i >= 0; i--) {
        nxt[0] = a->limbs[i];
        for (int j = 0; j < remn; j++) nxt[j + 1] = rem[j];
        int nn = remn + 1;
        while (nn > 0 && nxt[nn - 1] == 0) nn--;
        memcpy(rem, nxt, sizeof(uint32_t) * nn); remn = nn;
        uint64_t lo = 0, hi = KBIG_BASE - 1;
        while (lo < hi) {
            uint64_t mid = (lo + hi + 1) / 2;
            int tn = k_big_mul_small(b->limbs, bn, mid, tmp);
            if (k_big_cmp_mag(tmp, tn, rem, remn) <= 0) lo = mid; else hi = mid - 1;
        }
        quo[i] = (uint32_t)lo;
        if (lo > 0) { int tn = k_big_mul_small(b->limbs, bn, lo, tmp); remn = k_big_sub_mag(rem, remn, tmp, tn, rem); }
    }
    if (want_rem) return k_big_norm(a->neg, rem, remn);
    return k_big_norm(a->neg != b->neg, quo, an);
}
static KBig* k_big_pow(KValue av, int64_t exp) {
    KBig* result = k_big_from_i64(1);
    KValue base = av;
    uint64_t e = (uint64_t)exp;
    while (e > 0) {
        if (e & 1) result = k_big_mul(k_big_v(result), base);
        e >>= 1;
        if (e > 0) base = k_big_v(k_big_mul(base, base));
    }
    return result;
}
static int k_big_cmp(KValue av, KValue bv) {
    KBig* a = av.as.big; KBig* b = bv.as.big;
    int sa = a->n == 0 ? 0 : (a->neg ? -1 : 1), sb = b->n == 0 ? 0 : (b->neg ? -1 : 1);
    if (sa != sb) return sa < sb ? -1 : 1;
    int m = k_big_cmp_mag(a->limbs, a->n, b->limbs, b->n);
    return sa < 0 ? -m : m;
}
/* the `big` builtin: from an Int or a decimal Str */
static KValue k_big_builtin(KValue v) {
    if (v.tag == K_INT) return k_big_v(k_big_from_i64(v.as.i));
    if (v.tag == K_BIGINT) return v;
    if (v.tag == K_STR) {
        KBig* b = k_big_from_str(v.as.s);
        if (!b) { char m[128]; snprintf(m, sizeof m, "invalid BigInt: %s", v.as.s); k_panic(m); }
        return k_big_v(b);
    }
    k_panic("`big` needs an Int or a Str"); return k_unit();
}

/* ---- Rational: exact fractions over KBig, a C mirror of src/rational.rs.
   Always reduced, denominator > 0; to_decimal matches the Rust engine. ---- */
struct KRat { KBig* num; KBig* den; };
static KValue k_rat_v(KRat* r) { KValue x; x.tag = K_RATIONAL; x.as.rat = r; return x; }
static KBig* k_big_abs(KBig* a) { return k_big_norm(0, a->limbs, a->n); }
static KBig* k_big_gcd(KBig* a, KBig* b) {
    KBig* x = k_big_abs(a);
    KBig* y = k_big_abs(b);
    while (y->n != 0) {
        KBig* r = k_big_divmod(k_big_v(x), k_big_v(y), 1);
        x = y;
        y = r;
    }
    return x;
}
static int k_big_is_one(KBig* a) { return a->n == 1 && a->limbs[0] == 1 && !a->neg; }
static KRat* k_rat_norm(KBig* num, KBig* den) {
    if (den->n == 0) k_panic("division by zero");
    if (den->neg) { num = k_big_negate(num); den = k_big_negate(den); }
    KRat* r = (KRat*)k_alloc(sizeof(KRat));
    KBig* g = k_big_gcd(num, den);
    if (g->n == 0) { r->num = k_big_from_i64(0); r->den = k_big_from_i64(1); return r; }
    r->num = k_big_divmod(k_big_v(num), k_big_v(g), 0);
    r->den = k_big_divmod(k_big_v(den), k_big_v(g), 0);
    return r;
}
static KBig* k_rat_to_big(KValue v) {
    if (v.tag == K_INT) return k_big_from_i64(v.as.i);
    if (v.tag == K_BIGINT) return v.as.big;
    k_panic("`rat` needs Int or BigInt"); return 0;
}
static KValue k_rat_builtin(KValue n, KValue d) {
    return k_rat_v(k_rat_norm(k_rat_to_big(n), k_rat_to_big(d)));
}
static KRat* k_rat_add(KValue av, KValue bv) {
    KRat* a = av.as.rat; KRat* b = bv.as.rat;
    KBig* n = k_big_add(k_big_v(k_big_mul(k_big_v(a->num), k_big_v(b->den))),
                        k_big_v(k_big_mul(k_big_v(b->num), k_big_v(a->den))));
    return k_rat_norm(n, k_big_mul(k_big_v(a->den), k_big_v(b->den)));
}
static KRat* k_rat_sub(KValue av, KValue bv) {
    KRat* a = av.as.rat; KRat* b = bv.as.rat;
    KBig* n = k_big_sub(k_big_v(k_big_mul(k_big_v(a->num), k_big_v(b->den))),
                        k_big_v(k_big_mul(k_big_v(b->num), k_big_v(a->den))));
    return k_rat_norm(n, k_big_mul(k_big_v(a->den), k_big_v(b->den)));
}
static KRat* k_rat_mul(KValue av, KValue bv) {
    KRat* a = av.as.rat; KRat* b = bv.as.rat;
    return k_rat_norm(k_big_mul(k_big_v(a->num), k_big_v(b->num)),
                      k_big_mul(k_big_v(a->den), k_big_v(b->den)));
}
static KRat* k_rat_div(KValue av, KValue bv) {
    KRat* a = av.as.rat; KRat* b = bv.as.rat;
    if (b->num->n == 0) k_panic("division by zero");
    return k_rat_norm(k_big_mul(k_big_v(a->num), k_big_v(b->den)),
                      k_big_mul(k_big_v(a->den), k_big_v(b->num)));
}
static int k_rat_cmp(KValue av, KValue bv) {
    KRat* a = av.as.rat; KRat* b = bv.as.rat;
    return k_big_cmp(k_big_v(k_big_mul(k_big_v(a->num), k_big_v(b->den))),
                     k_big_v(k_big_mul(k_big_v(b->num), k_big_v(a->den))));
}
static char* k_rat_to_decimal(KRat* r) {
    char* n = k_big_to_decimal(r->num);
    if (k_big_is_one(r->den)) return n;
    char* d = k_big_to_decimal(r->den);
    int len = (int)(strlen(n) + strlen(d) + 2);
    char* out = (char*)k_alloc(len);
    snprintf(out, len, "%s/%s", n, d);
    return out;
}
static double k_rat_to_f64(KRat* r) {
    return strtod(k_big_to_decimal(r->num), 0) / strtod(k_big_to_decimal(r->den), 0);
}

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
static int k_op_overload(const char* name, KValue a, KValue b, KValue* out);

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
/* Same 100M-element sanity cap as interp::MAX_TENSOR_LEN — a huge size panics
   cleanly instead of hanging / OOM-killing the process. */
#define K_MAX_TENSOR_LEN 100000000LL
static KValue k_bt_zeros(KValue v) {
    if (v.tag != K_INT || v.as.i < 0) k_panic("zeros() needs a non-negative size");
    if (v.as.i > K_MAX_TENSOR_LEN) k_panic("zeros() size too large");
    double* d = k_alloc(sizeof(double) * (v.as.i < 1 ? 1 : v.as.i));
    for (int64_t i = 0; i < v.as.i; i++) d[i] = 0.0;
    return k_tensor_new(d, v.as.i);
}
static KValue k_bt_arange(KValue v) {
    if (v.tag != K_INT || v.as.i < 0) k_panic("arange() needs a non-negative size");
    if (v.as.i > K_MAX_TENSOR_LEN) k_panic("arange() size too large");
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
    /* Big enough for the full positional expansion of any f64: ~309 integer
       digits (near f64::MAX) or ~324 fractional (smallest subnormal), plus sign,
       point, and ".0". A 64-byte buffer used to TRUNCATE large whole values. */
    char tmp[512];
    /* Match Rust's f64 Display for non-finite values (the interpreter's Display
       path): NaN -> "NaN", infinities -> "inf"/"-inf". Also portable — some libc
       %g print NaN as "nan" or infinity as "1.#INF". (The `.fmt()` method uses
       k_format_float, which mirrors interp::format_float's lowercase "nan".) */
    if (isnan(f)) { kb_puts(b, "NaN"); return; }
    if (isinf(f)) { kb_puts(b, f < 0 ? "-inf" : "inf"); return; }
    if (isfinite(f) && f == floor(f)) {
        /* whole number -> "N.0" (matches the interpreter's Float Display) */
        snprintf(tmp, sizeof tmp, "%.1f", f);
    } else {
        /* Shortest FIXED-notation (positional, never scientific) representation
           that round-trips — Rust's f64 Display never uses exponents, so `%g`
           (which switches to scientific for |exp| >= 5 / large) diverged on values
           like 0.00001 -> "1e-05" and 1e-300. `%.*f` finds the shortest decimal-
           place count that round-trips, which is the same string Rust prints. */
        for (int prec = 1; prec <= 340; prec++) {
            snprintf(tmp, sizeof tmp, "%.*f", prec, f);
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
        case K_BIGINT: kb_puts(b, k_big_to_decimal(v.as.big)); break;
        case K_RATIONAL: kb_puts(b, k_rat_to_decimal(v.as.rat)); break;
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
        case K_COMPONENT:
            snprintf(tmp, sizeof tmp, "<component #%lld>", (long long)v.as.i);
            kb_puts(b, tmp);
            break;
        case K_SIZEDINT:
            /* the stored value always fits its width (≤64 bits); print signed
               widths with %lld and unsigned with %llu — matches value.rs {b.0} */
            if (k_iw_signed(v.as.sized->width))
                snprintf(tmp, sizeof tmp, "%lld", (long long)v.as.sized->v);
            else
                snprintf(tmp, sizeof tmp, "%llu", (unsigned long long)v.as.sized->v);
            kb_puts(b, tmp);
            break;
        case K_F32: {
            /* mirror value.rs F32 Display: whole -> "%.1f", else the shortest
               decimal that round-trips AS A FLOAT (strtof, not strtod) */
            float ff = v.as.f32v;
            if (isfinite(ff) && ff == floorf(ff)) {
                snprintf(tmp, sizeof tmp, "%.1f", (double)ff);
            } else {
                for (int prec = 1; prec <= 9; prec++) {
                    snprintf(tmp, sizeof tmp, "%.*g", prec, (double)ff);
                    if (strtof(tmp, 0) == ff) break;
                }
            }
            kb_puts(b, tmp);
            break;
        }
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
    if (a.tag == K_SIZEDINT && b.tag == K_SIZEDINT)
        return a.as.sized->width == b.as.sized->width && a.as.sized->v == b.as.sized->v;
    if (a.tag == K_F32 && b.tag == K_F32) return a.as.f32v == b.as.f32v;
    if (a.tag == K_BIGINT && b.tag == K_BIGINT) return k_big_cmp(a, b) == 0;
    if (a.tag == K_RATIONAL && b.tag == K_RATIONAL) return k_rat_cmp(a, b) == 0;
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
    if (a.tag == K_F32 && b.tag == K_F32) return k_f32(a.as.f32v + b.as.f32v);
    if (a.tag == K_SIZEDINT && b.tag == K_SIZEDINT) return k_sized_arith(a, b, 0);
    if (a.tag == K_STR && b.tag == K_STR) return k_concat(a, b);
    if (a.tag == K_TENSOR && b.tag == K_TENSOR) return k_tensor_binop(a, b, 0);
    if (a.tag == K_BIGINT && b.tag == K_BIGINT) return k_big_v(k_big_add(a, b));
    if (a.tag == K_RATIONAL && b.tag == K_RATIONAL) return k_rat_v(k_rat_add(a, b));
    { KValue _o; if (a.tag == K_CTOR && k_op_overload("add", a, b, &_o)) return _o; }
    k_panic("invalid operand types"); return k_unit();
}
static KValue k_sub(KValue a, KValue b) {
    if (a.tag == K_INT && b.tag == K_INT) {
        int64_t r;
        if (__builtin_sub_overflow(a.as.i, b.as.i, &r)) k_panic("integer overflow in subtraction");
        return k_int(r);
    }
    if (a.tag == K_FLOAT && b.tag == K_FLOAT) return k_float(a.as.f - b.as.f);
    if (a.tag == K_F32 && b.tag == K_F32) return k_f32(a.as.f32v - b.as.f32v);
    if (a.tag == K_SIZEDINT && b.tag == K_SIZEDINT) return k_sized_arith(a, b, 1);
    if (a.tag == K_TENSOR && b.tag == K_TENSOR) return k_tensor_binop(a, b, 1);
    if (a.tag == K_BIGINT && b.tag == K_BIGINT) return k_big_v(k_big_sub(a, b));
    if (a.tag == K_RATIONAL && b.tag == K_RATIONAL) return k_rat_v(k_rat_sub(a, b));
    { KValue _o; if (a.tag == K_CTOR && k_op_overload("sub", a, b, &_o)) return _o; }
    k_panic("invalid operand types"); return k_unit();
}
static KValue k_mul(KValue a, KValue b) {
    if (a.tag == K_INT && b.tag == K_INT) {
        int64_t r;
        if (__builtin_mul_overflow(a.as.i, b.as.i, &r)) k_panic("integer overflow in multiplication");
        return k_int(r);
    }
    if (a.tag == K_FLOAT && b.tag == K_FLOAT) return k_float(a.as.f * b.as.f);
    if (a.tag == K_F32 && b.tag == K_F32) return k_f32(a.as.f32v * b.as.f32v);
    if (a.tag == K_SIZEDINT && b.tag == K_SIZEDINT) return k_sized_arith(a, b, 2);
    if (a.tag == K_TENSOR && b.tag == K_TENSOR) return k_tensor_binop(a, b, 2);
    if (a.tag == K_BIGINT && b.tag == K_BIGINT) return k_big_v(k_big_mul(a, b));
    if (a.tag == K_RATIONAL && b.tag == K_RATIONAL) return k_rat_v(k_rat_mul(a, b));
    { KValue _o; if (a.tag == K_CTOR && k_op_overload("mul", a, b, &_o)) return _o; }
    k_panic("invalid operand types"); return k_unit();
}
static KValue k_div(KValue a, KValue b) {
    if (a.tag == K_INT && b.tag == K_INT) {
        if (b.as.i == 0) k_panic("division by zero");
        if (a.as.i == INT64_MIN && b.as.i == -1) k_panic("integer overflow in division");
        return k_int(a.as.i / b.as.i);
    }
    if (a.tag == K_FLOAT && b.tag == K_FLOAT) return k_float(a.as.f / b.as.f);
    if (a.tag == K_F32 && b.tag == K_F32) return k_f32(a.as.f32v / b.as.f32v);
    if (a.tag == K_SIZEDINT && b.tag == K_SIZEDINT) return k_sized_arith(a, b, 3);
    if (a.tag == K_TENSOR && b.tag == K_TENSOR) return k_tensor_binop(a, b, 3);
    if (a.tag == K_BIGINT && b.tag == K_BIGINT) return k_big_v(k_big_divmod(a, b, 0));
    if (a.tag == K_RATIONAL && b.tag == K_RATIONAL) return k_rat_v(k_rat_div(a, b));
    { KValue _o; if (a.tag == K_CTOR && k_op_overload("div", a, b, &_o)) return _o; }
    k_panic("invalid operand types"); return k_unit();
}
static KValue k_rem(KValue a, KValue b) {
    if (a.tag == K_BIGINT && b.tag == K_BIGINT) return k_big_v(k_big_divmod(a, b, 1));
    if (a.tag == K_INT && b.tag == K_INT) {
        if (b.as.i == 0) k_panic("remainder by zero");
        /* INT64_MIN % -1 overflows (like the division) — C would be UB; panic to
           match the interpreter instead of returning a bogus 0. */
        if (a.as.i == INT64_MIN && b.as.i == -1) k_panic("integer overflow in remainder");
        return k_int(a.as.i % b.as.i);
    }
    if (a.tag == K_FLOAT && b.tag == K_FLOAT) return k_float(fmod(a.as.f, b.as.f));
    if (a.tag == K_F32 && b.tag == K_F32) return k_f32(fmodf(a.as.f32v, b.as.f32v));
    if (a.tag == K_SIZEDINT && b.tag == K_SIZEDINT) return k_sized_arith(a, b, 4);
    { KValue _o; if (a.tag == K_CTOR && k_op_overload("rem", a, b, &_o)) return _o; }
    k_panic("invalid operand types"); return k_unit();
}
static KValue k_cmp(KValue a, KValue b, int op) { /* 0:< 1:<= 2:> 3:>= */
    double x, y; int is_str = 0; int c = 0;
    if (a.tag == K_INT && b.tag == K_INT) { x = 0; y = 0; c = (a.as.i < b.as.i) ? -1 : (a.as.i > b.as.i); }
    else if (a.tag == K_FLOAT && b.tag == K_FLOAT) { x = a.as.f; y = b.as.f; c = (x < y) ? -1 : (x > y); }
    else if (a.tag == K_F32 && b.tag == K_F32) { float p = a.as.f32v, q = b.as.f32v; c = (p < q) ? -1 : (p > q); }
    else if (a.tag == K_SIZEDINT && b.tag == K_SIZEDINT) { __int128 p = a.as.sized->v, q = b.as.sized->v; c = (p < q) ? -1 : (p > q); }
    else if (a.tag == K_STR && b.tag == K_STR) { is_str = 1; int r = strcmp(a.as.s, b.as.s); c = (r < 0) ? -1 : (r > 0); }
    else if (a.tag == K_BIGINT && b.tag == K_BIGINT) { c = k_big_cmp(a, b); }
    else if (a.tag == K_RATIONAL && b.tag == K_RATIONAL) { c = k_rat_cmp(a, b); }
    else if (a.tag == K_CTOR) {
        static const char* CMPFN[4] = { "lt", "le", "gt", "ge" };
        KValue _o;
        if (op >= 0 && op < 4 && k_op_overload(CMPFN[op], a, b, &_o)) return _o;
        k_panic("invalid operand types");
    }
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
static KValue k_ok(KValue v) { return k_ctor(2, &v, 1); }
static KValue k_err(KValue v) { return k_ctor(3, &v, 1); }

/* ---- JSON (mirrors src/json.rs byte-for-byte) ---- */
extern const int K_NCTORS;
static int k_ctor_by_name(const char* variant) {
    for (int i = 0; i < K_NCTORS; i++)
        if (!strcmp(CTORS[i].variant, variant)) return i;
    k_panic("unknown Json constructor"); return 0;
}
static KValue k_jc_(const char* name, KValue* fields, int n);
static void kb_putc(KBuf* b, char c) { char s[2] = { c, 0 }; kb_puts(b, s); }
static void kb_putcp(KBuf* b, unsigned int cp) {   /* UTF-8 encode a code point */
    if (cp >= 0xD800 && cp <= 0xDFFF) cp = 0xFFFD;  /* lone surrogate -> replacement */
    if (cp < 0x80) kb_putc(b, (char)cp);
    else if (cp < 0x800) { kb_putc(b, (char)(0xC0 | (cp >> 6))); kb_putc(b, (char)(0x80 | (cp & 0x3F))); }
    else { kb_putc(b, (char)(0xE0 | (cp >> 12))); kb_putc(b, (char)(0x80 | ((cp >> 6) & 0x3F))); kb_putc(b, (char)(0x80 | (cp & 0x3F))); }
}

/* --- serialize (mirror json.rs write_value/format_num/write_string) --- */
static void k_json_num(KBuf* b, double n) {
    if (isfinite(n) && n == floor(n) && fabs(n) < 1e15) {
        char t[32]; snprintf(t, sizeof t, "%lld", (long long)n); kb_puts(b, t);
    } else {  /* n.to_string(): shortest round-trip */
        char t[64];
        for (int p = 1; p <= 17; p++) { snprintf(t, sizeof t, "%.*g", p, n); if (strtod(t, 0) == n) break; }
        kb_puts(b, t);
    }
}
static void k_json_str(KBuf* b, const char* s) {
    kb_putc(b, '"');
    for (const unsigned char* p = (const unsigned char*)s; *p; p++) {
        unsigned char c = *p;
        if (c == '"') kb_puts(b, "\\\"");
        else if (c == '\\') kb_puts(b, "\\\\");
        else if (c == '\n') kb_puts(b, "\\n");
        else if (c == '\t') kb_puts(b, "\\t");
        else if (c == '\r') kb_puts(b, "\\r");
        else if (c == 0x08) kb_puts(b, "\\b");
        else if (c == 0x0C) kb_puts(b, "\\f");
        else if (c < 0x20) { char t[8]; snprintf(t, sizeof t, "\\u%04x", c); kb_puts(b, t); }
        else kb_putc(b, (char)c);
    }
    kb_putc(b, '"');
}
static void k_json_write(KBuf* b, KValue v) {
    if (v.tag != K_CTOR) k_panic("json_stringify needs a Json value");
    const char* var = CTORS[v.as.ctor->ctor].variant;
    KValue* f = v.as.ctor->fields;
    if (!strcmp(var, "JNull")) kb_puts(b, "null");
    else if (!strcmp(var, "JBool")) kb_puts(b, f[0].as.b ? "true" : "false");
    else if (!strcmp(var, "JNum")) k_json_num(b, f[0].as.f);
    else if (!strcmp(var, "JStr")) k_json_str(b, f[0].as.s);
    else if (!strcmp(var, "JArr")) {
        kb_putc(b, '[');
        KList* l = f[0].as.list;
        for (int64_t i = 0; i < l->len; i++) { if (i) kb_putc(b, ','); k_json_write(b, l->items[i]); }
        kb_putc(b, ']');
    } else if (!strcmp(var, "JObj")) {
        kb_putc(b, '{');
        KMap* m = f[0].as.map;
        for (int64_t i = 0; i < m->len; i++) { if (i) kb_putc(b, ','); k_json_str(b, m->keys[i].as.s); kb_putc(b, ':'); k_json_write(b, m->vals[i]); }
        kb_putc(b, '}');
    } else k_panic("not a Json constructor");
}
static KValue k_json_stringify(KValue v) {
    KBuf b = { 0, 0, 0 }; k_json_write(&b, v);
    return k_str(b.buf ? b.buf : (char*)"");
}

/* --- parse (mirror json.rs Parser); build Json ctors, wrap in Ok/Err --- */
typedef struct { const unsigned char* s; long pos, len; int failed; int depth; } KJP;
/* Same nesting cap as json::MAX_JSON_DEPTH — untrusted deep input fails cleanly
   instead of overflowing the (small) C stack. */
#define K_MAX_JSON_DEPTH 500
static KValue kjp_value(KJP* p);
static void kjp_ws(KJP* p) { while (p->pos < p->len) { unsigned char c = p->s[p->pos]; if (c==' '||c=='\t'||c=='\n'||c=='\r') p->pos++; else break; } }
static int kjp_peek(KJP* p) { return p->pos < p->len ? p->s[p->pos] : -1; }
static char* kjp_string(KJP* p) {  /* assumes current char is the opening quote */
    p->pos++;
    KBuf b = { 0, 0, 0 };
    for (;;) {
        if (p->pos >= p->len) { p->failed = 1; break; }
        int c = p->s[p->pos++];
        if (c == '"') break;
        if (c == '\\') {
            if (p->pos >= p->len) { p->failed = 1; break; }
            int e = p->s[p->pos++];
            if (e == '"') kb_putc(&b, '"');
            else if (e == '\\') kb_putc(&b, '\\');
            else if (e == '/') kb_putc(&b, '/');
            else if (e == 'n') kb_putc(&b, '\n');
            else if (e == 't') kb_putc(&b, '\t');
            else if (e == 'r') kb_putc(&b, '\r');
            else if (e == 'b') kb_putc(&b, 0x08);
            else if (e == 'f') kb_putc(&b, 0x0C);
            else if (e == 'u') {
                unsigned int code = 0; int bad = 0;
                for (int i = 0; i < 4; i++) {
                    if (p->pos >= p->len) { bad = 1; break; }
                    int d = p->s[p->pos++];
                    int hv = (d>='0'&&d<='9')?d-'0':(d>='a'&&d<='f')?d-'a'+10:(d>='A'&&d<='F')?d-'A'+10:-1;
                    if (hv < 0) { bad = 1; break; }
                    code = code * 16 + hv;
                }
                if (bad) { p->failed = 1; break; }
                kb_putcp(&b, code);
            } else { p->failed = 1; break; }
        } else kb_putc(&b, (char)c);
    }
    return b.buf ? b.buf : (char*)"";
}
static KValue kjp_number(KJP* p) {
    long start = p->pos;
    if (kjp_peek(p) == '-') p->pos++;
    while (p->pos < p->len) { unsigned char c = p->s[p->pos]; if ((c>='0'&&c<='9')||c=='.'||c=='e'||c=='E'||c=='+'||c=='-') p->pos++; else break; }
    char buf[64]; long n = p->pos - start; if (n >= (long)sizeof buf) n = sizeof buf - 1;
    memcpy(buf, p->s + start, n); buf[n] = 0;
    char* end; double d = strtod(buf, &end);
    if (end == buf || *end != 0) { p->failed = 1; return k_unit(); }
    KValue f = k_float(d); return k_jc_("JNum", &f, 1);
}
static KValue k_jc_(const char* name, KValue* fields, int n) { return k_ctor(k_ctor_by_name(name), fields, n); }
static KValue kjp_array(KJP* p) {
    p->pos++;  /* '[' */
    /* Heap-grown (not a large stack array): keeps the recursion frame tiny so the
       depth guard in kjp_value bounds nesting before the stack overflows, and
       removes the old 4096-element cap (matching the interpreter's unbounded Vec). */
    int cap = 8, n = 0;
    KValue* items = k_alloc(sizeof(KValue) * cap);
    kjp_ws(p);
    if (kjp_peek(p) == ']') { p->pos++; KValue l = k_list(items, 0); return k_jc_("JArr", &l, 1); }
    for (;;) {
        if (n >= cap) { int nc = cap * 2; KValue* ni = k_alloc(sizeof(KValue) * nc); memcpy(ni, items, sizeof(KValue) * n); items = ni; cap = nc; }
        items[n++] = kjp_value(p);
        if (p->failed) break;
        kjp_ws(p);
        int c = p->pos < p->len ? p->s[p->pos++] : -1;
        if (c == ',') continue;
        if (c == ']') break;
        p->failed = 1; break;
    }
    KValue l = k_list(items, n);
    return k_jc_("JArr", &l, 1);
}
static KValue kjp_object(KJP* p) {
    p->pos++;  /* '{' */
    int cap = 8, n = 0;
    KValue* keys = k_alloc(sizeof(KValue) * cap);
    KValue* vals = k_alloc(sizeof(KValue) * cap);
    kjp_ws(p);
    if (kjp_peek(p) == '}') { p->pos++; KMap* m = k_alloc(sizeof(KMap)); m->len = 0; m->keys = k_alloc(1); m->vals = k_alloc(1); KValue mv; mv.tag = K_MAP; mv.as.map = m; return k_jc_("JObj", &mv, 1); }
    for (;;) {
        kjp_ws(p);
        if (kjp_peek(p) != '"') { p->failed = 1; break; }
        char* key = kjp_string(p);
        if (p->failed) break;
        kjp_ws(p);
        if (!(p->pos < p->len && p->s[p->pos++] == ':')) { p->failed = 1; break; }
        KValue val = kjp_value(p);
        if (p->failed) break;
        KValue kv = k_str(key);
        int found = -1;
        for (int i = 0; i < n; i++) if (!strcmp(keys[i].as.s, key)) { found = i; break; }
        if (found >= 0) vals[found] = val;
        else {
            if (n >= cap) {
                int nc = cap * 2;
                KValue* nk = k_alloc(sizeof(KValue) * nc); memcpy(nk, keys, sizeof(KValue) * n); keys = nk;
                KValue* nv = k_alloc(sizeof(KValue) * nc); memcpy(nv, vals, sizeof(KValue) * n); vals = nv;
                cap = nc;
            }
            keys[n] = kv; vals[n] = val; n++;
        }
        kjp_ws(p);
        int c = p->pos < p->len ? p->s[p->pos++] : -1;
        if (c == ',') continue;
        if (c == '}') break;
        p->failed = 1; break;
    }
    KMap* m = k_alloc(sizeof(KMap));
    m->len = n; m->keys = k_alloc(sizeof(KValue) * (n < 1 ? 1 : n)); m->vals = k_alloc(sizeof(KValue) * (n < 1 ? 1 : n));
    memcpy(m->keys, keys, sizeof(KValue) * n); memcpy(m->vals, vals, sizeof(KValue) * n);
    KValue mv; mv.tag = K_MAP; mv.as.map = m; return k_jc_("JObj", &mv, 1);
}
static KValue kjp_value(KJP* p) {
    kjp_ws(p);
    int c = kjp_peek(p);
    if (c == '{' || c == '[') {
        if (++p->depth > K_MAX_JSON_DEPTH) { p->failed = 1; return k_unit(); }
        KValue v = (c == '{') ? kjp_object(p) : kjp_array(p);
        p->depth--;
        return v;
    }
    if (c == '"') { char* s = kjp_string(p); KValue sv = k_str(s); return k_jc_("JStr", &sv, 1); }
    if (c == 't') { if (p->pos+4<=p->len && !memcmp(p->s+p->pos,"true",4)) { p->pos+=4; KValue b=k_bool(1); return k_jc_("JBool",&b,1);} p->failed=1; return k_unit(); }
    if (c == 'f') { if (p->pos+5<=p->len && !memcmp(p->s+p->pos,"false",5)) { p->pos+=5; KValue b=k_bool(0); return k_jc_("JBool",&b,1);} p->failed=1; return k_unit(); }
    if (c == 'n') { if (p->pos+4<=p->len && !memcmp(p->s+p->pos,"null",4)) { p->pos+=4; return k_jc_("JNull",0,0);} p->failed=1; return k_unit(); }
    if (c == '-' || (c >= '0' && c <= '9')) return kjp_number(p);
    p->failed = 1; return k_unit();
}
static KValue k_json_parse(KValue s) {
    if (s.tag != K_STR) { const char* d = k_show(s); (void)d; }
    const char* str = (s.tag == K_STR) ? s.as.s : k_show(s);
    KJP p; p.s = (const unsigned char*)str; p.pos = 0; p.len = (long)strlen(str); p.failed = 0; p.depth = 0;
    kjp_ws(&p);
    KValue v = kjp_value(&p);
    kjp_ws(&p);
    if (p.failed || p.pos != p.len) return k_err(k_str("invalid JSON"));
    return k_ok(v);
}

/* ---- URL + CSV (mirror src/url.rs / src/csv.rs byte-for-byte) ---- */
static char* kb_take(KBuf* b) { return b->buf ? b->buf : (char*)""; }
static int k_hexval(int c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}
/* validate that `n` bytes are well-formed UTF-8 (mirrors String::from_utf8) */
static int k_utf8_ok(const unsigned char* s, long n) {
    long i = 0;
    while (i < n) {
        unsigned char c = s[i];
        int extra; unsigned int cp;
        if (c < 0x80) { i++; continue; }
        else if ((c & 0xE0) == 0xC0) { extra = 1; cp = c & 0x1F; }
        else if ((c & 0xF0) == 0xE0) { extra = 2; cp = c & 0x0F; }
        else if ((c & 0xF8) == 0xF0) { extra = 3; cp = c & 0x07; }
        else return 0;
        if (i + extra >= n) return 0;
        for (int k = 1; k <= extra; k++) { if ((s[i + k] & 0xC0) != 0x80) return 0; cp = (cp << 6) | (s[i + k] & 0x3F); }
        if (cp < (extra == 1 ? 0x80u : extra == 2 ? 0x800u : 0x10000u)) return 0; /* overlong */
        if (cp > 0x10FFFF || (cp >= 0xD800 && cp <= 0xDFFF)) return 0;
        i += extra + 1;
    }
    return 1;
}
static const char* k_as_str(KValue v) { return v.tag == K_STR ? v.as.s : k_show(v); }
static KValue k_url_encode(KValue s);   /* defined later in the runtime */

/* decode into `b`; returns "" on success or an error message (mirrors url.rs) */
static const char* k_url_decode_into(KBuf* b, const char* in) {
    const unsigned char* p = (const unsigned char*)in;
    long i = 0, n = (long)strlen(in);
    while (i < n) {
        if (p[i] == '%') {
            if (i + 2 >= n) return "invalid percent-encoding: truncated escape";
            int hi = k_hexval(p[i + 1]), lo = k_hexval(p[i + 2]);
            if (hi < 0 || lo < 0) return "invalid percent-encoding: bad hex";
            kb_putc(b, (char)((hi << 4) | lo)); i += 3;
        } else if (p[i] == '+') { kb_putc(b, ' '); i++; }
        else { kb_putc(b, (char)p[i]); i++; }
    }
    return "";
}
/* a decoded string that falls back to the raw text on a malformed escape */
static KValue k_url_decode_lossy(const char* seg) {
    KBuf b = { 0, 0, 0 };
    const char* err = k_url_decode_into(&b, seg);
    if (err[0] || !k_utf8_ok((const unsigned char*)kb_take(&b), (long)b.len)) {
        char* c = (char*)k_alloc(strlen(seg) + 1); strcpy(c, seg); return k_str(c);
    }
    return k_str(kb_take(&b));
}
static KValue k_query_parse(KValue s) {
    const char* in = k_as_str(s);
    KValue pairs[4096]; int np = 0;
    long start = 0, n = (long)strlen(in), i = 0;
    for (i = 0; i <= n; i++) {
        if (i == n || in[i] == '&') {
            long seglen = i - start;
            if (seglen > 0) {
                char* seg = (char*)k_alloc(seglen + 1); memcpy(seg, in + start, seglen); seg[seglen] = 0;
                char* eq = strchr(seg, '=');
                KValue kv, vv;
                if (eq) { *eq = 0; kv = k_url_decode_lossy(seg); vv = k_url_decode_lossy(eq + 1); }
                else { kv = k_url_decode_lossy(seg); vv = k_str(""); }
                KValue pair[2] = { kv, vv };
                if (np < 4096) pairs[np++] = k_list(pair, 2);
            }
            start = i + 1;
        }
    }
    return k_list(pairs, np);
}
static KValue k_query_build(KValue lst) {
    if (lst.tag != K_LIST) k_panic("query_build needs a list");
    KBuf b = { 0, 0, 0 };
    KList* rows = lst.as.list;
    for (int64_t r = 0; r < rows->len; r++) {
        if (r) kb_putc(&b, '&');
        KList* pair = rows->items[r].as.list;
        KValue kv = pair->len > 0 ? pair->items[0] : k_str("");
        KValue vv = pair->len > 1 ? pair->items[1] : k_str("");
        KValue ek = k_url_encode(kv), ev = k_url_encode(vv);
        kb_puts(&b, ek.as.s); kb_putc(&b, '='); kb_puts(&b, ev.as.s);
    }
    return k_str(kb_take(&b));
}
static KValue k_csv_parse(KValue s) {
    const char* in = k_as_str(s);
    long n = (long)strlen(in), i = 0;
    KValue rows[4096]; int nrows = 0;
    KValue row[4096]; int ncols = 0;
    KBuf field = { 0, 0, 0 };
    while (i < n) {
        char c = in[i];
        if (c == '"') {
            i++;
            for (;;) {
                if (i >= n) break;
                char q = in[i];
                if (q == '"') {
                    if (i + 1 < n && in[i + 1] == '"') { kb_putc(&field, '"'); i += 2; }
                    else { i++; break; }
                } else { kb_putc(&field, q); i++; }
            }
        } else if (c == ',') {
            if (ncols < 4096) row[ncols++] = k_str(kb_take(&field));
            field = (KBuf){ 0, 0, 0 }; i++;
        } else if (c == '\n' || c == '\r') {
            if (c == '\r' && i + 1 < n && in[i + 1] == '\n') i++;
            if (ncols < 4096) row[ncols++] = k_str(kb_take(&field));
            field = (KBuf){ 0, 0, 0 };
            if (nrows < 4096) rows[nrows++] = k_list(row, ncols);
            ncols = 0; i++;
        } else { kb_putc(&field, c); i++; }
    }
    // flush the final field/record unless input ended exactly on a newline
    if (field.len > 0 || ncols > 0) {
        if (ncols < 4096) row[ncols++] = k_str(kb_take(&field));
        if (nrows < 4096) rows[nrows++] = k_list(row, ncols);
    }
    return k_list(rows, nrows);
}
static void k_csv_field(KBuf* b, const char* f) {
    int needs = 0;
    for (const char* p = f; *p; p++) if (*p == ',' || *p == '"' || *p == '\n' || *p == '\r') { needs = 1; break; }
    if (needs) {
        kb_putc(b, '"');
        for (const char* p = f; *p; p++) { if (*p == '"') kb_putc(b, '"'); kb_putc(b, *p); }
        kb_putc(b, '"');
    } else kb_puts(b, f);
}
static KValue k_csv_stringify(KValue lst) {
    if (lst.tag != K_LIST) k_panic("csv_stringify needs a list");
    KBuf b = { 0, 0, 0 };
    KList* rows = lst.as.list;
    for (int64_t r = 0; r < rows->len; r++) {
        if (r) kb_putc(&b, '\n');
        KList* row = rows->items[r].as.list;
        for (int64_t c = 0; c < row->len; c++) {
            if (c) kb_putc(&b, ',');
            k_csv_field(&b, row->items[c].as.s);
        }
    }
    return k_str(kb_take(&b));
}

/* ---- HTTP (mirror interp::http_builtin — shell out to `curl`) ---- */
static void kb_write(KBuf* b, const char* s, long n) { kb_grow(b, n); memcpy(b->buf + b->len, s, n); b->len += n; b->buf[b->len] = 0; }
/* run curl via fork/exec (no shell — argv matches the interpreter's Command);
   optional `body` is piped to stdin. Ok(stdout) on exit 0, else Err(stderr|msg). */
static KValue k_run_curl(char* const argv[], const char* body) {
    int outp[2], errp[2], inp[2] = { -1, -1 };
    if (pipe(outp) || pipe(errp) || (body && pipe(inp))) return k_err(k_str("cannot run curl: pipe failed"));
    pid_t pid = fork();
    if (pid < 0) return k_err(k_str("cannot run curl: fork failed"));
    if (pid == 0) {
        dup2(outp[1], 1); dup2(errp[1], 2);
        if (body) dup2(inp[0], 0);
        close(outp[0]); close(outp[1]); close(errp[0]); close(errp[1]);
        if (body) { close(inp[0]); close(inp[1]); }
        execvp("curl", argv);
        _exit(127);
    }
    close(outp[1]); close(errp[1]);
    if (body) { close(inp[0]); long bl = (long)strlen(body); long off = 0; while (off < bl) { ssize_t w = write(inp[1], body + off, bl - off); if (w <= 0) break; off += w; } close(inp[1]); }
    KBuf out = { 0, 0, 0 }, er = { 0, 0, 0 };
    char buf[4096]; ssize_t n;
    while ((n = read(outp[0], buf, sizeof buf)) > 0) kb_write(&out, buf, n);
    while ((n = read(errp[0], buf, sizeof buf)) > 0) kb_write(&er, buf, n);
    close(outp[0]); close(errp[0]);
    int status = 0; waitpid(pid, &status, 0);
    int code = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
    if (code == 127) return k_err(k_str("cannot run curl: command not found"));
    if (code != 0) {
        char* e = er.buf ? er.buf : (char*)"";
        while (*e == ' ' || *e == '\t' || *e == '\n' || *e == '\r') e++;      /* trim start */
        long len = (long)strlen(e);
        while (len > 0 && (e[len-1] == ' ' || e[len-1] == '\t' || e[len-1] == '\n' || e[len-1] == '\r')) e[--len] = 0;
        if (len > 0) return k_err(k_str(e));
        char m[64]; snprintf(m, sizeof m, "request failed (curl exit %d)", code);
        return k_err(k_str(m));
    }
    return k_ok(k_str(out.buf ? out.buf : (char*)""));
}
/* exec(program, args) — fork/execvp an arbitrary program (no shell), capture
   stdout. Ok(stdout) on exit 0, else Err(trimmed stderr | "exited with status N"
   | "cannot run …"). Mirrors interp::exec_builtin's decision + shape. */
static KValue k_exec(KValue prog, KValue arglist) {
    const char* program = prog.as.s;
    KList* al = arglist.as.list;
    int argc = (int)al->len;
    char** argv = (char**)k_alloc(sizeof(char*) * (argc + 2));
    argv[0] = (char*)program;
    for (int i = 0; i < argc; i++) argv[i + 1] = (char*)al->items[i].as.s;
    argv[argc + 1] = 0;
    int outp[2], errp[2];
    if (pipe(outp) || pipe(errp)) return k_err(k_str("cannot run: pipe failed"));
    pid_t pid = fork();
    if (pid < 0) return k_err(k_str("cannot run: fork failed"));
    if (pid == 0) {
        dup2(outp[1], 1); dup2(errp[1], 2);
        close(outp[0]); close(outp[1]); close(errp[0]); close(errp[1]);
        execvp(program, argv);
        _exit(127);
    }
    close(outp[1]); close(errp[1]);
    KBuf out = { 0, 0, 0 }, er = { 0, 0, 0 };
    char buf[4096]; ssize_t n;
    while ((n = read(outp[0], buf, sizeof buf)) > 0) kb_write(&out, buf, n);
    while ((n = read(errp[0], buf, sizeof buf)) > 0) kb_write(&er, buf, n);
    close(outp[0]); close(errp[0]);
    int status = 0; waitpid(pid, &status, 0);
    int code = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
    if (code == 127) {
        char m[300]; snprintf(m, sizeof m, "cannot run %s: command not found", program);
        return k_err(k_str(k_strdup(m)));
    }
    if (code != 0) {
        char* e = er.buf ? er.buf : (char*)"";
        while (*e == ' ' || *e == '\t' || *e == '\n' || *e == '\r') e++;
        long len = (long)strlen(e);
        while (len > 0 && (e[len-1] == ' ' || e[len-1] == '\t' || e[len-1] == '\n' || e[len-1] == '\r')) e[--len] = 0;
        if (len > 0) return k_err(k_str(e));
        char m[64]; snprintf(m, sizeof m, "exited with status %d", code);
        return k_err(k_str(k_strdup(m)));
    }
    return k_ok(k_str(out.buf ? out.buf : (char*)""));
}

/* ---- pure `/`-path helpers (mirror interp::path_builtin) ---- */
static KValue k_path_join(KValue a, KValue b) {
    const char* pa = a.as.s; const char* pb = b.as.s;
    if (!pa[0]) return k_str(k_strdup(pb));
    if (pb[0] == '/') return k_str(k_strdup(pb));
    long la = (long)strlen(pa);
    while (la > 0 && pa[la - 1] == '/') la--;
    KBuf buf = { 0, 0, 0 };
    kb_write(&buf, pa, la);
    kb_write(&buf, "/", 1);
    kb_write(&buf, pb, (long)strlen(pb));
    return k_str(buf.buf ? buf.buf : "");
}
static KValue k_path_base(KValue p) {
    const char* slash = strrchr(p.as.s, '/');
    return k_str(k_strdup(slash ? slash + 1 : p.as.s));
}
static KValue k_path_dir(KValue p) {
    const char* s = p.as.s;
    const char* slash = strrchr(s, '/');
    if (!slash) return k_str("");
    long n = slash - s;
    char* c = (char*)k_alloc(n + 1); memcpy(c, s, n); c[n] = 0;
    return k_str(c);
}
static KValue k_path_ext(KValue p) {
    const char* s = p.as.s;
    const char* slash = strrchr(s, '/');
    const char* base = slash ? slash + 1 : s;
    const char* dot = strrchr(base, '.');
    if (dot && dot > base) return k_str(k_strdup(dot));
    return k_str("");
}

/* ---- directory ops (mirror interp::fs_builtin; list_dir is SORTED) ---- */
static int k_cmp_cstr(const void* a, const void* b) { return strcmp(*(const char* const*)a, *(const char* const*)b); }
static KValue k_list_dir(KValue path) {
    DIR* d = opendir(path.as.s);
    if (!d) return k_os_error(); /* matches interp's fs::read_dir io::Error */
    char** names = (char**)k_alloc(sizeof(char*) * 8192); int n = 0;
    struct dirent* e;
    while ((e = readdir(d)) && n < 8192) {
        if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, "..")) continue;
        names[n++] = k_strdup(e->d_name);
    }
    closedir(d);
    qsort(names, n, sizeof(char*), k_cmp_cstr);
    KValue* out = (KValue*)k_alloc(sizeof(KValue) * (n < 1 ? 1 : n));
    for (int i = 0; i < n; i++) out[i] = k_str(names[i]);
    return k_ok(k_list(out, n));
}
static int k_mkdirs(const char* path) {
    char tmp[4096];
    snprintf(tmp, sizeof tmp, "%s", path);
    long len = (long)strlen(tmp);
    if (len > 0 && tmp[len - 1] == '/') tmp[len - 1] = 0;
    for (char* q = tmp + 1; *q; q++) {
        if (*q == '/') { *q = 0; mkdir(tmp, 0777); *q = '/'; }
    }
    int r = mkdir(tmp, 0777);
    if (r == 0) return 0;
    /* EEXIST is success ONLY if the path is already a directory — mirrors interp's
       create_dir_all. If a FILE exists there, it's an error ("File exists"). */
    if (errno == EEXIST) {
        struct stat st;
        if (stat(tmp, &st) == 0 && S_ISDIR(st.st_mode)) return 0;
        errno = EEXIST;
    }
    return -1;
}
static KValue k_make_dir(KValue path) {
    if (k_mkdirs(path.as.s) != 0) return k_os_error(); /* matches interp io::Error */
    return k_ok(k_unit());
}
static int k_rmrf(const char* path) {
    DIR* d = opendir(path);
    if (d) {
        struct dirent* e;
        while ((e = readdir(d))) {
            if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, "..")) continue;
            char child[4096]; snprintf(child, sizeof child, "%s/%s", path, e->d_name);
            struct stat st;
            if (!stat(child, &st) && S_ISDIR(st.st_mode)) k_rmrf(child);
            else unlink(child);
        }
        closedir(d);
    }
    return rmdir(path);
}
static KValue k_remove_dir(KValue path) {
    /* k_rmrf mirrors interp's fs::remove_dir_all (recursive); the final rmdir sets
       errno (ENOTDIR on a file, ENOENT on missing) so the message matches interp. */
    if (k_rmrf(path.as.s) != 0) return k_os_error();
    return k_ok(k_unit());
}

/* http_serve(port, handler): a blocking HTTP server mirroring
   interp::serve_http. Binds 127.0.0.1:port, and for each request calls the KUPL
   handler value with (method, path) to get the response body. Err on bind
   failure; otherwise never returns. */
static KValue k_http_serve(KValue port, KValue handler) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) { char m[64]; snprintf(m, sizeof m, "cannot bind 127.0.0.1:%lld: socket", (long long)port.as.i); return k_err(k_str(k_strdup(m))); }
    int one = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof addr);
    addr.sin_family = AF_INET;
    addr.sin_port = htons((unsigned short)port.as.i);
    addr.sin_addr.s_addr = inet_addr("127.0.0.1");
    if (bind(fd, (struct sockaddr*)&addr, sizeof addr) < 0 || listen(fd, 128) < 0) {
        close(fd);
        char m[64]; snprintf(m, sizeof m, "cannot bind 127.0.0.1:%lld: in use", (long long)port.as.i);
        return k_err(k_str(k_strdup(m)));
    }
    for (;;) {
        int conn = accept(fd, 0, 0);
        if (conn < 0) continue;
        /* read the request head until the blank line (or a 64KB cap) */
        KBuf head = { 0, 0, 0 };
        char buf[1024]; ssize_t n;
        while ((n = read(conn, buf, sizeof buf)) > 0) {
            kb_write(&head, buf, n);
            if (head.len >= 4 && strstr(head.buf, "\r\n\r\n")) break;
            if (head.len > 64 * 1024) break;
        }
        /* parse the request line: METHOD PATH HTTP/1.1 */
        const char* method = "GET";
        const char* path = "/";
        if (head.buf) {
            char* line = head.buf;
            char* eol = strstr(line, "\r\n");
            if (eol) *eol = 0;
            char* sp1 = strchr(line, ' ');
            if (sp1) {
                *sp1 = 0; method = k_strdup(line);
                char* p = sp1 + 1;
                char* sp2 = strchr(p, ' ');
                if (sp2) *sp2 = 0;
                path = k_strdup(p);
            }
        }
        KValue hargs[2] = { k_str(method), k_str(path) };
        KValue rv = k_call(handler, hargs, 2);
        const char* body = rv.tag == K_STR ? rv.as.s : "";
        KBuf resp = { 0, 0, 0 };
        char hdr[192];
        int hn = snprintf(hdr, sizeof hdr,
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: %zu\r\nConnection: close\r\n\r\n",
            strlen(body));
        kb_write(&resp, hdr, hn);
        kb_write(&resp, body, (long)strlen(body));
        long off = 0;
        while (off < resp.len) { ssize_t w = write(conn, resp.buf + off, resp.len - off); if (w <= 0) break; off += w; }
        close(conn);
    }
}

static KValue k_http_get(KValue url) {
    char* argv[] = { (char*)"curl", (char*)"-sS", (char*)"--fail", (char*)"--max-time", (char*)"30", (char*)k_as_str(url), 0 };
    return k_run_curl(argv, 0);
}
static KValue k_http_post(KValue url, KValue body) {
    char* argv[] = { (char*)"curl", (char*)"-sS", (char*)"--fail", (char*)"--max-time", (char*)"30", (char*)"-X", (char*)"POST", (char*)"--data-binary", (char*)"@-", (char*)k_as_str(url), 0 };
    return k_run_curl(argv, k_as_str(body));
}

/* ---- ai fun (mock/deterministic path; mirrors src/ai.rs convert +
   value_from_json). Real-provider HTTP and tool-use rounds defer. ---- */
typedef struct KAiShape KAiShape;
struct KAiShape {
    int kind;                 /* 0 Str 1 Int 2 Float 3 Bool 4 List 5 Option 6 Record */
    const KAiShape* inner;    /* List/Option element */
    const char* variant;      /* Record: the constructor name */
    const char* const* fnames; const KAiShape* const* fshapes; int nfields;  /* Record */
};
typedef struct { const char* name; int fnid; const char* const* pnames; const KAiShape* const* pshapes; int nparams; } KAiTool;
typedef struct { const char* name; const char* mock_key; const KAiShape* shape; int wraps_result; int has_tools; const KAiTool* tools; int ntools; } KAiFun;
extern const KAiFun AI_FUNS[];

static int k_ai_ok = 1;
static char k_ai_err[256];
static const char* k_json_var(KValue j) { return j.tag == K_CTOR ? CTORS[j.as.ctor->ctor].variant : "?"; }
static KValue k_json_field0(KValue j) { return j.as.ctor->fields[0]; }

/* shape-directed conversion of a parsed Json value into the declared type */
static KValue k_ai_from_json(const KAiShape* s, KValue j) {
    const char* v = k_json_var(j);
    switch (s->kind) {
        case 0: if (!strcmp(v, "JStr")) return k_json_field0(j); break;
        case 1:
            if (!strcmp(v, "JNum")) {
                double n = k_json_field0(j).as.f;
                // A whole number that fits exactly in i64. Out-of-range values are
                // REJECTED (not saturated to i64::MAX) — matching the interpreter,
                // which rejects an integer the model returns that overflows i64.
                if (isfinite(n) && n == floor(n)
                    && n >= -9223372036854775808.0 && n < 9223372036854775808.0)
                    return k_int((int64_t)n);
                if (isfinite(n) && n == floor(n))
                    snprintf(k_ai_err, sizeof k_ai_err, "expected an integer, model returned %.0f", n);
                else
                    snprintf(k_ai_err, sizeof k_ai_err, "expected an integer, model returned a fraction");
                k_ai_ok = 0; return k_unit();
            }
            break;
        case 2: if (!strcmp(v, "JNum")) return k_float(k_json_field0(j).as.f); break;
        case 3: if (!strcmp(v, "JBool")) return k_json_field0(j); break;
        case 4:
            if (!strcmp(v, "JArr")) {
                KList* items = k_json_field0(j).as.list;
                KValue* out = (KValue*)k_alloc(sizeof(KValue) * (items->len < 1 ? 1 : items->len));
                for (int64_t i = 0; i < items->len; i++) {
                    out[i] = k_ai_from_json(s->inner, items->items[i]);
                    if (!k_ai_ok) return k_unit();
                }
                return k_list(out, (int)items->len);
            }
            break;
        case 5:
            if (!strcmp(v, "JNull")) return k_none();
            { KValue in = k_ai_from_json(s->inner, j); if (!k_ai_ok) return k_unit(); return k_some(in); }
        case 6:
            if (!strcmp(v, "JObj")) {
                KMap* m = k_json_field0(j).as.map;
                KValue* vals = (KValue*)k_alloc(sizeof(KValue) * (s->nfields < 1 ? 1 : s->nfields));
                for (int i = 0; i < s->nfields; i++) {
                    int found = 0; KValue fj = k_unit();
                    for (int64_t k = 0; k < m->len; k++)
                        if (!strcmp(m->keys[k].as.s, s->fnames[i])) { fj = m->vals[k]; found = 1; break; }
                    if (!found) { snprintf(k_ai_err, sizeof k_ai_err, "model response is missing field `%s`", s->fnames[i]); k_ai_ok = 0; return k_unit(); }
                    vals[i] = k_ai_from_json(s->fshapes[i], fj);
                    if (!k_ai_ok) return k_unit();
                }
                return k_ctor(k_ctor_by_name(s->variant), vals, s->nfields);
            }
            break;
    }
    snprintf(k_ai_err, sizeof k_ai_err, "model response does not match the declared type");
    k_ai_ok = 0; return k_unit();
}

/* strip a leading ```json fence (mirrors ai.rs::strip_fences); returns a copy */
static const char* k_ai_strip(const char* text) {
    const char* t = text;
    while (*t == ' ' || *t == '\t' || *t == '\n' || *t == '\r') t++;
    long n = (long)strlen(t);
    while (n > 0 && (t[n-1] == ' ' || t[n-1] == '\t' || t[n-1] == '\n' || t[n-1] == '\r')) n--;
    char* s = (char*)k_alloc(n + 1); memcpy(s, t, n); s[n] = 0;
    if (strncmp(s, "```", 3) != 0) return s;
    char* p = s + 3;
    if (strncmp(p, "json", 4) == 0) p += 4;
    while (*p == '\r' || *p == '\n') p++;
    long pn = (long)strlen(p);
    if (pn >= 3 && strcmp(p + pn - 3, "```") == 0) p[pn - 3] = 0;
    /* trim */
    while (*p == ' ' || *p == '\t' || *p == '\n' || *p == '\r') p++;
    pn = (long)strlen(p);
    while (pn > 0 && (p[pn-1] == ' ' || p[pn-1] == '\t' || p[pn-1] == '\n' || p[pn-1] == '\r')) p[--pn] = 0;
    return p;
}

static KValue k_ai_convert(const KAiShape* shape, const char* text) {
    k_ai_ok = 1;
    if (shape->kind == 0) {   /* -> Str: return the trimmed text */
        const char* t = text;
        while (*t == ' ' || *t == '\t' || *t == '\n' || *t == '\r') t++;
        long n = (long)strlen(t);
        while (n > 0 && (t[n-1] == ' ' || t[n-1] == '\t' || t[n-1] == '\n' || t[n-1] == '\r')) n--;
        char* c = (char*)k_alloc(n + 1); memcpy(c, t, n); c[n] = 0; return k_str(c);
    }
    const char* payload = k_ai_strip(text);
    KValue parsed = k_json_parse(k_str(payload));
    if (!strcmp(CTORS[parsed.as.ctor->ctor].variant, "Err")) {
        snprintf(k_ai_err, sizeof k_ai_err, "model response is not valid JSON"); k_ai_ok = 0; return k_unit();
    }
    KValue json = k_json_field0(parsed);
    /* accept a {"value": …} wrapper or a bare payload */
    KValue inner = json;
    if (!strcmp(k_json_var(json), "JObj")) {
        KMap* m = k_json_field0(json).as.map;
        for (int64_t k = 0; k < m->len; k++) if (!strcmp(m->keys[k].as.s, "value")) { inner = m->vals[k]; break; }
    }
    KValue v = k_ai_from_json(shape, inner);
    if (k_ai_ok) return v;
    /* retry against the whole object (mirrors convert's or_else) */
    char first[256]; snprintf(first, sizeof first, "%s", k_ai_err);
    k_ai_ok = 1;
    KValue v2 = k_ai_from_json(shape, json);
    if (!k_ai_ok) snprintf(k_ai_err, sizeof k_ai_err, "%s", first);
    return v2;
}

static const char* k_getenv_ne(const char* key) { const char* v = getenv(key); return (v && v[0]) ? v : 0; }

/* look up a field in a Json object's map; returns 1 + sets *out when present */
static int k_map_field(KMap* m, const char* key, KValue* out) {
    for (int64_t k = 0; k < m->len; k++)
        if (!strcmp(m->keys[k].as.s, key)) { *out = m->vals[k]; return 1; }
    return 0;
}

/* the tool-use mock path (mirrors ai.rs::tool_response + run_tool_loop with the
   MockProvider): the mock env value is a JSON array of rounds — {"tool": …} calls
   a KUPL function for its side effects (result discarded, as the mock ignores it)
   and {"final": …} ends the loop. The final text is converted via the return
   shape exactly like the non-tool path. */
static KValue k_ai_tool_call(const KAiFun* f, const char* script) {
    k_ai_ok = 1;
    KValue parsed = k_json_parse(k_str(script));
    const char* final_text = 0;
    if (strcmp(CTORS[parsed.as.ctor->ctor].variant, "Ok")) {
        final_text = script;                 /* parse failure => single final = raw script */
    } else {
        KValue j = k_json_field0(parsed);
        if (strcmp(k_json_var(j), "JArr")) { /* bare value => single final */
            final_text = !strcmp(k_json_var(j), "JStr") ? k_json_field0(j).as.s : k_json_stringify(j).as.s;
        } else {
            KList* rounds = k_json_field0(j).as.list;
            long limit = rounds->len < 8 ? (long)rounds->len : 8;
            for (long i = 0; i < limit; i++) {
                KValue r = rounds->items[i];
                if (strcmp(k_json_var(r), "JObj")) { snprintf(k_ai_err, sizeof k_ai_err, "mock round must be `{\"tool\": ...}` or `{\"final\": ...}`"); k_ai_ok = 0; return k_unit(); }
                KMap* m = k_json_field0(r).as.map;
                KValue fin;
                if (k_map_field(m, "final", &fin)) {
                    final_text = !strcmp(k_json_var(fin), "JStr") ? fin.as.ctor->fields[0].as.s : k_json_stringify(fin).as.s;
                    break;
                }
                KValue tn;
                if (!k_map_field(m, "tool", &tn) || strcmp(k_json_var(tn), "JStr")) { snprintf(k_ai_err, sizeof k_ai_err, "mock round must be `{\"tool\": ...}` or `{\"final\": ...}`"); k_ai_ok = 0; return k_unit(); }
                const char* name = tn.as.ctor->fields[0].as.s;
                const KAiTool* t = 0;
                for (int k = 0; k < f->ntools; k++) if (!strcmp(f->tools[k].name, name)) { t = &f->tools[k]; break; }
                if (!t) { snprintf(k_ai_err, sizeof k_ai_err, "model called unknown tool `%s`", name); k_ai_ok = 0; return k_unit(); }
                KValue input; int has_input = k_map_field(m, "input", &input);
                KMap* im = (has_input && !strcmp(k_json_var(input), "JObj")) ? k_json_field0(input).as.map : 0;
                KValue* targs = (KValue*)k_alloc(sizeof(KValue) * (t->nparams < 1 ? 1 : t->nparams));
                for (int p = 0; p < t->nparams; p++) {
                    KValue pj;
                    if (!im || !k_map_field(im, t->pnames[p], &pj)) { snprintf(k_ai_err, sizeof k_ai_err, "tool `%s` is missing argument `%s`", name, t->pnames[p]); k_ai_ok = 0; return k_unit(); }
                    targs[p] = k_ai_from_json(t->pshapes[p], pj);
                    if (!k_ai_ok) return k_unit();
                }
                k_call(k_fun(t->fnid), targs, t->nparams);   /* side effects; result discarded (mock) */
            }
            if (!final_text) { snprintf(k_ai_err, sizeof k_ai_err, "mock provider ran out of scripted rounds"); k_ai_ok = 0; return k_unit(); }
        }
    }
    return k_ai_convert(f->shape, final_text);
}

static KValue k_ai_call(int info) {
    const KAiFun* f = &AI_FUNS[info];
    const char* text = k_getenv_ne(f->mock_key);
    if (!text) text = k_getenv_ne("KUPL_AI_MOCK");
    if (!text) {
        const char* msg = "native `ai fun` requires a mock (KUPL_AI_MOCK or the per-function var); real providers via `kupl bundle`";
        if (f->wraps_result) return k_err(k_str(msg));
        k_panic(msg); return k_unit();
    }
    KValue v = f->has_tools ? k_ai_tool_call(f, text) : k_ai_convert(f->shape, text);
    if (k_ai_ok) return f->wraps_result ? k_ok(v) : v;
    if (f->wraps_result) return k_err(k_str(k_strdup(k_ai_err)));
    char b[320]; snprintf(b, sizeof b, "ai `%s`: %s", f->name, k_ai_err); k_panic(b); return k_unit();
}

/* ---- regex (mirrors src/regex.rs; byte-oriented — ASCII-correct, which is
   what the KUPL regex examples use; multi-byte class ranges would differ) ---- */
typedef struct KReAtom KReAtom;
typedef struct { KReAtom* atom; int quant; } KRePiece;    /* quant: 0 One 1 * 2 + 3 ? */
typedef struct { KRePiece* p; int n; } KReSeq;
typedef struct { KReSeq* a; int n; } KReAlts;
typedef struct { unsigned char lo, hi; } KReRange;
struct KReAtom {
    int kind;                 /* 0 Any, 1 Char, 2 Class, 3 Group */
    unsigned char ch;
    int negated; KReRange* ranges; int nranges;
    KReAlts group;
};
typedef struct { KReAlts alts; int astart, aend; } KRegex;
typedef struct { const unsigned char* s; int pos, len, aend, err; const char* msg; } KReP;

static KReAlts kre_alternation(KReP* p);
static void kre_fail(KReP* p, const char* m) { if (!p->err) { p->err = 1; p->msg = m; } }
static int kre_peek(KReP* p) { return p->pos < p->len ? p->s[p->pos] : -1; }

static void kre_class(KReP* p, KReAtom* a) {
    p->pos++;                 /* '[' */
    a->kind = 2; a->negated = 0; a->ranges = 0; a->nranges = 0;
    int cap = 0;
    if (kre_peek(p) == '^') { a->negated = 1; p->pos++; }
    #define REPUSH(LO, HI) do { if (a->nranges == cap) { cap = cap ? cap * 2 : 8; a->ranges = (KReRange*)realloc(a->ranges, sizeof(KReRange) * cap); } a->ranges[a->nranges].lo = (LO); a->ranges[a->nranges].hi = (HI); a->nranges++; } while (0)
    if (kre_peek(p) == ']') { REPUSH(']', ']'); p->pos++; }
    for (;;) {
        int c = kre_peek(p);
        if (c < 0) { kre_fail(p, "unclosed character class `[`"); return; }
        if (c == ']') { p->pos++; break; }
        if (c == '\\') {
            p->pos++;
            int e = kre_peek(p);
            if (e < 0) { kre_fail(p, "dangling `\\` in class"); return; }
            p->pos++;
            switch (e) {
                case 'd': REPUSH('0', '9'); break;
                case 'w': REPUSH('a', 'z'); REPUSH('A', 'Z'); REPUSH('0', '9'); REPUSH('_', '_'); break;
                case 's': REPUSH(' ', ' '); REPUSH('\t', '\t'); REPUSH('\n', '\n'); REPUSH('\r', '\r'); break;
                case 'n': REPUSH('\n', '\n'); break;
                case 't': REPUSH('\t', '\t'); break;
                case 'r': REPUSH('\r', '\r'); break;
                default: REPUSH((unsigned char)e, (unsigned char)e); break;
            }
        } else {
            p->pos++;
            /* range lo-hi when `-` is followed by a non-`]` */
            if (kre_peek(p) == '-' && p->pos + 1 < p->len && p->s[p->pos + 1] != ']') {
                p->pos++;
                int hi = kre_peek(p); p->pos++;
                if ((unsigned char)c <= (unsigned char)hi) REPUSH((unsigned char)c, (unsigned char)hi);
                else REPUSH((unsigned char)hi, (unsigned char)c);
            } else {
                REPUSH((unsigned char)c, (unsigned char)c);
            }
        }
    }
    #undef REPUSH
}

static KReAtom* kre_atom(KReP* p) {
    KReAtom* a = (KReAtom*)k_alloc(sizeof(KReAtom));
    memset(a, 0, sizeof(KReAtom));
    int c = kre_peek(p);
    if (c == '(') {
        p->pos++; a->kind = 3; a->group = kre_alternation(p);
        if (kre_peek(p) == ')') p->pos++; else kre_fail(p, "unclosed group `(`");
    } else if (c == '[') {
        kre_class(p, a);
    } else if (c == '.') { p->pos++; a->kind = 0; }
    else if (c == '\\') {
        p->pos++;
        int e = kre_peek(p);
        if (e < 0) { kre_fail(p, "dangling `\\` at end of pattern"); return a; }
        p->pos++;
        switch (e) {
            case 'd': a->kind = 2; a->negated = 0; a->ranges = (KReRange*)k_alloc(sizeof(KReRange)); a->ranges[0].lo = '0'; a->ranges[0].hi = '9'; a->nranges = 1; break;
            case 'D': a->kind = 2; a->negated = 1; a->ranges = (KReRange*)k_alloc(sizeof(KReRange)); a->ranges[0].lo = '0'; a->ranges[0].hi = '9'; a->nranges = 1; break;
            case 'w': case 'W': {
                a->kind = 2; a->negated = (e == 'W'); a->ranges = (KReRange*)k_alloc(sizeof(KReRange) * 4);
                a->ranges[0] = (KReRange){'a','z'}; a->ranges[1] = (KReRange){'A','Z'}; a->ranges[2] = (KReRange){'0','9'}; a->ranges[3] = (KReRange){'_','_'}; a->nranges = 4; break;
            }
            case 's': case 'S': {
                a->kind = 2; a->negated = (e == 'S'); a->ranges = (KReRange*)k_alloc(sizeof(KReRange) * 4);
                a->ranges[0] = (KReRange){' ',' '}; a->ranges[1] = (KReRange){'\t','\t'}; a->ranges[2] = (KReRange){'\n','\n'}; a->ranges[3] = (KReRange){'\r','\r'}; a->nranges = 4; break;
            }
            case 'n': a->kind = 1; a->ch = '\n'; break;
            case 't': a->kind = 1; a->ch = '\t'; break;
            case 'r': a->kind = 1; a->ch = '\r'; break;
            default: a->kind = 1; a->ch = (unsigned char)e; break;   /* escaped literal */
        }
    } else if (c == ')' || c == '|') { kre_fail(p, "unexpected metacharacter"); }
    else if (c == '*' || c == '+' || c == '?') { kre_fail(p, "quantifier with nothing to repeat"); }
    else if (c < 0) { kre_fail(p, "unexpected end of pattern"); }
    else { p->pos++; a->kind = 1; a->ch = (unsigned char)c; }
    return a;
}

static KReSeq kre_sequence(KReP* p) {
    KReSeq seq; seq.p = 0; seq.n = 0; int cap = 0;
    for (;;) {
        int c = kre_peek(p);
        if (c < 0 || c == '|' || c == ')') break;
        if (c == '$' && p->pos + 1 == p->len) { p->pos++; p->aend = 1; break; }
        KReAtom* atom = kre_atom(p);
        if (p->err) break;
        int q = 0, n = kre_peek(p);
        if (n == '*') { q = 1; p->pos++; } else if (n == '+') { q = 2; p->pos++; } else if (n == '?') { q = 3; p->pos++; }
        if (seq.n == cap) { cap = cap ? cap * 2 : 8; seq.p = (KRePiece*)realloc(seq.p, sizeof(KRePiece) * cap); }
        seq.p[seq.n].atom = atom; seq.p[seq.n].quant = q; seq.n++;
    }
    return seq;
}

static KReAlts kre_alternation(KReP* p) {
    KReAlts alts; alts.a = 0; alts.n = 0; int cap = 0;
    for (;;) {
        KReSeq s = kre_sequence(p);
        if (alts.n == cap) { cap = cap ? cap * 2 : 4; alts.a = (KReSeq*)realloc(alts.a, sizeof(KReSeq) * cap); }
        alts.a[alts.n++] = s;
        if (kre_peek(p) == '|') p->pos++; else break;
    }
    return alts;
}

/* compile; on error, k_panic("invalid regex: <msg>") like regex_builtin */
static KRegex k_re_compile(const char* pat) {
    KReP p; p.s = (const unsigned char*)pat; p.pos = 0; p.len = (int)strlen(pat); p.aend = 0; p.err = 0; p.msg = "";
    KRegex re; re.astart = 0; re.aend = 0;
    if (kre_peek(&p) == '^') { re.astart = 1; p.pos++; }
    re.alts = kre_alternation(&p);
    if (!p.err && p.pos != p.len) { p.err = 1; p.msg = "unexpected metacharacter in pattern"; }
    re.aend = p.aend;
    if (p.err) { char b[128]; snprintf(b, sizeof b, "invalid regex: %s", p.msg); k_panic(b); }
    return re;
}

/* matcher — recursive, mirrors regex.rs match_here/seq/piece/atom exactly */
static int kre_match_seq(KRePiece* pieces, int n, const unsigned char* t, int tlen, int pos, int* out);
static int kre_atom_match(KReAtom* a, const unsigned char* t, int tlen, int pos, int* np) {
    switch (a->kind) {
        case 0: if (pos < tlen) { *np = pos + 1; return 1; } return 0;
        case 1: if (pos < tlen && t[pos] == a->ch) { *np = pos + 1; return 1; } return 0;
        case 2: {
            if (pos >= tlen) return 0;
            unsigned char ch = t[pos]; int inside = 0;
            for (int i = 0; i < a->nranges; i++) if (ch >= a->ranges[i].lo && ch <= a->ranges[i].hi) { inside = 1; break; }
            if (inside != a->negated) { *np = pos + 1; return 1; } return 0;
        }
        default: { /* group */
            for (int i = 0; i < a->group.n; i++) {
                int e; if (kre_match_seq(a->group.a[i].p, a->group.a[i].n, t, tlen, pos, &e)) { *np = e; return 1; }
            }
            return 0;
        }
    }
}
static int kre_match_piece(KRePiece* first, KRePiece* rest, int nrest, const unsigned char* t, int tlen, int pos, int* out) {
    KReAtom* a = first->atom;
    if (first->quant == 0) {
        int np; if (!kre_atom_match(a, t, tlen, pos, &np)) return 0;
        return kre_match_seq(rest, nrest, t, tlen, np, out);
    }
    if (first->quant == 3) {   /* ? greedy: one then zero */
        int np;
        if (kre_atom_match(a, t, tlen, pos, &np)) { if (kre_match_seq(rest, nrest, t, tlen, np, out)) return 1; }
        return kre_match_seq(rest, nrest, t, tlen, pos, out);
    }
    /* * or + : collect greedy ends, then backtrack toward min */
    int cap = 16, cnt = 0; int* ends = (int*)malloc(sizeof(int) * cap); ends[cnt++] = pos;
    int cur = pos, np;
    while (kre_atom_match(a, t, tlen, cur, &np)) {
        if (np == cur) break;   /* zero-width guard */
        cur = np;
        if (cnt == cap) { cap *= 2; ends = (int*)realloc(ends, sizeof(int) * cap); }
        ends[cnt++] = cur;
    }
    int min = (first->quant == 2) ? 1 : 0;
    for (int k = cnt - 1; k >= min; k--) {
        if (kre_match_seq(rest, nrest, t, tlen, ends[k], out)) { free(ends); return 1; }
    }
    free(ends);
    return 0;
}
static int kre_match_seq(KRePiece* pieces, int n, const unsigned char* t, int tlen, int pos, int* out) {
    if (n == 0) { *out = pos; return 1; }
    return kre_match_piece(&pieces[0], pieces + 1, n - 1, t, tlen, pos, out);
}
/* try to match starting exactly at pos; returns end index or -1 (honors $) */
static int k_re_match_here(KRegex* re, const unsigned char* t, int tlen, int pos) {
    for (int i = 0; i < re->alts.n; i++) {
        int end;
        if (kre_match_seq(re->alts.a[i].p, re->alts.a[i].n, t, tlen, pos, &end)) {
            if (!re->aend || end == tlen) return end;
        }
    }
    return -1;
}
/* leftmost match: fills *start,*end; returns 1 if found */
static int k_re_leftmost(KRegex* re, const unsigned char* t, int tlen, int* start, int* end) {
    int last = re->astart ? 0 : tlen;
    for (int s = 0; s <= last; s++) {
        int e = k_re_match_here(re, t, tlen, s);
        if (e >= 0) { *start = s; *end = e; return 1; }
        if (re->astart) break;
    }
    return 0;
}
static KValue k_substr(const unsigned char* t, int a, int b) {
    char* c = (char*)k_alloc(b - a + 1); memcpy(c, t + a, b - a); c[b - a] = 0; return k_str(c);
}

static KValue k_re_match(KValue pat, KValue text) {
    KRegex re = k_re_compile(k_as_str(pat));
    const char* t = k_as_str(text); int tlen = (int)strlen(t);
    int s, e; return k_bool(k_re_leftmost(&re, (const unsigned char*)t, tlen, &s, &e));
}
static KValue k_re_find(KValue pat, KValue text) {
    KRegex re = k_re_compile(k_as_str(pat));
    const char* t = k_as_str(text); int tlen = (int)strlen(t);
    int s, e;
    if (k_re_leftmost(&re, (const unsigned char*)t, tlen, &s, &e)) return k_some(k_substr((const unsigned char*)t, s, e));
    return k_none();
}
static KValue k_re_find_all(KValue pat, KValue text) {
    KRegex re = k_re_compile(k_as_str(pat));
    const unsigned char* t = (const unsigned char*)k_as_str(text); int tlen = (int)strlen((const char*)t);
    KValue items[8192]; int n = 0; int i = 0;
    while (i <= tlen) {
        int e = k_re_match_here(&re, t, tlen, i);
        if (e >= 0) { if (n < 8192) items[n++] = k_substr(t, i, e); i = e > i ? e : i + 1; }
        else if (re.astart) break;
        else i++;
    }
    return k_list(items, n);
}
static KValue k_re_replace(KValue pat, KValue text, KValue repl) {
    KRegex re = k_re_compile(k_as_str(pat));
    const unsigned char* t = (const unsigned char*)k_as_str(text); int tlen = (int)strlen((const char*)t);
    const char* rep = k_as_str(repl);
    KBuf b = { 0, 0, 0 }; int i = 0;
    while (i < tlen) {
        int e = k_re_match_here(&re, t, tlen, i);
        if (e >= 0) { kb_puts(&b, rep); if (e > i) i = e; else { kb_putc(&b, (char)t[i]); i++; } }
        else { kb_putc(&b, (char)t[i]); i++; }
    }
    if (i == tlen && k_re_match_here(&re, t, tlen, i) == i) kb_puts(&b, rep);
    return k_str(kb_take(&b));
}

/* ---- file I/O builtins (effect io.fs); mirror interp::fs_builtin.
   The Ok/Err *structure* matches every engine; the Err message text is a
   platform OS description and may differ from the interpreter's wording. ---- */
static KValue k_read_file(KValue path) {
    FILE* f = fopen(path.as.s, "rb");
    if (!f) return k_os_error();
    /* fopen succeeds on a directory (then fread gives 0 bytes), but the
       interpreter's fs::read errors — return the same EISDIR error. */
    struct stat k_rf_st;
    if (fstat(fileno(f), &k_rf_st) == 0 && S_ISDIR(k_rf_st.st_mode)) {
        fclose(f);
        errno = EISDIR;
        return k_os_error();
    }
    fseek(f, 0, SEEK_END);
    long n = ftell(f);
    fseek(f, 0, SEEK_SET);
    if (n < 0) { fclose(f); return k_os_error(); }
    char* buf = k_alloc((size_t)n + 1);
    size_t got = fread(buf, 1, (size_t)n, f);
    buf[got] = 0;
    fclose(f);
    return k_ok(k_str(buf));
}
static KValue k_write_file(KValue path, KValue content, int append) {
    FILE* f = fopen(path.as.s, append ? "ab" : "wb");
    if (!f) return k_os_error();
    const char* s = content.as.s;
    size_t len = strlen(s);
    size_t w = fwrite(s, 1, len, f);
    fclose(f);
    if (w != len) return k_err(k_str("write error"));
    return k_ok(k_unit());
}
static KValue k_delete_file(KValue path) {
    if (remove(path.as.s) != 0) return k_os_error();
    return k_ok(k_unit());
}
static KValue k_file_exists(KValue path) {
    return k_bool(access(path.as.s, F_OK) == 0);
}

/* ---- environment & process builtins ---- */
static int k_argc = 0;
static char** k_argv = 0;
static KValue k_env_var(KValue name) {
    const char* v = getenv(name.as.s);
    return v ? k_some(k_str(v)) : k_none();
}
static KValue k_args(void) {
    /* the program's own args are argv[1..] */
    int n = k_argc > 1 ? k_argc - 1 : 0;
    KValue* out = k_alloc(sizeof(KValue) * (n < 1 ? 1 : n));
    for (int i = 0; i < n; i++) out[i] = k_str(k_argv[i + 1]);
    return k_list(out, n);
}
static KValue k_eprint(KValue v) {
    fprintf(stderr, "%s\n", k_show(v));
    return k_unit();
}

/* ---- numeric formatting; mirrors interp int_to_radix / int_isqrt ---- */
static KValue k_int_radix(int64_t v, int base) {
    static const char* D = "0123456789abcdefghijklmnopqrstuvwxyz";
    uint64_t n = v < 0 ? (uint64_t)(-(v + 1)) + 1 : (uint64_t)v; /* magnitude, MIN-safe */
    char tmp[70];
    int i = 0;
    if (n == 0) tmp[i++] = '0';
    while (n > 0) { tmp[i++] = D[n % (uint64_t)base]; n /= (uint64_t)base; }
    char* out = k_alloc((size_t)i + 2);
    int o = 0;
    if (v < 0) out[o++] = '-';
    for (int k = i - 1; k >= 0; k--) out[o++] = tmp[k];
    out[o] = 0;
    return k_str(out);
}
/* Rust-style saturating f64 -> i64 cast (matches the interpreter's `as i64`):
   NaN -> 0; values >= 2^63 -> i64::MAX; values < -2^63 -> i64::MIN; otherwise
   truncate toward zero. A raw (int64_t)double is UNDEFINED out of range (it
   returned garbage, diverging from interp/KVM). */
static int64_t k_f2i(double v) {
    if (v != v) return 0;                              /* NaN */
    if (v >= 9223372036854775808.0) return INT64_MAX;  /* >= 2^63 */
    if (v < -9223372036854775808.0) return INT64_MIN;  /* <  -2^63 */
    return (int64_t)v;
}
/* byte length of the UTF-8 char whose leading byte is `c` (1..4; 1 for a stray
   continuation/invalid byte). */
static int k_utf8_len(unsigned char c) {
    if (c < 0x80) return 1;
    if ((c & 0xE0) == 0xC0) return 2;
    if ((c & 0xF0) == 0xE0) return 3;
    if ((c & 0xF8) == 0xF0) return 4;
    return 1;
}
static int64_t k_isqrt(uint64_t n) {
    if (n == 0) return 0;
    uint64_t x = (uint64_t)sqrt((double)n);
    while (x * x > n) x--;
    while ((x + 1) * (x + 1) <= n) x++;
    return (int64_t)x;
}

/* ---- encodings & hash; mirrors src/encoding.rs exactly ---- */
static const char* K_B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
static int k_valid_utf8(const unsigned char* b, size_t n) {
    size_t i = 0;
    while (i < n) {
        unsigned char c = b[i];
        int len; unsigned int minv;
        if (c < 0x80) { i++; continue; }
        else if ((c & 0xE0) == 0xC0) { len = 2; minv = 0x80; }
        else if ((c & 0xF0) == 0xE0) { len = 3; minv = 0x800; }
        else if ((c & 0xF8) == 0xF0) { len = 4; minv = 0x10000; }
        else return 0;
        if (i + (size_t)len > n) return 0;
        unsigned int cp = c & (0x7F >> len);
        for (int k = 1; k < len; k++) {
            if ((b[i + k] & 0xC0) != 0x80) return 0;
            cp = (cp << 6) | (b[i + k] & 0x3F);
        }
        if (cp < minv || cp > 0x10FFFF || (cp >= 0xD800 && cp <= 0xDFFF)) return 0;
        i += len;
    }
    return 1;
}
static KValue k_base64_encode(KValue sv) {
    const unsigned char* s = (const unsigned char*)sv.as.s;
    size_t n = strlen(sv.as.s);
    char* out = k_alloc((n + 2) / 3 * 4 + 1);
    size_t o = 0;
    for (size_t i = 0; i < n; i += 3) {
        unsigned int b0 = s[i];
        unsigned int b1 = i + 1 < n ? s[i + 1] : 0;
        unsigned int b2 = i + 2 < n ? s[i + 2] : 0;
        unsigned int x = (b0 << 16) | (b1 << 8) | b2;
        out[o++] = K_B64[x >> 18 & 63];
        out[o++] = K_B64[x >> 12 & 63];
        out[o++] = (i + 1 < n) ? K_B64[x >> 6 & 63] : '=';
        out[o++] = (i + 2 < n) ? K_B64[x & 63] : '=';
    }
    out[o] = 0;
    return k_str(out);
}
static int k_b64val(unsigned char c) {
    if (c >= 'A' && c <= 'Z') return c - 'A';
    if (c >= 'a' && c <= 'z') return c - 'a' + 26;
    if (c >= '0' && c <= '9') return c - '0' + 52;
    if (c == '+') return 62;
    if (c == '/') return 63;
    return -1;
}
static KValue k_base64_decode(KValue sv) {
    const char* src = sv.as.s;
    /* strip newlines */
    size_t rn = 0, sl = strlen(src);
    unsigned char* raw = k_alloc(sl + 1);
    for (size_t i = 0; i < sl; i++) if (src[i] != '\n' && src[i] != '\r') raw[rn++] = (unsigned char)src[i];
    if (rn % 4 != 0) return k_err(k_str("invalid base64: length not a multiple of 4"));
    unsigned char* out = k_alloc(rn / 4 * 3 + 1);
    size_t o = 0;
    for (size_t i = 0; i < rn; i += 4) {
        int pad = 0;
        for (int k = 0; k < 4; k++) if (raw[i + k] == '=') pad++;
        if (pad > 2) return k_err(k_str("invalid base64: too much padding"));
        unsigned int x = 0;
        for (int k = 0; k < 4; k++) {
            int v;
            if (raw[i + k] == '=') {
                if (k < 4 - pad) return k_err(k_str("invalid base64: misplaced padding"));
                v = 0;
            } else {
                v = k_b64val(raw[i + k]);
                if (v < 0) return k_err(k_str("invalid base64: bad character"));
            }
            x = (x << 6) | (unsigned int)v;
        }
        out[o++] = (unsigned char)(x >> 16 & 0xFF);
        if (pad < 2) out[o++] = (unsigned char)(x >> 8 & 0xFF);
        if (pad < 1) out[o++] = (unsigned char)(x & 0xFF);
    }
    if (!k_valid_utf8(out, o)) return k_err(k_str("decoded bytes are not valid UTF-8"));
    out[o] = 0;
    return k_ok(k_str((char*)out));
}
static KValue k_hex_encode(KValue sv) {
    const unsigned char* s = (const unsigned char*)sv.as.s;
    size_t n = strlen(sv.as.s);
    const char* H = "0123456789abcdef";
    char* out = k_alloc(n * 2 + 1);
    for (size_t i = 0; i < n; i++) { out[2 * i] = H[s[i] >> 4]; out[2 * i + 1] = H[s[i] & 0xF]; }
    out[n * 2] = 0;
    return k_str(out);
}
static KValue k_hex_decode(KValue sv) {
    const char* s = sv.as.s;
    size_t n = strlen(s);
    if (n % 2 != 0) return k_err(k_str("invalid hex: odd length"));
    unsigned char* out = k_alloc(n / 2 + 1);
    for (size_t i = 0; i < n; i += 2) {
        int hi = -1, lo = -1;
        char a = s[i], b = s[i + 1];
        if (a >= '0' && a <= '9') hi = a - '0'; else if (a >= 'a' && a <= 'f') hi = a - 'a' + 10; else if (a >= 'A' && a <= 'F') hi = a - 'A' + 10;
        if (b >= '0' && b <= '9') lo = b - '0'; else if (b >= 'a' && b <= 'f') lo = b - 'a' + 10; else if (b >= 'A' && b <= 'F') lo = b - 'A' + 10;
        if (hi < 0 || lo < 0) return k_err(k_str("invalid hex: bad digit"));
        out[i / 2] = (unsigned char)((hi << 4) | lo);
    }
    if (!k_valid_utf8(out, n / 2)) return k_err(k_str("decoded bytes are not valid UTF-8"));
    out[n / 2] = 0;
    return k_ok(k_str((char*)out));
}
static KValue k_hash_fnv(KValue sv) {
    const unsigned char* s = (const unsigned char*)sv.as.s;
    uint64_t h = 0xcbf29ce484222325ULL;
    for (size_t i = 0; s[i]; i++) { h ^= s[i]; h *= 0x100000001b3ULL; }
    return k_int((int64_t)h);
}
static int k_url_unreserved(unsigned char b) {
    return (b >= 'A' && b <= 'Z') || (b >= 'a' && b <= 'z') || (b >= '0' && b <= '9')
        || b == '-' || b == '_' || b == '.' || b == '~';
}
static KValue k_url_encode(KValue sv) {
    const unsigned char* s = (const unsigned char*)sv.as.s;
    size_t n = strlen(sv.as.s);
    const char* H = "0123456789ABCDEF";
    char* out = k_alloc(n * 3 + 1);
    size_t o = 0;
    for (size_t i = 0; i < n; i++) {
        if (k_url_unreserved(s[i])) { out[o++] = (char)s[i]; }
        else { out[o++] = '%'; out[o++] = H[s[i] >> 4]; out[o++] = H[s[i] & 0xF]; }
    }
    out[o] = 0;
    return k_str(out);
}
static KValue k_url_decode(KValue sv) {
    const char* s = sv.as.s;
    size_t n = strlen(s);
    unsigned char* out = k_alloc(n + 1);
    size_t o = 0, i = 0;
    while (i < n) {
        if (s[i] == '%') {
            if (i + 2 >= n) return k_err(k_str("invalid percent-encoding: truncated escape"));
            int hi = -1, lo = -1;
            char a = s[i + 1], b = s[i + 2];
            if (a >= '0' && a <= '9') hi = a - '0'; else if (a >= 'a' && a <= 'f') hi = a - 'a' + 10; else if (a >= 'A' && a <= 'F') hi = a - 'A' + 10;
            if (b >= '0' && b <= '9') lo = b - '0'; else if (b >= 'a' && b <= 'f') lo = b - 'a' + 10; else if (b >= 'A' && b <= 'F') lo = b - 'A' + 10;
            if (hi < 0 || lo < 0) return k_err(k_str("invalid percent-encoding: bad hex"));
            out[o++] = (unsigned char)((hi << 4) | lo);
            i += 3;
        } else if (s[i] == '+') { out[o++] = ' '; i++; }
        else { out[o++] = (unsigned char)s[i]; i++; }
    }
    if (!k_valid_utf8(out, o)) return k_err(k_str("decoded bytes are not valid UTF-8"));
    out[o] = 0;
    return k_ok(k_str((char*)out));
}

/* ---- time/date; mirrors interp::time / src/time.rs exactly ---- */
static int64_t k_floor_div(int64_t a, int64_t b) {
    int64_t q = a / b;
    if ((a % b != 0) && ((a % b < 0) != (b < 0))) q -= 1;
    return q;
}
static int64_t k_floor_mod(int64_t a, int64_t b) { return a - k_floor_div(a, b) * b; }
static void k_civil(int64_t z, int64_t* y, int64_t* m, int64_t* d) {
    z += 719468;
    int64_t era = k_floor_div(z >= 0 ? z : z - 146096, 146097);
    int64_t doe = z - era * 146097;
    int64_t yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    int64_t yy = yoe + era * 400;
    int64_t doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    int64_t mp = (5 * doy + 2) / 153;
    int64_t dd = doy - (153 * mp + 2) / 5 + 1;
    int64_t mm = mp < 10 ? mp + 3 : mp - 9;
    *y = mm <= 2 ? yy + 1 : yy;
    *m = mm;
    *d = dd;
}
static void k_tsplit(int64_t t, int64_t* days, int64_t* secs) {
    *days = k_floor_div(t, 86400);
    *secs = k_floor_mod(t, 86400);
}
static KValue k_format_time(KValue tv) {
    int64_t days, secs, y, m, d;
    k_tsplit(tv.as.i, &days, &secs);
    k_civil(days, &y, &m, &d);
    int64_t hh = secs / 3600, mm = (secs % 3600) / 60, ss = secs % 60;
    char* buf = k_alloc(64);
    if (y < 0)
        snprintf(buf, 64, "-%04lld-%02lld-%02lld %02lld:%02lld:%02lld",
                 (long long)(-y), (long long)m, (long long)d, (long long)hh, (long long)mm, (long long)ss);
    else
        snprintf(buf, 64, "%04lld-%02lld-%02lld %02lld:%02lld:%02lld",
                 (long long)y, (long long)m, (long long)d, (long long)hh, (long long)mm, (long long)ss);
    return k_str(buf);
}
static KValue k_year_of(KValue tv) { int64_t dy, s, y, m, d; k_tsplit(tv.as.i, &dy, &s); k_civil(dy, &y, &m, &d); return k_int(y); }
static KValue k_month_of(KValue tv) { int64_t dy, s, y, m, d; k_tsplit(tv.as.i, &dy, &s); k_civil(dy, &y, &m, &d); return k_int(m); }
static KValue k_day_of(KValue tv) { int64_t dy, s, y, m, d; k_tsplit(tv.as.i, &dy, &s); k_civil(dy, &y, &m, &d); return k_int(d); }
static KValue k_hour_of(KValue tv) { int64_t dy, s; k_tsplit(tv.as.i, &dy, &s); return k_int(s / 3600); }
static KValue k_minute_of(KValue tv) { int64_t dy, s; k_tsplit(tv.as.i, &dy, &s); return k_int((s % 3600) / 60); }
static KValue k_second_of(KValue tv) { int64_t dy, s; k_tsplit(tv.as.i, &dy, &s); return k_int(s % 60); }
static KValue k_weekday_of(KValue tv) { int64_t dy, s; k_tsplit(tv.as.i, &dy, &s); return k_int(k_floor_mod(dy + 4, 7)); }
/* inverse of k_civil: days-since-epoch from a civil (y, m, d) */
static int64_t k_days_from_civil(int64_t y, int64_t m, int64_t d) {
    y = m <= 2 ? y - 1 : y;
    int64_t era = k_floor_div(y >= 0 ? y : y - 399, 400);
    int64_t yoe = y - era * 400;
    int64_t doy = (153 * (m > 2 ? m - 3 : m + 9) + 2) / 5 + d - 1;
    int64_t doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    return era * 146097 + doe - 719468;
}
static int64_t k_make(int64_t y, int64_t m, int64_t d, int64_t hh, int64_t mm, int64_t ss) {
    return k_days_from_civil(y, m, d) * 86400 + hh * 3600 + mm * 60 + ss;
}
static KValue k_date_make(KValue y, KValue m, KValue d, KValue hh, KValue mm, KValue ss) {
    return k_int(k_make(y.as.i, m.as.i, d.as.i, hh.as.i, mm.as.i, ss.as.i));
}
static KValue k_yearday_of(KValue tv) {
    int64_t dy, s, y, m, d; k_tsplit(tv.as.i, &dy, &s); k_civil(dy, &y, &m, &d);
    return k_int(dy - k_days_from_civil(y, 1, 1) + 1);
}
static KValue k_date_iso(KValue tv) {
    int64_t days, secs, y, m, d;
    k_tsplit(tv.as.i, &days, &secs);
    k_civil(days, &y, &m, &d);
    int64_t hh = secs / 3600, mm = (secs % 3600) / 60, ss = secs % 60;
    char* buf = k_alloc(64);
    if (y < 0)
        snprintf(buf, 64, "-%04lld-%02lld-%02lldT%02lld:%02lld:%02lldZ",
                 (long long)(-y), (long long)m, (long long)d, (long long)hh, (long long)mm, (long long)ss);
    else
        snprintf(buf, 64, "%04lld-%02lld-%02lldT%02lld:%02lld:%02lldZ",
                 (long long)y, (long long)m, (long long)d, (long long)hh, (long long)mm, (long long)ss);
    return k_str(buf);
}
/* parse "YYYY-MM-DD[(T| )HH:MM:SS][Z]" -> Ok(epoch) | Err(msg); mirrors time::parse_iso */
static KValue k_parse_iso(KValue sv) {
    const char* raw = sv.as.s;
    /* trim leading/trailing spaces + a trailing Z */
    while (*raw == ' ' || *raw == '\t' || *raw == '\n' || *raw == '\r') raw++;
    long n = (long)strlen(raw);
    while (n > 0 && (raw[n-1] == ' ' || raw[n-1] == '\t' || raw[n-1] == '\n' || raw[n-1] == '\r')) n--;
    if (n > 0 && raw[n-1] == 'Z') n--;
    char s[128];
    if (n >= (long)sizeof s) n = (long)sizeof s - 1;
    memcpy(s, raw, n); s[n] = 0;
    /* Heap-allocated: k_str stores the pointer without copying, so a stack buffer
       here would DANGLE after return and the Err message read as "" (empty). */
    char* errbuf = (char*)k_alloc(160);
    snprintf(errbuf, 160, "invalid ISO-8601 timestamp: %s", s);
    long y, mo, d, hh = 0, mi = 0, ss = 0;
    int neg = 0; char* p = s;
    if (*p == '-') { neg = 1; p++; }         /* negative year */
    char* time = 0;
    for (char* q = p; *q; q++) if (*q == 'T' || *q == ' ') { *q = 0; time = q + 1; break; }
    /* date part: Y-M-D */
    char* dash1 = strchr(p, '-');
    if (!dash1) return k_err(k_str(errbuf));
    *dash1 = 0; char* mstr = dash1 + 1;
    char* dash2 = strchr(mstr, '-');
    if (!dash2) return k_err(k_str(errbuf));
    *dash2 = 0; char* dstr = dash2 + 1;
    char* end;
    y = strtol(p, &end, 10);   if (*end) return k_err(k_str(errbuf));  if (neg) y = -y;
    mo = strtol(mstr, &end, 10); if (*end) return k_err(k_str(errbuf));
    d = strtol(dstr, &end, 10);  if (*end || dstr[0] == 0) return k_err(k_str(errbuf));
    if (mo < 1 || mo > 12 || d < 1 || d > 31) return k_err(k_str(errbuf));
    if (time && *time) {
        char* c1 = strchr(time, ':');
        if (!c1) return k_err(k_str(errbuf));
        *c1 = 0; char* mstr2 = c1 + 1;
        char* c2 = strchr(mstr2, ':');
        if (!c2) return k_err(k_str(errbuf));
        *c2 = 0; char* sstr = c2 + 1;
        hh = strtol(time, &end, 10);  if (*end) return k_err(k_str(errbuf));
        mi = strtol(mstr2, &end, 10); if (*end) return k_err(k_str(errbuf));
        ss = strtol(sstr, &end, 10);  if (*end || sstr[0] == 0) return k_err(k_str(errbuf));
        if (hh < 0 || hh > 23 || mi < 0 || mi > 59 || ss < 0 || ss > 60) return k_err(k_str(errbuf));
    }
    return k_ok(k_int(k_make(y, mo, d, hh, mi, ss)));
}
static KValue k_now(void) { return k_int((int64_t)time(0)); }
/* read one line from stdin (newline stripped); None at EOF */
static KValue k_read_line(void) {
    KBuf b = { 0, 0, 0 };
    int c, any = 0;
    while ((c = getchar()) != EOF) {
        any = 1;
        if (c == '\n') break;
        char ch = (char)c;
        kb_write(&b, &ch, 1);
    }
    if (!any) return k_none();
    /* strip a trailing \r (CRLF input) */
    if (b.len > 0 && b.buf[b.len - 1] == '\r') { b.len--; b.buf[b.len] = 0; }
    return k_some(k_str(b.buf ? b.buf : ""));
}
/* read all of stdin into a single string */
static KValue k_read_all(void) {
    KBuf b = { 0, 0, 0 };
    char chunk[4096]; size_t n;
    while ((n = fread(chunk, 1, sizeof chunk, stdin)) > 0) kb_write(&b, chunk, (long)n);
    return k_str(b.buf ? b.buf : "");
}

/* ---- seeded random (xorshift64*); mirrors interp::SeedRng exactly ---- */
static uint64_t k_rng_next(uint64_t* s) {
    uint64_t x = *s;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *s = x;
    return x * 0x2545F4914F6CDD1DULL;
}
static KValue k_random_ints(KValue seed, KValue count) {
    uint64_t s = (uint64_t)seed.as.i; if (s == 0) s = 1;
    int64_t n = count.as.i; if (n < 0) n = 0;
    if (n > 100000000) k_panic("random count too large");
    KValue* out = k_alloc(sizeof(KValue) * (n < 1 ? 1 : n));
    for (int64_t i = 0; i < n; i++) out[i] = k_int((int64_t)k_rng_next(&s));
    return k_list(out, (int)n);
}
static KValue k_random_floats(KValue seed, KValue count) {
    uint64_t s = (uint64_t)seed.as.i; if (s == 0) s = 1;
    int64_t n = count.as.i; if (n < 0) n = 0;
    if (n > 100000000) k_panic("random count too large");
    KValue* out = k_alloc(sizeof(KValue) * (n < 1 ? 1 : n));
    for (int64_t i = 0; i < n; i++)
        out[i] = k_float((double)(k_rng_next(&s) >> 11) * (1.0 / 9007199254740992.0));
    return k_list(out, (int)n);
}
static KValue k_shuffle(KValue seed, KValue lst) {
    uint64_t s = (uint64_t)seed.as.i; if (s == 0) s = 1;
    KList* l = lst.as.list;
    KValue* out = k_alloc(sizeof(KValue) * (l->len < 1 ? 1 : l->len));
    memcpy(out, l->items, sizeof(KValue) * l->len);
    int64_t i = l->len;
    while (i > 1) {
        i--;
        int64_t j = (int64_t)(k_rng_next(&s) % (uint64_t)(i + 1));
        KValue t = out[i]; out[i] = out[j]; out[j] = t;
    }
    return k_list(out, (int)l->len);
}

static KValue k_expose_call(KValue recv, const char* name, KValue* args, int argc);
/* UFCS: a top-level function reachable via method-call syntax `x.f(args)` */
typedef struct { const char* name; int fnid; } KUfcs;
extern const KUfcs UFCS_FUNS[];
extern const int K_NUFCS;
/* operator overloading: call a top-level operator function (`add`/`lt`/…) on
   user values. Returns 1 and sets *out if the function exists, else 0. */
static int k_op_overload(const char* name, KValue a, KValue b, KValue* out) {
    for (int i = 0; i < K_NUFCS; i++) {
        if (!strcmp(UFCS_FUNS[i].name, name)) {
            KValue args[2] = { a, b };
            *out = k_call(k_fun(UFCS_FUNS[i].fnid), args, 2);
            return 1;
        }
    }
    return 0;
}

/* stable sort by an Int key: qsort with an original-index tiebreak */
typedef struct { int64_t key; int idx; KValue v; } KSortItem;
static int k_sortby_cmp(const void* a, const void* b) {
    const KSortItem* x = (const KSortItem*)a;
    const KSortItem* y = (const KSortItem*)b;
    if (x->key < y->key) return -1;
    if (x->key > y->key) return 1;
    return x->idx - y->idx;
}

static KValue k_method(KValue recv, const char* name, KValue* args, int argc) {
    if (recv.tag == K_BIGINT) {
        if (!strcmp(name, "pow")) { (void)argc; if (args[0].as.i < 0) k_panic("`pow` exponent must be non-negative"); return k_big_v(k_big_pow(recv, args[0].as.i)); }
        if (!strcmp(name, "abs")) { KBig* a = recv.as.big; return k_big_v(k_big_norm(0, a->limbs, a->n)); }
        if (!strcmp(name, "is_negative")) return k_bool(recv.as.big->neg);
        if (!strcmp(name, "sign")) { KBig* a = recv.as.big; return k_int(a->n == 0 ? 0 : (a->neg ? -1 : 1)); }
    }
    if (recv.tag == K_RATIONAL) {
        (void)argc;
        if (!strcmp(name, "num")) return k_big_v(recv.as.rat->num);
        if (!strcmp(name, "den")) return k_big_v(recv.as.rat->den);
        if (!strcmp(name, "to_float")) return k_float(k_rat_to_f64(recv.as.rat));
        if (!strcmp(name, "recip")) { KRat* r = recv.as.rat; if (r->num->n == 0) k_panic("reciprocal of zero"); return k_rat_v(k_rat_norm(r->den, r->num)); }
    }
    (void)argc;
    if (recv.tag == K_COMPONENT) return k_expose_call(recv, name, args, argc);
    /* Int/sized -> a sized width (checked), mirrors interp shared_method */
    if (recv.tag == K_INT || recv.tag == K_SIZEDINT) {
        int tw = k_width_of(name);
        if (tw >= 0) {
            __int128 x = (recv.tag == K_INT) ? (__int128)recv.as.i : recv.as.sized->v;
            if (x < k_iw_min(tw) || x > k_iw_max(tw)) {
                char b0[96], num[48]; k_i128_print(num, sizeof num, x);
                snprintf(b0, sizeof b0, "%s out of range for `%s`", num, k_iw_name(tw));
                k_panic(b0);
            }
            return k_sized(x, tw);
        }
    }
    if (recv.tag == K_SIZEDINT) {
        __int128 a = recv.as.sized->v; int w = recv.as.sized->width;
        if (!strcmp(name, "to_int")) {
            if (a < INT64_MIN || a > INT64_MAX) {
                char b0[80], num[48]; k_i128_print(num, sizeof num, a);
                snprintf(b0, sizeof b0, "%s does not fit in Int (i64)", num); k_panic(b0);
            }
            return k_int((int64_t)a);
        }
        if (!strcmp(name, "to_str")) {
            char num[48]; k_i128_print(num, sizeof num, a);
            char* c = (char*)k_alloc(strlen(num) + 1); strcpy(c, num); return k_str(c);
        }
        if (!strcmp(name, "to_float")) return k_float((double)a);
        int wsb = !strcmp(name,"wrapping_add")||!strcmp(name,"wrapping_sub")||!strcmp(name,"wrapping_mul")
                ||!strcmp(name,"saturating_add")||!strcmp(name,"saturating_sub")||!strcmp(name,"saturating_mul")
                ||!strcmp(name,"band")||!strcmp(name,"bor")||!strcmp(name,"bxor");
        if (wsb) {
            if (argc < 1 || args[0].tag != K_SIZEDINT || args[0].as.sized->width != w) {
                char b0[64]; snprintf(b0, sizeof b0, "`%s` needs a `%s`", name, k_iw_name(w)); k_panic(b0);
            }
            __int128 rhs = args[0].as.sized->v, mask = ((__int128)1 << k_iw_bits(w)) - 1, r;
            if (!strcmp(name,"wrapping_add")) r = k_iw_wrap(w, a + rhs);
            else if (!strcmp(name,"wrapping_sub")) r = k_iw_wrap(w, a - rhs);
            else if (!strcmp(name,"wrapping_mul")) r = k_iw_wrap(w, a * rhs);
            else if (!strcmp(name,"saturating_add")) r = k_iw_sat(w, a + rhs);
            else if (!strcmp(name,"saturating_sub")) r = k_iw_sat(w, a - rhs);
            else if (!strcmp(name,"saturating_mul")) r = k_iw_sat(w, a * rhs);
            else if (!strcmp(name,"band")) r = k_iw_wrap(w, (a & mask) & (rhs & mask));
            else if (!strcmp(name,"bor")) r = k_iw_wrap(w, (a & mask) | (rhs & mask));
            else r = k_iw_wrap(w, (a & mask) ^ (rhs & mask));
            return k_sized(r, w);
        }
        if (!strcmp(name, "bnot")) {
            __int128 mask = ((__int128)1 << k_iw_bits(w)) - 1;
            return k_sized(k_iw_wrap(w, (a & mask) ^ mask), w);
        }
        if (!strcmp(name, "shl") || !strcmp(name, "shr")) {
            if (argc < 1 || args[0].tag != K_INT) {
                char b0[64]; snprintf(b0, sizeof b0, "`%s` needs an Int shift amount", name); k_panic(b0);
            }
            long long sh = args[0].as.i;
            if (sh < 0 || sh >= k_iw_bits(w)) {
                char b0[64]; snprintf(b0, sizeof b0, "shift amount must be in 0..=%d", k_iw_bits(w) - 1); k_panic(b0);
            }
            __int128 mask = ((__int128)1 << k_iw_bits(w)) - 1, r;
            if (!strcmp(name, "shl")) r = k_iw_wrap(w, (a & mask) << sh);
            else if (k_iw_signed(w)) r = k_iw_wrap(w, a >> sh);
            else r = k_iw_wrap(w, (a & mask) >> sh);
            return k_sized(r, w);
        }
        /* unmatched sized method falls through to the generic "no such method" */
    }
    if (recv.tag == K_F32) {
        if (!strcmp(name, "to_float")) return k_float((double)recv.as.f32v);
        if (!strcmp(name, "to_str")) {
            const char* s = k_show(recv);
            char* c = (char*)k_alloc(strlen(s) + 1); strcpy(c, s); return k_str(c);
        }
    }
    if (recv.tag == K_FLOAT && !strcmp(name, "to_f32")) return k_f32((float)recv.as.f);
    if (recv.tag == K_LIST) {
        KList* l = recv.as.list;
        if (!strcmp(name, "len")) return k_int(l->len);
        if (!strcmp(name, "map") || !strcmp(name, "par_map")) {
            KValue* out = k_alloc(sizeof(KValue) * (l->len < 1 ? 1 : l->len));
            for (int64_t i = 0; i < l->len; i++) out[i] = k_call(args[0], &l->items[i], 1);
            KValue r = k_list(out, (int)l->len);
            return r;
        }
        if (!strcmp(name, "zip_with")) {
            KList* o = args[0].as.list;
            int64_t n = l->len < o->len ? l->len : o->len;
            KValue* out = k_alloc(sizeof(KValue) * (n < 1 ? 1 : n));
            for (int64_t i = 0; i < n; i++) {
                KValue fa[2] = { l->items[i], o->items[i] };
                out[i] = k_call(args[1], fa, 2);
            }
            return k_list(out, (int)n);
        }
        if (!strcmp(name, "filter") || !strcmp(name, "par_filter")) {
            KValue* out = k_alloc(sizeof(KValue) * (l->len < 1 ? 1 : l->len));
            int n = 0;
            for (int64_t i = 0; i < l->len; i++)
                if (k_truthy(k_call(args[0], &l->items[i], 1))) out[n++] = l->items[i];
            return k_list(out, n);
        }
        if (!strcmp(name, "take_while")) {
            KValue* out = k_alloc(sizeof(KValue) * (l->len < 1 ? 1 : l->len));
            int n = 0;
            for (int64_t i = 0; i < l->len; i++) {
                if (!k_truthy(k_call(args[0], &l->items[i], 1))) break;
                out[n++] = l->items[i];
            }
            return k_list(out, n);
        }
        if (!strcmp(name, "drop_while")) {
            int64_t i = 0;
            while (i < l->len && k_truthy(k_call(args[0], &l->items[i], 1))) i++;
            return k_list(l->items + i, (int)(l->len - i));
        }
        if (!strcmp(name, "par_each")) {
            for (int64_t i = 0; i < l->len; i++) k_call(args[0], &l->items[i], 1);
            return k_unit();
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
        if (!strcmp(name, "is_empty")) return k_bool(l->len == 0);
        if (!strcmp(name, "init")) return k_list(l->items, (int)(l->len ? l->len - 1 : 0));
        if (!strcmp(name, "tail")) return k_list(l->items + (l->len ? 1 : 0), (int)(l->len ? l->len - 1 : 0));
        if (!strcmp(name, "concat")) {
            KList* o = args[0].as.list;
            KValue* out = k_alloc(sizeof(KValue) * (l->len + o->len < 1 ? 1 : l->len + o->len));
            memcpy(out, l->items, sizeof(KValue) * l->len);
            memcpy(out + l->len, o->items, sizeof(KValue) * o->len);
            return k_list(out, (int)(l->len + o->len));
        }
        if (!strcmp(name, "unique")) {
            KValue* out = k_alloc(sizeof(KValue) * (l->len < 1 ? 1 : l->len));
            int64_t n = 0;
            for (int64_t i = 0; i < l->len; i++) {
                int dup = 0;
                for (int64_t j = 0; j < n; j++) if (k_eq(out[j], l->items[i])) { dup = 1; break; }
                if (!dup) out[n++] = l->items[i];
            }
            return k_list(out, (int)n);
        }
        if (!strcmp(name, "product")) {
            int64_t pi = 1; double pf = 1; int isf = 0;
            for (int64_t i = 0; i < l->len; i++) {
                KValue it = l->items[i];
                if (it.tag == K_INT) {
                    if (__builtin_mul_overflow(pi, it.as.i, &pi)) k_panic("integer overflow in product");
                } else if (it.tag == K_FLOAT) { isf = 1; pf *= it.as.f; }
                else k_panic("cannot multiply non-numeric");
            }
            return isf ? k_float(pf * (double)pi) : k_int(pi);
        }
        if (!strcmp(name, "min") || !strcmp(name, "max")) {
            int wmin = name[1] == 'i';
            if (l->len == 0) return k_none();
            KValue best = l->items[0];
            for (int64_t i = 1; i < l->len; i++) {
                KValue it = l->items[i];
                int lt;
                if (it.tag == K_INT && best.tag == K_INT) lt = it.as.i < best.as.i;
                else if (it.tag == K_FLOAT && best.tag == K_FLOAT) lt = it.as.f < best.as.f;
                else if (it.tag == K_STR && best.tag == K_STR) lt = strcmp(it.as.s, best.as.s) < 0;
                else { k_panic("`min`/`max` need Int, Float, or Str elements"); lt = 0; }
                if (wmin ? lt : !lt && !k_eq(it, best)) best = it;
            }
            return k_some(best);
        }
        if (!strcmp(name, "min_by") || !strcmp(name, "max_by")) {
            int wmin = name[1] == 'i';
            if (l->len == 0) return k_none();
            KValue best = l->items[0];
            KValue best_key = k_call(args[0], &best, 1);
            for (int64_t i = 1; i < l->len; i++) {
                KValue key = k_call(args[0], &l->items[i], 1);
                KValue c = k_cmp(key, best_key, wmin ? 0 : 2); /* min: key<best  max: key>best */
                if (c.tag == K_BOOL && c.as.b) { best = l->items[i]; best_key = key; }
            }
            return k_some(best);
        }
        if (!strcmp(name, "flatten")) {
            int64_t total = 0;
            for (int64_t i = 0; i < l->len; i++) {
                if (l->items[i].tag != K_LIST) k_panic("`flatten` needs a List of Lists");
                total += l->items[i].as.list->len;
            }
            KValue* out = k_alloc(sizeof(KValue) * (total < 1 ? 1 : total));
            int64_t n = 0;
            for (int64_t i = 0; i < l->len; i++) {
                KList* inner = l->items[i].as.list;
                memcpy(out + n, inner->items, sizeof(KValue) * inner->len);
                n += inner->len;
            }
            return k_list(out, (int)total);
        }
        if (!strcmp(name, "count")) {
            int64_t n = 0;
            for (int64_t i = 0; i < l->len; i++)
                if (k_truthy(k_call(args[0], &l->items[i], 1))) n++;
            return k_int(n);
        }
        if (!strcmp(name, "flat_map")) {
            KValue subs[4096]; int ns = 0; int64_t total = 0;
            for (int64_t i = 0; i < l->len && ns < 4096; i++) {
                KValue r = k_call(args[0], &l->items[i], 1);
                if (r.tag != K_LIST) k_panic("`flat_map` function must return a List");
                subs[ns++] = r; total += r.as.list->len;
            }
            KValue* out = k_alloc(sizeof(KValue) * (total < 1 ? 1 : total));
            int64_t n = 0;
            for (int i = 0; i < ns; i++) {
                KList* inner = subs[i].as.list;
                memcpy(out + n, inner->items, sizeof(KValue) * inner->len);
                n += inner->len;
            }
            return k_list(out, (int)total);
        }
        if (!strcmp(name, "sort_by")) {
            int64_t n = l->len;
            KSortItem* arr = (KSortItem*)k_alloc(sizeof(KSortItem) * (n < 1 ? 1 : n));
            for (int64_t i = 0; i < n; i++) {
                arr[i].key = k_call(args[0], &l->items[i], 1).as.i;
                arr[i].idx = (int)i;
                arr[i].v = l->items[i];
            }
            qsort(arr, n, sizeof(KSortItem), k_sortby_cmp);
            KValue* out = (KValue*)k_alloc(sizeof(KValue) * (n < 1 ? 1 : n));
            for (int64_t i = 0; i < n; i++) out[i] = arr[i].v;
            return k_list(out, (int)n);
        }
        if (!strcmp(name, "group_by")) {
            int64_t n = l->len;
            KValue* keys = k_alloc(sizeof(KValue) * (n < 1 ? 1 : n));
            KValue** buckets = k_alloc(sizeof(KValue*) * (n < 1 ? 1 : n));
            int64_t* counts = k_alloc(sizeof(int64_t) * (n < 1 ? 1 : n));
            int64_t ng = 0;
            for (int64_t i = 0; i < n; i++) {
                KValue key = k_call(args[0], &l->items[i], 1);
                int64_t g = -1;
                for (int64_t j = 0; j < ng; j++) if (k_eq(keys[j], key)) { g = j; break; }
                if (g < 0) {
                    g = ng;
                    keys[ng] = key;
                    buckets[ng] = k_alloc(sizeof(KValue) * (n < 1 ? 1 : n));
                    counts[ng] = 0;
                    ng++;
                }
                buckets[g][counts[g]++] = l->items[i];
            }
            KValue* vals = k_alloc(sizeof(KValue) * (ng < 1 ? 1 : ng));
            for (int64_t j = 0; j < ng; j++) vals[j] = k_list(buckets[j], (int)counts[j]);
            return k_map_make(keys, vals, ng);
        }
        if (!strcmp(name, "position")) {
            for (int64_t i = 0; i < l->len; i++)
                if (k_truthy(k_call(args[0], &l->items[i], 1))) return k_some(k_int(i));
            return k_none();
        }
        if (!strcmp(name, "partition")) {
            int64_t n = l->len;
            KValue* yes = (KValue*)k_alloc(sizeof(KValue) * (n < 1 ? 1 : n));
            KValue* no = (KValue*)k_alloc(sizeof(KValue) * (n < 1 ? 1 : n));
            int ny = 0, nn = 0;
            for (int64_t i = 0; i < n; i++) {
                if (k_truthy(k_call(args[0], &l->items[i], 1))) yes[ny++] = l->items[i];
                else no[nn++] = l->items[i];
            }
            KValue pair[2] = { k_list(yes, ny), k_list(no, nn) };
            return k_list(pair, 2);
        }
        if (!strcmp(name, "window")) {
            if (args[0].tag != K_INT || args[0].as.i < 1) k_panic("`window` needs a positive Int");
            int64_t w = args[0].as.i;
            if (l->len < w) return k_list((KValue*)0, 0);
            int64_t cnt = l->len - w + 1;
            KValue* out = k_alloc(sizeof(KValue) * cnt);
            for (int64_t i = 0; i < cnt; i++) out[i] = k_list(l->items + i, (int)w);
            return k_list(out, (int)cnt);
        }
        if (!strcmp(name, "chunk")) {
            if (args[0].tag != K_INT || args[0].as.i < 1) k_panic("`chunk` needs a positive Int");
            int64_t w = args[0].as.i;
            int64_t cnt = (l->len + w - 1) / w;
            KValue* out = k_alloc(sizeof(KValue) * (cnt < 1 ? 1 : cnt));
            int64_t n = 0;
            for (int64_t i = 0; i < l->len; i += w) {
                int64_t len = l->len - i < w ? l->len - i : w;
                out[n++] = k_list(l->items + i, (int)len);
            }
            return k_list(out, (int)cnt);
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
        if (!strcmp(name, "trim") || !strcmp(name, "trim_start") || !strcmp(name, "trim_end")) {
            int do_start = strcmp(name, "trim_end") != 0;   /* trim + trim_start */
            int do_end = strcmp(name, "trim_start") != 0;   /* trim + trim_end */
            const char* a = s;
            const char* z = s + strlen(s);
            if (do_start)
                while (*a == ' ' || *a == '\t' || *a == '\n' || *a == '\r') a++;
            if (do_end)
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
            if (fl == 0) k_panic("`replace` needs a non-empty pattern");
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
            /* Match Rust's i64::from_str: no leading/surrounding whitespace
               (strtoll skips leading ws — reject it), and overflow is a failure
               (strtoll saturates + sets ERANGE — reject it), not a saturated value. */
            char c0 = s[0];
            if (c0==' '||c0=='\t'||c0=='\n'||c0=='\r'||c0=='\v'||c0=='\f') return k_none();
            char* end;
            errno = 0;
            long long v = strtoll(s, &end, 10);
            if (end == s || *end != 0 || errno == ERANGE) return k_none();
            return k_some(k_int((int64_t)v));
        }
        if (!strcmp(name, "parse_float")) {
            /* Match Rust: no leading whitespace (strtod skips it). Overflow to
               inf is fine — Rust parses "1e999" as inf too. */
            char c0 = s[0];
            if (c0==' '||c0=='\t'||c0=='\n'||c0=='\r'||c0=='\v'||c0=='\f') return k_none();
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
        if (!strcmp(name, "is_empty")) return k_bool(s[0] == 0);
        if (!strcmp(name, "reverse")) {
            /* collect UTF-8 char boundaries, then emit in reverse */
            const char* starts[8192]; int lens[8192]; int nc = 0;
            const char* p = s;
            while (*p && nc < 8192) {
                int len = 1;
                if ((*p & 0xF8) == 0xF0) len = 4;
                else if ((*p & 0xF0) == 0xE0) len = 3;
                else if ((*p & 0xE0) == 0xC0) len = 2;
                starts[nc] = p; lens[nc] = len; nc++;
                p += len;
            }
            size_t sl = strlen(s);
            char* out = k_alloc(sl + 1);
            size_t o = 0;
            for (int i = nc - 1; i >= 0; i--) { memcpy(out + o, starts[i], (size_t)lens[i]); o += lens[i]; }
            out[o] = 0;
            return k_str(out);
        }
        if (!strcmp(name, "lines")) {
            KValue parts[4096]; int n = 0;
            const char* p = s;
            while (*p && n < 4095) {
                const char* q = strchr(p, '\n');
                const char* end = q ? q : p + strlen(p);
                const char* z = end;
                if (z > p && z[-1] == '\r') z--;              /* strip trailing CR */
                char* piece = k_alloc((size_t)(z - p) + 1);
                memcpy(piece, p, (size_t)(z - p)); piece[z - p] = 0;
                parts[n++] = k_str(piece);
                if (!q) break;
                p = q + 1;
            }
            return k_list(parts, n);
        }
        if (!strcmp(name, "rfind")) {
            const char* sub = args[0].as.s;
            size_t sl = strlen(sub);
            const char* last = 0;
            if (sl == 0) {
                last = s + strlen(s);       /* Rust rfind("") == len */
            } else {
                const char* p = s;
                for (;;) { const char* q = strstr(p, sub); if (!q) break; last = q; p = q + 1; }
            }
            if (!last) return k_none();
            int64_t idx = 0;
            for (const char* c = s; c < last; c++) if ((*c & 0xC0) != 0x80) idx++;
            return k_some(k_int(idx));
        }
        if (!strcmp(name, "replace_first")) {
            const char* from = args[0].as.s;
            const char* to = args[1].as.s;
            if (!from[0]) k_panic("`replace_first` needs a non-empty pattern");
            const char* q = strstr(s, from);
            if (!q) return recv;            /* not found (from non-empty): unchanged */
            KBuf b = { 0, 0, 0 };
            kb_write(&b, s, (long)(q - s));
            kb_write(&b, to, (long)strlen(to));
            kb_write(&b, q + strlen(from), (long)strlen(q + strlen(from)));
            return k_str(b.buf ? b.buf : "");
        }
        if (!strcmp(name, "split_once")) {
            const char* sep = args[0].as.s;
            const char* q = strstr(s, sep);
            if (!q) return k_none();
            size_t sl = strlen(sep);
            char* a = (char*)k_alloc((size_t)(q - s) + 1);
            memcpy(a, s, (size_t)(q - s)); a[q - s] = 0;
            const char* tail = q + sl;
            char* bb = (char*)k_alloc(strlen(tail) + 1);
            memcpy(bb, tail, strlen(tail) + 1);
            KValue pair[2] = { k_str(a), k_str(bb) };
            return k_some(k_list(pair, 2));
        }
        if (!strcmp(name, "index_of")) {
            const char* q = strstr(s, args[0].as.s);
            if (!q) return k_none();
            int64_t idx = 0;
            for (const char* p = s; p < q; p++) if ((*p & 0xC0) != 0x80) idx++;
            return k_some(k_int(idx));
        }
        if (!strcmp(name, "count")) {
            const char* sub = args[0].as.s;
            size_t sublen = strlen(sub);
            if (sublen == 0) k_panic("`count` needs a non-empty Str");
            int64_t n = 0;
            const char* p = s;
            for (;;) { const char* q = strstr(p, sub); if (!q) break; n++; p = q + sublen; }
            return k_int(n);
        }
        if (!strcmp(name, "slice")) {
            int64_t a = args[0].as.i, b = args[1].as.i;
            const char* starts[8192]; int lens[8192]; int nc = 0;
            const char* p = s;
            while (*p && nc < 8192) {
                int len = 1;
                if ((*p & 0xF8) == 0xF0) len = 4;
                else if ((*p & 0xF0) == 0xE0) len = 3;
                else if ((*p & 0xE0) == 0xC0) len = 2;
                starts[nc] = p; lens[nc] = len; nc++;
                p += len;
            }
            int64_t lo = a < 0 ? 0 : (a > nc ? nc : a);
            int64_t amax = a < 0 ? 0 : a;
            int64_t hi = b < amax ? amax : (b > nc ? nc : b);
            KBuf buf = {0};
            for (int64_t i = lo; i < hi; i++) {
                char c[5]; memcpy(c, starts[i], (size_t)lens[i]); c[lens[i]] = 0;
                kb_puts(&buf, c);
            }
            return k_str(buf.buf ? buf.buf : "");
        }
        if (!strcmp(name, "pad_left") || !strcmp(name, "pad_right")) {
            int left = name[4] == 'l';
            if (args[0].tag != K_INT) k_panic("`pad` needs an Int width");
            int64_t width = args[0].as.i;
            const char* fill = args[1].as.s;
            /* fill with the first CHAR of `fill` (a full UTF-8 codepoint, not one
               byte) — matches the interpreter; a byte-only fill corrupted multibyte
               pad chars (é.pad_right(3,"日") -> "é??"). Empty fill -> space. */
            const char* fc = fill[0] ? fill : " ";
            int fcl = k_utf8_len((unsigned char)fc[0]);
            int64_t cur = 0;
            for (const char* p = s; *p; p++) if ((*p & 0xC0) != 0x80) cur++;
            if (cur >= width || width > 100000000) return k_str(s);
            int64_t pad = width - cur;
            size_t sl = strlen(s);
            char* out = k_alloc(sl + (size_t)pad * fcl + 1);
            if (left) {
                for (int64_t i = 0; i < pad; i++) memcpy(out + i * fcl, fc, fcl);
                memcpy(out + (size_t)pad * fcl, s, sl + 1);
            } else {
                memcpy(out, s, sl);
                for (int64_t i = 0; i < pad; i++) memcpy(out + sl + i * fcl, fc, fcl);
                out[sl + (size_t)pad * fcl] = 0;
            }
            return k_str(out);
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
        if (!strcmp(name, "pow")) {
            int64_t e = args[0].as.i;
            if (e < 0) k_panic("`pow` needs a non-negative exponent");
            int64_t r = 1, base = recv.as.i;
            for (int64_t i = 0; i < e; i++)
                if (__builtin_mul_overflow(r, base, &r)) k_panic("integer overflow in pow");
            return k_int(r);
        }
        if (!strcmp(name, "gcd")) {
            uint64_t a = recv.as.i < 0 ? (uint64_t)(-(recv.as.i + 1)) + 1 : (uint64_t)recv.as.i;
            uint64_t b = args[0].as.i < 0 ? (uint64_t)(-(args[0].as.i + 1)) + 1 : (uint64_t)args[0].as.i;
            while (b) { uint64_t t = b; b = a % b; a = t; }
            return k_int((int64_t)a);
        }
        if (!strcmp(name, "clamp")) {
            int64_t lo = args[0].as.i, hi = args[1].as.i;
            if (lo > hi) k_panic("`clamp`: lo must not exceed hi");
            int64_t v = recv.as.i;
            return k_int(v < lo ? lo : (v > hi ? hi : v));
        }
        if (!strcmp(name, "sign")) return k_int(recv.as.i > 0 ? 1 : (recv.as.i < 0 ? -1 : 0));
        if (!strcmp(name, "is_even")) return k_bool(recv.as.i % 2 == 0);
        if (!strcmp(name, "is_odd")) return k_bool(recv.as.i % 2 != 0);
        if (!strcmp(name, "to_hex")) return k_int_radix(recv.as.i, 16);
        if (!strcmp(name, "to_binary")) return k_int_radix(recv.as.i, 2);
        if (!strcmp(name, "to_octal")) return k_int_radix(recv.as.i, 8);
        if (!strcmp(name, "to_radix")) {
            int64_t b = args[0].as.i;
            if (b < 2 || b > 36) k_panic("`to_radix` base must be in 2..=36");
            return k_int_radix(recv.as.i, (int)b);
        }
        if (!strcmp(name, "isqrt")) {
            if (recv.as.i < 0) k_panic("`isqrt` of a negative Int");
            return k_int(k_isqrt((uint64_t)recv.as.i));
        }
        if (!strcmp(name, "band")) return k_int(recv.as.i & args[0].as.i);
        if (!strcmp(name, "bor")) return k_int(recv.as.i | args[0].as.i);
        if (!strcmp(name, "bxor")) return k_int(recv.as.i ^ args[0].as.i);
        if (!strcmp(name, "bnot")) return k_int(~recv.as.i);
        if (!strcmp(name, "shl") || !strcmp(name, "shr") || !strcmp(name, "ushr")) {
            int64_t n = args[0].as.i;
            if (n < 0 || n > 63) k_panic("shift amount must be in 0..=63");
            if (!strcmp(name, "shl")) return k_int(recv.as.i << n);
            if (!strcmp(name, "ushr")) return k_int((int64_t)((uint64_t)recv.as.i >> n));
            return k_int(recv.as.i >> n);                               /* shr (arithmetic) */
        }
    }
    if (recv.tag == K_FLOAT) {
        if (!strcmp(name, "to_str")) return k_to_str(recv);
        if (!strcmp(name, "fmt")) return k_format_float(recv.as.f, args[0].as.i);
        if (!strcmp(name, "to_int")) return k_int(k_f2i(recv.as.f));
        if (!strcmp(name, "abs")) return k_float(fabs(recv.as.f));
        if (!strcmp(name, "sqrt")) return k_float(sqrt(recv.as.f));
        if (!strcmp(name, "floor")) return k_float(floor(recv.as.f));
        if (!strcmp(name, "ceil")) return k_float(ceil(recv.as.f));
        if (!strcmp(name, "round")) return k_float(round(recv.as.f));
        if (!strcmp(name, "min")) return k_float(recv.as.f < args[0].as.f ? recv.as.f : args[0].as.f);
        if (!strcmp(name, "max")) return k_float(recv.as.f > args[0].as.f ? recv.as.f : args[0].as.f);
        if (!strcmp(name, "pow")) return k_float(pow(recv.as.f, args[0].as.f));
        if (!strcmp(name, "log")) return k_float(log(recv.as.f));
        if (!strcmp(name, "log10")) return k_float(log10(recv.as.f));
        if (!strcmp(name, "log2")) return k_float(log2(recv.as.f));
        if (!strcmp(name, "cbrt")) return k_float(cbrt(recv.as.f));
        if (!strcmp(name, "atan2")) return k_float(atan2(recv.as.f, args[0].as.f));
        if (!strcmp(name, "hypot")) return k_float(hypot(recv.as.f, args[0].as.f));
        if (!strcmp(name, "format")) {
            int64_t d = args[0].as.i;
            if (d < 0 || d > 100) k_panic("`format` decimals must be in 0..=100");
            char* buf = k_alloc(340 + (size_t)d);
            snprintf(buf, 340 + (size_t)d, "%.*f", (int)d, recv.as.f);
            return k_str(buf);
        }
        if (!strcmp(name, "exp")) return k_float(exp(recv.as.f));
        if (!strcmp(name, "sin")) return k_float(sin(recv.as.f));
        if (!strcmp(name, "cos")) return k_float(cos(recv.as.f));
        if (!strcmp(name, "tan")) return k_float(tan(recv.as.f));
        if (!strcmp(name, "sign")) {
            double v = recv.as.f;
            return k_float(v > 0 ? 1.0 : (v < 0 ? -1.0 : v));
        }
        if (!strcmp(name, "is_nan")) return k_bool(recv.as.f != recv.as.f);
        if (!strcmp(name, "is_infinite")) return k_bool(isinf(recv.as.f));
        if (!strcmp(name, "clamp")) {
            double lo = args[0].as.f, hi = args[1].as.f;
            if (lo > hi) k_panic("`clamp`: lo must not exceed hi");
            double v = recv.as.f;
            return k_float(v < lo ? lo : (v > hi ? hi : v));
        }
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
        if (!strcmp(name, "is_empty")) return k_bool(m->len == 0);
        if (!strcmp(name, "get_or")) {
            for (int64_t i = 0; i < m->len; i++) if (k_eq(m->keys[i], args[0])) return m->vals[i];
            return args[1];
        }
        if (!strcmp(name, "map_values")) {
            KValue* ks = k_alloc(sizeof(KValue) * (m->len < 1 ? 1 : m->len));
            KValue* vs = k_alloc(sizeof(KValue) * (m->len < 1 ? 1 : m->len));
            for (int64_t i = 0; i < m->len; i++) { ks[i] = m->keys[i]; vs[i] = k_call(args[0], &m->vals[i], 1); }
            return k_map_make(ks, vs, m->len);
        }
        if (!strcmp(name, "filter")) {
            KValue* ks = k_alloc(sizeof(KValue) * (m->len < 1 ? 1 : m->len));
            KValue* vs = k_alloc(sizeof(KValue) * (m->len < 1 ? 1 : m->len));
            int64_t n = 0;
            for (int64_t i = 0; i < m->len; i++) {
                KValue fa[2] = { m->keys[i], m->vals[i] };
                KValue keep = k_call(args[0], fa, 2);
                if (keep.tag == K_BOOL && keep.as.b) { ks[n] = m->keys[i]; vs[n] = m->vals[i]; n++; }
            }
            return k_map_make(ks, vs, n);
        }
        if (!strcmp(name, "fold")) {
            KValue acc = args[0];
            for (int64_t i = 0; i < m->len; i++) {
                KValue fa[3] = { acc, m->keys[i], m->vals[i] };
                acc = k_call(args[1], fa, 3);
            }
            return acc;
        }
        if (!strcmp(name, "merge")) {
            KMap* o = args[0].as.map;
            KValue* ks = k_alloc(sizeof(KValue) * (m->len + o->len < 1 ? 1 : m->len + o->len));
            KValue* vs = k_alloc(sizeof(KValue) * (m->len + o->len < 1 ? 1 : m->len + o->len));
            memcpy(ks, m->keys, sizeof(KValue) * m->len);
            memcpy(vs, m->vals, sizeof(KValue) * m->len);
            int64_t n = m->len;
            for (int64_t i = 0; i < o->len; i++) {
                int found = 0;
                for (int64_t j = 0; j < n; j++) if (k_eq(ks[j], o->keys[i])) { vs[j] = o->vals[i]; found = 1; break; }
                if (!found) { ks[n] = o->keys[i]; vs[n] = o->vals[i]; n++; }
            }
            return k_map_make(ks, vs, n);
        }
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
        if (!strcmp(name, "symmetric_difference")) {
            KSet* o = args[0].as.set;
            KValue* out = k_alloc(sizeof(KValue) * ((st->len + o->len) < 1 ? 1 : st->len + o->len));
            int64_t n = 0;
            for (int64_t i = 0; i < st->len; i++) {
                int found = 0;
                for (int64_t j = 0; j < o->len; j++) if (k_eq(st->items[i], o->items[j])) { found = 1; break; }
                if (!found) out[n++] = st->items[i];
            }
            for (int64_t i = 0; i < o->len; i++) {
                int found = 0;
                for (int64_t j = 0; j < st->len; j++) if (k_eq(o->items[i], st->items[j])) { found = 1; break; }
                if (!found) out[n++] = o->items[i];
            }
            return k_set_make(out, n);
        }
        if (!strcmp(name, "to_list")) return k_list(st->items, (int)st->len);
        if (!strcmp(name, "is_empty")) return k_bool(st->len == 0);
        if (!strcmp(name, "is_subset")) {
            KSet* o = args[0].as.set;
            for (int64_t i = 0; i < st->len; i++) {
                int found = 0;
                for (int64_t j = 0; j < o->len; j++) if (k_eq(st->items[i], o->items[j])) { found = 1; break; }
                if (!found) return k_bool(0);
            }
            return k_bool(1);
        }
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
        /* Option/Result combinators — guarded on the variant so user ADTs with a
           like-named method still fall through to the UFCS lookup below. */
        {
            int is_some = k_ctor_variant_is(recv, "Some");
            int is_none = k_ctor_variant_is(recv, "None");
            int is_ok = k_ctor_variant_is(recv, "Ok");
            int is_err = k_ctor_variant_is(recv, "Err");
            KValue inner = recv.as.ctor->nfields ? recv.as.ctor->fields[0] : k_unit();
            if (!strcmp(name, "map") && (is_some || is_none || is_ok || is_err)) {
                if (is_some) return k_some(k_call(args[0], &inner, 1));
                if (is_ok) return k_ok(k_call(args[0], &inner, 1));
                return recv;
            }
            if (!strcmp(name, "and_then") && (is_some || is_none || is_ok || is_err)) {
                if (is_some || is_ok) return k_call(args[0], &inner, 1);
                return recv;
            }
            if (!strcmp(name, "filter") && (is_some || is_none)) {
                if (is_some) { KValue b = k_call(args[0], &inner, 1); return (b.tag == K_BOOL && b.as.b) ? recv : k_none(); }
                return k_none();
            }
            if (!strcmp(name, "ok_or") && (is_some || is_none)) {
                return is_some ? k_ok(inner) : k_err(args[0]);
            }
            if (!strcmp(name, "map_err") && (is_ok || is_err)) {
                if (is_err) return k_err(k_call(args[0], &inner, 1));
                return recv;
            }
            if (!strcmp(name, "ok") && (is_ok || is_err)) {
                return is_ok ? k_some(inner) : k_none();
            }
        }
    }
    // UFCS: no built-in method matched — call a top-level function of this name
    // with the receiver prepended (`recv.f(args)` -> `f(recv, args…)`).
    for (int i = 0; i < K_NUFCS; i++) {
        if (!strcmp(UFCS_FUNS[i].name, name)) {
            KValue* full = (KValue*)k_alloc(sizeof(KValue) * (argc + 1));
            full[0] = recv;
            for (int j = 0; j < argc; j++) full[j + 1] = args[j];
            return k_call(k_fun(UFCS_FUNS[i].fnid), full, argc + 1);
        }
    }
    k_panic("no such method");
    return k_unit();
}
"#;

/// Component runtime (native component apps): instances, state, wires, the
/// message queue, and drain — a structural mirror of `vm.rs`. Emitted after
/// RUNTIME; references CHUNKS (declared in RUNTIME) and COMPS (emitted per
/// module). Byte-identity hinges on reproducing vm.rs orderings exactly:
/// creation-order instance ids, @start order, FIFO queue, wire push-order.
const COMPONENT_RUNTIME: &str = r#"
/* --- component runtime (native component apps) --- */
typedef struct { const char* port; int chunk; int has_param; } KHandler;
typedef struct { int chunk; int every; long long interval_ms; } KTimerMeta;
typedef struct { const char* name; int chunk; } KExpose;
typedef struct { const char* name; int is_app; int nslots; int init_chunk; int restart_chunk;
                 const KHandler* handlers; int nhandlers;
                 const KTimerMeta* timers; int ntimers;
                 const KExpose* exposes; int nexposes; } KCompMeta;
extern const KCompMeta COMPS[];
static KValue k_component(int id) { KValue v; v.tag = K_COMPONENT; v.as.i = id; return v; }

typedef struct { const char* out_port; int to; const char* in_port; } KWire;
/* a live timer on an instance (armed copy of the component's KTimerMeta) */
typedef struct { int chunk; int every; long long interval; long long next_fire; int active; } KTimer;
typedef struct { int comp; KValue* slots; int nslots;
                 KWire* wires; int nwires; int restart_on_failure;
                 KTimer* timers; int ntimers; } KInstance;
static KInstance* k_insts = 0;
static int k_ninsts = 0;
static int k_cur_inst = -1;
static int k_print_unwired = 0;
static long long k_vnow = 0;  /* virtual clock (ms), advanced explicitly */

/* forward declarations for the mutually-recursive component driver */
static void k_run_lifecycle(int id, const char* key);
static void k_arm_timers(int id);
static void k_restart(int id, const char* msg);
static void k_dispatch(int id, int chunk, KValue* arg);

/* FIFO message queue (grow-only; head advances — arena model, bounded runs) */
typedef struct { int id; const char* port; KValue value; } KMsg;
static KMsg* k_queue = 0;
static int k_qhead = 0, k_qlen = 0, k_qcap = 0;
static void k_enqueue(int id, const char* port, KValue v) {
    if (k_qlen == k_qcap) { k_qcap = k_qcap ? k_qcap * 2 : 16;
        k_queue = (KMsg*)realloc(k_queue, sizeof(KMsg) * k_qcap); }
    k_queue[k_qlen].id = id; k_queue[k_qlen].port = port; k_queue[k_qlen].value = v; k_qlen++;
}

static int k_instantiate(int comp_idx, KValue* props, int nprops) {
    k_insts = (KInstance*)realloc(k_insts, sizeof(KInstance) * (k_ninsts + 1));
    int id = k_ninsts++;                 /* id assigned BEFORE init runs (DFS pre-order) */
    int ns = COMPS[comp_idx].nslots;
    k_insts[id].comp = comp_idx;
    k_insts[id].nslots = ns;
    k_insts[id].slots = (KValue*)malloc(sizeof(KValue) * (ns > 0 ? ns : 1));
    for (int i = 0; i < ns; i++) k_insts[id].slots[i] = (i < nprops) ? props[i] : k_unit();
    k_insts[id].wires = 0; k_insts[id].nwires = 0; k_insts[id].restart_on_failure = 0;
    k_insts[id].timers = 0; k_insts[id].ntimers = 0;
    int saved = k_cur_inst;
    k_cur_inst = id;
    CHUNKS[COMPS[comp_idx].init_chunk](0, 0);   /* children created here get higher ids */
    k_cur_inst = saved;
    return id;
}

static KValue k_state_get(int slot) { return k_insts[k_cur_inst].slots[slot]; }
static void k_state_set(int slot, KValue v) { k_insts[k_cur_inst].slots[slot] = v; }

/* call an expose on a component instance: run its chunk with THAT instance
   current (so its state ops hit the right slots) — mirrors vm.rs Op::Method. */
static KValue k_expose_call(KValue recv, const char* name, KValue* args, int argc) {
    (void)argc;
    int id = (int)recv.as.i;
    const KCompMeta* cm = &COMPS[k_insts[id].comp];
    for (int i = 0; i < cm->nexposes; i++) {
        if (!strcmp(cm->exposes[i].name, name)) {
            int saved = k_cur_inst; k_cur_inst = id;
            KValue r = CHUNKS[cm->exposes[i].chunk](0, args);
            k_cur_inst = saved;
            return r;
        }
    }
    fprintf(stderr, "panic: component `%s` does not expose `%s`\n", cm->name, name);
    exit(101);
}

static void k_wire(int from, const char* out_port, int to, const char* in_port) {
    KInstance* s = &k_insts[from];
    s->wires = (KWire*)realloc(s->wires, sizeof(KWire) * (s->nwires + 1));
    s->wires[s->nwires].out_port = out_port;
    s->wires[s->nwires].to = to;
    s->wires[s->nwires].in_port = in_port;
    s->nwires++;
}

/* emit on the CURRENT instance's out port: fan out to wired targets in push
   order; if none and print_unwired, print "{comp}.{port} = {value}". */
static void k_emit(const char* port, KValue value) {
    KInstance* inst = &k_insts[k_cur_inst];
    int found = 0;
    for (int i = 0; i < inst->nwires; i++) {
        if (!strcmp(inst->wires[i].out_port, port)) {
            found = 1;
            k_enqueue(inst->wires[i].to, inst->wires[i].in_port, value);
        }
    }
    if (!found && k_print_unwired) {
        printf("%s.%s = %s\n", COMPS[inst->comp].name, port, k_show(value));
    }
}

/* Dispatch a chunk on an instance. If the instance is supervised, catch a panic
   via a setjmp pad and restart it (mirrors the VM's restart-on-failure branch);
   otherwise a panic propagates (k_pad unchanged → exit 101 at top level). */
static void k_dispatch(int id, int chunk, KValue* arg) {
    if (!k_insts[id].restart_on_failure) {
        int saved = k_cur_inst; k_cur_inst = id;
        CHUNKS[chunk](0, arg);
        k_cur_inst = saved;
        return;
    }
    jmp_buf pad; jmp_buf* prev = k_pad; k_pad = &pad;
    int saved = k_cur_inst; k_cur_inst = id;
    if (setjmp(pad) == 0) {
        CHUNKS[chunk](0, arg);
        k_cur_inst = saved; k_pad = prev;
    } else {
        /* panic caught: restore, then restart this instance */
        k_cur_inst = saved; k_pad = prev;
        k_restart(id, k_panic_buf);
    }
}

/* drain the queue to quiescence: pop front, dispatch by first-match handler */
static void k_drain(void) {
    while (k_qhead < k_qlen) {
        KMsg m = k_queue[k_qhead++];
        KInstance* inst = &k_insts[m.id];
        const KCompMeta* cm = &COMPS[inst->comp];
        for (int i = 0; i < cm->nhandlers; i++) {
            if (!strcmp(cm->handlers[i].port, m.port)) {
                KValue a = m.value;
                k_dispatch(m.id, cm->handlers[i].chunk, cm->handlers[i].has_param ? &a : 0);
                break;   /* linear first-match */
            }
        }
    }
}

/* run a named lifecycle handler (@start/@stop) on an instance, if present */
static void k_run_lifecycle(int id, const char* key) {
    const KCompMeta* cm = &COMPS[k_insts[id].comp];
    for (int i = 0; i < cm->nhandlers; i++) {
        if (!strcmp(cm->handlers[i].port, key)) {
            k_dispatch(id, cm->handlers[i].chunk, 0);
            return;
        }
    }
}

/* arm an instance's timers relative to the current virtual time */
static void k_arm_timers(int id) {
    const KCompMeta* cm = &COMPS[k_insts[id].comp];
    KInstance* inst = &k_insts[id];
    inst->ntimers = cm->ntimers;
    inst->timers = cm->ntimers ? (KTimer*)malloc(sizeof(KTimer) * cm->ntimers) : 0;
    for (int i = 0; i < cm->ntimers; i++) {
        inst->timers[i].chunk = cm->timers[i].chunk;
        inst->timers[i].every = cm->timers[i].every;
        inst->timers[i].interval = cm->timers[i].interval_ms;
        inst->timers[i].next_fire = k_vnow + cm->timers[i].interval_ms;
        inst->timers[i].active = 1;
    }
}

/* supervision restart: [supervise] line, reset state, re-run @start, re-arm */
static void k_restart(int id, const char* msg) {
    const KCompMeta* cm = &COMPS[k_insts[id].comp];
    fprintf(stderr, "[supervise] %s restarted after panic: %s\n", cm->name, msg);
    int saved = k_cur_inst; k_cur_inst = id;
    CHUNKS[cm->restart_chunk](0, 0);
    k_cur_inst = saved;
    k_run_lifecycle(id, "@start");
    k_arm_timers(id);
}

/* advance the virtual clock to now+dur, firing due timers in (time, instance,
   decl) order, draining between fires — verbatim from vm.rs::advance */
static void k_advance(long long dur) {
    long long target = k_vnow + dur;
    for (;;) {
        long long bt = 0; int bi = -1, btk = -1;
        for (int iid = 0; iid < k_ninsts; iid++) {
            KInstance* in = &k_insts[iid];
            for (int ti = 0; ti < in->ntimers; ti++) {
                KTimer* t = &in->timers[ti];
                if (t->active && t->next_fire <= target) {
                    if (bi < 0 || t->next_fire < bt || (t->next_fire == bt && (iid < bi || (iid == bi && ti < btk)))) {
                        bt = t->next_fire; bi = iid; btk = ti;
                    }
                }
            }
        }
        if (bi < 0) break;
        k_vnow = bt;
        k_dispatch(bi, k_insts[bi].timers[btk].chunk, 0);
        k_drain();
        KTimer* t = &k_insts[bi].timers[btk];
        if (t->every) t->next_fire += t->interval; else t->active = 0;
    }
    k_vnow = target;
}

/* bounded timer firing for `kupl run` — mirrors vm.rs::run_timers(100) */
static void k_run_timers(int max_fires) {
    for (int n = 0; n < max_fires; n++) {
        long long bt = 0; int bi = -1, btk = -1;
        for (int iid = 0; iid < k_ninsts; iid++) {
            KInstance* in = &k_insts[iid];
            for (int ti = 0; ti < in->ntimers; ti++) {
                KTimer* t = &in->timers[ti];
                if (t->active) {
                    if (bi < 0 || t->next_fire < bt || (t->next_fire == bt && (iid < bi || (iid == bi && ti < btk)))) {
                        bt = t->next_fire; bi = iid; btk = ti;
                    }
                }
            }
        }
        if (bi < 0) break;
        k_advance(bt - k_vnow);
    }
}
"#;

#[cfg(test)]
mod tests {
    fn cc() -> String {
        std::env::var("CC").unwrap_or_else(|_| "cc".to_string())
    }
    fn cc_available() -> bool {
        std::process::Command::new(cc())
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// A single-component app compiles to native and prints the same as the
    /// interpreter/KVM. Skipped (passes) where no C compiler is available.
    #[test]
    fn native_single_component_app() {
        if !cc_available() {
            return;
        }
        let src = "app C {\n    state n: Int = 0\n    on start {\n        n = n + 1\n        n = n + 41\n        print(\"n={n}\")\n    }\n}\n";
        let compiled = crate::run::compile(src).expect("program compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let c = super::emit_c(&module).expect("emit_c succeeds for a single-component app");

        let base = std::env::temp_dir().join(format!("kupl-cgen-{}", std::process::id()));
        let cpath = base.with_extension("c");
        let bin = base.with_extension("out");
        std::fs::write(&cpath, &c).unwrap();
        let status = std::process::Command::new(cc())
            .args(["-O2", "-o", bin.to_str().unwrap(), cpath.to_str().unwrap()])
            .status()
            .expect("cc runs");
        assert!(status.success(), "generated C must compile");
        let out = std::process::Command::new(&bin).output().expect("binary runs");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "n=42\n");

        let _ = std::fs::remove_file(&cpath);
        let _ = std::fs::remove_file(&bin);
    }

    /// A multi-component app (children + wires + emit) compiles to native and
    /// matches the interpreter, exercising the message queue and drain.
    #[test]
    fn native_multi_component_wires() {
        if !cc_available() {
            return;
        }
        // Driver emits three ticks into Sink via a wire; Sink accumulates + prints.
        let src = "component Sink {\n    in tick: Int\n    state total: Int = 0\n    \
                   on tick(n) {\n        total = total + n\n        print(\"total={total}\")\n    }\n}\n\
                   component Driver {\n    out pulse: Int\n    \
                   on start {\n        emit pulse(10)\n        emit pulse(20)\n        emit pulse(30)\n    }\n}\n\
                   app A {\n    let sink = Sink()\n    let driver = Driver()\n    \
                   wire driver.pulse -> sink.tick\n}\n";
        let compiled = crate::run::compile(src).expect("program compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let c = super::emit_c(&module).expect("multi-component app compiles to C");
        let base = std::env::temp_dir().join(format!("kupl-cgen-mc-{}", std::process::id()));
        let cpath = base.with_extension("c");
        let bin = base.with_extension("out");
        std::fs::write(&cpath, &c).unwrap();
        let status = std::process::Command::new(cc())
            .args(["-O2", "-o", bin.to_str().unwrap(), cpath.to_str().unwrap()])
            .status()
            .expect("cc runs");
        assert!(status.success(), "generated C must compile");
        let out = std::process::Command::new(&bin).output().expect("binary runs");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "total=10\ntotal=30\ntotal=60\n");
        let _ = std::fs::remove_file(&cpath);
        let _ = std::fs::remove_file(&bin);
    }

    /// Compile `src` (a component app) to native, run it, and return stdout.
    #[cfg(test)]
    fn native_stdout(src: &str, tag: &str) -> String {
        let compiled = crate::run::compile(src).expect("program compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let c = super::emit_c(&module).expect("emit_c succeeds");
        let base = std::env::temp_dir().join(format!("kupl-cgen-{tag}-{}", std::process::id()));
        let cpath = base.with_extension("c");
        let bin = base.with_extension("out");
        std::fs::write(&cpath, &c).unwrap();
        let status = std::process::Command::new(cc())
            .args(["-O2", "-o", bin.to_str().unwrap(), cpath.to_str().unwrap()])
            .status()
            .expect("cc runs");
        assert!(status.success(), "generated C must compile");
        let out = std::process::Command::new(&bin).output().expect("binary runs");
        let _ = std::fs::remove_file(&cpath);
        let _ = std::fs::remove_file(&bin);
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Float.fmt (it73) compiles to native and matches the interpreter's manual
    /// fixed-precision formatting byte-for-byte.
    #[test]
    fn native_format_float() {
        let src = "fun main() uses io {\n    print(3.14159.fmt(2))\n    print(2.5.fmt(0))\n    \
                   print((0.0 - 1.5).fmt(1))\n    print(0.0.fmt(2))\n}\n";
        if cc_available() {
            assert_eq!(native_main_stdout(src, "format"), "3.14\n3\n-1.5\n0.00\n");
        }
    }

    /// Option/Result combinators (it77) compile to native (callbacks via k_call):
    /// a map/and_then/ok_or/map_err/ok chain matches the interpreter.
    #[test]
    fn native_combinators() {
        let src = "fun main() uses io {\n    \
                   print(\"8\".parse_int().map(fn x { x * 2 }).unwrap_or(0))\n    \
                   print(\"bad\".parse_int().map(fn x { x + 1 }).unwrap_or(-1))\n    \
                   print(Ok(3).map(fn x { x + 1 }).ok().unwrap_or(0))\n    \
                   print(Some(5).ok_or(\"no\").map_err(fn e { e }).unwrap_or(0))\n}\n";
        if cc_available() {
            assert_eq!(native_main_stdout(src, "combinators"), "16\n-1\n4\n5\n");
        }
    }

    /// Native ai-fun response conversion matches the interpreter on Ok VALUES for
    /// Float/Bool/List shapes, and on the REJECT decision for malformed responses
    /// (the error-message text may differ — engine-dependent, like JSON errors).
    #[test]
    fn native_ai_typed_shapes_consistent() {
        if !cc_available() {
            return;
        }
        let build = |ret: &str| -> std::path::PathBuf {
            let src = format!(
                "ai fun f(x: Str) -> {ret} {{\n    intent \"v {{x}}\"\n}}\n\
                 fun main() uses io {{\n    print(f(\"x\"))\n}}\n"
            );
            let compiled = crate::run::compile(&src).expect("compiles");
            let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
                .expect("module");
            let c = super::emit_c(&module).expect("emit_c");
            let base = std::env::temp_dir()
                .join(format!("kupl-aishape-{}-{}", ret.replace(['[', ']'], "_"), std::process::id()));
            let cpath = base.with_extension("c");
            let bin = base.with_extension("out");
            std::fs::write(&cpath, &c).unwrap();
            assert!(std::process::Command::new(cc())
                .args(["-O2", "-o", bin.to_str().unwrap(), cpath.to_str().unwrap()])
                .status()
                .unwrap()
                .success());
            let _ = std::fs::remove_file(&cpath);
            bin
        };
        let run = |bin: &std::path::Path, mock: &str| -> (String, String) {
            let out = std::process::Command::new(bin)
                .env("KUPL_AI_MOCK_F", mock)
                .output()
                .unwrap();
            (
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
                String::from_utf8_lossy(&out.stderr).to_string(),
            )
        };
        let flt = build("Float");
        assert_eq!(run(&flt, "1.5").0, "1.5");
        assert_eq!(run(&flt, "3").0, "3.0");
        assert_eq!(run(&flt, "1e999").0, "inf");
        assert!(run(&flt, "abc").1.contains("not valid JSON")); // rejected
        let bl = build("Bool");
        assert_eq!(run(&bl, "true").0, "true");
        assert_eq!(run(&bl, "false").0, "false");
        assert!(!run(&bl, "1").1.is_empty()); // Num is not a Bool -> rejected
        let li = build("List[Int]");
        assert_eq!(run(&li, "[1,2,3]").0, "[1, 2, 3]");
        assert_eq!(run(&li, "[]").0, "[]");
        assert!(run(&li, "[999999999999999999999]").1.contains("expected an integer")); // overflow elem rejected
        let _ = std::fs::remove_file(&flt);
        let _ = std::fs::remove_file(&bl);
        let _ = std::fs::remove_file(&li);
    }

    /// Native Display of nested/complex values (nested lists, Map/Set, Option
    /// nesting, reduced Rationals) is byte-identical to the interpreter. PR-it40.
    #[test]
    fn native_nested_value_display() {
        if !cc_available() {
            return;
        }
        let src = "fun main() uses io {\n    print([[1, 2], [3], []])\n    \
                   print([Some(1), None, Some(3)])\n    \
                   print(Map().insert(\"a\", [1, 2]).insert(\"b\", [3]))\n    \
                   print(Set([3, 1, 2]))\n    print([rat(1, 2), rat(2, 4)])\n}\n";
        assert_eq!(
            native_main_stdout(src, "nestdisp").trim(),
            "[[1, 2], [3], []]\n[Some(1), None, Some(3)]\nMap{\"a\": [1, 2], \"b\": [3]}\nSet{3, 1, 2}\n[1/2, 1/2]"
        );
    }

    /// Directory IO ops (list_dir/remove_dir/make_dir) match the interpreter's
    /// Ok/Err decision AND io::Error message on edge inputs. PR-it39.
    #[test]
    fn native_dir_io_matches_interp() {
        if !cc_available() {
            return;
        }
        // list_dir of the current dir (exists) is Ok; of a missing path errors with
        // the os-error message (was a custom "cannot read directory: …").
        let miss = "fun main() uses io {\n    \
                    match list_dir(\"/no/such/dir/xyzzy\") { Ok(_) => print(\"ok\"), Err(e) => print(\"err:{e}\") }\n}\n";
        assert_eq!(native_main_stdout(miss, "lsmiss").trim(), "err:No such file or directory (os error 2)");
        // make_dir on an existing FILE errors "File exists" (not a bogus Ok).
        let onfile = "fun main() uses io {\n    let _ = write_file(\"/tmp/kupl_it39_probe\", \"x\")\n    \
                      match make_dir(\"/tmp/kupl_it39_probe\") { Ok(_) => print(\"ok\"), Err(e) => print(\"err:{e}\") }\n}\n";
        assert_eq!(native_main_stdout(onfile, "mkfile").trim(), "err:File exists (os error 17)");
    }

    /// Native IO error VALUES match the interpreter (Rust io::Error Display):
    /// "<message> (os error N)", owned (k_strdup). Reading a directory ERRORS like
    /// the interpreter (not Ok("")). PR-it37 (lifetime) + PR-it38 (text + isdir).
    #[test]
    fn native_io_error_message_owned() {
        if !cc_available() {
            return;
        }
        let miss = "fun main() uses io {\n    \
                    match read_file(\"/no/such/path/xyzzy\") { Ok(c) => print(c), Err(e) => print(\"err:{e}\") }\n}\n";
        assert_eq!(
            native_main_stdout(miss, "ioerr").trim(),
            "err:No such file or directory (os error 2)"
        );
        // reading the current directory (always exists) errors, not Ok("").
        let dir = "fun main() uses io {\n    \
                   match read_file(\".\") { Ok(c) => print(\"ok:{c.len()}\"), Err(e) => print(\"err:{e}\") }\n}\n";
        assert_eq!(native_main_stdout(dir, "iodir").trim(), "err:Is a directory (os error 21)");
    }

    /// Native parse_iso returns the SAME descriptive Err message as the interpreter
    /// (was Err("") — a dangling stack buffer). PR-it36.
    #[test]
    fn native_parse_iso_error_message() {
        if !cc_available() {
            return;
        }
        let src = "fun main() uses io {\n    \
                   match parse_iso(\"2020-13-01T00:00:00Z\") { Ok(t) => print(t), Err(m) => print(m) }\n}\n";
        assert_eq!(
            native_main_stdout(src, "isoerr").trim(),
            "invalid ISO-8601 timestamp: 2020-13-01T00:00:00"
        );
    }

    /// Native JSON round-trip preserves object key order + .sort_by is stable,
    /// matching the interpreter. PR-it32.
    #[test]
    fn native_json_order_and_sort_stable() {
        if !cc_available() {
            return;
        }
        let json = "fun main() uses io {\n    \
                    match json_parse(\"{{ \\\"b\\\": 1, \\\"a\\\": 2, \\\"c\\\": 3 }}\") { \
                    Ok(j) => print(json_stringify(j)), Err(e) => print(e) }\n}\n";
        assert_eq!(native_main_stdout(json, "jord").trim(), "{\"b\":1,\"a\":2,\"c\":3}");
        let sort = "type R = R(k: Int, t: Str)\nfun main() uses io {\n    var o = \"\"\n    \
                    for r in [R(2, \"a\"), R(1, \"b\"), R(2, \"c\"), R(1, \"d\"), R(3, \"e\"), R(1, \"f\")].sort_by(fn r { r.k }) { o = o + \"{r.t}\" }\n    print(o)\n}\n";
        assert_eq!(native_main_stdout(sort, "sstab").trim(), "bdface");
    }

    /// Native Map/Set iterate in INSERTION order — deterministic and identical to
    /// the interpreter (no randomized-HashMap ordering). PR-it31.
    #[test]
    fn native_map_set_insertion_order() {
        if !cc_available() {
            return;
        }
        let src = "fun main() uses io {\n    \
                   let m = Map().insert(\"b\", 1).insert(\"a\", 2).insert(\"c\", 3)\n    \
                   print(m.keys())\n    print(m.remove(\"a\").keys())\n    \
                   print(Set([5, 1, 3, 9, 2, 7, 1, 5]).to_list())\n}\n";
        assert_eq!(
            native_main_stdout(src, "mapord").trim(),
            "[\"b\", \"a\", \"c\"]\n[\"b\", \"c\"]\n[5, 1, 3, 9, 2, 7]"
        );
    }

    /// Native f64 Display is positional shortest-round-trip for ALL magnitudes,
    /// matching the interpreter — small values are not scientific and large whole
    /// values are not truncated (the 64-byte buffer clipped 1e300). PR-it30.
    #[test]
    fn native_float_display_positional() {
        if !cc_available() {
            return;
        }
        // small: positional, exact match to interp
        assert_eq!(native_main_stdout("fun main() uses io {\n    print(0.00001)\n}\n", "fsm").trim(), "0.00001");
        // 1e-300: positional (no exponent), long, starts with the leading zeros
        let tiny = native_main_stdout("fun main() uses io {\n    print(1e-300)\n}\n", "ftiny");
        let tiny = tiny.trim();
        assert!(!tiny.contains(['e', 'E']), "1e-300 must be positional, got {tiny:?}");
        assert!(tiny.starts_with("0.00000000") && tiny.len() > 290, "unexpected {tiny:?}");
        // 1e300: not truncated (was clipped at ~63 chars), no exponent, ends ".0"
        let big = native_main_stdout("fun main() uses io {\n    print(1e300)\n}\n", "fbig");
        let big = big.trim();
        assert!(!big.contains(['e', 'E']) && big.ends_with(".0") && big.len() > 290, "unexpected {big:?}");
    }

    /// Native split/replace/replace_first panic on an empty separator/pattern
    /// (native replace used to no-op, diverging from the interpreter). PR-it29.
    #[test]
    fn native_empty_separator_panics() {
        if !cc_available() {
            return;
        }
        for (src, tag) in [
            ("fun main() uses io {\n    print(\"abc\".split(\"\").len())\n}\n", "spl"),
            ("fun main() uses io {\n    print(\"abc\".replace(\"\", \"x\"))\n}\n", "rep"),
            ("fun main() uses io {\n    print(\"abc\".replace_first(\"\", \"x\"))\n}\n", "rf"),
        ] {
            let out = native_main_stdout(src, tag);
            assert!(out.trim().is_empty(), "{tag}: expected a panic, got stdout {out:?}");
        }
        // normal replace still works
        assert_eq!(
            native_main_stdout("fun main() uses io {\n    print(\"aXbXc\".replace(\"X\", \"-\"))\n}\n", "repok").trim(),
            "a-b-c"
        );
    }

    /// Native .pad_* fills with a full UTF-8 codepoint (was: one byte, corrupting a
    /// multibyte fill char). PR-it28.
    #[test]
    fn native_pad_multibyte_fill() {
        if !cc_available() {
            return;
        }
        let src = "fun main() uses io {\n    print(\"é\".pad_right(3, \"日\"))\n    \
                   print(\"é\".pad_left(3, \"日\"))\n    print(\"x\".pad_right(3, \"🎉\"))\n}\n";
        assert_eq!(native_main_stdout(src, "padmb").trim(), "é日日\n日日é\nx🎉🎉");
    }

    /// Native sized-int narrowing / .pow / abs overflow all PANIC (no C-UB wrap or
    /// bogus value) — matching the interpreter. Certified in PR-it27.
    #[test]
    fn native_numeric_overflow_panics() {
        if !cc_available() {
            return;
        }
        // each program overflows; a clean panic writes to stderr and leaves stdout
        // empty (a C-UB wrap would have printed a bogus value).
        for (src, tag) in [
            ("fun main() uses io {\n    print(300.to_i8())\n}\n", "toi8"),
            ("fun main() uses io {\n    print(2.pow(100))\n}\n", "pow"),
            ("fun main() uses io {\n    print(((0 - 9223372036854775807) - 1).abs())\n}\n", "absmin"),
        ] {
            let out = native_main_stdout(src, tag);
            assert!(out.trim().is_empty(), "{tag}: expected a panic, got stdout {out:?}");
        }
    }

    /// Native Float.to_int() saturates like the interpreter's `as i64` (was a raw
    /// C cast — UB out of range, returned garbage). PR-it26.
    #[test]
    fn native_float_to_int_saturates() {
        if !cc_available() {
            return;
        }
        let src = "fun main() uses io {\n    print((1e30).to_int())\n    \
                   print((0.0 - 1e30).to_int())\n    print((0.0 / 0.0).to_int())\n    \
                   print((1.0 / 0.0).to_int())\n    print((3.7).to_int())\n}\n";
        let out = native_main_stdout(src, "f2i");
        assert_eq!(
            out.trim(),
            "9223372036854775807\n-9223372036854775808\n0\n9223372036854775807\n3"
        );
    }

    /// i64::MIN % -1 overflows: native must panic "integer overflow in remainder"
    /// (C's `%` is UB there and returned a bogus 0 — diverging from the interp,
    /// which itself used to ICE). PR-it25.
    #[test]
    fn native_int_min_rem_overflow() {
        if !cc_available() {
            return;
        }
        let src = "fun main() uses io {\n    let m = (0 - 9223372036854775807) - 1\n    print(m % (0 - 1))\n}\n";
        let out = native_main_stdout(src, "aiminrem");
        // native_main_stdout returns stdout; the panic goes to stderr and aborts,
        // so stdout must be empty (no bogus "0").
        assert!(!out.contains('0'), "expected a panic, not a value; got {out:?}");
    }

    /// A model integer that overflows i64 is REJECTED natively (was: saturated to
    /// i64::MAX — a wrong value), matching the interpreter.
    #[test]
    fn native_ai_int_overflow_rejected() {
        if !cc_available() {
            return;
        }
        let src = "ai fun score(t: Str) -> Int {\n    intent \"rate {t}\"\n}\n\
                   fun main() uses io {\n    print(score(\"x\"))\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let c = super::emit_c(&module).expect("emit_c");
        let base = std::env::temp_dir().join(format!("kupl-aiovf-{}", std::process::id()));
        let cpath = base.with_extension("c");
        let bin = base.with_extension("out");
        std::fs::write(&cpath, &c).unwrap();
        assert!(std::process::Command::new(cc())
            .args(["-O2", "-o", bin.to_str().unwrap(), cpath.to_str().unwrap()])
            .status()
            .unwrap()
            .success());
        let out = std::process::Command::new(&bin)
            .env("KUPL_AI_MOCK_SCORE", "999999999999999999999")
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&cpath);
        let _ = std::fs::remove_file(&bin);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stderr.contains("expected an integer"), "expected rejection, got out={stdout:?} err={stderr:?}");
        assert!(!stdout.contains("9223372036854775807"), "must not saturate to i64::MAX");
    }

    /// Native codec decoders (base64/hex/url) match the interpreter on Ok values
    /// AND detailed Err messages.
    #[test]
    fn native_codec_errors_match() {
        let src = "fun main() uses io {\n    \
                   print(hex_decode(\"abc\"))\n    \
                   print(url_decode(\"a%ZZ\"))\n    \
                   print(base64_decode(\"aGVsbG8=\"))\n}\n";
        if cc_available() {
            assert_eq!(
                native_main_stdout(src, "codec"),
                "Err(\"invalid hex: odd length\")\n\
                 Err(\"invalid percent-encoding: bad hex\")\n\
                 Ok(\"hello\")\n"
            );
        }
    }

    /// NaN/infinity Display matches the interpreter natively (was: `%g` -> "nan").
    #[test]
    fn native_nan_inf_display() {
        let src = "fun main() uses io {\n    print(0.0 / 0.0)\n    \
                   print(1.0 / 0.0)\n    print(-1.0 / 0.0)\n}\n";
        if cc_available() {
            assert_eq!(native_main_stdout(src, "naninf"), "NaN\ninf\n-inf\n");
        }
    }

    /// A control byte followed by a hex digit escapes to native C correctly:
    /// `\xNN` is greedy and would merge (`\x1b`+`f` -> one byte), so cgen emits
    /// fixed-width octal `\NNN`. The string keeps both bytes -> length matches.
    #[test]
    fn native_control_byte_escape_no_merge() {
        // "a", ESC (0x1b), "f" — three chars; ESC is a raw source byte.
        let src = "fun main() uses io {\n    print(\"a\u{1b}f\".len())\n}\n";
        if cc_available() {
            assert_eq!(native_main_stdout(src, "ctrlesc"), "3\n");
        }
    }

    /// Deep recursion in native code hits the same 10 000-frame guard as the
    /// interpreter/KVM and panics cleanly (was: a C-stack segfault).
    #[test]
    fn native_deep_recursion_guard() {
        if !cc_available() {
            return;
        }
        let src = "fun rec(n: Int) -> Int {\n    if n == 0 { 0 } else { rec(n - 1) }\n}\n\
                   fun main() uses io {\n    print(rec(50000))\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let c = super::emit_c(&module).expect("emit_c");
        let base = std::env::temp_dir().join(format!("kupl-rec-{}", std::process::id()));
        let cpath = base.with_extension("c");
        let bin = base.with_extension("out");
        std::fs::write(&cpath, &c).unwrap();
        assert!(std::process::Command::new(cc())
            .args(["-O2", "-o", bin.to_str().unwrap(), cpath.to_str().unwrap()])
            .status()
            .unwrap()
            .success());
        let out = std::process::Command::new(&bin).output().unwrap();
        let _ = std::fs::remove_file(&cpath);
        let _ = std::fs::remove_file(&bin);
        let stderr = String::from_utf8_lossy(&out.stderr);
        // clean panic, not a segfault (segfault -> no output + a signal exit)
        assert!(
            stderr.contains("stack overflow (10000 frames)"),
            "expected clean recursion-depth panic, got stderr={stderr:?} status={:?}",
            out.status
        );
    }

    /// List.take_while / drop_while (it95) compile to native.
    #[test]
    fn native_take_drop_while() {
        let src = "fun main() uses io {\n    \
                   let xs = [2, 4, 5, 6]\n    \
                   print(xs.take_while(fn n { n % 2 == 0 }))\n    \
                   print(xs.drop_while(fn n { n % 2 == 0 }))\n}\n";
        if cc_available() {
            assert_eq!(native_main_stdout(src, "listmore"), "[2, 4]\n[5, 6]\n");
        }
    }

    /// List.group_by (it94) compiles to native, first-seen key order preserved.
    #[test]
    fn native_group_by() {
        let src = "fun main() uses io {\n    \
                   let g = [1, 2, 3, 4, 5].group_by(fn n { n % 2 })\n    \
                   print(g.get(1))\n    print(g.get(0))\n}\n";
        if cc_available() {
            assert_eq!(native_main_stdout(src, "sortgroup"), "Some([1, 3, 5])\nSome([2, 4])\n");
        }
    }

    /// List.zip_with and Str.trim_start/trim_end (it91) compile to native.
    #[test]
    fn native_zip_and_trim() {
        let src = "fun main() uses io {\n    \
                   print([1, 2, 3].zip_with([10, 20, 30], fn a, b { a + b }))\n    \
                   print(\"[\" + \"  hi  \".trim_start() + \"]\")\n}\n";
        if cc_available() {
            assert_eq!(native_main_stdout(src, "listops"), "[11, 22, 33]\n[hi  ]\n");
        }
    }

    /// Map.filter and Map.fold (it89) compile to native (callbacks via k_call),
    /// matching the interpreter.
    #[test]
    fn native_map_filter_fold() {
        let src = "fun main() uses io {\n    \
                   let m = Map().insert(\"a\", 1).insert(\"b\", 2).insert(\"c\", 3)\n    \
                   print(m.filter(fn k, v { v >= 2 }).values())\n    \
                   print(m.fold(0, fn acc, k, v { acc + v }))\n}\n";
        if cc_available() {
            assert_eq!(native_main_stdout(src, "maps"), "[2, 3]\n6\n");
        }
    }

    /// Set.symmetric_difference and List.min_by/max_by (it84) compile to native
    /// (min_by/max_by via k_call + k_cmp), matching the interpreter.
    #[test]
    fn native_set_and_minby() {
        let src = "fun main() uses io {\n    \
                   print(Set([1, 2, 3]).symmetric_difference(Set([2, 3, 4])).to_list())\n    \
                   print([\"a\", \"ccc\", \"bb\"].max_by(fn s { s.len() }))\n}\n";
        if cc_available() {
            assert_eq!(native_main_stdout(src, "sets"), "[1, 4]\nSome(\"ccc\")\n");
        }
    }

    /// Operator overloading (it71): `+` and `<` on a user type resolve to
    /// top-level `add`/`lt` functions and compile to native, matching interp.
    #[test]
    fn native_operator_overload() {
        let src = "type V = { x: Int }\n\
                   fun add(a: V, b: V) -> V { V(x: a.x + b.x) }\n\
                   fun lt(a: V, b: V) -> Bool { a.x < b.x }\n\
                   fun main() uses io {\n    print((V(x: 2) + V(x: 3)).x)\n    print(V(x: 1) < V(x: 9))\n}\n";
        if cc_available() {
            assert_eq!(native_main_stdout(src, "operators"), "5\ntrue\n");
        }
    }

    /// Rational (it70) compiles to native, reusing the C bignum: exact fraction
    /// arithmetic is byte-identical to the interpreter.
    #[test]
    fn native_rational() {
        let src = "fun main() uses io {\n    print(rat(1, 3) + rat(1, 6))\n    print(rat(2, 4))\n    \
                   print(rat(1, 3) / rat(1, 2))\n    print(rat(3, 7).recip())\n}\n";
        if cc_available() {
            assert_eq!(native_main_stdout(src, "rational"), "1/2\n1/2\n2/3\n7/3\n");
        }
    }

    /// http_serve (it68) compiles to native and serves real requests: a native
    /// server binary answers GET /world with "GET /world".
    #[test]
    fn native_http_serve() {
        if !cc_available() {
            return;
        }
        use std::io::{Read, Write};
        let src = "fun h(m: Str, p: Str) -> Str { \"{m} {p}\" }\n\
                   fun main() uses io { let _ = http_serve(38121, h) }\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked).unwrap();
        let c = super::emit_c(&module).expect("http_serve compiles to C (no longer a defer)");
        let base = std::env::temp_dir().join(format!("kupl-cgen-srv-{}", std::process::id()));
        let (cp, bin) = (base.with_extension("c"), base.with_extension("out"));
        std::fs::write(&cp, &c).unwrap();
        assert!(std::process::Command::new(cc())
            .args(["-O2", "-o", bin.to_str().unwrap(), cp.to_str().unwrap()])
            .status().unwrap().success());
        let mut child = std::process::Command::new(&bin).spawn().expect("server runs");
        // Connect with a generous retry budget (~12s): under heavy parallel test
        // load (many concurrent `cc` invocations) the spawned server can be starved
        // of scheduling before it binds — a short window made this test flaky.
        let mut stream = None;
        for _ in 0..300 {
            std::thread::sleep(std::time::Duration::from_millis(40));
            if let Ok(s) = std::net::TcpStream::connect(("127.0.0.1", 38121u16)) {
                stream = Some(s);
                break;
            }
        }
        let result = (|| {
            let mut s = stream.ok_or("server should be listening")?;
            s.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
            s.write_all(b"GET /world HTTP/1.1\r\nHost: x\r\n\r\n").map_err(|e| e.to_string())?;
            let mut resp = String::new();
            let _ = s.read_to_string(&mut resp);
            if !resp.contains("HTTP/1.1 200 OK") || !resp.ends_with("GET /world") {
                return Err(format!("bad response: {resp}"));
            }
            Ok::<(), String>(())
        })();
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&cp);
        let _ = std::fs::remove_file(&bin);
        result.unwrap();
    }

    /// BigInt compiles to native (it65 C bignum): a big factorial, 2^128, and a
    /// large division are byte-identical to the interpreter.
    #[test]
    fn native_bigint() {
        let src = "fun fact(n: Int) -> BigInt {\n    var a = big(1)\n    var i = 1\n    \
                   while i <= n {\n        a = a * big(i)\n        i = i + 1\n    }\n    a\n}\n\
                   fun main() uses io {\n    print(fact(30))\n    print(big(2).pow(128))\n    \
                   print(big(\"1000000000000000000000\") / big(\"7\"))\n    print(big(\"1000000000000000000000\") % big(\"7\"))\n}\n";
        if cc_available() {
            assert_eq!(
                native_main_stdout(src, "bigint"),
                "265252859812191058636308480000000\n340282366920938463463374607431768211456\n142857142857142857142\n6\n"
            );
        }
    }

    /// The static-site-generator's markdown transformer (it63) — string-ops
    /// bold + link rendering — compiles to native byte-identically.
    #[test]
    fn native_ssg_markdown() {
        let src = "fun bold(s: Str) -> Str {\n    var acc = \"\"\n    var i = 0\n    \
                   for part in s.split(\"**\") {\n        \
                   if i % 2 == 1 { acc = acc + \"<b>\" + part + \"</b>\" } else { acc = acc + part }\n        \
                   i = i + 1\n    }\n    acc\n}\n\
                   fun main() uses io {\n    print(bold(\"a **b** c **d**\"))\n}\n";
        if cc_available() {
            assert_eq!(native_main_stdout(src, "ssg"), "a <b>b</b> c <b>d</b>\n");
        }
    }

    /// Default parameters + named arguments (it62) resolve to positional calls
    /// before codegen, so native == interp.
    #[test]
    fn native_defaults_named() {
        let src = "fun mk(a: Int, b: Int = 10, c: Int = 100) -> Int { a + b + c }\n\
                   fun main() uses io {\n    \
                   print(mk(1))\n    print(mk(1, 2))\n    print(mk(1, 2, 3))\n    \
                   print(mk(c: 3, a: 1))\n}\n";
        if cc_available() {
            // 1+10+100=111; 1+2+100=103; 1+2+3=6; a=1,b=10,c=3 -> 14
            assert_eq!(native_main_stdout(src, "defs"), "111\n103\n6\n14\n");
        }
    }

    /// Path helpers + list_dir (it61) compile to native byte-identically:
    /// pure `/`-path math, and a sorted directory round-trip in a temp dir.
    #[test]
    fn native_paths() {
        let src = "fun main() uses io {\n    \
                   print(\"{path_join(\"a/b\", \"c.txt\")} {path_base(\"a/b/c.txt\")} {path_dir(\"a/b/c.txt\")} {path_ext(\"a/b/c.txt\")}\")\n    \
                   let d = \"kupl_it61_native_tmp\"\n    let _ = remove_dir(d)\n    let _ = make_dir(d)\n    \
                   let _ = write_file(path_join(d, \"b.txt\"), \"b\")\n    let _ = write_file(path_join(d, \"a.txt\"), \"a\")\n    \
                   match list_dir(d) {\n        Ok(n) => print(\"{n}\")\n        Err(_) => print(\"err\")\n    }\n    \
                   let _ = remove_dir(d)\n}\n";
        if cc_available() {
            assert_eq!(
                native_main_stdout(src, "paths"),
                "a/b/c.txt c.txt a/b .txt\n[\"a.txt\", \"b.txt\"]\n"
            );
        }
    }

    /// exec (it60): argv-based subprocess. `echo` captures stdout (single arg
    /// with a space stays one arg — no shell splitting); a missing program is
    /// an Err. Native == interp.
    #[test]
    fn native_exec() {
        let src = "fun main() uses io {\n    \
                   match exec(\"echo\", [\"a b\"]) {\n        Ok(t) => print(\"[{t}]\")\n        Err(_) => print(\"err\")\n    }\n    \
                   match exec(\"no_such_prog_xyz\", []) {\n        Ok(_) => print(\"ok\")\n        Err(_) => print(\"missing\")\n    }\n}\n";
        if cc_available() {
            assert_eq!(native_main_stdout(src, "exec"), "[a b\n]\nmissing\n");
        }
    }

    /// Stdin builtins (it59): read_line strips the newline and returns None at
    /// EOF; read_all reads everything. Native == the deterministic expectations
    /// for both piped input and empty stdin.
    #[test]
    fn native_stdin() {
        if !cc_available() {
            return;
        }
        let src = "fun main() uses io {\n    var n = 0\n    var c = 0\n    \
                   while let Some(l) = read_line() {\n        n = n + 1\n        c = c + l.len()\n    }\n    \
                   print(\"lines={n} chars={c}\")\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked).unwrap();
        let c = super::emit_c(&module).expect("emit_c");
        let base = std::env::temp_dir().join(format!("kupl-cgen-stdin-{}", std::process::id()));
        let (cp, bin) = (base.with_extension("c"), base.with_extension("out"));
        std::fs::write(&cp, &c).unwrap();
        assert!(std::process::Command::new(cc())
            .args(["-O2", "-o", bin.to_str().unwrap(), cp.to_str().unwrap()])
            .status().unwrap().success());
        let run_with = |input: &str| -> String {
            use std::io::Write;
            let mut child = std::process::Command::new(&bin)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn().unwrap();
            child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
            let out = child.wait_with_output().unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        assert_eq!(run_with("ab cd\nX\n"), "lines=2 chars=6\n"); // "ab cd"=5 + "X"=1
        assert_eq!(run_with(""), "lines=0 chars=0\n"); // EOF-safe
        let _ = std::fs::remove_file(&cp);
        let _ = std::fs::remove_file(&bin);
    }

    /// A virtual-clock timer (`on every`) compiles and fires up to the 100-fire
    /// bound, matching the interpreter's deterministic output.
    #[test]
    fn native_timers_run() {
        if !cc_available() {
            return;
        }
        let src = "app A {\n    intent \"x\"\n    state n: Int = 0\n    \
                   on every 5s {\n        n = n + 1\n        print(\"tick {n}\")\n    }\n}\n";
        let out = native_stdout(src, "tmr");
        // bounded to 100 fires — tick 1 .. tick 100
        assert!(out.starts_with("tick 1\ntick 2\n"), "head: {out:?}");
        assert!(out.ends_with("tick 100\n"), "tail: {out:?}");
        assert_eq!(out.lines().count(), 100);
    }

    /// A supervised child that panics restarts (state reset), printing the
    /// `[supervise]` line to stderr — semantics match the interpreter.
    #[test]
    fn native_supervision_restart() {
        if !cc_available() {
            return;
        }
        let src = "component W {\n    intent \"x\"\n    in tick: Int\n    state seen: Int = 0\n    \
                   on tick(n) {\n        seen = seen + 1\n        if seen == 2 {\n            print(\"ok n={n}\")\n        } else {\n            panic(\"boom\")\n        }\n    }\n}\n\
                   component D {\n    intent \"x\"\n    out pulse: Int\n    \
                   on start {\n        emit pulse(1)\n        emit pulse(2)\n    }\n}\n\
                   app A {\n    intent \"x\"\n    let w = W()\n    let d = D()\n    \
                   supervise w restart on_failure\n    wire d.pulse -> w.tick\n}\n";
        // both ticks panic (restart resets seen to 0 each time) — no "ok" line,
        // and the app survives (doesn't exit 101). stdout is empty; the two
        // [supervise] lines go to stderr. Just assert clean stdout + termination.
        let out = native_stdout(src, "sup");
        assert_eq!(out, "", "supervised panics keep stdout clean: {out:?}");
    }

    /// Compile a `fun main` program to native, run it, return stdout.
    #[cfg(test)]
    fn native_main_stdout(src: &str, tag: &str) -> String {
        let compiled = crate::run::compile(src).expect("program compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked)
            .expect("module compiles");
        let c = super::emit_c(&module).expect("emit_c succeeds");
        let base = std::env::temp_dir().join(format!("kupl-cgen-{tag}-{}", std::process::id()));
        let cpath = base.with_extension("c");
        let bin = base.with_extension("out");
        std::fs::write(&cpath, &c).unwrap();
        let status = std::process::Command::new(cc())
            .args(["-O2", "-o", bin.to_str().unwrap(), cpath.to_str().unwrap()])
            .status()
            .expect("cc runs");
        assert!(status.success(), "generated C must compile");
        let out = std::process::Command::new(&bin).output().expect("binary runs");
        let _ = std::fs::remove_file(&cpath);
        let _ = std::fs::remove_file(&bin);
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Sized integers compile to native and match the interpreter — arithmetic,
    /// conversions, wrapping/saturating/bitwise/shift methods, and u64 values
    /// above i64::MAX (computed via __int128).
    #[test]
    fn native_sized_integers() {
        if !cc_available() {
            return;
        }
        let src = "fun main() uses io {\n    \
                   print(200u8 + 55u8)\n    print(0xFFu8)\n    print(1000i16 * 3i16)\n    \
                   print((255u8).to_int() + 1)\n    print(42.to_u8())\n    \
                   print((200u8).wrapping_add(100u8))\n    print((200u8).saturating_add(100u8))\n    \
                   print((0xF0u8).band(0x0Fu8))\n    print((1u8).shl(4))\n    print((0i8 - 2i8).shr(1))\n    \
                   print(18000000000000000000u64 + 1u64)\n}\n";
        assert_eq!(
            native_main_stdout(src, "sz"),
            "255\n255\n3000\n256\n42\n44\n255\n0\n16\n-1\n18000000000000000001\n"
        );
        // overflow panics with the interpreter's exact message (to stderr)
        let compiled = crate::run::compile("fun main() uses io { print(200u8 + 100u8) }").unwrap();
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked).unwrap();
        let c = super::emit_c(&module).unwrap();
        let base = std::env::temp_dir().join(format!("kupl-cgen-of-{}", std::process::id()));
        let (cp, bin) = (base.with_extension("c"), base.with_extension("out"));
        std::fs::write(&cp, &c).unwrap();
        assert!(std::process::Command::new(cc())
            .args(["-O2", "-o", bin.to_str().unwrap(), cp.to_str().unwrap()])
            .status().unwrap().success());
        let out = std::process::Command::new(&bin).output().unwrap();
        assert!(String::from_utf8_lossy(&out.stderr).contains("integer overflow in addition"));
        let _ = std::fs::remove_file(&cp);
        let _ = std::fs::remove_file(&bin);
    }

    /// f32 compiles to native and matches the interpreter — arithmetic,
    /// non-integer shortest-round-trip display, whole-value ".0", conversions.
    #[test]
    fn native_f32() {
        if !cc_available() {
            return;
        }
        // each `expected` was verified against `kupl run` on the same source
        for (src, expected) in [
            ("fun main() uses io { print(22.0f32 / 7.0f32) }", "3.142857\n"),
            ("fun main() uses io { print(10.0f32) }", "10.0\n"),
            ("fun main() uses io { print(1.5f32 + 0.25f32) }", "1.75\n"),
            ("fun main() uses io { print((1.0).to_f32() + 0.5f32) }", "1.5\n"),
            ("fun main() uses io { print((3.5f32).to_float() * 2.0) }", "7.0\n"),
        ] {
            assert_eq!(native_main_stdout(src, "f32"), expected, "src: {src}");
        }
    }

    /// JSON compiles to native: stringify (canonical form, whole numbers without
    /// `.0`, string escapes) and parse (round-trip, error handling) == interp.
    #[test]
    fn native_json() {
        if !cc_available() {
            return;
        }
        // stringify: key order preserved, whole floats as ints, escapes
        assert_eq!(
            native_main_stdout(
                "fun main() uses io { print(json_stringify(JObj(Map().insert(\"b\", JNum(1.0)).insert(\"a\", JArr([JBool(true), JNull, JNum(2.5)]))))) }",
                "js"
            ),
            "{\"b\":1,\"a\":[true,null,2.5]}\n"
        );
        // round-trip parse -> stringify (match arms need newlines)
        assert_eq!(
            native_main_stdout(
                "fun main() uses io {\n    match json_parse(\"[1, 2.0, 2.5, \\\"x\\\"]\") {\n        Ok(j) => print(json_stringify(j))\n        Err(e) => print(e)\n    }\n}\n",
                "jp"
            ),
            "[1,2,2.5,\"x\"]\n"
        );
        // malformed input is an Err, never a crash
        assert_eq!(
            native_main_stdout(
                "fun main() uses io {\n    match json_parse(\"[1, 2\") {\n        Ok(_) => print(\"bad\")\n        Err(_) => print(\"handled\")\n    }\n}\n",
                "je"
            ),
            "handled\n"
        );
    }

    /// CSV + URL/query builtins compile to native, byte-identical to the
    /// interpreter (quoting, percent-encoding, round-trips).
    #[test]
    fn native_csv_url() {
        if !cc_available() {
            return;
        }
        // url_encode + query_build
        assert_eq!(
            native_main_stdout(
                "fun main() uses io { print(query_build([[\"a\", \"b c\"], [\"d\", \"e&f\"]])) }",
                "qb"
            ),
            "a=b%20c&d=e%26f\n"
        );
        // csv_stringify quotes fields with commas/quotes
        assert_eq!(
            native_main_stdout(
                "fun main() uses io { print(csv_stringify([[\"a\", \"b,c\"], [\"d\\\"e\", \"f\"]])) }",
                "cs"
            ),
            "a,\"b,c\"\n\"d\"\"e\",f\n"
        );
        // csv_parse handles quoted fields containing a comma
        assert_eq!(
            native_main_stdout(
                "fun main() uses io {\n    let rows = csv_parse(\"1,2\\n\\\"x,y\\\",z\")\n    print(\"{rows.len()}\")\n    for r in rows { print(r.join(\"|\")) }\n}\n",
                "cp"
            ),
            "2\n1|2\nx,y|z\n"
        );
    }

    /// The regex engine compiles to native, byte-identical to src/regex.rs —
    /// anchors, classes, quantifiers (greedy + zero-width), groups, alternation,
    /// find/find_all/replace. Each expected value verified against `kupl run`.
    #[test]
    fn native_regex() {
        if !cc_available() {
            return;
        }
        for (src, expected) in [
            ("fun main() uses io { print(re_match(\"^\\\\d+$\", \"12345\")) }", "true\n"),
            ("fun main() uses io { print(re_match(\"^\\\\d+$\", \"12a45\")) }", "false\n"),
            ("fun main() uses io { print(re_find(\"@[\\\\w.]+\", \"user@ex.com\")) }", "Some(\"@ex.com\")\n"),
            ("fun main() uses io { print(re_find_all(\"\\\\d+\", \"a1b22c333\").join(\",\")) }", "1,22,333\n"),
            ("fun main() uses io { print(re_replace(\"\\\\s+\", \"a  b   c\", \"_\")) }", "a_b_c\n"),
            // zero-width greedy: a* matches at each position -> ["","aaa",""]
            ("fun main() uses io { print(re_find_all(\"a*\", \"baaa\").join(\"|\")) }", "|aaa|\n"),
            ("fun main() uses io { print(re_match(\"(cat|dog)s?\", \"dogs\")) }", "true\n"),
            ("fun main() uses io { print(re_replace(\"[aeiou]\", \"hello world\", \"*\")) }", "h*ll* w*rld\n"),
        ] {
            assert_eq!(native_main_stdout(src, "re"), expected, "src: {src}");
        }
    }

    /// `if let` / `while let` (it58) desugar to match, so they compile to
    /// native byte-identically.
    #[test]
    fn native_if_while_let() {
        let src = "fun step(k: Int) -> Option[Int] { if k > 0 { Some(k) } else { None } }\n\
                   fun main() uses io {\n    \
                   if let Some(n) = Some(7) { print(n) }\n    \
                   if let Some(n) = step(0) { print(n) } else { print(-1) }\n    \
                   var i = 3\n    while let Some(v) = step(i) {\n        print(v)\n        i = i - 1\n    }\n}\n";
        if cc_available() {
            assert_eq!(native_main_stdout(src, "iflet"), "7\n-1\n3\n2\n1\n");
        }
    }

    /// UFCS (it57): `x.f(args)` resolves to a top-level `f(x, args)` when there
    /// is no built-in method, including chaining — byte-identical on native.
    #[test]
    fn native_ufcs() {
        let src = "type V = { n: Int }\n\
                   fun inc(v: V) -> V { V(n: v.n + 1) }\n\
                   fun dbl(v: V) -> V { V(n: v.n * 2) }\n\
                   fun get(v: V) -> Int { v.n }\n\
                   fun main() uses io { print(V(n: 3).inc().dbl().get()) }\n";
        if cc_available() {
            assert_eq!(native_main_stdout(src, "ufcs"), "8\n");
        }
    }

    /// Match `@` bindings and range patterns (it56) compile to native
    /// byte-identically: ranges lower to two compares, `@` to a Move + inner.
    #[test]
    fn native_match_at_range() {
        let src = "type S = C(r: Int)\n\
                   fun b(n: Int) -> Str {\n    match n {\n        1..10 => \"s\"\n        10..=99 => \"m\"\n        _ => \"l\"\n    }\n}\n\
                   fun d(x: S) -> Int {\n    match x {\n        w @ C(r) if r > 5 => r + 100\n        C(r) => r\n    }\n}\n\
                   fun main() uses io {\n    print(\"{b(5)} {b(10)} {b(99)} {b(100)}\")\n    print(\"{d(C(8))} {d(C(3))}\")\n}\n";
        if cc_available() {
            assert_eq!(native_main_stdout(src, "matchar"), "s m m l\n108 3\n");
        }
    }

    /// Match guards and or-patterns (it55) compile to native byte-identically:
    /// or-patterns fan out to one body, guards fall through on false.
    #[test]
    fn native_match_guards_or() {
        let src = "type D = A | B | C\n\
                   fun k(d: D) -> Int {\n    match d {\n        A | B => 1\n        C => 2\n    }\n}\n\
                   fun g(n: Int) -> Str {\n    match n {\n        x if x < 0 => \"neg\"\n        0 => \"zero\"\n        _ => \"pos\"\n    }\n}\n\
                   fun main() uses io {\n    print(\"{k(A)} {k(C)}\")\n    print(\"{g(-2)} {g(0)} {g(5)}\")\n}\n";
        if cc_available() {
            assert_eq!(native_main_stdout(src, "matchg"), "1 2\nneg zero pos\n");
        }
    }

    /// The it54 stdlib methods (sort_by / position / partition / rfind /
    /// replace_first / split_once) compile to native byte-identically.
    #[test]
    fn native_stdlib_it54() {
        let src = "fun main() uses io {\n    \
                   let ns = [5, 3, 8, 1, 9, 2]\n    \
                   print(\"{ns.sort_by(fn n { 0 - n })}\")\n    \
                   print(\"{ns.position(fn n { n > 7 })} {ns.partition(fn n { n % 2 == 0 })}\")\n    \
                   let p = \"a.b.c\"\n    \
                   print(\"{p.rfind(\".\")} {p.replace_first(\".\", \"/\")} {p.split_once(\".\")}\")\n}\n";
        if cc_available() {
            assert_eq!(
                native_main_stdout(src, "std54"),
                "[9, 8, 5, 3, 2, 1]\nSome(2) [[8, 2], [5, 3, 1, 9]]\nSome(3) a/b.c Some([\"a\", \"b.c\"])\n"
            );
        }
    }

    /// The deterministic date/time surface (epoch-based, pure integer civil
    /// math) compiles to native byte-identically: compose, format, extract,
    /// and round-trip through parse_iso.
    #[test]
    fn native_datetime() {
        let src = "fun main() uses io {\n    \
                   let e = date_make(2001, 9, 9, 1, 46, 40)\n    \
                   print(date_iso(e))\n    \
                   print(\"{year_of(e)} {weekday_of(e)} {yearday_of(e)}\")\n    \
                   match parse_iso(date_iso(e)) {\n        Ok(t) => print(\"{t}\")\n        Err(m) => print(m)\n    }\n}\n";
        if cc_available() {
            assert_eq!(
                native_main_stdout(src, "datetime"),
                "2001-09-09T01:46:40Z\n2001 0 252\n1000000000\n"
            );
        }
    }

    /// HTTP compiles to native (was a defer). A live request is non-
    /// deterministic, so we test the DETERMINISTIC error path: an unresolvable
    /// host returns Err on both engines.
    #[test]
    fn native_http() {
        // emit_c succeeds for an http program (no longer a compile-time defer)
        let src = "fun main() uses io {\n    match http_get(\"http://nonexistent.invalid.localhost.example\") {\n        Ok(_) => print(\"ok\")\n        Err(_) => print(\"handled\")\n    }\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked).unwrap();
        assert!(super::emit_c(&module).is_ok(), "native should compile http now");
        // and the unresolvable-host error path prints the Err branch natively
        if cc_available() {
            assert_eq!(native_main_stdout(src, "http"), "handled\n");
        }
    }

    /// ai funs compile to native via the deterministic mock path: `-> Str`,
    /// structured records, `List`, and `Result[T, Str]` all match the
    /// interpreter's mock output. (Tool-using ai funs defer at runtime.)
    #[test]
    fn native_ai_mock() {
        if !cc_available() {
            return;
        }
        let src = "type Sent = { label: Str, score: Float }\n\
                   ai fun haiku(t: Str) -> Str { intent \"x\" }\n\
                   ai fun classify(r: Str) -> Result[Sent, Str] { intent \"x\" }\n\
                   ai fun keywords(t: Str) -> Result[List[Str], Str] { intent \"x\" }\n\
                   fun main() uses io {\n    print(haiku(\"s\"))\n    \
                   match classify(\"g\") {\n        Ok(s) => print(\"{s.label} {s.score}\")\n        Err(e) => print(e)\n    }\n    \
                   match keywords(\"a\") {\n        Ok(k) => print(\"{k}\")\n        Err(e) => print(e)\n    }\n}\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked).unwrap();
        let c = super::emit_c(&module).expect("ai funs compile to C via the mock path");
        let base = std::env::temp_dir().join(format!("kupl-cgen-ai-{}", std::process::id()));
        let (cp, bin) = (base.with_extension("c"), base.with_extension("out"));
        std::fs::write(&cp, &c).unwrap();
        assert!(std::process::Command::new(cc())
            .args(["-O2", "-o", bin.to_str().unwrap(), cp.to_str().unwrap()])
            .status().unwrap().success());
        let out = std::process::Command::new(&bin)
            .env("KUPL_AI_MOCK_HAIKU", "cherry blossoms")
            .env("KUPL_AI_MOCK_CLASSIFY", "{\"label\":\"positive\",\"score\":0.9}")
            .env("KUPL_AI_MOCK_KEYWORDS", "{\"value\":[\"alpha\",\"beta\"]}")
            .output()
            .expect("runs");
        // exact bytes verified against `kupl run` under the same mock env
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "cherry blossoms\npositive 0.9\n[\"alpha\", \"beta\"]\n"
        );
        let _ = std::fs::remove_file(&cp);
        let _ = std::fs::remove_file(&bin);
    }

    /// A tool-using ai fun compiles to native and runs the mock tool loop: each
    /// `{"tool":…}` round invokes the compiled KUPL function; the `{"final":…}`
    /// round's text is converted via the return type — byte-identical to interp.
    #[test]
    fn native_ai_tools() {
        if !cc_available() {
            return;
        }
        let src = "fun add(a: Int, b: Int) -> Int { a + b }\n\
                   ai fun assist(q: Str) -> Str tools [add] { intent \"x\" }\n\
                   fun main() uses io { print(assist(\"2+3?\")) }\n";
        let compiled = crate::run::compile(src).expect("compiles");
        let module = crate::compile::compile_module(&compiled.program, &compiled.checked).unwrap();
        let c = super::emit_c(&module).expect("tool-using ai fun compiles to C");
        let base = std::env::temp_dir().join(format!("kupl-cgen-ait-{}", std::process::id()));
        let (cp, bin) = (base.with_extension("c"), base.with_extension("out"));
        std::fs::write(&cp, &c).unwrap();
        assert!(std::process::Command::new(cc())
            .args(["-O2", "-o", bin.to_str().unwrap(), cp.to_str().unwrap()])
            .status().unwrap().success());
        let out = std::process::Command::new(&bin)
            .env("KUPL_AI_MOCK_ASSIST", "[{\"tool\":\"add\",\"input\":{\"a\":2,\"b\":3}},{\"final\":\"5\"}]")
            .output()
            .expect("runs");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "5\n");
        let _ = std::fs::remove_file(&cp);
        let _ = std::fs::remove_file(&bin);
    }

    /// Direct cross-component expose calls compile to native and dispatch to the
    /// right instance's state — native stdout == the interpreter.
    #[test]
    fn native_expose_calls() {
        if !cc_available() {
            return;
        }
        let src = "component Store {\n    intent \"x\"\n    state v: Int = 0\n    \
                   expose fun get() -> Int { v }\n    expose fun put(n: Int) { v = n }\n}\n\
                   app A {\n    intent \"x\"\n    let s = Store()\n    \
                   on start {\n        s.put(41)\n        print(\"got {s.get() + 1}\")\n    }\n}\n";
        assert_eq!(native_stdout(src, "exp"), "got 42\n");

        // two independent instances keep separate state through their exposes
        let two = "component Cell {\n    intent \"x\"\n    state v: Int = 0\n    \
                   expose fun set(n: Int) { v = n }\n    expose fun get() -> Int { v }\n}\n\
                   app A {\n    intent \"x\"\n    let a = Cell()\n    let b = Cell()\n    \
                   on start {\n        a.set(10)\n        b.set(20)\n        print(\"{a.get()} {b.get()}\")\n    }\n}\n";
        assert_eq!(native_stdout(two, "exp2"), "10 20\n");
    }
}
