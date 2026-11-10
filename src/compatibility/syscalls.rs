//! Linux system call -> Windows API mapping and runtime shims.
//!
//! Windows has no stable system call interface, so every `syscall` is
//! replaced by a call to a small *shim* that implements the Linux system
//! call on top of the Win32 API:
//!
//! | Linux | number | Win32 |
//! |---|---|---|
//! | `read(fd, buf, count)` | 0 | `ReadFile` |
//! | `write(fd, buf, count)` | 1 | `WriteFile` (+ `SetFilePointerEx` for `O_APPEND`) |
//! | `open(path, flags, mode)` | 2 | `CreateFileA` |
//! | `close(fd)` | 3 | `CloseHandle` |
//! | `getpid()` | 39 | `GetCurrentProcessId` |
//! | `exit(status)` / `exit_group(status)` | 60 / 231 | `ExitProcess(status & 0xff)` |
//!
//! # Shim contract
//!
//! A shim behaves exactly like the Linux `syscall` instruction as far as
//! the program can observe it:
//!
//! * arguments in rdi, rsi, rdx (r10, r8, r9 unused by the supported calls);
//! * result in rax: a non-negative value or `-errno`;
//! * **all** other registers and RFLAGS are preserved (Linux clobbers only
//!   rcx and r11, which the shims clobber too);
//! * the stack below rsp is not touched: the call site skips the System V
//!   red zone (`lea rsp, [rsp - 128]` / `lea rsp, [rsp + 128]`, which keep
//!   the flags), and the shim aligns the stack itself.
//!
//! # File descriptors
//!
//! A table maps Linux file descriptors (0..[`FD_LIMIT`]) to Windows
//! `HANDLE`s plus flags (readable, writable, append, standard stream).
//! fds 0, 1, 2 refer to `GetStdHandle(STD_INPUT/OUTPUT/ERROR_HANDLE)`,
//! fetched on first use. `open` returns the lowest free descriptor, like
//! Linux. Reading a write-only or writing a read-only descriptor fails with
//! `EBADF`, like Linux. Closing a standard descriptor only frees the slot:
//! on Windows stdout and stderr are often the same console handle.
//!
//! # Errors
//!
//! Win32 failures are converted from `GetLastError()` to `-errno` via
//! [`ERRNO_MAP`]; unmapped codes become `-EIO`. A broken pipe makes `read`
//! return 0 (end of file) and `write` return `-EPIPE`.

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

/// A translated system call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shim {
    /// Shim key (`exit_group` shares the `exit` shim).
    pub key: &'static str,
    /// Win32 functions it uses (for reports).
    pub win32: &'static str,
}

/// The shim for a Linux system call number, if it is supported.
pub fn shim_for(number: i64) -> Option<Shim> {
    let (key, win32) = match number {
        0 => ("read", "ReadFile"),
        1 => ("write", "WriteFile"),
        2 => ("open", "CreateFileA"),
        3 => ("close", "CloseHandle"),
        39 => ("getpid", "GetCurrentProcessId"),
        60 | 231 => ("exit", "ExitProcess"),
        _ => return None,
    };
    Some(Shim { key, win32 })
}

/// Supported system calls, for help messages.
pub const SUPPORTED: &str =
    "read (0), write (1), open (2), close (3), getpid (39), exit (60), exit_group (231)";

/// Symbol name of a shim.
pub fn shim_name(key: &str) -> String {
    format!("__l2w_sys_{key}")
}

/// Bytes skipped below rsp at a call site (System V red zone).
pub const RED_ZONE: u32 = 128;

/// Instructions replacing a `syscall` instruction.
pub fn call_site(key: &str) -> String {
    format!(
        "lea rsp, [rsp - {RED_ZONE}]\ncall {}\nlea rsp, [rsp + {RED_ZONE}]\n",
        shim_name(key)
    )
}

/// Number of Linux file descriptors the runtime supports.
pub const FD_LIMIT: u32 = 256;

/// `open` flags the runtime implements: O_ACCMODE, O_CREAT, O_EXCL,
/// O_NOCTTY, O_TRUNC, O_APPEND, O_LARGEFILE and O_CLOEXEC (the last three
/// have no effect without `exec`/32-bit offsets/terminals).
pub const OPEN_SUPPORTED_FLAGS: i64 =
    0o3 | 0o100 | 0o200 | 0o400 | 0o1000 | 0o2000 | 0o100000 | 0o2000000;

pub const O_ACCMODE: i64 = 3;

/// Win32 error code -> Linux errno.
pub const ERRNO_MAP: &[(u32, u32, &str)] = &[
    (2, 2, "ERROR_FILE_NOT_FOUND -> ENOENT"),
    (3, 2, "ERROR_PATH_NOT_FOUND -> ENOENT"),
    (4, 24, "ERROR_TOO_MANY_OPEN_FILES -> EMFILE"),
    (5, 13, "ERROR_ACCESS_DENIED -> EACCES"),
    (6, 9, "ERROR_INVALID_HANDLE -> EBADF"),
    (80, 17, "ERROR_FILE_EXISTS -> EEXIST"),
    (183, 17, "ERROR_ALREADY_EXISTS -> EEXIST"),
    (87, 22, "ERROR_INVALID_PARAMETER -> EINVAL"),
    (123, 22, "ERROR_INVALID_NAME -> EINVAL"),
    (109, 32, "ERROR_BROKEN_PIPE -> EPIPE"),
    (232, 32, "ERROR_NO_DATA -> EPIPE"),
    (112, 28, "ERROR_DISK_FULL -> ENOSPC"),
    (206, 36, "ERROR_FILENAME_EXCED_RANGE -> ENAMETOOLONG"),
    (998, 14, "ERROR_NOACCESS -> EFAULT"),
];
const EIO: u32 = 5;
const EBADF: u32 = 9;
const EMFILE: u32 = 24;
const EINVAL: u32 = 22;
const EPIPE: u32 = 32;

// fd flag bits
const F_READ: u32 = 1;
const F_WRITE: u32 = 2;
const F_APPEND: u32 = 4;
const F_LAZY: u32 = 8;
const F_STD: u32 = 0x10;
const F_OPEN: u32 = 0x80;

/// Linux enforces this limit on read/write sizes as well.
const MAX_RW: u32 = 0x7fff_f000;

/// Saves everything a Win32 call may clobber that `syscall` preserves
/// (rdx, r8, r9, r10; rbx is used as scratch), aligns the stack and
/// reserves a 64-byte call frame: shadow space `[rsp]..[rsp + 32]`,
/// stack arguments 5..7 at `[rsp + 32]`, `[rsp + 40]`, `[rsp + 48]` and a
/// local at `[rsp + 56]`.
fn prologue(name: &str) -> String {
    format!(
        "{name}:\n    pushfq\n    push rbp\n    mov rbp, rsp\n    push rbx\n    push rdx\n    push r8\n    push r9\n    push r10\n    \
         and rsp, -16\n    sub rsp, 64\n"
    )
}

fn epilogue(label: &str) -> String {
    format!(
        "{label}:\n    lea rsp, [rbp - 40]\n    pop r10\n    pop r9\n    pop r8\n    pop rdx\n    pop rbx\n    pop rbp\n    popfq\n    ret\n"
    )
}

fn errno_routine() -> String {
    let mut s = String::from(
        "__l2w_errno:\n    sub rsp, 40\n    call GetLastError\n    add rsp, 40\n    mov ecx, eax\n",
    );
    let mut last = None;
    for &(win, errno, _) in ERRNO_MAP {
        if last != Some(errno) {
            s += &format!("    mov rax, -{errno}\n");
            last = Some(errno);
        }
        s += &format!("    cmp ecx, {win}\n    je .Ll2w_errno_done\n");
    }
    s += &format!("    mov rax, -{EIO}\n.Ll2w_errno_done:\n    ret\n");
    s
}

/// `rcx` = fd, `edx` = required flag bits -> `rax` = handle (0 if the fd
/// is not open with these flags), `edx` = flags. Preserves rcx, rbx, rsi,
/// rdi; clobbers rax, rdx, r8–r11.
fn lookup_routine() -> String {
    format!(
        "__l2w_fd_lookup:
    cmp rcx, {FD_LIMIT}
    jae .Ll2w_lookup_bad
    lea r8, [rip + __l2w_fd_flags]
    movzx eax, byte ptr [r8 + rcx]
    mov r9d, eax
    and r9d, edx
    cmp r9d, edx
    jne .Ll2w_lookup_bad
    test al, {F_LAZY}
    jz .Ll2w_lookup_have
    push rcx
    sub rsp, 32
    mov edx, -10
    sub edx, ecx
    mov ecx, edx
    call GetStdHandle
    add rsp, 32
    pop rcx
    lea r8, [rip + __l2w_fd_handles]
    mov qword ptr [r8 + rcx*8], rax
    lea r8, [rip + __l2w_fd_flags]
    and byte ptr [r8 + rcx], {not_lazy}
    test rax, rax
    jz .Ll2w_lookup_closed
    cmp rax, -1
    jne .Ll2w_lookup_have
.Ll2w_lookup_closed:
    mov byte ptr [r8 + rcx], 0
    jmp .Ll2w_lookup_bad
.Ll2w_lookup_have:
    lea r8, [rip + __l2w_fd_flags]
    movzx edx, byte ptr [r8 + rcx]
    lea r8, [rip + __l2w_fd_handles]
    mov rax, qword ptr [r8 + rcx*8]
    ret
.Ll2w_lookup_bad:
    xor eax, eax
    ret
",
        not_lazy = !(F_LAZY as i8)
    )
}

fn read_write(key: &str) -> String {
    let (need, api, append) = match key {
        "read" => (F_OPEN | F_READ, "ReadFile", false),
        _ => (F_OPEN | F_WRITE, "WriteFile", true),
    };
    let l = |s: &str| format!(".Ll2w_{key}_{s}");
    let mut s = prologue(&shim_name(key));
    s += &format!(
        "    mov rbx, rdx\n    movsxd rcx, edi\n    mov edx, {need}\n    call __l2w_fd_lookup\n    test rax, rax\n    jz {badf}\n",
        badf = l("badf")
    );
    if append {
        s += &format!(
            "    test dl, {F_APPEND}\n    jz {go}\n    mov qword ptr [rsp + 56], rax\n    mov rcx, rax\n    xor edx, edx\n    \
             xor r8d, r8d\n    mov r9d, 2\n    call SetFilePointerEx\n    mov rax, qword ptr [rsp + 56]\n{go}:\n",
            go = l("go")
        );
    }
    s += &format!(
        "    mov rcx, rax\n    mov rdx, rsi\n    mov r8d, {MAX_RW}\n    cmp rbx, r8\n    jae {cap}\n    mov r8, rbx\n{cap}:\n    \
         mov qword ptr [rsp + 56], 0\n    lea r9, [rsp + 56]\n    mov qword ptr [rsp + 32], 0\n    call {api}\n    test eax, eax\n    \
         jz {err}\n    mov eax, dword ptr [rsp + 56]\n    jmp {done}\n{err}:\n    call __l2w_errno\n",
        cap = l("cap"),
        err = l("err"),
        done = l("done"),
    );
    if key == "read" {
        // Broken pipe: the writer is gone, which is end-of-file on Linux.
        s += &format!(
            "    cmp rax, -{EPIPE}\n    jne {done}\n    xor eax, eax\n",
            done = l("done")
        );
    }
    s += &format!(
        "    jmp {}\n{}:\n    mov rax, -{EBADF}\n",
        l("done"),
        l("badf")
    );
    s + &epilogue(&l("done"))
}

fn open() -> String {
    let unsupported = !OPEN_SUPPORTED_FLAGS as i32;
    let mut s = prologue(&shim_name("open"));
    s += &format!(
        "    mov r10, rdx
    test esi, {unsupported}
    jnz .Ll2w_open_inval
    mov eax, esi
    and eax, {O_ACCMODE}
    cmp eax, {O_ACCMODE}
    je .Ll2w_open_inval
    lea r8, [rip + __l2w_fd_flags]
    xor ebx, ebx
.Ll2w_open_scan:
    cmp byte ptr [r8 + rbx], 0
    je .Ll2w_open_found
    inc ebx
    cmp ebx, {FD_LIMIT}
    jb .Ll2w_open_scan
    mov rax, -{EMFILE}
    jmp .Ll2w_open_done
.Ll2w_open_found:
    mov edx, {generic_read}
    cmp eax, 1
    jne .Ll2w_open_acc1
    mov edx, {generic_write}
.Ll2w_open_acc1:
    cmp eax, 2
    jne .Ll2w_open_acc2
    mov edx, {generic_rw}
.Ll2w_open_acc2:
    mov r9d, 3
    test esi, 0x200
    jz .Ll2w_open_d1
    mov r9d, 5
.Ll2w_open_d1:
    test esi, 0x40
    jz .Ll2w_open_d2
    mov r9d, 4
    test esi, 0x200
    jz .Ll2w_open_d3
    mov r9d, 2
.Ll2w_open_d3:
    test esi, 0x80
    jz .Ll2w_open_d2
    mov r9d, 1
.Ll2w_open_d2:
    mov qword ptr [rsp + 32], r9
    mov r9d, 0x80
    test esi, 0x40
    jz .Ll2w_open_attr
    test r10d, 0x80
    jnz .Ll2w_open_attr
    mov r9d, 1
.Ll2w_open_attr:
    mov qword ptr [rsp + 40], r9
    mov qword ptr [rsp + 48], 0
    mov rcx, rdi
    mov r8d, 7
    xor r9d, r9d
    call CreateFileA
    cmp rax, -1
    je .Ll2w_open_err
    lea r8, [rip + __l2w_fd_handles]
    mov qword ptr [r8 + rbx*8], rax
    mov eax, esi
    and eax, {O_ACCMODE}
    inc eax
    or eax, {F_OPEN}
    test esi, 0x400
    jz .Ll2w_open_store
    or eax, {F_APPEND}
.Ll2w_open_store:
    lea r8, [rip + __l2w_fd_flags]
    mov byte ptr [r8 + rbx], al
    mov eax, ebx
    jmp .Ll2w_open_done
.Ll2w_open_err:
    call __l2w_errno
    jmp .Ll2w_open_done
.Ll2w_open_inval:
    mov rax, -{EINVAL}
",
        generic_read = 0x8000_0000u32 as i32,
        generic_write = 0x4000_0000,
        generic_rw = 0xC000_0000u32 as i32,
    );
    s + &epilogue(".Ll2w_open_done")
}

fn close() -> String {
    let mut s = prologue(&shim_name("close"));
    s += &format!(
        "    movsxd rcx, edi
    mov edx, {F_OPEN}
    call __l2w_fd_lookup
    test rax, rax
    jz .Ll2w_close_badf
    lea r8, [rip + __l2w_fd_flags]
    mov byte ptr [r8 + rcx], 0
    test dl, {F_STD}
    jnz .Ll2w_close_ok
    mov rcx, rax
    call CloseHandle
    test eax, eax
    jnz .Ll2w_close_ok
    call __l2w_errno
    jmp .Ll2w_close_done
.Ll2w_close_ok:
    xor eax, eax
    jmp .Ll2w_close_done
.Ll2w_close_badf:
    mov rax, -{EBADF}
"
    );
    s + &epilogue(".Ll2w_close_done")
}

fn getpid() -> String {
    prologue(&shim_name("getpid"))
        + "    call GetCurrentProcessId\n    mov eax, eax\n"
        + &epilogue(".Ll2w_getpid_done")
}

/// Linux reports `status & 0xff` to the parent.
fn exit() -> String {
    format!(
        "{}:\n    and rsp, -16\n    sub rsp, 32\n    movzx ecx, dil\n    call ExitProcess\n",
        shim_name("exit")
    )
}

/// Win32 functions the runtime may call (they must not be defined by the
/// program itself).
pub const WIN32_IMPORTS: &[&str] = &[
    "GetStdHandle",
    "GetLastError",
    "ReadFile",
    "WriteFile",
    "SetFilePointerEx",
    "CreateFileA",
    "CloseHandle",
    "GetCurrentProcessId",
    "ExitProcess",
];

/// The runtime functions needed for the given shims, as `(name, source)`.
pub fn runtime_code(keys: &[&str]) -> Vec<(String, String)> {
    let uses_fds = keys
        .iter()
        .any(|k| matches!(*k, "read" | "write" | "open" | "close"));
    let mut out: Vec<(String, String)> = keys
        .iter()
        .map(|&k| {
            let src = match k {
                "read" | "write" => read_write(k),
                "open" => open(),
                "close" => close(),
                "getpid" => getpid(),
                "exit" => exit(),
                other => unreachable!("no shim for {other}"),
            };
            (shim_name(k), src)
        })
        .collect();
    if uses_fds {
        if keys.iter().any(|k| *k != "open") {
            out.push(("__l2w_fd_lookup".into(), lookup_routine()));
        }
        out.push(("__l2w_errno".into(), errno_routine()));
    }
    out
}

/// The file-descriptor table (`.data`), if any shim needs it.
pub fn runtime_data(keys: &[&str]) -> Option<String> {
    let uses_fds = keys
        .iter()
        .any(|k| matches!(*k, "read" | "write" | "open" | "close"));
    let std = F_OPEN | F_STD | F_LAZY | F_READ | F_WRITE;
    uses_fds.then(|| {
        format!(
            ".balign 8\n__l2w_fd_handles:\n.zero {}\n__l2w_fd_flags:\n.byte {std}, {std}, {std}\n.zero {}\n",
            8 * FD_LIMIT,
            FD_LIMIT - 3
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(linux_syscall_name(1), Some("write"));
        assert_eq!(linux_syscall_name(60), Some("exit"));
        assert_eq!(linux_syscall_name(9999), None);
        assert_eq!(shim_for(231).unwrap().key, "exit");
        assert!(shim_for(9).is_none());
    }

    #[test]
    fn runtime_snippets_are_valid_assembly() {
        let keys = ["read", "write", "open", "close", "getpid", "exit"];
        for (name, src) in runtime_code(&keys) {
            let p = crate::parser::parse(&src);
            assert!(!p.has_errors(), "{name}:\n{src}\n{:#?}", p.diagnostics);
            let l = crate::ir::lower(&p.program);
            assert!(!l.has_errors(), "{name}:\n{src}\n{:#?}", l.diagnostics);
        }
        let data = runtime_data(&keys).unwrap();
        assert!(data.contains(".byte 155, 155, 155\n.zero 253"), "{data}");
        assert!(runtime_data(&["exit", "getpid"]).is_none());
    }

    #[test]
    fn shims_preserve_registers() {
        let w = read_write("write");
        for r in [
            "pushfq", "push rdx", "push r8", "push r9", "push r10", "push rbx",
        ] {
            assert!(w.contains(r), "{r}");
        }
        assert!(w.ends_with("pop r10\n    pop r9\n    pop r8\n    pop rdx\n    pop rbx\n    pop rbp\n    popfq\n    ret\n"));
        assert!(w.contains("call SetFilePointerEx"));
        assert!(!read_write("read").contains("SetFilePointerEx"));
    }
}
