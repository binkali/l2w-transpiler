//! Semantic round-trip test: parse -> pretty-print -> assemble both the
//! original and the printed source with GNU `as` and compare the resulting
//! object contents and relocations. Skipped when `as`/`objdump` are missing.

use std::path::{Path, PathBuf};
use std::process::Command;

fn tool_available(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn scratch_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("roundtrip");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Assembles `src` and returns `objdump -s -r` output without the header line.
fn assemble_dump(src: &str, stem: &str) -> String {
    let dir = scratch_dir();
    let s = dir.join(format!("{stem}.s"));
    let o = dir.join(format!("{stem}.o"));
    std::fs::write(&s, src).unwrap();
    let out = Command::new("as")
        .args(["--64", "-o"])
        .arg(&o)
        .arg(&s)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "as failed for {stem}:\n{}\n--- source ---\n{src}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dump = Command::new("objdump")
        .args(["-s", "-r"])
        .arg(&o)
        .output()
        .unwrap();
    String::from_utf8(dump.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.contains("file format"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn check_roundtrip(name: &str, src: &str) {
    let parsed = l2w::parser::parse(src);
    assert!(!parsed.has_errors(), "{name}: {:#?}", parsed.diagnostics);
    let printed = parsed.program.to_string();
    let a = assemble_dump(src, &format!("{name}-orig"));
    let b = assemble_dump(&printed, &format!("{name}-printed"));
    assert_eq!(
        a, b,
        "{name}: object code differs after round trip\n--- printed ---\n{printed}"
    );
}

#[test]
fn examples_assemble_identically_after_roundtrip() {
    if !tool_available("as") || !tool_available("objdump") {
        eprintln!("skipping: GNU as/objdump not available");
        return;
    }
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    for entry in std::fs::read_dir(examples).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        if path.extension().is_some_and(|e| e == "s") && name != "errors" {
            check_roundtrip(&name, &std::fs::read_to_string(&path).unwrap());
        }
    }
}

#[test]
fn tricky_syntax_assembles_identically_after_roundtrip() {
    if !tool_available("as") || !tool_available("objdump") {
        eprintln!("skipping: GNU as/objdump not available");
        return;
    }
    let src = r#"
.intel_syntax noprefix
.text
.globl f
.type f, @function
f:
    mov qword ptr [rbp + rcx*8 - 16], rax
    mov eax, dword ptr [4*rbx + 0x20]
    lea rsi, [rip + msg + 4]
    lea rdi, msg[rip]
    mov rdx, (len * 2) | 1
    mov rax, offset msg
    mov rcx, qword ptr fs:0x28
    mov al, byte ptr [rsi - (2 + 3)]
    movzx eax, word ptr [rdi + rsi*2]
    imul rax, rbx, -7
    and r8d, ~0xF
    rep stosb
    lock inc dword ptr [rip + counter]
1:  dec ecx
    jnz 1b
    jmp 2f
    nop
2:  call f
    ret
.size f, . - f
.data
counter: .long 0
vals: .byte 1, 'A', -1
      .word 0x1234
      .quad msg, len, 1b
.section .rodata, "a", @progbits
msg: .ascii "a\"b\\c\n\001\377"
     .asciz "tail"
     .string "s"
len = . - msg
.p2align 4,,15
.balign 8, 0x90
.bss
buf: .zero 64
     .skip 16
.comm big, 4096, 32
"#;
    check_roundtrip("tricky", src);
}
