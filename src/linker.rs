//! Assembling and linking with external tools (Phase 6).
//!
//! Will locate a Windows-targeting GNU assembler/linker (MinGW-w64, e.g.
//! `x86_64-w64-mingw32-as` / `x86_64-w64-mingw32-gcc`), print installation
//! hints if they are missing, and produce a `.exe`.
