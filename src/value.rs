//! Runtime values and environments.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use crate::ast::Block;

#[derive(Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(Rc<String>),
    Unit,
    List(Rc<Vec<Value>>),
    /// ADT value: `Ctor { ty: "Shape", variant: "Circle", fields: [1.5] }`.
    Ctor {
        ty: Rc<String>,
        variant: Rc<String>,
        fields: Rc<Vec<Value>>,
    },
    Closure(Rc<Closure>),
    /// Reference to a named top-level function.
    Fun(Rc<String>),
    /// Reference to a component instance in the runtime.
    Component(usize),
    Range(i64, i64, bool),
}

pub struct Closure {
    pub params: Vec<String>,
    pub body: Rc<Block>,
    pub env: Env,
}

impl Value {
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(Rc::new(s.into()))
    }
    pub fn some(v: Value) -> Value {
        Value::Ctor {
            ty: Rc::new("Option".into()),
            variant: Rc::new("Some".into()),
            fields: Rc::new(vec![v]),
        }
    }
    pub fn none() -> Value {
        Value::Ctor {
            ty: Rc::new("Option".into()),
            variant: Rc::new("None".into()),
            fields: Rc::new(vec![]),
        }
    }
    pub fn ok(v: Value) -> Value {
        Value::Ctor {
            ty: Rc::new("Result".into()),
            variant: Rc::new("Ok".into()),
            fields: Rc::new(vec![v]),
        }
    }
    pub fn err(v: Value) -> Value {
        Value::Ctor {
            ty: Rc::new("Result".into()),
            variant: Rc::new("Err".into()),
            fields: Rc::new(vec![v]),
        }
    }
    pub fn type_name(&self) -> String {
        match self {
            Value::Int(_) => "Int".into(),
            Value::Float(_) => "Float".into(),
            Value::Bool(_) => "Bool".into(),
            Value::Str(_) => "Str".into(),
            Value::Unit => "Unit".into(),
            Value::List(_) => "List".into(),
            Value::Ctor { ty, .. } => ty.as_str().into(),
            Value::Closure(_) => "fn".into(),
            Value::Fun(_) => "fn".into(),
            Value::Component(_) => "component".into(),
            Value::Range(..) => "Range".into(),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Unit, Value::Unit) => true,
            (Value::List(a), Value::List(b)) => a == b,
            (
                Value::Ctor { ty: t1, variant: v1, fields: f1 },
                Value::Ctor { ty: t2, variant: v2, fields: f2 },
            ) => t1 == t2 && v1 == v2 && f1 == f2,
            (Value::Component(a), Value::Component(b)) => a == b,
            (Value::Range(a, b, i), Value::Range(c, d, j)) => a == c && b == d && i == j,
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(v) => write!(f, "{v}"),
            Value::Float(v) => {
                if v.fract() == 0.0 && v.is_finite() {
                    write!(f, "{v:.1}")
                } else {
                    write!(f, "{v}")
                }
            }
            Value::Bool(v) => write!(f, "{v}"),
            Value::Str(s) => write!(f, "{s}"),
            Value::Unit => write!(f, "()"),
            Value::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", DebugStr(item))?;
                }
                write!(f, "]")
            }
            Value::Ctor { variant, fields, .. } => {
                write!(f, "{variant}")?;
                if !fields.is_empty() {
                    write!(f, "(")?;
                    for (i, field) in fields.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", DebugStr(field))?;
                    }
                    write!(f, ")")?;
                }
                Ok(())
            }
            Value::Closure(_) => write!(f, "<fn>"),
            Value::Fun(name) => write!(f, "<fn {name}>"),
            Value::Component(id) => write!(f, "<component #{id}>"),
            Value::Range(a, b, incl) => write!(f, "{a}..{}{b}", if *incl { "=" } else { "" }),
        }
    }
}

/// Like Display, but strings are quoted (used inside containers).
struct DebugStr<'a>(&'a Value);

impl fmt::Display for DebugStr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Value::Str(s) => write!(f, "\"{s}\""),
            other => write!(f, "{other}"),
        }
    }
}

/// Lexically scoped, shared, mutable environment (closures capture it).
#[derive(Clone)]
pub struct Env(Rc<RefCell<EnvInner>>);

struct EnvInner {
    vars: HashMap<String, Value>,
    parent: Option<Env>,
}

impl Env {
    pub fn new() -> Env {
        Env(Rc::new(RefCell::new(EnvInner { vars: HashMap::new(), parent: None })))
    }
    pub fn child(&self) -> Env {
        Env(Rc::new(RefCell::new(EnvInner {
            vars: HashMap::new(),
            parent: Some(self.clone()),
        })))
    }
    pub fn define(&self, name: &str, value: Value) {
        self.0.borrow_mut().vars.insert(name.to_string(), value);
    }
    pub fn get(&self, name: &str) -> Option<Value> {
        let inner = self.0.borrow();
        if let Some(v) = inner.vars.get(name) {
            return Some(v.clone());
        }
        match &inner.parent {
            Some(p) => p.get(name),
            None => None,
        }
    }
    /// Assign to an existing binding (walks up the chain). Returns false if unbound.
    pub fn set(&self, name: &str, value: Value) -> bool {
        let mut inner = self.0.borrow_mut();
        if inner.vars.contains_key(name) {
            inner.vars.insert(name.to_string(), value);
            return true;
        }
        match inner.parent.clone() {
            Some(p) => {
                drop(inner);
                p.set(name, value)
            }
            None => false,
        }
    }
}

impl Default for Env {
    fn default() -> Self {
        Env::new()
    }
}
