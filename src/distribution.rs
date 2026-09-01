//! `weight distributed` (`docs/design/AGENTS.md` §4, `docs/design/
//! DISTRIBUTION.md`): real, node-to-node actor transport over plain TCP,
//! built entirely on `kser`'s framing (`kser::write_value_frame`/
//! `read_value_frame`) so the protocol layer needs zero new binary-format
//! code -- every `DistMsg` below is just a tagged `PortableValue::Ctor`
//! shape, encoded/decoded through the SAME machinery `kser.rs` already has
//! full round-trip test coverage for.
//!
//! **Security posture, stated plainly (matching this whole codebase's own
//! "known limitation, not silently ignored" discipline, `docs/PRODUCTION.md`
//! §"Known limitations"):** this is a SHARED-SECRET-AUTHENTICATED channel,
//! NOT an ENCRYPTED one. A `DistAuth` token (a plain string, compared with
//! a constant-time check) gates who may spawn actors or exchange messages,
//! closing the "any host on the network can silently RCE this" failure
//! mode a fully open listener would have -- but the token itself, and
//! every message after it, travels in PLAINTEXT. Do not run a distributed
//! `kupl node` across any network you don't already trust; put it behind a
//! VPN/SSH tunnel/service mesh (exactly the same guidance `docs/design/
//! DISTRIBUTION.md`'s own "Phase 6+" sequencing already gives for the
//! eventual mTLS story) if the link crosses anything untrusted. Hand-
//! rolling real TLS from scratch, under time pressure, without expert
//! review, is explicitly the kind of risk that doc's own sequencing
//! reasoning was written to avoid -- this module does not attempt it.

use crate::parallel::PortableValue;

/// Env var a `weight distributed` spawn site reads to learn (1) where to
/// connect and (2) the shared secret to authenticate with. Format:
/// `<token>@<host:port>`. Chosen over a KUPL-level capability/builtin
/// (unlike `CapNet`/`CapFs`, which the RUNNING PROGRAM constructs and
/// passes around as ordinary values) because a distributed node address is
/// DEPLOYMENT configuration, not program logic -- it should never be
/// something the checker has to reason about or that ends up serialized
/// into a portable value by accident.
pub const KUPL_DISTRIBUTED_NODE_ENV: &str = "KUPL_DISTRIBUTED_NODE";

/// Parses `KUPL_DISTRIBUTED_NODE`'s own `<token>@<host:port>` format.
pub fn parse_node_env(raw: &str) -> Result<(String, String), String> {
    let Some((token, addr)) = raw.split_once('@') else {
        return Err(format!(
            "{KUPL_DISTRIBUTED_NODE_ENV} must be in the form `<token>@<host:port>`, got {raw:?}"
        ));
    };
    if token.is_empty() {
        return Err(format!("{KUPL_DISTRIBUTED_NODE_ENV}: the shared-secret token before `@` must not be empty"));
    }
    if addr.is_empty() {
        return Err(format!("{KUPL_DISTRIBUTED_NODE_ENV}: the `host:port` after `@` must not be empty"));
    }
    Ok((token.to_string(), addr.to_string()))
}

/// One message in the `weight distributed` wire protocol. Every variant
/// round-trips through `to_wire`/`from_wire` below as a `PortableValue::
/// Ctor` tagged `"DistMsg"` -- reusing `kser`'s already-tested encoder
/// entirely, rather than a second, parallel binary format.
#[derive(Debug, Clone, PartialEq)]
pub enum DistMsg {
    /// First message on every connection, both directions optional in
    /// principle but always sent by the CLIENT first in practice: proves
    /// the sender knows the shared secret before the server does anything
    /// else on this connection.
    Auth { token: String },
    AuthOk,
    /// The connection is closed immediately after this is sent -- there is
    /// no retry-with-a-different-token dance, matching how a rejected TLS
    /// handshake or a rejected SSH key both just end the connection.
    AuthFailed,
    /// Spawn a new instance of `comp_name` on the remote node, exactly like
    /// a local `instantiate_concurrent` call -- `args` are already-
    /// evaluated, already-portable constructor arguments (K0306 guarantees
    /// every `weight distributed` constructor arg is portable, the same
    /// guarantee `instantiate_concurrent`'s own `to_portable` call already
    /// relies on for `Pooled`/`Dedicated`).
    Spawn { comp_name: String, args: Vec<(Option<String>, PortableValue)> },
    /// `remote_id` is this connection's own private handle for the newly
    /// spawned actor -- meaningful only on THIS connection, not a
    /// cluster-wide identity (matching `Pooled`'s own `local_id`, which is
    /// likewise only meaningful within its own worker).
    SpawnOk { remote_id: u64 },
    SpawnErr { msg: String },
    /// Fire-and-forget, mirroring `ActorMsg::Deliver` exactly.
    Deliver { remote_id: u64, port: String, value: PortableValue },
    /// Blocking, mirroring `ActorMsg::Call` -- `call_id` (not `remote_id`)
    /// is what `CallReply` echoes back, since a single connection can have
    /// several `Call`s in flight to DIFFERENT `remote_id`s at once.
    Call { remote_id: u64, fn_name: String, args: Vec<PortableValue>, call_id: u64 },
    CallReply { call_id: u64, result: Result<PortableValue, String> },
}

fn ctor(variant: &str, fields: Vec<PortableValue>) -> PortableValue {
    PortableValue::Ctor { ty: "DistMsg".to_string(), variant: variant.to_string(), fields }
}

fn args_to_wire(args: &[(Option<String>, PortableValue)]) -> PortableValue {
    PortableValue::List(
        args.iter()
            .map(|(name, v)| {
                let name_v = match name {
                    Some(n) => PortableValue::Str(n.clone()),
                    None => PortableValue::Unit,
                };
                PortableValue::List(vec![name_v, v.clone()])
            })
            .collect(),
    )
}

fn args_from_wire(v: PortableValue) -> Result<Vec<(Option<String>, PortableValue)>, String> {
    let PortableValue::List(items) = v else {
        return Err("DistMsg: expected a List for constructor args".to_string());
    };
    items
        .into_iter()
        .map(|item| {
            let PortableValue::List(mut pair) = item else {
                return Err("DistMsg: expected a 2-element List for one constructor arg".to_string());
            };
            if pair.len() != 2 {
                return Err(format!("DistMsg: expected exactly 2 elements in a constructor-arg pair, got {}", pair.len()));
            }
            let value = pair.pop().unwrap();
            let name_v = pair.pop().unwrap();
            let name = match name_v {
                PortableValue::Unit => None,
                PortableValue::Str(s) => Some(s),
                other => return Err(format!("DistMsg: expected Unit or Str for an arg name, got {other:?}")),
            };
            Ok((name, value))
        })
        .collect()
}

fn result_to_wire(r: &Result<PortableValue, String>) -> PortableValue {
    match r {
        Ok(v) => ctor("CallOk", vec![v.clone()]),
        Err(msg) => ctor("CallErr", vec![PortableValue::Str(msg.clone())]),
    }
}

fn result_from_wire(v: PortableValue) -> Result<Result<PortableValue, String>, String> {
    let PortableValue::Ctor { ty, variant, mut fields } = v else {
        return Err("DistMsg: expected a Ctor for a Call result".to_string());
    };
    if ty != "DistMsg" {
        return Err(format!("DistMsg: expected ty \"DistMsg\" for a Call result, got {ty:?}"));
    }
    match (variant.as_str(), fields.len()) {
        ("CallOk", 1) => Ok(Ok(fields.pop().unwrap())),
        ("CallErr", 1) => match fields.pop().unwrap() {
            PortableValue::Str(s) => Ok(Err(s)),
            other => Err(format!("DistMsg: expected Str for CallErr's message, got {other:?}")),
        },
        (other, n) => Err(format!("DistMsg: unknown Call-result shape {other:?} with {n} field(s)")),
    }
}

impl DistMsg {
    pub fn to_wire(&self) -> PortableValue {
        match self {
            DistMsg::Auth { token } => ctor("Auth", vec![PortableValue::Str(token.clone())]),
            DistMsg::AuthOk => ctor("AuthOk", vec![]),
            DistMsg::AuthFailed => ctor("AuthFailed", vec![]),
            DistMsg::Spawn { comp_name, args } => {
                ctor("Spawn", vec![PortableValue::Str(comp_name.clone()), args_to_wire(args)])
            }
            DistMsg::SpawnOk { remote_id } => ctor("SpawnOk", vec![PortableValue::Int(*remote_id as i64)]),
            DistMsg::SpawnErr { msg } => ctor("SpawnErr", vec![PortableValue::Str(msg.clone())]),
            DistMsg::Deliver { remote_id, port, value } => {
                ctor("Deliver", vec![PortableValue::Int(*remote_id as i64), PortableValue::Str(port.clone()), value.clone()])
            }
            DistMsg::Call { remote_id, fn_name, args, call_id } => ctor(
                "Call",
                vec![
                    PortableValue::Int(*remote_id as i64),
                    PortableValue::Str(fn_name.clone()),
                    PortableValue::List(args.clone()),
                    PortableValue::Int(*call_id as i64),
                ],
            ),
            DistMsg::CallReply { call_id, result } => {
                ctor("CallReply", vec![PortableValue::Int(*call_id as i64), result_to_wire(result)])
            }
        }
    }

    pub fn from_wire(v: PortableValue) -> Result<DistMsg, String> {
        let PortableValue::Ctor { ty, variant, mut fields } = v else {
            return Err("DistMsg: expected a Ctor at the top level".to_string());
        };
        if ty != "DistMsg" {
            return Err(format!("DistMsg: expected ty \"DistMsg\", got {ty:?}"));
        }
        match (variant.as_str(), fields.len()) {
            ("Auth", 1) => match fields.pop().unwrap() {
                PortableValue::Str(token) => Ok(DistMsg::Auth { token }),
                other => Err(format!("DistMsg::Auth: expected Str, got {other:?}")),
            },
            ("AuthOk", 0) => Ok(DistMsg::AuthOk),
            ("AuthFailed", 0) => Ok(DistMsg::AuthFailed),
            ("Spawn", 2) => {
                let args_v = fields.pop().unwrap();
                let comp_name = match fields.pop().unwrap() {
                    PortableValue::Str(s) => s,
                    other => return Err(format!("DistMsg::Spawn: expected Str for comp_name, got {other:?}")),
                };
                Ok(DistMsg::Spawn { comp_name, args: args_from_wire(args_v)? })
            }
            ("SpawnOk", 1) => match fields.pop().unwrap() {
                PortableValue::Int(n) if n >= 0 => Ok(DistMsg::SpawnOk { remote_id: n as u64 }),
                other => Err(format!("DistMsg::SpawnOk: expected a non-negative Int, got {other:?}")),
            },
            ("SpawnErr", 1) => match fields.pop().unwrap() {
                PortableValue::Str(msg) => Ok(DistMsg::SpawnErr { msg }),
                other => Err(format!("DistMsg::SpawnErr: expected Str, got {other:?}")),
            },
            ("Deliver", 3) => {
                let value = fields.pop().unwrap();
                let port = match fields.pop().unwrap() {
                    PortableValue::Str(s) => s,
                    other => return Err(format!("DistMsg::Deliver: expected Str for port, got {other:?}")),
                };
                let remote_id = match fields.pop().unwrap() {
                    PortableValue::Int(n) if n >= 0 => n as u64,
                    other => return Err(format!("DistMsg::Deliver: expected a non-negative Int for remote_id, got {other:?}")),
                };
                Ok(DistMsg::Deliver { remote_id, port, value })
            }
            ("Call", 4) => {
                let call_id = match fields.pop().unwrap() {
                    PortableValue::Int(n) if n >= 0 => n as u64,
                    other => return Err(format!("DistMsg::Call: expected a non-negative Int for call_id, got {other:?}")),
                };
                let args = match fields.pop().unwrap() {
                    PortableValue::List(xs) => xs,
                    other => return Err(format!("DistMsg::Call: expected List for args, got {other:?}")),
                };
                let fn_name = match fields.pop().unwrap() {
                    PortableValue::Str(s) => s,
                    other => return Err(format!("DistMsg::Call: expected Str for fn_name, got {other:?}")),
                };
                let remote_id = match fields.pop().unwrap() {
                    PortableValue::Int(n) if n >= 0 => n as u64,
                    other => return Err(format!("DistMsg::Call: expected a non-negative Int for remote_id, got {other:?}")),
                };
                Ok(DistMsg::Call { remote_id, fn_name, args, call_id })
            }
            ("CallReply", 2) => {
                let result_v = fields.pop().unwrap();
                let call_id = match fields.pop().unwrap() {
                    PortableValue::Int(n) if n >= 0 => n as u64,
                    other => return Err(format!("DistMsg::CallReply: expected a non-negative Int for call_id, got {other:?}")),
                };
                Ok(DistMsg::CallReply { call_id, result: result_from_wire(result_v)? })
            }
            (other, n) => Err(format!("DistMsg: unknown variant {other:?} with {n} field(s)")),
        }
    }
}

/// Constant-time string comparison for the shared-secret check -- an
/// ordinary `==` short-circuits on the first mismatched byte, which leaks
/// how many leading bytes of a guessed token were correct through timing
/// (a real, well-known class of attack against naive token comparisons).
/// Deliberately simple (XOR-accumulate every byte, compare lengths first)
/// rather than reaching for a crypto crate this zero-dependency project
/// doesn't have.
pub fn tokens_match(a: &str, b: &str) -> bool {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    if ab.len() != bb.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..ab.len() {
        diff |= ab[i] ^ bb[i];
    }
    diff == 0
}

/// Connect to a `kupl node` listener and complete the auth handshake.
/// Pure transport -- no `Interp` dependency at all, which is exactly what
/// makes this independently testable against a hand-rolled mock listener
/// (see `tests` below) rather than needing a full actor runtime just to
/// exercise the wire protocol.
pub fn connect_and_authenticate(addr: &str, token: &str) -> Result<std::net::TcpStream, String> {
    let mut stream = std::net::TcpStream::connect(addr)
        .map_err(|e| format!("distributed spawn: could not connect to {addr}: {e}"))?;
    crate::kser::write_value_frame(&mut stream, &DistMsg::Auth { token: token.to_string() }.to_wire())
        .map_err(|e| format!("distributed spawn: sending Auth to {addr} failed: {e}"))?;
    let reply = crate::kser::read_value_frame(&mut stream)
        .map_err(|e| format!("distributed spawn: reading Auth reply from {addr} failed: {e}"))?;
    match DistMsg::from_wire(reply)? {
        DistMsg::AuthOk => Ok(stream),
        DistMsg::AuthFailed => Err(format!("distributed spawn: {addr} rejected the shared-secret token")),
        other => Err(format!("distributed spawn: expected AuthOk/AuthFailed from {addr}, got {other:?}")),
    }
}

/// Send `Spawn` on an already-authenticated connection and wait for
/// `SpawnOk`/`SpawnErr`. Synchronous and blocking by design: the server
/// only ever sends DIRECT replies on this connection (never an unsolicited
/// push -- see this module's own `DistMsg` doc comment), so "the next
/// frame is this request's own reply" is always correct, no call-id
/// matching needed for a fresh connection with exactly one request in
/// flight.
pub fn spawn_remote(
    stream: &mut std::net::TcpStream,
    comp_name: &str,
    args: Vec<(Option<String>, PortableValue)>,
) -> Result<u64, String> {
    crate::kser::write_value_frame(stream, &DistMsg::Spawn { comp_name: comp_name.to_string(), args }.to_wire())
        .map_err(|e| format!("distributed spawn: sending Spawn failed: {e}"))?;
    let reply = crate::kser::read_value_frame(stream).map_err(|e| format!("distributed spawn: reading Spawn reply failed: {e}"))?;
    match DistMsg::from_wire(reply)? {
        DistMsg::SpawnOk { remote_id } => Ok(remote_id),
        DistMsg::SpawnErr { msg } => Err(format!("distributed spawn of `{comp_name}` failed on the remote node: {msg}")),
        other => Err(format!("distributed spawn: expected SpawnOk/SpawnErr, got {other:?}")),
    }
}

/// Fire-and-forget `Deliver` -- no reply is read (there is none), mirroring
/// `emit`'s own non-blocking semantics for every other actor route. Real
/// backpressure comes for free from the OS's own TCP send-buffer behavior
/// (a `write_all` on a full buffer blocks briefly rather than dropping) --
/// no custom bounded-retry wrapper is added here, unlike `Dedicated`'s own
/// `try_send_with_backoff`, since that mailbox-depth problem doesn't exist
/// the same way over a byte STREAM with genuine flow control.
pub fn deliver_remote(stream: &mut std::net::TcpStream, remote_id: u64, port: &str, value: PortableValue) -> std::io::Result<()> {
    crate::kser::write_value_frame(stream, &DistMsg::Deliver { remote_id, port: port.to_string(), value }.to_wire())
}

/// Blocking `Call` -- send, then read exactly one reply frame (see
/// `spawn_remote`'s own doc comment for why this doesn't need call-id
/// matching on a fresh single-actor connection). `call_id` is still
/// threaded through and validated against the reply for defense in
/// depth, even though only one value is possible here.
pub fn call_remote_over_wire(
    stream: &mut std::net::TcpStream,
    remote_id: u64,
    fn_name: &str,
    args: Vec<PortableValue>,
    call_id: u64,
) -> Result<Result<PortableValue, String>, String> {
    crate::kser::write_value_frame(stream, &DistMsg::Call { remote_id, fn_name: fn_name.to_string(), args, call_id }.to_wire())
        .map_err(|e| format!("distributed call: sending Call failed: {e}"))?;
    let reply = crate::kser::read_value_frame(stream).map_err(|e| format!("distributed call: reading CallReply failed: {e}"))?;
    match DistMsg::from_wire(reply)? {
        DistMsg::CallReply { call_id: got_id, result } if got_id == call_id => Ok(result),
        DistMsg::CallReply { call_id: got_id, .. } => {
            Err(format!("distributed call: reply call_id mismatch (sent {call_id}, got {got_id})"))
        }
        other => Err(format!("distributed call: expected CallReply, got {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: DistMsg) {
        let wire = msg.to_wire();
        let back = DistMsg::from_wire(wire).unwrap_or_else(|e| panic!("decode failed for {msg:?}: {e}"));
        assert_eq!(back, msg, "DistMsg round-trip mismatch");
    }

    #[test]
    fn every_variant_roundtrips_through_the_wire_shape() {
        roundtrip(DistMsg::Auth { token: "secret-123".to_string() });
        roundtrip(DistMsg::AuthOk);
        roundtrip(DistMsg::AuthFailed);
        roundtrip(DistMsg::Spawn {
            comp_name: "Worker".to_string(),
            args: vec![(Some("n".to_string()), PortableValue::Int(5)), (None, PortableValue::Str("x".to_string()))],
        });
        roundtrip(DistMsg::Spawn { comp_name: "Empty".to_string(), args: vec![] });
        roundtrip(DistMsg::SpawnOk { remote_id: 0 });
        roundtrip(DistMsg::SpawnOk { remote_id: 42 });
        roundtrip(DistMsg::SpawnErr { msg: "unknown component `Foo`".to_string() });
        roundtrip(DistMsg::Deliver { remote_id: 1, port: "num".to_string(), value: PortableValue::Int(7) });
        roundtrip(DistMsg::Call {
            remote_id: 1,
            fn_name: "current".to_string(),
            args: vec![PortableValue::Bool(true)],
            call_id: 99,
        });
        roundtrip(DistMsg::CallReply { call_id: 99, result: Ok(PortableValue::Int(7)) });
        roundtrip(DistMsg::CallReply { call_id: 99, result: Err("boom".to_string()) });
    }

    /// The whole point of routing through `kser` instead of a second
    /// format: a `DistMsg` genuinely survives a real frame round-trip over
    /// an in-memory stream, not just a `to_wire`/`from_wire` call.
    #[test]
    fn a_dist_msg_survives_a_real_frame_round_trip() {
        let msg = DistMsg::Deliver { remote_id: 3, port: "kick".to_string(), value: PortableValue::Int(42) };
        let mut buf = Vec::new();
        crate::kser::write_value_frame(&mut buf, &msg.to_wire()).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let decoded_wire = crate::kser::read_value_frame(&mut cursor).unwrap();
        assert_eq!(DistMsg::from_wire(decoded_wire).unwrap(), msg);
    }

    #[test]
    fn a_malformed_wire_shape_is_rejected_cleanly_not_a_panic() {
        assert!(DistMsg::from_wire(PortableValue::Int(1)).is_err());
        assert!(DistMsg::from_wire(PortableValue::Ctor {
            ty: "NotDistMsg".to_string(),
            variant: "Auth".to_string(),
            fields: vec![PortableValue::Str("x".to_string())],
        })
        .is_err());
        assert!(DistMsg::from_wire(PortableValue::Ctor {
            ty: "DistMsg".to_string(),
            variant: "TotallyUnknown".to_string(),
            fields: vec![],
        })
        .is_err());
        assert!(DistMsg::from_wire(PortableValue::Ctor {
            ty: "DistMsg".to_string(),
            variant: "Auth".to_string(),
            fields: vec![], // wrong field count
        })
        .is_err());
    }

    #[test]
    fn parse_node_env_accepts_the_documented_shape() {
        assert_eq!(parse_node_env("s3cr3t@127.0.0.1:9000").unwrap(), ("s3cr3t".to_string(), "127.0.0.1:9000".to_string()));
    }

    #[test]
    fn parse_node_env_rejects_missing_pieces() {
        assert!(parse_node_env("no-at-sign-here").is_err());
        assert!(parse_node_env("@127.0.0.1:9000").is_err());
        assert!(parse_node_env("token@").is_err());
    }

    #[test]
    fn tokens_match_is_correct_for_equal_and_unequal_and_different_length() {
        assert!(tokens_match("abc", "abc"));
        assert!(!tokens_match("abc", "abd"));
        assert!(!tokens_match("abc", "ab"));
        assert!(!tokens_match("", "a"));
        assert!(tokens_match("", ""));
    }

    /// A hand-rolled mock server (a plain background thread speaking the
    /// SAME `DistMsg` protocol, no `Interp` involved at all) -- proves the
    /// CLIENT-side transport functions (`connect_and_authenticate`,
    /// `spawn_remote`, `deliver_remote`, `call_remote_over_wire`) are
    /// correct over a REAL socket, independent of whether the real
    /// actor-hosting server (`interp.rs`) is implemented correctly. Each
    /// test binds an OS-assigned free port (`127.0.0.1:0`), matching this
    /// codebase's own existing mock-HTTP-server test precedent
    /// (`ai.rs`/`cgen.rs`).
    fn spawn_mock_server<F>(handler: F) -> String
    where
        F: FnOnce(std::net::TcpStream) + Send + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port");
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept one connection");
            handler(stream);
        });
        addr
    }

    #[test]
    fn connect_and_authenticate_succeeds_with_the_right_token() {
        let addr = spawn_mock_server(|mut stream| {
            let req = crate::kser::read_value_frame(&mut stream).unwrap();
            assert_eq!(DistMsg::from_wire(req).unwrap(), DistMsg::Auth { token: "right".to_string() });
            crate::kser::write_value_frame(&mut stream, &DistMsg::AuthOk.to_wire()).unwrap();
        });
        connect_and_authenticate(&addr, "right").unwrap();
    }

    #[test]
    fn connect_and_authenticate_fails_cleanly_with_the_wrong_token() {
        let addr = spawn_mock_server(|mut stream| {
            let _ = crate::kser::read_value_frame(&mut stream).unwrap();
            crate::kser::write_value_frame(&mut stream, &DistMsg::AuthFailed.to_wire()).unwrap();
        });
        let err = connect_and_authenticate(&addr, "wrong").unwrap_err();
        assert!(err.contains("rejected"), "{err}");
    }

    #[test]
    fn connect_and_authenticate_fails_cleanly_when_nothing_is_listening() {
        // A free port that was bound-then-immediately-dropped is very
        // likely refused -- deterministic enough for this test's purpose
        // (proving a connection failure surfaces as a clean `Err`, not a
        // panic or a hang), without asserting on the exact OS error text.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        assert!(connect_and_authenticate(&addr, "x").is_err());
    }

    #[test]
    fn spawn_remote_returns_the_remote_id_on_success() {
        let addr = spawn_mock_server(|mut stream| {
            let auth = crate::kser::read_value_frame(&mut stream).unwrap();
            assert_eq!(DistMsg::from_wire(auth).unwrap(), DistMsg::Auth { token: "t".to_string() });
            crate::kser::write_value_frame(&mut stream, &DistMsg::AuthOk.to_wire()).unwrap();
            let spawn = crate::kser::read_value_frame(&mut stream).unwrap();
            assert_eq!(
                DistMsg::from_wire(spawn).unwrap(),
                DistMsg::Spawn { comp_name: "Worker".to_string(), args: vec![(Some("n".to_string()), PortableValue::Int(5))] }
            );
            crate::kser::write_value_frame(&mut stream, &DistMsg::SpawnOk { remote_id: 7 }.to_wire()).unwrap();
        });
        let mut stream = connect_and_authenticate(&addr, "t").unwrap();
        let remote_id = spawn_remote(&mut stream, "Worker", vec![(Some("n".to_string()), PortableValue::Int(5))]).unwrap();
        assert_eq!(remote_id, 7);
    }

    #[test]
    fn spawn_remote_surfaces_a_spawn_err_as_a_clean_result_err() {
        let addr = spawn_mock_server(|mut stream| {
            let _ = crate::kser::read_value_frame(&mut stream).unwrap();
            crate::kser::write_value_frame(&mut stream, &DistMsg::AuthOk.to_wire()).unwrap();
            let _ = crate::kser::read_value_frame(&mut stream).unwrap();
            crate::kser::write_value_frame(&mut stream, &DistMsg::SpawnErr { msg: "unknown component `Nope`".to_string() }.to_wire()).unwrap();
        });
        let mut stream = connect_and_authenticate(&addr, "t").unwrap();
        let err = spawn_remote(&mut stream, "Nope", vec![]).unwrap_err();
        assert!(err.contains("unknown component"), "{err}");
    }

    #[test]
    fn deliver_remote_and_call_remote_over_wire_round_trip_against_a_mock_server() {
        let addr = spawn_mock_server(|mut stream| {
            let _ = crate::kser::read_value_frame(&mut stream).unwrap(); // Auth
            crate::kser::write_value_frame(&mut stream, &DistMsg::AuthOk.to_wire()).unwrap();
            let _ = crate::kser::read_value_frame(&mut stream).unwrap(); // Spawn
            crate::kser::write_value_frame(&mut stream, &DistMsg::SpawnOk { remote_id: 0 }.to_wire()).unwrap();
            // Deliver: no reply expected, just consume the frame.
            let deliver = crate::kser::read_value_frame(&mut stream).unwrap();
            assert_eq!(
                DistMsg::from_wire(deliver).unwrap(),
                DistMsg::Deliver { remote_id: 0, port: "num".to_string(), value: PortableValue::Int(9) }
            );
            // Call: reply with a matching call_id.
            let call = crate::kser::read_value_frame(&mut stream).unwrap();
            let DistMsg::Call { call_id, .. } = DistMsg::from_wire(call).unwrap() else { panic!("expected Call") };
            crate::kser::write_value_frame(
                &mut stream,
                &DistMsg::CallReply { call_id, result: Ok(PortableValue::Str("hi".to_string())) }.to_wire(),
            )
            .unwrap();
        });
        let mut stream = connect_and_authenticate(&addr, "t").unwrap();
        let remote_id = spawn_remote(&mut stream, "Worker", vec![]).unwrap();
        deliver_remote(&mut stream, remote_id, "num", PortableValue::Int(9)).unwrap();
        let result = call_remote_over_wire(&mut stream, remote_id, "greet", vec![], 1).unwrap();
        assert_eq!(result, Ok(PortableValue::Str("hi".to_string())));
    }

    #[test]
    fn call_remote_over_wire_surfaces_a_call_error_as_ok_err_not_a_transport_failure() {
        let addr = spawn_mock_server(|mut stream| {
            let _ = crate::kser::read_value_frame(&mut stream).unwrap();
            crate::kser::write_value_frame(&mut stream, &DistMsg::AuthOk.to_wire()).unwrap();
            let call = crate::kser::read_value_frame(&mut stream).unwrap();
            let DistMsg::Call { call_id, .. } = DistMsg::from_wire(call).unwrap() else { panic!("expected Call") };
            crate::kser::write_value_frame(
                &mut stream,
                &DistMsg::CallReply { call_id, result: Err("boom".to_string()) }.to_wire(),
            )
            .unwrap();
        });
        let mut stream = connect_and_authenticate(&addr, "t").unwrap();
        let result = call_remote_over_wire(&mut stream, 0, "f", vec![], 1).unwrap();
        assert_eq!(result, Err("boom".to_string()));
    }
}
