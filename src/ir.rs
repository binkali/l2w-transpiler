//! Intermediate representation (Phase 2).
//!
//! Will separate architecture-level information (instructions, registers,
//! operands) from OS-level information (sections, external symbols,
//! function calls, system calls) so that the Linux-specific parts of a
//! program can be identified and rewritten independently.
