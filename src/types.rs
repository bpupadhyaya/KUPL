//! Semantic types and unification.

use std::collections::HashMap;
use std::fmt;

pub use crate::value::IntW;

#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Int,
    /// A fixed-width integer type (`i8`…`u64`).
    IntW(IntW),
    Float,
    Bool,
    Str,
    Unit,
    /// The payload-less message type for ports (`in click: Event`).
    Event,
    List(Box<Ty>),
    Option(Box<Ty>),
    Result(Box<Ty>, Box<Ty>),
    /// A user-declared ADT / record / newtype (monomorphic in v0.1).
    Named(String),
    /// A reference to a component instance.
    Component(String),
    /// An interface type: any component that `fulfills` this contract. Values
    /// are component instances; dispatch is dynamic through the contract's
    /// exposed functions.
    Contract(String),
    Fun(Vec<Ty>, Box<Ty>),
    Range,
    /// Rank-1 f64 tensor (v0; dtype/shape parameters arrive with KIR).
    Tensor,
    Map(Box<Ty>, Box<Ty>),
    Set(Box<Ty>),
    /// Inference variable.
    Var(u32),
}

impl Ty {
    pub fn is_numeric(&self) -> bool {
        matches!(self, Ty::Int | Ty::Float | Ty::IntW(_))
    }
}

#[derive(Default)]
pub struct Unifier {
    subst: Vec<Option<Ty>>,
}

impl Unifier {
    pub fn fresh(&mut self) -> Ty {
        let id = self.subst.len() as u32;
        self.subst.push(None);
        Ty::Var(id)
    }

    /// Follow substitutions one level at a time until a non-var (or unbound var).
    pub fn resolve(&self, ty: &Ty) -> Ty {
        match ty {
            Ty::Var(id) => match &self.subst[*id as usize] {
                Some(t) => self.resolve(&t.clone()),
                None => ty.clone(),
            },
            _ => ty.clone(),
        }
    }

    /// Deep-resolve a type for display / storage.
    pub fn apply(&self, ty: &Ty) -> Ty {
        let t = self.resolve(ty);
        match t {
            Ty::List(e) => Ty::List(Box::new(self.apply(&e))),
            Ty::Set(e) => Ty::Set(Box::new(self.apply(&e))),
            Ty::Map(k, v) => Ty::Map(Box::new(self.apply(&k)), Box::new(self.apply(&v))),
            Ty::Option(e) => Ty::Option(Box::new(self.apply(&e))),
            Ty::Result(a, b) => Ty::Result(Box::new(self.apply(&a)), Box::new(self.apply(&b))),
            Ty::Fun(ps, r) => Ty::Fun(
                ps.iter().map(|p| self.apply(p)).collect(),
                Box::new(self.apply(&r)),
            ),
            other => other,
        }
    }

    fn occurs(&self, id: u32, ty: &Ty) -> bool {
        match self.resolve(ty) {
            Ty::Var(other) => other == id,
            Ty::List(e) | Ty::Option(e) | Ty::Set(e) => self.occurs(id, &e),
            Ty::Map(k, v) => self.occurs(id, &k) || self.occurs(id, &v),
            Ty::Result(a, b) => self.occurs(id, &a) || self.occurs(id, &b),
            Ty::Fun(ps, r) => ps.iter().any(|p| self.occurs(id, p)) || self.occurs(id, &r),
            _ => false,
        }
    }

    pub fn unify(&mut self, a: &Ty, b: &Ty) -> Result<(), (Ty, Ty)> {
        let ra = self.resolve(a);
        let rb = self.resolve(b);
        match (&ra, &rb) {
            (Ty::Var(x), Ty::Var(y)) if x == y => Ok(()),
            (Ty::Var(x), _) => {
                if self.occurs(*x, &rb) {
                    return Err((ra.clone(), rb.clone()));
                }
                self.subst[*x as usize] = Some(rb);
                Ok(())
            }
            (_, Ty::Var(y)) => {
                if self.occurs(*y, &ra) {
                    return Err((ra.clone(), rb.clone()));
                }
                self.subst[*y as usize] = Some(ra);
                Ok(())
            }
            // sized ints unify only with the *same* width — differing widths
            // are a type error (mixed-width arithmetic needs explicit conversion)
            (Ty::IntW(x), Ty::IntW(y)) if x == y => Ok(()),
            (Ty::Int, Ty::Int)
            | (Ty::Float, Ty::Float)
            | (Ty::Bool, Ty::Bool)
            | (Ty::Str, Ty::Str)
            | (Ty::Unit, Ty::Unit)
            | (Ty::Event, Ty::Event)
            | (Ty::Tensor, Ty::Tensor)
            | (Ty::Range, Ty::Range) => Ok(()),
            (Ty::Named(x), Ty::Named(y)) if x == y => Ok(()),
            (Ty::Component(x), Ty::Component(y)) if x == y => Ok(()),
            (Ty::Contract(x), Ty::Contract(y)) if x == y => Ok(()),
            (Ty::List(x), Ty::List(y)) => self.unify(&x.clone(), &y.clone()),
            (Ty::Set(x), Ty::Set(y)) => self.unify(&x.clone(), &y.clone()),
            (Ty::Map(xk, xv), Ty::Map(yk, yv)) => {
                self.unify(&xk.clone(), &yk.clone())?;
                self.unify(&xv.clone(), &yv.clone())
            }
            (Ty::Option(x), Ty::Option(y)) => self.unify(&x.clone(), &y.clone()),
            (Ty::Result(xa, xb), Ty::Result(ya, yb)) => {
                self.unify(&xa.clone(), &ya.clone())?;
                self.unify(&xb.clone(), &yb.clone())
            }
            (Ty::Fun(xp, xr), Ty::Fun(yp, yr)) => {
                if xp.len() != yp.len() {
                    return Err((ra.clone(), rb.clone()));
                }
                for (x, y) in xp.clone().iter().zip(yp.clone().iter()) {
                    self.unify(x, y)?;
                }
                self.unify(&xr.clone(), &yr.clone())
            }
            _ => Err((ra, rb)),
        }
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Int => write!(f, "Int"),
            Ty::IntW(w) => write!(f, "{}", w.name()),
            Ty::Float => write!(f, "Float"),
            Ty::Bool => write!(f, "Bool"),
            Ty::Str => write!(f, "Str"),
            Ty::Unit => write!(f, "Unit"),
            Ty::Event => write!(f, "Event"),
            Ty::List(e) => write!(f, "List[{e}]"),
            Ty::Option(e) => write!(f, "Option[{e}]"),
            Ty::Result(a, b) => write!(f, "Result[{a}, {b}]"),
            Ty::Named(n) => write!(f, "{n}"),
            Ty::Component(n) => write!(f, "{n}"),
            Ty::Contract(n) => write!(f, "{n}"),
            Ty::Fun(ps, r) => {
                write!(f, "fn(")?;
                for (i, p) in ps.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{p}")?;
                }
                write!(f, ") -> {r}")
            }
            Ty::Range => write!(f, "Range"),
            Ty::Tensor => write!(f, "Tensor"),
            Ty::Map(k, v) => write!(f, "Map[{k}, {v}]"),
            Ty::Set(e) => write!(f, "Set[{e}]"),
            Ty::Var(id) => write!(f, "?{id}"),
        }
    }
}

/// A variant of a user-declared type.
#[derive(Debug, Clone)]
pub struct VariantSig {
    pub name: String,
    pub fields: Vec<(String, Ty)>,
}

/// Signature of a user-declared type.
#[derive(Debug, Clone)]
pub struct TypeSig {
    pub name: String,
    pub variants: Vec<VariantSig>,
    /// True when declared as a record / newtype (single variant named like the type).
    pub is_record: bool,
}

/// Signature of a component's interface.
#[derive(Debug, Clone, Default)]
pub struct ComponentSig {
    pub in_ports: HashMap<String, Ty>,
    pub out_ports: HashMap<String, Ty>,
    pub props: Vec<(String, Ty, bool)>, // name, ty, has_default
    pub exposes: HashMap<String, (Vec<Ty>, Ty)>,
    /// Contracts this component `fulfills` — used for contract-type assignability.
    pub fulfills: Vec<String>,
}

/// Signature of a contract: exposed function signatures with effects.
#[derive(Debug, Clone, Default)]
pub struct ContractSig {
    pub sigs: HashMap<String, (Vec<Ty>, Ty, Vec<String>)>,
}
