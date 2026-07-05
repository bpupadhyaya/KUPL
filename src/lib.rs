//! KUPL — K Universal Programming Language.
//!
//! AI-first, component-oriented. This crate is the v0.1 toolchain:
//! lexer → parser → type/effect checker → tree-walking interpreter with a
//! deterministic component runtime, plus a REPL and an example-test runner.

pub mod ai;
pub mod json;
pub mod regex;
pub mod time;
pub mod encoding;
pub mod csv;
pub mod url;
pub mod manifest;
pub mod ast;
pub mod bytecode;
pub mod cgen;
pub mod check;
pub mod compile;
pub mod diag;
pub mod bigint;
pub mod rational;
pub mod callargs;
pub mod effects;
pub mod fmt;
pub mod vm;
pub mod interp;
pub mod parallel;
pub mod kx;
pub mod lexer;
pub mod loader;
pub mod resolve;
pub mod lsp;
pub mod parser;
pub mod prop;
pub mod repl;
pub mod run;
pub mod sdiff;
pub mod token;
pub mod types;
pub mod value;
