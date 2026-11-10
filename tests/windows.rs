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

#[test]
fn mingw_assembles_translations_without_absolute_32bit_relocations() {
    if !available(MINGW_AS) || !available(MINGW_OBJDUMP) {
        eprintln!(
            "skipping: MinGW-w64 binutils not installed (apt install binutils-mingw-w64-x86-64)"
        );
        return;
    }
    for (name, src) in [("library", LIBRARY), ("extra", EXTRA)] {
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
long fib_at(long); long sum_table(void); long bump(long);
long strlen_l2w(const char *); long max3(long, long, long); int extra(void);
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
    let build_and_run = |tag: &str, lib: &str, extra: &str| -> String {
        let lib_s = d.join(format!("{tag}-library.s"));
        let extra_s = d.join(format!("{tag}-extra.s"));
        let exe = d.join(format!("{tag}-test"));
        std::fs::write(&lib_s, lib).unwrap();
        std::fs::write(&extra_s, extra).unwrap();
        // -no-pie: the original Linux code uses 32-bit absolute addresses.
        run(Command::new("gcc")
            .args(["-no-pie", "-o"])
            .arg(&exe)
            .arg(&harness)
            .arg(&lib_s)
            .arg(&extra_s));
        run(&mut Command::new(&exe))
    };
    let original = build_and_run("orig", LIBRARY, EXTRA);
    let translated = build_and_run(
        "translated",
        &coff_to_elf_sections(&translate(LIBRARY, "library")),
        &coff_to_elf_sections(&translate(EXTRA, "extra")),
    );
    assert_eq!(original, translated);
    assert!(
        original.contains("-1 -1 1 1 2 3 5 8 13 21 -1 -1"),
        "{original}"
    );
    assert!(original.contains("5 12 12"), "{original}");
    assert!(original.contains("Hello from l2w"), "{original}");
}
