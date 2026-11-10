//! `l2w` — a Linux-to-Windows x86-64 assembly transpiler.
//!
//! Pipeline:
//!
//! ```text
//! source --lexer--> tokens --parser--> AST --(ir)--> IR --transpiler--> Windows IR
//!        --codegen::windows--> GAS/COFF assembly --linker--> .exe (MinGW-w64)
//! ```
//!
//! See README.md for the phase-by-phase design.

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
