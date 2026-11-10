//! Linux system call -> Windows API mapping and runtime shims (Phase 5).
//!
//! Planned: read(0) -> ReadFile, write(1) -> WriteFile, open(2) -> CreateFileA,
//! close(3) -> CloseHandle, getpid(39) -> GetCurrentProcessId,
//! exit(60) -> ExitProcess, including a file-descriptor-to-HANDLE table.
