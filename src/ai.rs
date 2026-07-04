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
//! byte-identical behavior. The native backend rejects modules with `ai fun`s
//! for now (a clear error at `kupl native` time).

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
}

/// Build an [`AiShape`] from a resolved return type. `records` maps a type
/// name to its single variant (name, fields) — multi-variant types are not
/// representable as structured output in v1.
pub fn build_shape(
    ty: &Ty,
    records: &HashMap<String, (String, Vec<(String, Ty)>)>,
    visiting: &mut Vec<String>,
) -> Result<AiShape, String> {
    match ty {
        Ty::Str => Ok(AiShape::Str),
        Ty::Int => Ok(AiShape::Int),
        Ty::Float => Ok(AiShape::Float),
        Ty::Bool => Ok(AiShape::Bool),
        Ty::List(t) => Ok(AiShape::List(Box::new(build_shape(t, records, visiting)?))),
        Ty::Option(t) => Ok(AiShape::Option(Box::new(build_shape(t, records, visiting)?))),
        Ty::Named(name) => {
            if visiting.iter().any(|v| v == name) {
                return Err(format!("recursive type `{name}` is not supported"));
            }
            let Some((variant, fields)) = records.get(name) else {
                return Err(format!(
                    "type `{name}` has multiple variants — structured output needs a record"
                ));
            };
            visiting.push(name.clone());
            let mut fs = Vec::with_capacity(fields.len());
            for (fname, fty) in fields {
                fs.push((fname.clone(), build_shape(fty, records, visiting)?));
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

/// Convert parsed JSON to a KUPL value guided by the shape.
pub fn value_from_json(shape: &AiShape, json: &Json) -> Result<Value, String> {
    match (shape, json) {
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
        (want, got) => Err(format!("expected {want:?}, model returned {got:?}")),
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
fn build_prompt(meta: &AiFunMeta, args: &[Value]) -> String {
    let mut prompt = meta.intent.clone();
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

fn raw_response(meta: &AiFunMeta, args: &[Value]) -> Result<String, String> {
    if let Some(text) = mock_response(&meta.name) {
        return Ok(text);
    }
    let prompt = build_prompt(meta, args);
    match env("KUPL_AI_PROVIDER").as_deref() {
        None | Some("anthropic") => anthropic_call(meta, &prompt),
        Some("openai") => openai_call(meta, &prompt, "https://api.openai.com", true),
        Some("ollama") => openai_call(meta, &prompt, "http://localhost:11434", false),
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

/// Execute one `ai fun` call. `Err` means panic (unless the function wraps
/// its result — then failures come back as `Ok(Err(msg))`).
pub fn ai_call(meta: &AiFunMeta, args: &[Value]) -> Result<Value, String> {
    let outcome = raw_response(meta, args).and_then(|text| convert(meta, &text));
    match outcome {
        Ok(v) if meta.wraps_result => Ok(Value::ok(v)),
        Ok(v) => Ok(v),
        Err(msg) if meta.wraps_result => Ok(Value::err(Value::str(msg))),
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
        }
    }

    #[test]
    fn mock_str_roundtrip() {
        std::env::set_var("KUPL_AI_MOCK_T_STR", "  hello  ");
        let v = ai_call(&meta("t_str", AiShape::Str, false), &[Value::Int(1)]).unwrap();
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
        let v = ai_call(&meta("t_rec", shape, false), &[Value::str("great")]).unwrap();
        assert_eq!(v.to_string(), "Sentiment(\"positive\", 0.9)");
    }

    #[test]
    fn wrapped_result_captures_errors() {
        std::env::set_var("KUPL_AI_MOCK_T_BAD", "not json");
        let v = ai_call(&meta("t_bad", AiShape::Int, true), &[Value::Int(1)]).unwrap();
        assert!(v.to_string().starts_with("Err("), "{v}");
    }

    #[test]
    fn code_fences_are_stripped() {
        std::env::set_var("KUPL_AI_MOCK_T_FENCE", "```json\n{\"value\": 42}\n```");
        let v = ai_call(&meta("t_fence", AiShape::Int, false), &[Value::Int(1)]).unwrap();
        assert_eq!(v, Value::Int(42));
    }

    #[test]
    fn shape_schema_wraps_scalars() {
        assert_eq!(
            wire_schema(&AiShape::Int),
            "{\"type\":\"object\",\"properties\":{\"value\":{\"type\":\"integer\"}},\"required\":[\"value\"],\"additionalProperties\":false}"
        );
    }
}
