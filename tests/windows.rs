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
    for (name, src) in [("library", LIBRARY), ("extra", EXTRA), ("bridge", BRIDGE)] {
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
