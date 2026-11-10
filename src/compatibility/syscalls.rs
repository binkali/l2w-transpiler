//! Linux system call -> Windows API mapping and runtime shims (Phase 5).
//!
//! Planned: read(0) -> ReadFile, write(1) -> WriteFile, open(2) -> CreateFileA,
//! close(3) -> CloseHandle, getpid(39) -> GetCurrentProcessId,
//! exit(60) -> ExitProcess, including a file-descriptor-to-HANDLE table.

/// Name of a Linux x86-64 system call number (for diagnostics and reports).
pub fn linux_syscall_name(number: i64) -> Option<&'static str> {
    Some(match number {
        0 => "read",
        1 => "write",
        2 => "open",
        3 => "close",
        4 => "stat",
        5 => "fstat",
        8 => "lseek",
        9 => "mmap",
        11 => "munmap",
        12 => "brk",
        39 => "getpid",
        57 => "fork",
        59 => "execve",
        60 => "exit",
        62 => "kill",
        201 => "time",
        228 => "clock_gettime",
        231 => "exit_group",
        257 => "openat",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn names() {
        assert_eq!(super::linux_syscall_name(1), Some("write"));
        assert_eq!(super::linux_syscall_name(60), Some("exit"));
        assert_eq!(super::linux_syscall_name(9999), None);
    }
}
