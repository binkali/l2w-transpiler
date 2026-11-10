//! Windows code generation tests with real toolchains (skipped when the
//! tools are not installed):
//!
//! * MinGW-w64 `as` must accept every translation, and the COFF objects
//!   must not contain 32-bit absolute relocations.
//! * Behavioral equivalence: phase 3 does not change the calling
//!   convention, so translated code can be reassembled for Linux (only the
//!   COFF section directives are mapped back) and called from a C harness.
//!   Its output must equal that of the original code.

use std::path::{Path, PathBuf};
use std::process::Command;

const MINGW_AS: &str = "x86_64-w64-mingw32-as";
const MINGW_OBJDUMP: &str = "x86_64-w64-mingw32-objdump";

fn available(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn dir() -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join("windows");
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn translate(src: &str, name: &str) -> String {
    let p = l2w::parser::parse(src);
    assert!(!p.has_errors(), "{name}: {:#?}", p.diagnostics);
    let l = l2w::ir::lower(&p.program);
    assert!(!l.has_errors(), "{name}: {:#?}", l.diagnostics);
    let t = l2w::transpiler::transpile(&l.module);
    assert!(!t.has_errors(), "{name}: {:#?}", t.diagnostics);
    l2w::codegen::windows::emit(&t, name)
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
    assert!(
        out.status.success(),
        "{cmd:?} failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

const LIBRARY: &str = include_str!("../examples/library.s");

const EXTRA: &str = r#"
.intel_syntax noprefix
.section .rodata.cst8, "aM", @progbits, 8
k8: .quad 0x1122334455667788
.data
arr: .long 10, 20, 30, 40
p:   .quad arr + 4
rel: .long arr - .
.text
.globl extra
extra:
    mov eax, dword ptr arr
    add eax, dword ptr [arr + 12]
    mov rcx, offset arr
    add eax, dword ptr [rcx + 4]
    mov rdx, qword ptr [rip + p]
    add eax, dword ptr [rdx]
    mov rdx, qword ptr [rip + arr@GOTPCREL]
    add eax, dword ptr [rdx + 8]
    mov rcx, k8
    shr rcx, 56
    add eax, ecx
    call helper@PLT
    add eax, edx
    ret
helper:
    mov edx, 1000
    ret
"#;

/// Calling-convention boundaries: exported functions (called from Windows
/// code), calls and tail calls to external (Windows) functions with ten
/// arguments, rsi/rdi clobbering, and gcc -O0 style red-zone use.
const BRIDGE: &str = r#"
.intel_syntax noprefix
.text
.globl sum10, call_ext10, clobber, rz_add, tail_twice
.type rz_add, @function

# long sum10(long a1, ..., long a10)
sum10:
    lea rax, [rdi + rsi]
    add rax, rdx
    add rax, rcx
    add rax, r8
    add rax, r9
    add rax, qword ptr [rsp + 8]
    add rax, qword ptr [rsp + 16]
    add rax, qword ptr [rsp + 24]
    add rax, qword ptr [rsp + 32]
    ret

# long call_ext10(void) = ext_sum10(1, 2, 4, ..., 512) + 7
call_ext10:
    push rbx
    mov rbx, 7                  # callee-saved: must survive the external call
    sub rsp, 32
    mov qword ptr [rsp], 64
    mov qword ptr [rsp + 8], 128
    mov qword ptr [rsp + 16], 256
    mov qword ptr [rsp + 24], 512
    mov edi, 1
    mov esi, 2
    mov edx, 4
    mov ecx, 8
    mov r8d, 16
    mov r9d, 32
    call ext_sum10
    add rsp, 32
    add rax, rbx
    pop rbx
    ret

# System V allows clobbering rsi/rdi; Windows callers expect them preserved.
clobber:
    mov rsi, -1
    mov rdi, -1
    mov eax, 7
    ret

# gcc -O0: int rz_add(int a, int b) { int c = a + b; return c; }  (red zone)
rz_add:
    push rbp
    mov rbp, rsp
    mov dword ptr [rbp - 20], edi
    mov dword ptr [rbp - 24], esi
    mov edx, dword ptr [rbp - 20]
    mov eax, dword ptr [rbp - 24]
    add eax, edx
    mov dword ptr [rbp - 4], eax
    mov eax, dword ptr [rbp - 4]
    pop rbp
    ret

# long tail_twice(long x) = ext_twice(x + 1)   (tail call into Windows code)
tail_twice:
    add rdi, 1
    jmp ext_twice
"#;

/// Windows-convention implementation of `ext_sum10` used by the translated
/// build: it scribbles over its shadow space (allowed for a Windows callee;
/// crashes if the caller did not reserve it) and checks stack alignment.
const WIN_EXT: &str = r#"
.intel_syntax noprefix
.text
.globl ext_sum10
ext_sum10:
    mov qword ptr [rsp + 8], -1
    mov qword ptr [rsp + 16], -1
    mov qword ptr [rsp + 24], -1
    mov qword ptr [rsp + 32], -1
    mov r10, -1
    mov r11, -1
    lea rax, [rcx + rdx]
    add rax, r8
    add rax, r9
    add rax, qword ptr [rsp + 40]
    add rax, qword ptr [rsp + 48]
    add rax, qword ptr [rsp + 56]
    add rax, qword ptr [rsp + 64]
    add rax, qword ptr [rsp + 72]
    add rax, qword ptr [rsp + 80]
    test rsp, 15
    jnz 1f
    add rax, 1000000            # entry rsp must be 8 mod 16
1:  ret
.section .note.GNU-stack,"",@progbits
"#;

/// Calls `fn` with the Windows convention and reports whether rsi/rdi
/// survived (bit 0: rsi clobbered, bit 1: rdi clobbered).
const PROBE: &str = r#"
.intel_syntax noprefix
.text
.globl probe_preserve
probe_preserve:
    push rbx
    push rsi
    push rdi
    push r12
    mov r12, rdi
    mov rsi, 0x1111
    mov rdi, 0x2222
    sub rsp, 40
    call r12
    add rsp, 40
    xor eax, eax
    cmp rsi, 0x1111
    je 1f
    or eax, 1
1:  cmp rdi, 0x2222
    je 2f
    or eax, 2
2:  pop r12
    pop rdi
    pop rsi
    pop rbx
    ret
.section .note.GNU-stack,"",@progbits
"#;

#[test]
fn mingw_assembles_translations_without_absolute_32bit_relocations() {
    if !available(MINGW_AS) || !available(MINGW_OBJDUMP) {
        eprintln!(
            "skipping: MinGW-w64 binutils not installed (apt install binutils-mingw-w64-x86-64)"
        );
        return;
    }
    for (name, src) in [
        ("library", LIBRARY),
        ("extra", EXTRA),
        ("bridge", BRIDGE),
        ("hello", HELLO),
        ("loop", LOOP),
        ("functions", FUNCTIONS),
        ("syscalls", SYSCALLS),
    ] {
        let asm = translate(src, name);
        let s = dir().join(format!("{name}.win.s"));
        let o = dir().join(format!("{name}.win.o"));
        std::fs::write(&s, &asm).unwrap();
        run(Command::new(MINGW_AS).arg("-o").arg(&o).arg(&s));
        let header = run(Command::new(MINGW_OBJDUMP).arg("-f").arg(&o));
        assert!(
            header.contains("pe-x86-64"),
            "{name}: not a COFF x64 object:\n{header}"
        );
        let relocs = run(Command::new(MINGW_OBJDUMP).arg("-r").arg(&o));
        for bad in [
            "R_X86_64_32",
            "IMAGE_REL_AMD64_ADDR32 ",
            "IMAGE_REL_AMD64_ADDR32\n",
        ] {
            assert!(
                !relocs.contains(bad),
                "{name}: 32-bit absolute relocation `{bad}`:\n{relocs}\n{asm}"
            );
        }
        assert!(
            relocs.contains("IMAGE_REL_AMD64_REL32"),
            "{name}:\n{relocs}"
        );
    }
}

/// Maps the COFF section directives of a translation back to ELF ones so the
/// translated instructions can be executed on the Linux test host.
fn coff_to_elf_sections(asm: &str) -> String {
    asm.lines()
        .filter(|l| !l.starts_with(".def "))
        .map(|l| {
            if let Some(rest) = l.strip_prefix(".section ") {
                let flags = rest.rsplit_once(',').map(|(_, f)| f).unwrap_or("");
                match flags {
                    "\"dr\"" => ".section .rodata".to_string(),
                    "\"dw\"" => ".data".to_string(),
                    "\"bw\"" => ".bss".to_string(),
                    "\"xr\"" => ".text".to_string(),
                    other => panic!("unexpected section flags {other}"),
                }
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n.section .note.GNU-stack,\"\",@progbits\n"
}

const HARNESS: &str = r#"
#include <stdio.h>
#ifdef TRANSLATED
#define ABI __attribute__((ms_abi))   /* translated exports use the Windows convention */
long probe_preserve(void *fn);
#else
#define ABI
long ext_sum10(long a, long b, long c, long d, long e, long f, long g, long h, long i, long j) {
    return a + b + c + d + e + f + g + h + i + j;
}
#endif
long ABI ext_twice(long x) { return 2 * x; }

long ABI fib_at(long); long ABI sum_table(void); long ABI bump(long);
long ABI strlen_l2w(const char *); long ABI max3(long, long, long); int ABI extra(void);
long ABI sum10(long, long, long, long, long, long, long, long, long, long);
long ABI call_ext10(void); long ABI clobber(void); int ABI rz_add(int, int); long ABI tail_twice(long);
extern long counter; extern char shared[128]; extern void *ptrs[2];

int main(void) {
    for (long i = -2; i < 10; i++) printf("%ld ", fib_at(i));
    printf("\n%ld\n", sum_table());
    long a = bump(5); long b = bump(7);
    printf("%ld %ld %ld\n", a, b, counter);
    printf("%ld %ld\n", strlen_l2w("hello, world"), strlen_l2w(""));
    printf("%ld %ld %ld\n", max3(1, 2, 3), max3(9, -2, 3), max3(-5, -7, -6));
    printf("%s\n", (const char *)ptrs[1]);
    shared[127] = 42; printf("%d\n", shared[127]);
    printf("%d\n", extra());
    printf("sum10 %ld\n", sum10(1, 2, 3, 4, 5, 6, 7, 8, 9, 10));
    printf("call_ext10 %ld\n", call_ext10());
    printf("clobber %ld\n", clobber());
    printf("rz_add %d\n", rz_add(40, 2));
    printf("tail_twice %ld\n", tail_twice(20));
#ifdef TRANSLATED
    printf("preserve %ld\n", probe_preserve((void *)clobber));
#endif
    return 0;
}
"#;

#[test]
fn translated_code_behaves_like_the_original() {
    if !available("gcc") {
        eprintln!("skipping: gcc not available");
        return;
    }
    let d = dir();
    let harness = d.join("harness.c");
    std::fs::write(&harness, HARNESS).unwrap();
    let build_and_run = |tag: &str, translated: bool, sources: &[(&str, String)]| -> String {
        let exe = d.join(format!("{tag}-test"));
        let mut cmd = Command::new("gcc");
        // -no-pie: the original Linux code uses 32-bit absolute addresses.
        cmd.args(["-no-pie", "-o"]).arg(&exe).arg(&harness);
        if translated {
            cmd.arg("-DTRANSLATED");
        }
        for (name, src) in sources {
            let path = d.join(format!("{tag}-{name}.s"));
            std::fs::write(&path, src).unwrap();
            cmd.arg(path);
        }
        run(&mut cmd);
        run(&mut Command::new(&exe))
    };
    let original = build_and_run(
        "orig",
        false,
        &[
            ("library", LIBRARY.into()),
            ("extra", EXTRA.into()),
            ("bridge", BRIDGE.into()),
        ],
    );
    let tr = |src: &str, name: &str| coff_to_elf_sections(&translate(src, name));
    let translated = build_and_run(
        "translated",
        true,
        &[
            ("library", tr(LIBRARY, "library")),
            ("extra", tr(EXTRA, "extra")),
            ("bridge", tr(BRIDGE, "bridge")),
            ("win_ext", WIN_EXT.into()),
            ("probe", PROBE.into()),
        ],
    );
    // rsi/rdi must survive a Windows-convention call into translated code.
    assert!(translated.contains("preserve 0\n"), "{translated}");
    let translated: String = translated
        .lines()
        .filter(|l| !l.starts_with("preserve"))
        .map(|l| format!("{l}\n"))
        .collect();
    assert_eq!(original, translated);
    for expected in [
        "-1 -1 1 1 2 3 5 8 13 21 -1 -1",
        "5 12 12",
        "Hello from l2w",
        "sum10 55\n",
        "call_ext10 1030\n",
        "clobber 7\n",
        "rz_add 42\n",
        "tail_twice 42\n",
    ] {
        assert!(
            original.contains(expected),
            "missing {expected:?}:\n{original}"
        );
    }
}

// ---------------------------------------------------------------------------
// Phase 5: system calls. The translated programs call the Win32 API; on Linux
// they are linked against WIN32_MOCK, an ms_abi implementation of the used
// kernel32 functions on top of POSIX. The mock checks the Windows calling
// convention at every call (16-byte alignment), writes its arguments into
// the caller's shadow space (gcc -O0) and clobbers all volatile registers.

const HELLO: &str = include_str!("../examples/hello.s");
const LOOP: &str = include_str!("../examples/loop.s");
const FUNCTIONS: &str = include_str!("../examples/functions.s");

/// File I/O semantics: lowest-free fd allocation, O_TRUNC/O_APPEND/O_EXCL,
/// -errno results (EBADF for wrong access mode or closed fds, EEXIST,
/// ENOENT), register and flag preservation across `syscall`, and
/// `exit` status truncation to 8 bits.
const SYSCALLS: &str = r#"
.intel_syntax noprefix
.section .rodata
path:    .asciz "l2w-sys.txt"
missing: .asciz "l2w-missing/none.txt"
first:   .ascii "abc\n"
second:  .ascii "def\n"
.bss
buf:     .zero 64
res:     .zero 16
.text
.globl _start
_start:
    mov eax, 2                  # open(path, O_WRONLY|O_CREAT|O_TRUNC, 0644)
    lea rdi, [rip + path]
    mov esi, 0x241
    mov edx, 0x1a4
    syscall
    mov r12, rax                # 3: lowest free fd
    mov eax, 1                  # write(fd, "abc\n", 4)
    mov rdi, r12
    lea rsi, [rip + first]
    mov edx, 4
    syscall
    mov r13, rax                # 4
    xor eax, eax                # read on a write-only fd
    mov rdi, r12
    lea rsi, [rip + buf]
    mov edx, 64
    syscall
    mov r14, rax                # -EBADF
    mov eax, 3                  # close(fd)
    mov rdi, r12
    syscall
    add r14, rax                # + 0
    mov eax, 1                  # write to the closed fd
    mov rdi, r12
    lea rsi, [rip + first]
    mov edx, 4
    syscall
    mov r15, rax                # -EBADF
    mov eax, 2                  # open(path, O_WRONLY|O_APPEND)
    lea rdi, [rip + path]
    mov esi, 0x401
    syscall
    mov rbx, rax
    mov eax, 1
    mov rdi, rbx
    lea rsi, [rip + second]
    mov edx, 4
    syscall
    mov eax, 3
    mov rdi, rbx
    syscall
    mov eax, 2                  # open(path, O_WRONLY|O_CREAT|O_EXCL) -> -EEXIST
    lea rdi, [rip + path]
    mov esi, 0xc1
    mov edx, 0x1a4
    syscall
    mov qword ptr [rip + res], rax
    mov eax, 2                  # open(missing, O_RDONLY) -> -ENOENT
    lea rdi, [rip + missing]
    xor esi, esi
    syscall
    mov qword ptr [rip + res + 8], rax
    mov eax, 2                  # read the file back and print it
    lea rdi, [rip + path]
    xor esi, esi
    syscall
    mov rbx, rax
    xor eax, eax
    mov rdi, rbx
    lea rsi, [rip + buf]
    mov edx, 64
    syscall
    mov rdx, rax                # 8 bytes: "abc\ndef\n"
    mov eax, 1
    mov edi, 1
    lea rsi, [rip + buf]
    syscall
    mov eax, 1                  # write to a read-only fd -> -EBADF
    mov rdi, rbx
    lea rsi, [rip + buf]
    mov edx, 1
    syscall
    add r15, rax
    mov eax, 3
    mov rdi, rbx
    syscall
    mov eax, 3                  # close(200) -> -EBADF
    mov edi, 200
    syscall
    add r15, rax
    mov r8, 0x1111              # registers and flags survive a syscall
    mov r9, 0x2222
    mov r10, 0x3333
    mov rsi, 0x4444
    mov edi, 1
    xor edx, edx
    mov eax, 39
    cmp eax, 39                 # ZF = 1
    syscall                     # getpid
    jne fail
    test rax, rax
    jle fail
    cmp r8, 0x1111
    jne fail
    cmp r9, 0x2222
    jne fail
    cmp r10, 0x3333
    jne fail
    cmp rsi, 0x4444
    jne fail
    test rdx, rdx
    jne fail
    cmp rdi, 1
    jne fail
    mov eax, 1                  # write(1, buf, 0) -> 0
    syscall
    test rax, rax
    jne fail
    # status = 256 + 3 + 4 + 9 + 27 + 17 + 2 = 318; Linux reports 318 & 0xff = 62
    lea rdi, [r12 + 256]
    add rdi, r13
    sub rdi, r14
    sub rdi, r15
    sub rdi, qword ptr [rip + res]
    sub rdi, qword ptr [rip + res + 8]
    mov eax, 60
    syscall
fail:
    mov eax, 60
    mov edi, 1
    syscall
.section .note.GNU-stack,"",@progbits
"#;

const WIN32_MOCK: &str = r#"
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <unistd.h>
#define WINAPI __attribute__((ms_abi))
#define H(fd) ((void *)(intptr_t)((fd) + 0x1000))
#define FD(h) ((int)((intptr_t)(h) - 0x1000))

static uint32_t last_error;

/* At -O0 rbp = entry rsp - 8, so a correctly aligned call gives rbp % 16 == 0.
   gcc -O0 spills ms_abi register arguments into the caller's shadow space
   ([rbp + 16, rbp + 48)), so a missing shadow space corrupts the caller.
   Volatile registers are clobbered as a real Windows function may do. */
#define CHECK do { \
    uint64_t *fp = __builtin_frame_address(0); \
    if ((uintptr_t)fp & 15) { write(2, "misaligned call\n", 16); _exit(99); } \
    __asm__ volatile("mov $-1, %%r10\n mov $-1, %%r11\n mov $-1, %%r8\n mov $-1, %%r9\n mov $-1, %%rdx" \
                     ::: "r8", "r9", "r10", "r11", "rdx"); \
} while (0)

static uint32_t win_error(int e) {
    switch (e) {
    case ENOENT: return 2;      case EMFILE: return 4;   case EACCES: return 5;
    case EBADF: return 6;       case EEXIST: return 80;  case EINVAL: return 87;
    case EPIPE: return 109;     case ENOSPC: return 112; case ENAMETOOLONG: return 206;
    default: return 31;
    }
}

WINAPI void *GetStdHandle(uint32_t n) {
    CHECK;
    switch ((int32_t)n) { case -10: return H(0); case -11: return H(1); case -12: return H(2); }
    return (void *)-1;
}
WINAPI uint32_t GetLastError(void) { CHECK; return last_error; }
WINAPI int ReadFile(void *h, void *buf, uint32_t n, uint32_t *got, void *ov) {
    CHECK;
    if (ov) abort();
    ssize_t r = read(FD(h), buf, n);
    if (r < 0) { last_error = win_error(errno); *got = 0; return 0; }
    *got = (uint32_t)r; return 1;
}
WINAPI int WriteFile(void *h, const void *buf, uint32_t n, uint32_t *put, void *ov) {
    CHECK;
    if (ov) abort();
    ssize_t r = write(FD(h), buf, n);
    if (r < 0) { last_error = win_error(errno); *put = 0; return 0; }
    *put = (uint32_t)r; return 1;
}
WINAPI void *CreateFileA(const char *path, uint32_t access, uint32_t share, void *sa,
                         uint32_t disp, uint32_t attrs, void *tmpl) {
    CHECK;
    if (share != 7 || sa || tmpl || (attrs != 0x80 && attrs != 1)) abort();
    int fl;
    switch (access) {
    case 0x80000000u: fl = O_RDONLY; break;
    case 0x40000000u: fl = O_WRONLY; break;
    case 0xC0000000u: fl = O_RDWR; break;
    default: abort();
    }
    switch (disp) {
    case 1: fl |= O_CREAT | O_EXCL; break;   /* CREATE_NEW */
    case 2: fl |= O_CREAT | O_TRUNC; break;  /* CREATE_ALWAYS */
    case 3: break;                           /* OPEN_EXISTING */
    case 4: fl |= O_CREAT; break;            /* OPEN_ALWAYS */
    case 5: fl |= O_TRUNC; break;            /* TRUNCATE_EXISTING */
    default: abort();
    }
    int fd = open(path, fl, attrs == 1 ? 0444 : 0666);
    if (fd < 0) { last_error = win_error(errno); return (void *)-1; }
    return H(fd);
}
WINAPI int CloseHandle(void *h) {
    CHECK;
    if (close(FD(h)) < 0) { last_error = win_error(errno); return 0; }
    return 1;
}
WINAPI int SetFilePointerEx(void *h, int64_t dist, int64_t *pos, uint32_t method) {
    CHECK;
    off_t r = lseek(FD(h), dist, method == 0 ? SEEK_SET : method == 1 ? SEEK_CUR : SEEK_END);
    if (r < 0) { last_error = win_error(errno); return 0; }
    if (pos) *pos = r;
    return 1;
}
WINAPI uint32_t GetCurrentProcessId(void) { CHECK; return (uint32_t)getpid(); }
WINAPI void ExitProcess(uint32_t code) { CHECK; _exit((int)code); }

/* The Windows loader calls the entry point like a function. */
WINAPI void l2w_entry(void);
int main(void) { l2w_entry(); return 111; }
"#;

/// Renames the symbol `_start` (not `__l2w_sysv__start`) so the translated
/// entry point does not clash with the C runtime's `_start`.
fn rename_entry(asm: &str) -> String {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '$';
    let mut out = String::new();
    let mut rest = asm;
    while let Some(i) = rest.find("_start") {
        let before = rest[..i].chars().last();
        let after = rest[i + 6..].chars().next();
        out += &rest[..i];
        if before.map_or(true, |c| !ident(c)) && after.map_or(true, |c| !ident(c)) {
            out += "l2w_entry";
        } else {
            out += "_start";
        }
        rest = &rest[i + 6..];
    }
    out + rest
}

/// Runs `exe` in a fresh directory, returns (stdout, exit code).
fn run_in(exe: &Path, cwd: &Path) -> (String, i32) {
    let _ = std::fs::remove_dir_all(cwd);
    std::fs::create_dir_all(cwd).unwrap();
    let out = Command::new(exe).current_dir(cwd).output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stderr).is_empty(),
        "{exe:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code().expect("exited normally"),
    )
}

#[test]
fn translated_syscalls_behave_like_linux() {
    if !available("gcc") {
        eprintln!("skipping: gcc not available");
        return;
    }
    let d = dir().join("syscalls");
    std::fs::create_dir_all(&d).unwrap();
    let mock = d.join("win32_mock.c");
    std::fs::write(&mock, WIN32_MOCK).unwrap();
    for (name, src) in [
        ("hello", HELLO),
        ("loop", LOOP),
        ("functions", FUNCTIONS),
        ("syscalls", SYSCALLS),
    ] {
        // Original: a static Linux executable.
        let orig_s = d.join(format!("{name}.s"));
        std::fs::write(&orig_s, src).unwrap();
        let orig = d.join(format!("{name}-linux"));
        run(Command::new("gcc")
            .args(["-nostdlib", "-static", "-no-pie", "-o"])
            .arg(&orig)
            .arg(&orig_s));
        // Translated: Windows code + Win32 mock.
        let tr_s = d.join(format!("{name}-win.s"));
        std::fs::write(
            &tr_s,
            rename_entry(&coff_to_elf_sections(&translate(src, name))),
        )
        .unwrap();
        let tr = d.join(format!("{name}-win"));
        run(Command::new("gcc")
            .args(["-no-pie", "-O0", "-fno-omit-frame-pointer", "-o"])
            .arg(&tr)
            .arg(&mock)
            .arg(&tr_s));
        let expected = run_in(&orig, &d.join(format!("{name}-run-linux")));
        let actual = run_in(&tr, &d.join(format!("{name}-run-win")));
        assert_eq!(expected, actual, "{name}");
        if name == "syscalls" {
            assert_eq!(expected, ("abc\ndef\n".to_string(), 62));
        }
        if name == "hello" {
            assert_eq!(expected.0, "Hello, World!\n");
        }
    }
}
