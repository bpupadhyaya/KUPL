//! KUPL — K Universal Programming Language.
//!
//! AI-first, component-oriented. This crate is the v0.1 toolchain:
//! lexer → parser → type/effect checker → tree-walking interpreter with a
//! deterministic component runtime, plus a REPL and an example-test runner.

pub mod ast;
pub mod check;
pub mod diag;
pub mod interp;
pub mod lexer;
pub mod parser;
pub mod repl;
pub mod run;
pub mod token;
pub mod types;
pub mod value;
