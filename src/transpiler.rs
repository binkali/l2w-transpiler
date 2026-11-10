//! Linux -> Windows translation passes over the IR (Phases 3-5).
//!
//! Will translate instructions and directives, apply the calling-convention
//! compatibility layer and replace Linux system calls with Windows API calls.
