//! Windows code generation tests with real toolchains (skipped when the
//! tools are not installed):
//!
//! * MinGW-w64 `as` must accept every translation, and the COFF objects
//!   must not contain 32-bit absolute relocations.
//! * Behavioral equivalence: phase 3 does not change the calling
//!   convention, so translated code can be reassembled for Linux (only the
//!   COFF section directives are mapped back) and called from a C harness.
//!   Its output must equal that of the original code.

mod common;
use common::*;
use std::process::Command;

const MINGW_AS: &str = "x86_64-w64-mingw32-as";
const MINGW_OBJDUMP: &str = "x86_64-w64-mingw32-objdump";

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
    let Some(gcc) = linux_gcc() else {
        eprintln!("skipping: {LINUX_HOST_HINT}");
        return;
    };
    let d = dir();
    let harness = d.join("harness.c");
    std::fs::write(&harness, HARNESS).unwrap();
    let build_and_run = |tag: &str, translated: bool, sources: &[(&str, String)]| -> String {
        let exe = d.join(format!("{tag}-test"));
        let mut cmd = Command::new(&gcc);
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
res:     .zero 24
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
    mov eax, 8                  # lseek(fd, 4, SEEK_SET) -> 4
    mov rdi, rbx
    mov esi, 4
    xor edx, edx
    syscall
    mov r8, rax
    mov eax, 8                  # lseek(fd, -2, SEEK_END) -> 6
    mov rdi, rbx
    mov rsi, -2
    mov edx, 2
    syscall
    add r8, rax
    mov eax, 8                  # lseek(fd, 0, SEEK_CUR) -> 6
    mov rdi, rbx
    xor esi, esi
    mov edx, 1
    syscall
    add r8, rax
    mov eax, 8                  # lseek(fd, -100, SEEK_SET) -> -EINVAL
    mov rdi, rbx
    mov rsi, -100
    xor edx, edx
    syscall
    add r8, rax
    mov eax, 8                  # lseek(fd, 0, 7) -> -EINVAL (bad whence)
    mov rdi, rbx
    xor esi, esi
    mov edx, 7
    syscall
    add r8, rax
    mov eax, 8                  # lseek(1, 0, SEEK_CUR): stdout is a pipe -> -ESPIPE
    mov edi, 1
    xor esi, esi
    mov edx, 1
    syscall
    add r8, rax
    mov eax, 8                  # lseek(200, 0, SEEK_SET) -> -EBADF
    mov edi, 200
    xor esi, esi
    xor edx, edx
    syscall
    add r8, rax
    mov qword ptr [rip + res + 16], r8   # 4 + 6 + 6 - 22 - 22 - 29 - 9 = -66
    xor eax, eax                # read from position 6: "f\n"
    mov rdi, rbx
    lea rsi, [rip + buf]
    mov edx, 64
    syscall
    mov rdx, rax                # 2
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
    # status = 256 + 3 + 4 + 9 + 27 + 17 + 2 + 66 = 384; Linux reports 384 & 0xff = 128
    lea rdi, [r12 + 256]
    add rdi, r13
    sub rdi, r14
    sub rdi, r15
    sub rdi, qword ptr [rip + res]
    sub rdi, qword ptr [rip + res + 8]
    sub rdi, qword ptr [rip + res + 16]
    mov eax, 60
    syscall
fail:
    mov eax, 60
    mov edi, 1
    syscall
.section .note.GNU-stack,"",@progbits
"#;

#[test]
fn translated_syscalls_behave_like_linux() {
    let Some(gcc) = linux_gcc() else {
        eprintln!("skipping: {LINUX_HOST_HINT}");
        return;
    };
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
        run(Command::new(&gcc)
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
        run(Command::new(&gcc)
            .args(["-no-pie", "-O0", "-fno-omit-frame-pointer", "-o"])
            .arg(&tr)
            .arg(&mock)
            .arg(&tr_s));
        let expected = run_in(&orig, &d.join(format!("{name}-run-linux")));
        let actual = run_in(&tr, &d.join(format!("{name}-run-win")));
        assert_eq!(expected, actual, "{name}");
        // The same translation as a real .exe against Wine's kernel32.
        if let Some(wine) = wine_outcome(name, src, "") {
            assert_eq!(expected, wine, "{name}: under Wine");
        }
        if name == "syscalls" {
            assert_eq!(expected, ("abc\ndef\ndef\nf\n".to_string(), 128));
        }
        if name == "hello" {
            assert_eq!(expected.0, "Hello, World!\n");
        }
    }
}
