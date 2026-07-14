//! AI-native runtime: `ai fun` typed prompt functions.
//!
//! An `ai fun` declares a typed signature and an `intent`; calling it sends a
//! prompt to an LLM provider and converts the response to the declared return
//! type. The return type drives structured output: `-> Str` returns raw text,
//! any other supported type is requested as JSON and parsed into a value.
//! Declaring `-> Result[T, Str]` captures provider/parse failures as `Err`;
//! any other return type panics on failure (supervision applies as usual).
//!
//! Providers (selected by `KUPL_AI_PROVIDER`, default `anthropic`):
//! - `anthropic` — Messages API (`ANTHROPIC_API_KEY`), structured output via
//!   `output_config.format` json_schema. Default model `claude-opus-4-8`.
//! - `openai` — any OpenAI-compatible `/v1/chat/completions` endpoint
//!   (`OPENAI_API_KEY`, `KUPL_AI_BASE_URL`); `KUPL_AI_MODEL` required.
//! - `ollama` — OpenAI-compatible endpoint at `http://localhost:11434`
//!   (no key); `KUPL_AI_MODEL` required.
//! - `mock` — deterministic, no network: the response text comes from
//!   `KUPL_AI_MOCK_<FUN_NAME>` (upper-cased) or `KUPL_AI_MOCK`. If either
//!   variable is set, the mock provider is used regardless of
//!   `KUPL_AI_PROVIDER` — this is what makes `ai fun`s testable.
//!
//! Transport is the system `curl` (KUPL has zero Rust dependencies).
//! Both engines (interpreter and KVM) call `ai_call` — one implementation,
//! byte-identical behavior. `kupl native` also compiles `ai fun`s: the
//! deterministic mock path is fully native, while a real provider call (or
//! a tool-using `ai fun`) defers to `kupl bundle` at runtime (see
//! `cgen.rs`'s `k_ai_call`).

use std::collections::HashMap;

use crate::diag::json_escape;
use crate::lsp::{parse_json, Json};
use crate::types::Ty;
use crate::value::Value;

/// The shape of an `ai fun` return type — the bridge between KUPL types and
/// JSON structured output. Built at check time, carried in the module.
#[derive(Debug, Clone, PartialEq)]
pub enum AiShape {
    Str,
    Int,
    Float,
    Bool,
    List(Box<AiShape>),
    Option(Box<AiShape>),
    /// A record / ADT variant: constructed as `Ctor { ty, variant, fields }`.
    Record {
        ty: String,
        variant: String,
        fields: Vec<(String, AiShape)>,
    },
}

/// A KUPL function exposed to the model as a callable tool.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolMeta {
    pub name: String,
    pub description: String,
    pub params: Vec<(String, AiShape)>,
    pub ret: AiShape,
}

/// Everything the runtime needs to execute one `ai fun`.
#[derive(Debug, Clone, PartialEq)]
pub struct AiFunMeta {
    pub name: String,
    pub intent: String,
    /// Per-function model override (`model "..."` in the body).
    pub model: Option<String>,
    pub params: Vec<String>,
    pub shape: AiShape,
    /// True when declared `-> Result[T, Str]`: failures become `Err(msg)`.
    pub wraps_result: bool,
    /// Functions the model may call while producing the answer (`tools [...]`).
    pub tools: Vec<ToolMeta>,
}

/// The engine calls its own KUPL functions on behalf of the model. The
/// interpreter and the KVM each implement this so an `ai fun` with `tools`
/// runs identically on both.
pub trait ToolHost {
    fn call_tool(&mut self, name: &str, args: Vec<Value>) -> Result<Value, String>;
}

/// A host that refuses every tool call — used where no tools are configured
/// (and by unit tests).
pub struct NullToolHost;

impl ToolHost for NullToolHost {
    fn call_tool(&mut self, name: &str, _args: Vec<Value>) -> Result<Value, String> {
        Err(format!("model requested tool `{name}` but no tools are available"))
    }
}

/// Build an [`AiShape`] from a resolved return type. `records` maps a type
/// name to its single variant (name, fields, and the type's own `qvars` --
/// the fresh inference-var ids bound to its `type_params` at declaration
/// time) — multi-variant types are not representable as structured output in
/// v1. A generic record's field types are stored UNINSTANTIATED (still
/// referencing the type's own `qvars`), so a `Ty::Named(name, args)` carrying
/// CONCRETE type arguments (e.g. `Box[Int]`) must substitute `qvars -> args`
/// into each field type before recursing -- a REAL bug found+fixed
/// (production-hardening PR-it702): without this substitution, a generic
/// record's fields recursed as raw, unbound `Ty::Var` ids, which the
/// catch-all error arm below then formatted straight into the diagnostic as
/// a meaningless `?0`, rejecting every generic record as ai structured
/// output with no clear explanation.
pub fn build_shape(
    ty: &Ty,
    records: &HashMap<String, (String, Vec<(String, Ty)>, Vec<u32>)>,
    visiting: &mut Vec<String>,
) -> Result<AiShape, String> {
    match ty {
        Ty::Str => Ok(AiShape::Str),
        Ty::Int => Ok(AiShape::Int),
        Ty::Float => Ok(AiShape::Float),
        Ty::Bool => Ok(AiShape::Bool),
        Ty::List(t) => Ok(AiShape::List(Box::new(build_shape(t, records, visiting)?))),
        Ty::Option(t) => Ok(AiShape::Option(Box::new(build_shape(t, records, visiting)?))),
        Ty::Named(name, args) => {
            if visiting.iter().any(|v| v == name) {
                return Err(format!("recursive type `{name}` is not supported"));
            }
            let Some((variant, fields, qvars)) = records.get(name) else {
                return Err(format!(
                    "type `{name}` has multiple variants — structured output needs a record"
                ));
            };
            if qvars.len() != args.len() {
                // Structurally shouldn't happen (the checker always resolves a
                // `Named` type with exactly as many args as the type declares
                // params for) -- guarded rather than indexed into blindly, since
                // this function parses caller-supplied `Ty`s, not just checker-
                // internal ones.
                return Err(format!(
                    "type `{name}` expects {} type argument(s), got {}",
                    qvars.len(),
                    args.len()
                ));
            }
            let subst: HashMap<u32, Ty> = qvars.iter().copied().zip(args.iter().cloned()).collect();
            visiting.push(name.clone());
            let mut fs = Vec::with_capacity(fields.len());
            for (fname, fty) in fields {
                fs.push((fname.clone(), build_shape(&fty.subst(&subst), records, visiting)?));
            }
            visiting.pop();
            Ok(AiShape::Record { ty: name.clone(), variant: variant.clone(), fields: fs })
        }
        other => Err(format!("`{other}` is not supported as ai structured output")),
    }
}

/// JSON Schema text for a shape (draft-07 subset all providers accept).
fn schema_json(shape: &AiShape) -> String {
    match shape {
        AiShape::Str => "{\"type\":\"string\"}".into(),
        AiShape::Int => "{\"type\":\"integer\"}".into(),
        AiShape::Float => "{\"type\":\"number\"}".into(),
        AiShape::Bool => "{\"type\":\"boolean\"}".into(),
        AiShape::List(inner) => format!("{{\"type\":\"array\",\"items\":{}}}", schema_json(inner)),
        AiShape::Option(inner) => {
            format!("{{\"anyOf\":[{},{{\"type\":\"null\"}}]}}", schema_json(inner))
        }
        AiShape::Record { fields, .. } => {
            let props: Vec<String> = fields
                .iter()
                .map(|(n, s)| format!("\"{}\":{}", json_escape(n), schema_json(s)))
                .collect();
            let req: Vec<String> =
                fields.iter().map(|(n, _)| format!("\"{}\"", json_escape(n))).collect();
            format!(
                "{{\"type\":\"object\",\"properties\":{{{}}},\"required\":[{}],\"additionalProperties\":false}}",
                props.join(","),
                req.join(",")
            )
        }
    }
}

/// The wire schema: the value is wrapped in `{"value": ...}` so every target
/// shape (including scalars) is a JSON object at the top level.
fn wire_schema(shape: &AiShape) -> String {
    format!(
        "{{\"type\":\"object\",\"properties\":{{\"value\":{}}},\"required\":[\"value\"],\"additionalProperties\":false}}",
        schema_json(shape)
    )
}

/// Object schema for a named-field list (tool parameters).
fn params_schema(params: &[(String, AiShape)]) -> String {
    let props: Vec<String> = params
        .iter()
        .map(|(n, s)| format!("\"{}\":{}", json_escape(n), schema_json(s)))
        .collect();
    let req: Vec<String> =
        params.iter().map(|(n, _)| format!("\"{}\"", json_escape(n))).collect();
    format!(
        "{{\"type\":\"object\",\"properties\":{{{}}},\"required\":[{}],\"additionalProperties\":false}}",
        props.join(","),
        req.join(",")
    )
}

/// Serialize a KUPL value to JSON, guided by its shape (records need the field
/// names, which the value itself does not carry positionally).
fn value_to_json(shape: &AiShape, v: &Value) -> String {
    match (shape, v) {
        (AiShape::Str, Value::Str(s)) => format!("\"{}\"", json_escape(s)),
        (AiShape::Int, Value::Int(n)) => n.to_string(),
        (AiShape::Float, Value::Float(f)) => {
            if f.is_finite() {
                if f.fract() == 0.0 {
                    format!("{f:.1}")
                } else {
                    format!("{f}")
                }
            } else {
                "null".into()
            }
        }
        (AiShape::Bool, Value::Bool(b)) => b.to_string(),
        (AiShape::List(inner), Value::List(items)) => {
            let parts: Vec<String> = items.iter().map(|x| value_to_json(inner, x)).collect();
            format!("[{}]", parts.join(","))
        }
        (AiShape::Option(inner), Value::Ctor { variant, fields, .. }) => {
            if variant.as_str() == "Some" && !fields.is_empty() {
                value_to_json(inner, &fields[0])
            } else {
                "null".into()
            }
        }
        (AiShape::Record { fields, .. }, Value::Ctor { fields: vals, .. }) => {
            let parts: Vec<String> = fields
                .iter()
                .zip(vals.iter())
                .map(|((name, s), val)| format!("\"{}\":{}", json_escape(name), value_to_json(s, val)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        // shape/value mismatch: fall back to the Display form as a JSON string
        (_, other) => format!("\"{}\"", json_escape(&other.to_string())),
    }
}

/// Serialize a parsed [`Json`] value back to a compact JSON string (used to
/// echo assistant tool_use content and tool inputs into the message history).
fn dump_json(j: &Json) -> String {
    match j {
        Json::Null => "null".into(),
        Json::Bool(b) => b.to_string(),
        Json::Num(n) => {
            if n.fract() == 0.0 && n.is_finite() {
                format!("{}", *n as i64)
            } else {
                format!("{n}")
            }
        }
        Json::Str(s) => format!("\"{}\"", json_escape(s)),
        Json::Arr(items) => {
            let parts: Vec<String> = items.iter().map(dump_json).collect();
            format!("[{}]", parts.join(","))
        }
        Json::Obj(pairs) => {
            let parts: Vec<String> = pairs
                .iter()
                .map(|(k, v)| format!("\"{}\":{}", json_escape(k), dump_json(v)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

/// Convert parsed JSON to a KUPL value guided by the shape.
pub fn value_from_json(shape: &AiShape, json: &Json) -> Result<Value, String> {
    match (shape, json) {
        // A REAL bug found+fixed (production-hardening PR-it690): unlike
        // `json.rs`'s OWN parser (which rejects a ` ` escape at parse
        // time -- "not allowed in a KUPL Str, K0008") and ~15 other input
        // boundaries across this codebase, this module parses a model's JSON
        // response with `lsp::parse_json`, which decodes ` ` to a
        // literal NUL character with NO such guard -- so a model response
        // (or a misbehaving/malicious provider) could construct a
        // `Value::Str` violating KUPL's own "Str is NUL-free UTF-8 text"
        // invariant, structurally unreachable from any KUPL-source literal.
        // Confirmed live before this fix (calling `value_from_json`/
        // `convert` directly, bypassing env-var mocking -- `std::env::
        // set_var` itself rejects a NUL in the value, so this couldn't be
        // demonstrated via the existing `KUPL_AI_MOCK*` test harness) that a
        // NUL genuinely reaches a `Value::Str` untouched. Traced the
        // downstream consequence: `cgen.rs`'s SOLE native string
        // constructor, `k_str()`, derives its length via C's `strlen()`,
        // which stops at the first NUL byte -- so on native this wouldn't
        // just be an invariant violation, it would SILENTLY TRUNCATE the
        // string at the embedded NUL while interp/vm's Rust `String` (which
        // tracks length explicitly) keeps everything after it: a genuine
        // cross-engine BYTE-IDENTITY divergence, not just a broken
        // invariant. Rejected here, matching every other K0008 boundary's
        // wording, mirrored in `cgen.rs`'s `k_ai_from_json` (case 0).
        (AiShape::Str, Json::Str(s)) if s.contains('\0') => Err(
            "model response contains a NUL byte, not allowed in a KUPL Str (K0008)".to_string(),
        ),
        (AiShape::Str, Json::Str(s)) => Ok(Value::str(s.clone())),
        (AiShape::Int, Json::Num(n)) => {
            if n.fract() == 0.0 && n.is_finite() && *n >= i64::MIN as f64 && *n <= i64::MAX as f64 {
                Ok(Value::Int(*n as i64))
            } else {
                Err(format!("expected an integer, model returned {n}"))
            }
        }
        (AiShape::Float, Json::Num(n)) => Ok(Value::Float(*n)),
        (AiShape::Bool, Json::Bool(b)) => Ok(Value::Bool(*b)),
        (AiShape::List(inner), Json::Arr(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(value_from_json(inner, item)?);
            }
            Ok(Value::List(std::rc::Rc::new(out)))
        }
        (AiShape::Option(_), Json::Null) => Ok(Value::none()),
        (AiShape::Option(inner), other) => Ok(Value::some(value_from_json(inner, other)?)),
        (AiShape::Record { ty, variant, fields }, Json::Obj(_)) => {
            let mut vals = Vec::with_capacity(fields.len());
            for (fname, fshape) in fields {
                let Some(fjson) = json.get(fname) else {
                    return Err(format!("model response is missing field `{fname}`"));
                };
                vals.push(value_from_json(fshape, fjson)?);
            }
            Ok(Value::Ctor {
                ty: std::rc::Rc::new(ty.clone()),
                variant: std::rc::Rc::new(variant.clone()),
                fields: std::rc::Rc::new(vals),
            })
        }
        (want, got) => Err(format!("expected {}, model returned {}", shape_name(want), dump_json(got))),
    }
}

/// A KUPL-syntax rendering of an `AiShape`, for user-facing shape-mismatch
/// messages -- the fallback error used to dump the shape via Rust's `{:?}`
/// (e.g. `Record { ty: "Shape", variant: "Circle", fields: [...] }`, exposing
/// internal representation instead of the KUPL type the user actually wrote).
fn shape_name(shape: &AiShape) -> String {
    match shape {
        AiShape::Str => "Str".to_string(),
        AiShape::Int => "Int".to_string(),
        AiShape::Float => "Float".to_string(),
        AiShape::Bool => "Bool".to_string(),
        AiShape::List(inner) => format!("List[{}]", shape_name(inner)),
        AiShape::Option(inner) => format!("Option[{}]", shape_name(inner)),
        AiShape::Record { ty, .. } => ty.clone(),
    }
}

// ---------------- providers ----------------

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn mock_response(fun_name: &str) -> Option<String> {
    let key = format!("KUPL_AI_MOCK_{}", fun_name.to_uppercase());
    env(&key).or_else(|| env("KUPL_AI_MOCK"))
}

/// Run `curl` against `url` with headers and a JSON body; return the body.
fn http_post(url: &str, headers: &[String], body: &str) -> Result<String, String> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-sS", "--max-time", "120", "-X", "POST", url]);
    for h in headers {
        cmd.args(["-H", h]);
    }
    cmd.args(["-H", "content-type: application/json", "--data-binary", "@-"]);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("cannot run curl: {e}"))?;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(body.as_bytes()).map_err(|e| format!("curl stdin: {e}"))?;
    }
    let out = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "provider request failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|_| "provider returned invalid UTF-8".into())
}

/// Build the prompt: intent + rendered arguments + (for structured shapes)
/// the JSON instruction. Deterministic — the mock provider ignores it.
/// `intent` is the interpolated intent, already resolved in the call scope.
fn build_prompt(meta: &AiFunMeta, intent: &str, args: &[Value]) -> String {
    let mut prompt = intent.to_string();
    if !args.is_empty() {
        prompt.push_str("\n");
        for (name, value) in meta.params.iter().zip(args) {
            prompt.push_str(&format!("\n{name}: {value}"));
        }
    }
    if meta.shape != AiShape::Str {
        prompt.push_str(&format!(
            "\n\nRespond with only a JSON object matching this JSON Schema (no prose, no code fences):\n{}",
            wire_schema(&meta.shape)
        ));
    }
    prompt
}

fn anthropic_call(meta: &AiFunMeta, prompt: &str) -> Result<String, String> {
    let key = env("ANTHROPIC_API_KEY")
        .ok_or("ANTHROPIC_API_KEY is not set (or set KUPL_AI_MOCK for the mock provider)")?;
    let model = meta
        .model
        .clone()
        .or_else(|| env("KUPL_AI_MODEL"))
        .unwrap_or_else(|| "claude-opus-4-8".into());
    let mut body = format!(
        "{{\"model\":\"{}\",\"max_tokens\":4096,\"messages\":[{{\"role\":\"user\",\"content\":\"{}\"}}]",
        json_escape(&model),
        json_escape(prompt)
    );
    if meta.shape != AiShape::Str {
        body.push_str(&format!(
            ",\"output_config\":{{\"format\":{{\"type\":\"json_schema\",\"schema\":{}}}}}",
            wire_schema(&meta.shape)
        ));
    }
    body.push('}');
    let base = env("KUPL_AI_BASE_URL").unwrap_or_else(|| "https://api.anthropic.com".into());
    let resp = http_post(
        &format!("{base}/v1/messages"),
        &[format!("x-api-key: {key}"), "anthropic-version: 2023-06-01".into()],
        &body,
    )?;
    let json = parse_json(&resp).map_err(|e| format!("bad provider response: {e}"))?;
    if json.get("type").and_then(Json::str) == Some("error") {
        let msg = json
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Json::str)
            .unwrap_or("unknown provider error");
        return Err(format!("anthropic: {msg}"));
    }
    let Some(Json::Arr(blocks)) = json.get("content") else {
        return Err("anthropic: response has no content".into());
    };
    let text: String = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Json::str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Json::str))
        .collect();
    if text.is_empty() {
        return Err(format!(
            "anthropic: empty response (stop_reason: {})",
            json.get("stop_reason").and_then(Json::str).unwrap_or("unknown")
        ));
    }
    Ok(text)
}

fn openai_call(meta: &AiFunMeta, prompt: &str, default_base: &str, need_key: bool) -> Result<String, String> {
    let model = meta
        .model
        .clone()
        .or_else(|| env("KUPL_AI_MODEL"))
        .ok_or("KUPL_AI_MODEL is not set (required for openai/ollama providers)")?;
    let mut headers = Vec::new();
    match env("OPENAI_API_KEY") {
        Some(key) => headers.push(format!("Authorization: Bearer {key}")),
        None if need_key => return Err("OPENAI_API_KEY is not set".into()),
        None => {}
    }
    let mut body = format!(
        "{{\"model\":\"{}\",\"messages\":[{{\"role\":\"user\",\"content\":\"{}\"}}]",
        json_escape(&model),
        json_escape(prompt)
    );
    if meta.shape != AiShape::Str {
        body.push_str(",\"response_format\":{\"type\":\"json_object\"}");
    }
    body.push('}');
    let base = env("KUPL_AI_BASE_URL").unwrap_or_else(|| default_base.into());
    let resp = http_post(&format!("{base}/v1/chat/completions"), &headers, &body)?;
    let json = parse_json(&resp).map_err(|e| format!("bad provider response: {e}"))?;
    if let Some(err) = json.get("error") {
        let msg = err.get("message").and_then(Json::str).unwrap_or("unknown provider error");
        return Err(format!("provider: {msg}"));
    }
    json.get("choices")
        .and_then(|c| c.index(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Json::str)
        .map(str::to_string)
        .ok_or_else(|| "provider: response has no message content".into())
}

fn raw_response(meta: &AiFunMeta, intent: &str, args: &[Value]) -> Result<String, String> {
    if let Some(text) = mock_response(&meta.name) {
        return Ok(text);
    }
    let prompt = build_prompt(meta, intent, args);
    match env("KUPL_AI_PROVIDER").as_deref() {
        None | Some("anthropic") => anthropic_call(meta, &prompt),
        Some("openai") => openai_call(meta, &prompt, "https://api.openai.com", true),
        Some("ollama") => openai_call(meta, &prompt, "http://localhost:11434", false),
        // debug provider: returns the composed prompt verbatim (no network) so
        // you can see exactly what an ai fun would send, incl. resolved intent.
        Some("echo") => Ok(prompt),
        Some("mock") => Err(format!(
            "mock provider: set KUPL_AI_MOCK or KUPL_AI_MOCK_{} to the canned response",
            meta.name.to_uppercase()
        )),
        Some(other) => Err(format!(
            "unknown KUPL_AI_PROVIDER `{other}` (use anthropic, openai, ollama, or mock)"
        )),
    }
}

/// Strip optional markdown code fences around a JSON payload.
fn strip_fences(text: &str) -> &str {
    let t = text.trim();
    let Some(rest) = t.strip_prefix("```") else { return t };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    rest.trim_start_matches(['\r', '\n']).strip_suffix("```").unwrap_or(rest).trim()
}

fn convert(meta: &AiFunMeta, text: &str) -> Result<Value, String> {
    if meta.shape == AiShape::Str {
        // Same "Str is NUL-free" concern as `value_from_json`'s `AiShape::
        // Str` arm above (PR-it690), for the RAW-TEXT fast path (no JSON
        // parsing at all here, so no ` `-escape angle -- a literal NUL
        // byte anywhere in the model's raw response text reaches this point
        // completely unchecked).
        if text.contains('\0') {
            return Err(
                "model response contains a NUL byte, not allowed in a KUPL Str (K0008)".to_string(),
            );
        }
        return Ok(Value::str(text.trim().to_string()));
    }
    let payload = strip_fences(text);
    let json = parse_json(payload)
        .map_err(|e| format!("model response is not valid JSON ({e}): {payload}"))?;
    // Accept both the documented `{"value": ...}` wrapper and a bare payload
    // (some models unwrap single-field objects despite instructions).
    let inner = json.get("value").unwrap_or(&json);
    value_from_json(&meta.shape, inner)
        .or_else(|first| value_from_json(&meta.shape, &json).map_err(|_| first))
}

// ---------------- tool-calling loop (agentic ai funs) ----------------

/// Bound on model↔tool round-trips, so a misbehaving model or mock can't loop
/// forever. Deterministic per call.
const MAX_TOOL_ROUNDS: usize = 8;

/// One tool the model asked to run this round.
struct ToolReq {
    id: String,
    name: String,
    input: Json,
}

/// What a provider returns for one round.
enum Reply {
    Final(String),
    Tools(Vec<ToolReq>),
}

/// A provider that can carry a multi-turn tool conversation. Each impl owns
/// its own message history in its own wire format.
trait ToolProvider {
    fn round(&mut self) -> Result<Reply, String>;
    fn push_results(&mut self, results: Vec<(ToolReq, String)>);
}

/// Convert a model tool request into KUPL values, run it through the host,
/// and serialize the result back to JSON for the next round.
fn execute_tool(host: &mut dyn ToolHost, meta: &AiFunMeta, req: &ToolReq) -> Result<String, String> {
    let tool = meta
        .tools
        .iter()
        .find(|t| t.name == req.name)
        .ok_or_else(|| format!("model called unknown tool `{}`", req.name))?;
    let mut args = Vec::with_capacity(tool.params.len());
    for (pname, pshape) in &tool.params {
        let pj = req
            .input
            .get(pname)
            .ok_or_else(|| format!("tool `{}` is missing argument `{pname}`", req.name))?;
        args.push(value_from_json(pshape, pj)?);
    }
    let result = host.call_tool(&req.name, args)?;
    Ok(value_to_json(&tool.ret, &result))
}

/// The engine-agnostic loop: round → execute tools → feed results → repeat,
/// until the provider produces a final answer.
fn run_tool_loop(
    provider: &mut dyn ToolProvider,
    host: &mut dyn ToolHost,
    meta: &AiFunMeta,
) -> Result<String, String> {
    for _ in 0..MAX_TOOL_ROUNDS {
        match provider.round()? {
            Reply::Final(text) => return Ok(text),
            Reply::Tools(reqs) => {
                if reqs.is_empty() {
                    return Err("provider asked to use zero tools".into());
                }
                let mut results = Vec::with_capacity(reqs.len());
                for req in reqs {
                    let out = execute_tool(host, meta, &req)?;
                    results.push((req, out));
                }
                provider.push_results(results);
            }
        }
    }
    Err(format!("tool loop exceeded {MAX_TOOL_ROUNDS} rounds without a final answer"))
}

fn anthropic_tools_json(tools: &[ToolMeta]) -> String {
    let arr: Vec<String> = tools
        .iter()
        .map(|t| {
            format!(
                "{{\"name\":\"{}\",\"description\":\"{}\",\"input_schema\":{}}}",
                json_escape(&t.name),
                json_escape(&t.description),
                params_schema(&t.params)
            )
        })
        .collect();
    format!("[{}]", arr.join(","))
}

fn openai_tools_json(tools: &[ToolMeta]) -> String {
    let arr: Vec<String> = tools
        .iter()
        .map(|t| {
            format!(
                "{{\"type\":\"function\",\"function\":{{\"name\":\"{}\",\"description\":\"{}\",\"parameters\":{}}}}}",
                json_escape(&t.name),
                json_escape(&t.description),
                params_schema(&t.params)
            )
        })
        .collect();
    format!("[{}]", arr.join(","))
}

/// Deterministic tool provider: a scripted array of rounds, each one of
/// `{"tool": name, "input": {...}}` (a single tool call), `{"tools": [{"tool":
/// name, "input": {...}}, ...]}` (MULTIPLE tool calls in the same round --
/// mirrors what a real provider can do when a model requests several tools at
/// once, e.g. Anthropic's `content` array carrying more than one `tool_use`
/// block; PR-it524), or `{"final": <payload>}`.
struct MockProvider {
    rounds: Vec<Json>,
    idx: usize,
}

impl ToolProvider for MockProvider {
    fn round(&mut self) -> Result<Reply, String> {
        let r = self
            .rounds
            .get(self.idx)
            .cloned()
            .ok_or("mock provider ran out of scripted rounds")?;
        self.idx += 1;
        if let Some(final_) = r.get("final") {
            return Ok(Reply::Final(match final_ {
                Json::Str(s) => s.clone(),
                other => dump_json(other),
            }));
        }
        if let Some(Json::Str(name)) = r.get("tool") {
            let input = r.get("input").cloned().unwrap_or(Json::Obj(Vec::new()));
            return Ok(Reply::Tools(vec![ToolReq {
                id: format!("mock_tool_{}", self.idx),
                name: name.clone(),
                input,
            }]));
        }
        if let Some(Json::Arr(items)) = r.get("tools") {
            let mut reqs = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                let Some(Json::Str(name)) = item.get("tool") else {
                    return Err("mock round: each entry in `tools` must have a `tool` name".into());
                };
                let input = item.get("input").cloned().unwrap_or(Json::Obj(Vec::new()));
                reqs.push(ToolReq {
                    id: format!("mock_tool_{}_{i}", self.idx),
                    name: name.clone(),
                    input,
                });
            }
            return Ok(Reply::Tools(reqs));
        }
        Err("mock round must be `{\"tool\": ...}`, `{\"tools\": [...]}`, or `{\"final\": ...}`".into())
    }
    fn push_results(&mut self, _results: Vec<(ToolReq, String)>) {}
}

/// Anthropic Messages API tool loop.
struct AnthropicProvider {
    model: String,
    key: String,
    base: String,
    tools_json: String,
    messages: Vec<String>,
}

impl ToolProvider for AnthropicProvider {
    fn round(&mut self) -> Result<Reply, String> {
        let body = format!(
            "{{\"model\":\"{}\",\"max_tokens\":4096,\"tools\":{},\"messages\":[{}]}}",
            json_escape(&self.model),
            self.tools_json,
            self.messages.join(",")
        );
        let resp = http_post(
            &format!("{}/v1/messages", self.base),
            &[format!("x-api-key: {}", self.key), "anthropic-version: 2023-06-01".into()],
            &body,
        )?;
        let json = parse_json(&resp).map_err(|e| format!("bad provider response: {e}"))?;
        if json.get("type").and_then(Json::str) == Some("error") {
            let msg = json
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Json::str)
                .unwrap_or("unknown provider error");
            return Err(format!("anthropic: {msg}"));
        }
        let Some(Json::Arr(content)) = json.get("content") else {
            return Err("anthropic: response has no content".into());
        };
        let stop = json.get("stop_reason").and_then(Json::str).unwrap_or("");
        if stop == "tool_use" {
            // echo the assistant turn verbatim, then collect the tool requests
            self.messages.push(format!(
                "{{\"role\":\"assistant\",\"content\":{}}}",
                dump_json(&Json::Arr(content.clone()))
            ));
            let mut reqs = Vec::new();
            for block in content {
                if block.get("type").and_then(Json::str) == Some("tool_use") {
                    let id = block.get("id").and_then(Json::str).unwrap_or("").to_string();
                    let name = block.get("name").and_then(Json::str).unwrap_or("").to_string();
                    let input = block.get("input").cloned().unwrap_or(Json::Obj(Vec::new()));
                    reqs.push(ToolReq { id, name, input });
                }
            }
            return Ok(Reply::Tools(reqs));
        }
        let text: String = content
            .iter()
            .filter(|b| b.get("type").and_then(Json::str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Json::str))
            .collect();
        Ok(Reply::Final(text))
    }

    fn push_results(&mut self, results: Vec<(ToolReq, String)>) {
        let blocks: Vec<String> = results
            .iter()
            .map(|(req, out)| {
                format!(
                    "{{\"type\":\"tool_result\",\"tool_use_id\":\"{}\",\"content\":{}}}",
                    json_escape(&req.id),
                    // tool results are sent as a JSON string content block
                    format!("\"{}\"", json_escape(out))
                )
            })
            .collect();
        self.messages.push(format!("{{\"role\":\"user\",\"content\":[{}]}}", blocks.join(",")));
    }
}

/// OpenAI-compatible (`/v1/chat/completions`) tool loop.
struct OpenAiProvider {
    model: String,
    headers: Vec<String>,
    url: String,
    tools_json: String,
    messages: Vec<String>,
}

impl ToolProvider for OpenAiProvider {
    fn round(&mut self) -> Result<Reply, String> {
        let body = format!(
            "{{\"model\":\"{}\",\"tools\":{},\"messages\":[{}]}}",
            json_escape(&self.model),
            self.tools_json,
            self.messages.join(",")
        );
        let resp = http_post(&self.url, &self.headers, &body)?;
        let json = parse_json(&resp).map_err(|e| format!("bad provider response: {e}"))?;
        if let Some(err) = json.get("error") {
            let msg = err.get("message").and_then(Json::str).unwrap_or("unknown provider error");
            return Err(format!("provider: {msg}"));
        }
        let message = json
            .get("choices")
            .and_then(|c| c.index(0))
            .and_then(|c| c.get("message"))
            .ok_or("provider: response has no message")?;
        if let Some(Json::Arr(calls)) = message.get("tool_calls") {
            self.messages.push(dump_json(message));
            let mut reqs = Vec::new();
            for call in calls {
                let id = call.get("id").and_then(Json::str).unwrap_or("").to_string();
                let func = call.get("function");
                let name = func
                    .and_then(|f| f.get("name"))
                    .and_then(Json::str)
                    .unwrap_or("")
                    .to_string();
                let args_str =
                    func.and_then(|f| f.get("arguments")).and_then(Json::str).unwrap_or("{}");
                let input = parse_json(args_str).unwrap_or(Json::Obj(Vec::new()));
                reqs.push(ToolReq { id, name, input });
            }
            return Ok(Reply::Tools(reqs));
        }
        let text = message.get("content").and_then(Json::str).unwrap_or("").to_string();
        Ok(Reply::Final(text))
    }

    fn push_results(&mut self, results: Vec<(ToolReq, String)>) {
        for (req, out) in results {
            self.messages.push(format!(
                "{{\"role\":\"tool\",\"tool_call_id\":\"{}\",\"content\":\"{}\"}}",
                json_escape(&req.id),
                json_escape(&out)
            ));
        }
    }
}

fn user_message(prompt: &str) -> String {
    format!("{{\"role\":\"user\",\"content\":\"{}\"}}", json_escape(prompt))
}

fn tool_response(
    meta: &AiFunMeta,
    intent: &str,
    args: &[Value],
    host: &mut dyn ToolHost,
) -> Result<String, String> {
    let prompt = build_prompt(meta, intent, args);
    if let Some(script) = mock_response(&meta.name) {
        // Only interpret the mock as scripted rounds when it is STRICT, complete
        // JSON. `lsp::parse_json` is lenient about trailing garbage ("42 aardvark"
        // -> 42), but the native runtime uses the strict `json` parser; gate on it
        // so interp and native agree — malformed/garbage mock text becomes the raw
        // final answer on both.
        let strict = crate::json::parse(&script).is_ok();
        let rounds = match parse_json(&script) {
            Ok(Json::Arr(a)) if strict => a,
            Ok(other) if strict => vec![Json::Obj(vec![("final".into(), other)])],
            _ => vec![Json::Obj(vec![("final".into(), Json::Str(script.clone()))])],
        };
        let mut p = MockProvider { rounds, idx: 0 };
        return run_tool_loop(&mut p, host, meta);
    }
    match env("KUPL_AI_PROVIDER").as_deref() {
        None | Some("anthropic") => {
            let key = env("ANTHROPIC_API_KEY")
                .ok_or("ANTHROPIC_API_KEY is not set (or set KUPL_AI_MOCK for the mock provider)")?;
            let model = meta
                .model
                .clone()
                .or_else(|| env("KUPL_AI_MODEL"))
                .unwrap_or_else(|| "claude-opus-4-8".into());
            let base = env("KUPL_AI_BASE_URL").unwrap_or_else(|| "https://api.anthropic.com".into());
            let mut p = AnthropicProvider {
                model,
                key,
                base,
                tools_json: anthropic_tools_json(&meta.tools),
                messages: vec![user_message(&prompt)],
            };
            run_tool_loop(&mut p, host, meta)
        }
        Some(provider @ ("openai" | "ollama")) => {
            let model = meta
                .model
                .clone()
                .or_else(|| env("KUPL_AI_MODEL"))
                .ok_or("KUPL_AI_MODEL is not set (required for openai/ollama providers)")?;
            let mut headers = Vec::new();
            match env("OPENAI_API_KEY") {
                Some(key) => headers.push(format!("Authorization: Bearer {key}")),
                None if provider == "openai" => return Err("OPENAI_API_KEY is not set".into()),
                None => {}
            }
            let default_base = if provider == "openai" {
                "https://api.openai.com"
            } else {
                "http://localhost:11434"
            };
            let base = env("KUPL_AI_BASE_URL").unwrap_or_else(|| default_base.into());
            let mut p = OpenAiProvider {
                model,
                headers,
                url: format!("{base}/v1/chat/completions"),
                tools_json: openai_tools_json(&meta.tools),
                messages: vec![user_message(&prompt)],
            };
            run_tool_loop(&mut p, host, meta)
        }
        Some("mock") => Err(format!(
            "mock provider: set KUPL_AI_MOCK_{} to a scripted round array",
            meta.name.to_uppercase()
        )),
        Some(other) => Err(format!("unknown KUPL_AI_PROVIDER `{other}`")),
    }
}

/// Execute one `ai fun` call. `Err` means panic (unless the function wraps
/// its result — then failures come back as `Ok(Err(msg))`). `host` lets an
/// ai fun with `tools` call back into the engine's KUPL functions.
pub fn ai_call(
    meta: &AiFunMeta,
    intent: &str,
    args: &[Value],
    host: &mut dyn ToolHost,
) -> Result<Value, String> {
    let text = if meta.tools.is_empty() {
        raw_response(meta, intent, args)
    } else {
        tool_response(meta, intent, args, host)
    };
    let outcome = text.and_then(|t| convert(meta, &t));
    match outcome {
        Ok(v) if meta.wraps_result => Ok(Value::ok(v)),
        Ok(v) => Ok(v),
        Err(msg) if meta.wraps_result => Ok(Value::err(Value::str(msg))),
        // A REAL diagnostics-quality bug found+fixed (production-hardening
        // PR-it756): a self-referential/mutually-recursive `ai fun` <-> tool
        // call chain (an `ai fun` with `tools [t]` where `t` eventually
        // calls back into an `ai fun`, directly or through a longer cycle)
        // is safely bounded by the shared `MAX_CALL_DEPTH` recursion guard
        // (`interp.rs`/`vm.rs`, same mechanism ordinary KUPL recursion
        // uses) -- it does NOT hang or crash the process. But every one of
        // the thousands of nested `ai_call` frames on the way back up the
        // unwind re-wraps the SAME error with another `"ai \`name\`: "`
        // prefix (this exact line), so the final panic message balloons to
        // tens of KB before the actually-useful "stack overflow" text at
        // the very end -- a genuine diagnostics-quality defect, not a
        // correctness bug. Confirmed live BEFORE this fix: a mutually-
        // recursive `ai fun`/tool pair driven by a mock provider that
        // always calls the SAME tool back produced a panic message
        // measured in tens of KB. Since this format string is the ONLY
        // site in this file that ever produces a message starting with
        // `"ai \`"`, checking for that prefix unambiguously identifies "this
        // message was already wrapped by a deeper `ai_call` frame" -- skip
        // re-wrapping in that case, so the FINAL message attributes to the
        // ai fun where the failure actually originated (the deepest frame,
        // wrapped exactly once) instead of accumulating one prefix per
        // recursion level. An ordinary, non-recursive failure's message
        // never starts with this prefix, so it is still wrapped exactly
        // once, unchanged from before.
        Err(msg) if msg.starts_with("ai `") => Err(msg),
        Err(msg) => Err(format!("ai `{}`: {msg}", meta.name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(name: &str, shape: AiShape, wraps: bool) -> AiFunMeta {
        AiFunMeta {
            name: name.into(),
            intent: "test".into(),
            model: None,
            params: vec!["x".into()],
            shape,
            wraps_result: wraps,
            tools: Vec::new(),
        }
    }

    fn run(m: &AiFunMeta, args: &[Value]) -> Result<Value, String> {
        let intent = m.intent.clone();
        ai_call(m, &intent, args, &mut NullToolHost)
    }

    #[test]
    fn mock_str_roundtrip() {
        std::env::set_var("KUPL_AI_MOCK_T_STR", "  hello  ");
        let v = run(&meta("t_str", AiShape::Str, false), &[Value::Int(1)]).unwrap();
        assert_eq!(v, Value::str("hello"));
    }

    /// A REAL bug found+fixed (production-hardening PR-it690): unlike
    /// `json.rs`'s own parser and ~15 other input boundaries, neither
    /// `convert`'s raw-text fast path (`-> Str` ai funs) nor
    /// `value_from_json`'s `AiShape::Str` arm (used for a JSON ` `
    /// escape, decoded by `lsp::parse_json` with no NUL guard) rejected a
    /// NUL byte reaching a `Value::Str` -- violating KUPL's "Str is
    /// NUL-free" invariant via a path structurally unreachable from any
    /// KUPL-source literal. Confirmed live before this fix by calling
    /// `convert`/`value_from_json` directly (bypassing env-var mocking,
    /// since `std::env::set_var` itself rejects a NUL in the value) that a
    /// `Value::Str` genuinely ended up containing a raw `\0`. Traced the
    /// consequence: `cgen.rs`'s sole native string constructor, `k_str()`,
    /// derives its length via C's `strlen()` (stops at the first NUL) --
    /// so this wasn't just a broken invariant, it was a genuine
    /// cross-engine BYTE-IDENTITY divergence waiting to happen (interp/vm's
    /// Rust `String` keeps everything after the NUL; native would silently
    /// truncate there).
    #[test]
    fn ai_response_containing_a_nul_byte_is_a_clean_error_not_a_smuggled_value() {
        // raw-text fast path (`-> Str` ai funs, `convert`'s first branch).
        let m = meta("t_nul_raw", AiShape::Str, false);
        let err = super::convert(&m, "hi\u{0}there").unwrap_err();
        assert!(err.contains("NUL"), "{err}");

        // JSON-shape path (`value_from_json`'s `AiShape::Str` arm), reached
        // via a ` ` escape that `lsp::parse_json` decodes to a literal
        // NUL with no guard of its own.
        let json = crate::lsp::parse_json("{\"value\": \"hi\\u0000there\"}").unwrap();
        let inner = json.get("value").unwrap();
        let err2 = super::value_from_json(&AiShape::Str, inner).unwrap_err();
        assert!(err2.contains("NUL"), "{err2}");

        // an ordinary, NUL-free response is entirely unaffected.
        let m2 = meta("t_ok", AiShape::Str, false);
        let v = super::convert(&m2, "  hello  ").unwrap();
        assert_eq!(v, Value::str("hello"));
    }

    #[test]
    fn mock_record_parses() {
        let shape = AiShape::Record {
            ty: "Sentiment".into(),
            variant: "Sentiment".into(),
            fields: vec![("label".into(), AiShape::Str), ("score".into(), AiShape::Float)],
        };
        std::env::set_var(
            "KUPL_AI_MOCK_T_REC",
            "{\"value\":{\"label\":\"positive\",\"score\":0.9}}",
        );
        let v = run(&meta("t_rec", shape, false), &[Value::str("great")]).unwrap();
        assert_eq!(v.to_string(), "Sentiment(\"positive\", 0.9)");
    }

    /// A REAL bug found+fixed (production-hardening PR-it702): `records`' field
    /// types are stored UNSUBSTITUTED (still referencing the type's own
    /// `qvars`, the fresh inference-var ids bound to its `type_params` at
    /// declaration time), so a `Ty::Named(name, args)` instantiation carrying
    /// CONCRETE type arguments (e.g. `Box[Int]`) used to recurse into `value:
    /// T`'s raw, unbound `Ty::Var` id instead of substituting it with `Int` --
    /// the catch-all error arm then formatted that unbound var straight into
    /// the diagnostic as a meaningless `?0`, rejecting EVERY generic record as
    /// ai structured output. Confirmed live before this fix via `kupl check`
    /// on `type Box[T] = Box(value: T)` used as `ai fun get_box() -> Box[Int]`.
    #[test]
    fn build_shape_substitutes_generic_type_arguments() {
        // type Box[T] = Box(value: T) -- `T` is inference-var id 0.
        let mut records = HashMap::new();
        records.insert(
            "Box".to_string(),
            ("Box".to_string(), vec![("value".to_string(), Ty::Var(0))], vec![0u32]),
        );
        let ty = Ty::Named("Box".to_string(), vec![Ty::Int]);
        let shape = super::build_shape(&ty, &records, &mut Vec::new()).expect("Box[Int] must build a shape");
        assert_eq!(
            shape,
            AiShape::Record {
                ty: "Box".to_string(),
                variant: "Box".to_string(),
                fields: vec![("value".to_string(), AiShape::Int)],
            }
        );

        // multi-param: type Pair[A, B] = Pair(first: A, second: B).
        let mut records2 = HashMap::new();
        records2.insert(
            "Pair".to_string(),
            (
                "Pair".to_string(),
                vec![("first".to_string(), Ty::Var(0)), ("second".to_string(), Ty::Var(1))],
                vec![0u32, 1u32],
            ),
        );
        let ty2 = Ty::Named("Pair".to_string(), vec![Ty::Str, Ty::Int]);
        let shape2 = super::build_shape(&ty2, &records2, &mut Vec::new()).expect("Pair[Str, Int] must build a shape");
        assert_eq!(
            shape2,
            AiShape::Record {
                ty: "Pair".to_string(),
                variant: "Pair".to_string(),
                fields: vec![("first".to_string(), AiShape::Str), ("second".to_string(), AiShape::Int)],
            }
        );

        // nested: `List[Box[Int]]` -- substitution must apply through List too.
        let list_ty = Ty::List(Box::new(Ty::Named("Box".to_string(), vec![Ty::Int])));
        let list_shape = super::build_shape(&list_ty, &records, &mut Vec::new()).expect("List[Box[Int]] must build a shape");
        assert_eq!(
            list_shape,
            AiShape::List(Box::new(AiShape::Record {
                ty: "Box".to_string(),
                variant: "Box".to_string(),
                fields: vec![("value".to_string(), AiShape::Int)],
            }))
        );

        // a monomorphic record (no qvars, no args) is entirely unaffected.
        let mut records3 = HashMap::new();
        records3.insert(
            "Point".to_string(),
            ("Point".to_string(), vec![("x".to_string(), Ty::Int), ("y".to_string(), Ty::Int)], Vec::new()),
        );
        let ty3 = Ty::Named("Point".to_string(), Vec::new());
        let shape3 = super::build_shape(&ty3, &records3, &mut Vec::new()).expect("monomorphic Point must build a shape");
        assert_eq!(
            shape3,
            AiShape::Record {
                ty: "Point".to_string(),
                variant: "Point".to_string(),
                fields: vec![("x".to_string(), AiShape::Int), ("y".to_string(), AiShape::Int)],
            }
        );
    }

    #[test]
    fn shape_mismatch_message_is_kupl_syntax_not_rust_debug() {
        // Bug-hunt (ai-fun 5th round, PR-it534): the fallback arm of
        // `value_from_json` used Rust's `{:?}` Debug formatting for BOTH the
        // expected `AiShape` and the model's actual `Json` -- for a Record
        // shape this dumped the internal representation verbatim (`Record {
        // ty: "Shape", variant: "Circle", fields: [...] }` instead of just
        // `Shape`), and for the json side `Str("hello")` instead of KUPL/JSON
        // syntax `"hello"`. Confirmed via a real CLI run FIRST (both engines
        // byte-identical, so this was a message-QUALITY issue, not a
        // cross-engine divergence). Fixed with `shape_name`/`dump_json`.
        std::env::set_var("KUPL_AI_MOCK_T_MISMATCH_INT", "\"hello\"");
        let err = run(&meta("t_mismatch_int", AiShape::Int, false), &[Value::Int(1)]).unwrap_err();
        assert_eq!(err, "ai `t_mismatch_int`: expected Int, model returned \"hello\"", "{err}");

        let shape = AiShape::Record {
            ty: "Shape".into(),
            variant: "Circle".into(),
            fields: vec![("r".into(), AiShape::Float)],
        };
        std::env::set_var("KUPL_AI_MOCK_T_MISMATCH_REC", "42");
        let err2 = run(&meta("t_mismatch_rec", shape, false), &[Value::Int(1)]).unwrap_err();
        assert_eq!(err2, "ai `t_mismatch_rec`: expected Shape, model returned 42", "{err2}");

        // nested shapes render as KUPL generic syntax too
        std::env::set_var("KUPL_AI_MOCK_T_MISMATCH_LIST", "false");
        let err3 = run(&meta("t_mismatch_list", AiShape::List(Box::new(AiShape::Int)), false), &[Value::Int(1)]).unwrap_err();
        assert_eq!(err3, "ai `t_mismatch_list`: expected List[Int], model returned false", "{err3}");
    }

    #[test]
    fn wrapped_result_captures_errors() {
        std::env::set_var("KUPL_AI_MOCK_T_BAD", "not json");
        let v = run(&meta("t_bad", AiShape::Int, true), &[Value::Int(1)]).unwrap();
        assert!(v.to_string().starts_with("Err("), "{v}");
    }

    #[test]
    fn code_fences_are_stripped() {
        std::env::set_var("KUPL_AI_MOCK_T_FENCE", "```json\n{\"value\": 42}\n```");
        let v = run(&meta("t_fence", AiShape::Int, false), &[Value::Int(1)]).unwrap();
        assert_eq!(v, Value::Int(42));
    }

    #[test]
    fn shape_schema_wraps_scalars() {
        assert_eq!(
            wire_schema(&AiShape::Int),
            "{\"type\":\"object\",\"properties\":{\"value\":{\"type\":\"integer\"}},\"required\":[\"value\"],\"additionalProperties\":false}"
        );
    }

    /// A fresh axis (production-hardening PR-it622), per it621's own guidance:
    /// the AI mock-provider infrastructure's genuine JSON-parser crash was
    /// already found and fixed (it620, in the SHARED `lsp::parse_json`), but
    /// this file's OWN response-handling code (`convert`, `value_from_json`,
    /// `strip_fences`) had never been given a dedicated adversarial pass of
    /// its own -- `KUPL_AI_MOCK_*` env vars are effectively untrusted,
    /// model-controlled text (a real provider's response is no more trusted
    /// than this), so a malformed/adversarial mock string should behave
    /// exactly like a malformed/adversarial real provider response would.
    /// Feeds a battery of adversarial mock texts (empty, truncated JSON,
    /// mismatched code fences, a NUL byte, multibyte UTF-8, a huge string,
    /// JSON nested past `MAX_JSON_DEPTH`, an overflowing number, a bare
    /// non-object payload) through the REAL `ai_call` entry point for a
    /// structured (Record) shape and asserts it never panics -- always
    /// returns a clean `Result`, in this case always `Err` since none of
    /// these adversarial payloads are valid `Sentiment` records. Each case
    /// uses a distinct fun name (matching this file's existing
    /// `KUPL_AI_MOCK_T_*` convention) since Rust runs tests in the same
    /// process concurrently and env vars are global state.
    #[test]
    fn mock_response_fuzz_never_panics_across_adversarial_text() {
        let shape = AiShape::Record {
            ty: "Sentiment".into(),
            variant: "Sentiment".into(),
            fields: vec![("label".into(), AiShape::Str), ("score".into(), AiShape::Float)],
        };
        let deep_nesting = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        let huge = "x".repeat(500_000);
        let cases: Vec<(&str, String)> = vec![
            ("it622_empty", String::new()),
            ("it622_whitespace", "   \n\t  ".into()),
            ("it622_not_json", "not json at all".into()),
            ("it622_unterminated_obj", "{".into()),
            ("it622_unterminated_str", "{\"value\": \"unterminated".into()),
            ("it622_lone_close", "}".into()),
            ("it622_deep_nesting", deep_nesting),
            ("it622_huge", huge),
            ("it622_multibyte", "日本語 🎉🎉🎉 テスト".into()),
            // Note: a genuine NUL byte in the mock text is NOT testable via
            // this mechanism -- `std::env::set_var` itself panics on a NUL in
            // the VALUE (env vars are OS-level C strings, a platform
            // constraint entirely independent of KUPL's own code -- a real
            // provider response could still contain one, but exercising that
            // would need a fake HTTP layer, not the env-var mock path).
            ("it622_bad_fence_unclosed", "```json\n{\"value\": 1".into()),
            ("it622_bad_fence_extra_backtick", "```\n{\"value\": 1}\n````".into()),
            ("it622_overflow_number", "{\"value\": 1e400}".into()),
            ("it622_bare_non_object", "42".into()),
            ("it622_bare_array", "[1, 2, 3]".into()),
            ("it622_trailing_garbage", "{\"value\": 1} aardvark".into()),
            ("it622_nested_value_wrapper", "{\"value\": {\"value\": {\"value\": 1}}}".into()),
            ("it622_control_chars", "\u{1}\u{7f}{\"value\":1}".into()),
        ];
        for (name, text) in cases {
            std::env::set_var(format!("KUPL_AI_MOCK_{}", name.to_uppercase()), &text);
            let result = std::panic::catch_unwind(|| run(&meta(name, shape.clone(), false), &[Value::str("x")]));
            assert!(
                result.is_ok(),
                "ai_call panicked on adversarial mock text for case {name:?}: {text:?}"
            );
            std::env::remove_var(format!("KUPL_AI_MOCK_{}", name.to_uppercase()));
        }
    }

    /// The SAME adversarial-text battery, but through the OTHER, structurally
    /// distinct mock path: `tool_response`/`MockProvider`/`run_tool_loop`
    /// (used whenever an `ai fun` declares `tools [...]`) -- a scripted-round
    /// mock has its OWN parsing shape (`{"tool":...}`/`{"tools":[...]}`/
    /// `{"final":...}` per round, gated on `crate::json::parse`'s strictness
    /// check to decide interp/native agreement) entirely separate from the
    /// no-tools `convert()` path above, so it needs its own adversarial pass
    /// rather than assuming the first test's coverage carries over. Also
    /// includes a case specific to this path: a `"tools"` round with a huge
    /// number of entries, exercising `Vec::with_capacity` and the loop that
    /// builds one `ToolReq` per entry.
    #[test]
    fn tool_calling_mock_fuzz_never_panics_across_adversarial_scripts() {
        let tool = ToolMeta {
            name: "lookup".into(),
            description: "look something up".into(),
            params: vec![("q".into(), AiShape::Str)],
            ret: AiShape::Str,
        };
        let many_tool_calls = format!(
            "[{{\"tools\": [{}]}}]",
            (0..2000).map(|i| format!("{{\"tool\":\"lookup\",\"input\":{{\"q\":\"{i}\"}}}}")).collect::<Vec<_>>().join(",")
        );
        let cases: Vec<(&str, String)> = vec![
            ("it622_tool_empty", String::new()),
            ("it622_tool_not_json", "not json".into()),
            ("it622_tool_unterminated", "[{\"tool\": \"lookup\"".into()),
            ("it622_tool_wrong_type_name", "[{\"tool\": 42}]".into()),
            ("it622_tool_missing_tool_and_final", "[{}]".into()),
            ("it622_tool_unknown_tool_name", "[{\"tool\": \"does_not_exist\"}]".into()),
            ("it622_tool_deep_nesting", format!("[{{\"final\": {}{}}}]", "[".repeat(100_000), "]".repeat(100_000))),
            ("it622_tool_many_entries", many_tool_calls),
            ("it622_tool_empty_tools_array", "[{\"tools\": []}]".into()),
            ("it622_tool_malformed_tools_entry", "[{\"tools\": [{\"no_tool_key\": 1}]}]".into()),
            ("it622_tool_multibyte_final", "[{\"final\": \"日本語 🎉\"}]".into()),
            ("it622_tool_zero_rounds_scripted", "[]".into()),
        ];
        for (name, text) in cases {
            std::env::set_var(format!("KUPL_AI_MOCK_{}", name.to_uppercase()), &text);
            let mut m = meta(name, AiShape::Str, false);
            m.tools = vec![tool.clone()];
            let intent = m.intent.clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ai_call(&m, &intent, &[Value::str("x")], &mut NullToolHost)
            }));
            assert!(
                result.is_ok(),
                "ai_call (tool path) panicked on adversarial scripted-round text for case {name:?}: {text:?}"
            );
            std::env::remove_var(format!("KUPL_AI_MOCK_{}", name.to_uppercase()));
        }
    }

    /// A REAL diagnostics-quality bug found+fixed (production-hardening
    /// PR-it756): a self-referential/mutually-recursive `ai fun` <-> tool
    /// call chain is safely bounded by the shared `MAX_CALL_DEPTH`
    /// recursion guard (`interp.rs`/`vm.rs`) -- it does NOT hang or crash
    /// the process (confirmed separately via a live `kupl run` repro,
    /// `KUPL_AI_MOCK_CALL_A='[{"tool":"helper_a","input":{"x":1}}]'` on a
    /// program where `ai fun call_a(x) tools [helper_a]` and `fun
    /// helper_a(x) { call_a(x) }` mutually recurse -- pre-fix, the panic
    /// message was 65,201 BYTES; post-fix, 214 bytes). But every one of the
    /// thousands of nested `ai_call` frames on the way back up the unwind
    /// used to re-wrap the SAME error with another `"ai \`name\`: "`
    /// prefix, so the message ballooned linearly with recursion depth.
    /// This test isolates the wrapping logic itself (`ai_call`'s own
    /// `Err(msg) => Err(format!("ai \`{}\`: {msg}", meta.name))` arm) with
    /// a custom, deterministic `ToolHost` that recurses `ai_call` a fixed
    /// number of times (bypassing the real interpreter's `MAX_CALL_DEPTH`
    /// entirely, so this stays fast and doesn't need a 2GB-stacked thread)
    /// -- proving the fix scales to depth WITHOUT needing to actually hit
    /// the real 10,000-frame guard.
    #[test]
    fn a_self_referential_ai_fun_tool_recursion_does_not_balloon_its_panic_message() {
        struct RecursingHost {
            remaining: std::cell::Cell<u32>,
        }
        impl ToolHost for RecursingHost {
            fn call_tool(&mut self, _name: &str, _args: Vec<Value>) -> Result<Value, String> {
                let n = self.remaining.get();
                if n == 0 {
                    return Err("bottomed out".to_string());
                }
                self.remaining.set(n - 1);
                ai_call(&recursing_meta(), "loop", &[], self)
            }
        }
        fn recursing_meta() -> AiFunMeta {
            AiFunMeta {
                name: "it756_rec".into(),
                intent: "loop".into(),
                model: None,
                params: vec![],
                shape: AiShape::Int,
                wraps_result: false,
                tools: vec![ToolMeta {
                    name: "rec_tool".into(),
                    description: "recurse".into(),
                    params: vec![],
                    ret: AiShape::Int,
                }],
            }
        }

        // Run on a production-sized (8 MiB) stack -- the default 2 MiB test-
        // thread stack is smaller than the real CLI main thread's (2 GiB,
        // `main.rs`), and 500 levels of genuine Rust-level recursion through
        // `ai_call`/`tool_response`/`run_tool_loop`/`execute_tool` (several
        // stack frames per logical recursion level) needs more headroom than
        // the default test-thread stack provides -- matches this codebase's
        // own established pattern for deliberately-deep-recursion tests
        // (e.g. `check.rs::deep_nesting_is_a_clean_error_not_a_hang`).
        std::env::set_var("KUPL_AI_MOCK_IT756_REC", r#"[{"tool":"rec_tool","input":{}}]"#);
        let err = std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let mut host = RecursingHost { remaining: std::cell::Cell::new(500) };
                ai_call(&recursing_meta(), "loop", &[], &mut host).unwrap_err()
            })
            .unwrap()
            .join()
            .unwrap();
        std::env::remove_var("KUPL_AI_MOCK_IT756_REC");

        assert!(err.contains("bottomed out"), "the root cause must still be present: {err}");
        assert_eq!(
            err.matches("ai `it756_rec`:").count(),
            1,
            "the prefix must be added EXACTLY ONCE regardless of recursion depth, not once per level: {err}"
        );
        assert!(
            err.len() < 200,
            "a 500-level-deep recursive failure must not balloon the message size: {} bytes: {err}",
            err.len()
        );
    }
}
