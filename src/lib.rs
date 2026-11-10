//! `l2w` — a Linux-to-Windows x86-64 assembly transpiler.
//!
//! Pipeline:
//!
//! ```text
//! source --lexer--> tokens --parser--> AST --(ir)--> IR --transpiler--> Windows IR
//!        --codegen::windows--> GAS/COFF assembly --linker--> .exe (MinGW-w64)
//! ```
//!
//! Phase 1 implements the lexer, parser, AST, diagnostics and CLI. The
//! remaining modules are placeholders filled in by later phases.

pub mod ast;
pub mod cli;
pub mod codegen;
pub mod compatibility;
pub mod diagnostics;
pub mod ir;
pub mod lexer;
pub mod linker;
pub mod parser;
pub mod transpiler;
