//! Semantic round-trip test: parse -> pretty-print -> assemble both the
//! original and the printed source with GNU `as` and compare the resulting
//! object contents and relocations. Skipped when GNU `as`/`objdump` for
//! x86-64 ELF are missing (see [`common::GNU_AS`]).

mod common;
use common::{gnu_tool, GNU_AS, GNU_OBJDUMP};
use std::path::{Path, PathBuf};
use std::process::Command;

/// `(as, objdump)`, or `None` with a skip message.
fn binutils() -> Option<(String, String)> {
    match (gnu_tool(GNU_AS), gnu_tool(GNU_OBJDUMP)) {
        (Some(a), Some(o)) => Some((a, o)),
        _ => {
            eprintln!(
                "skipping: GNU as/objdump for x86-64 ELF not available (tried {})",
                GNU_AS.join(", ")
            );
            None
        }
    }
}

fn scratch_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("roundtrip");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Assembles `src` and returns `objdump -s -r` output without the header line.
fn assemble_dump(tools: &(String, String), src: &str, stem: &str) -> String {
    let (as_, objdump) = tools;
    let dir = scratch_dir();
    let s = dir.join(format!("{stem}.s"));
    let o = dir.join(format!("{stem}.o"));
    std::fs::write(&s, src).unwrap();
    let out = Command::new(as_)
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
    let dump = Command::new(objdump)
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

fn check_roundtrip(tools: &(String, String), name: &str, src: &str) {
    let parsed = l2w::parser::parse(src);
    assert!(!parsed.has_errors(), "{name}: {:#?}", parsed.diagnostics);
    let printed = parsed.program.to_string();
    let a = assemble_dump(tools, src, &format!("{name}-orig"));
    let b = assemble_dump(tools, &printed, &format!("{name}-printed"));
    assert_eq!(
        a, b,
        "{name}: object code differs after round trip\n--- printed ---\n{printed}"
    );
}

#[test]
fn examples_assemble_identically_after_roundtrip() {
    let Some(tools) = binutils() else { return };
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    for entry in std::fs::read_dir(examples).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        if path.extension().is_some_and(|e| e == "s") && !name.starts_with("errors") {
            check_roundtrip(&tools, &name, &std::fs::read_to_string(&path).unwrap());
        }
    }
}

#[test]
fn tricky_syntax_assembles_identically_after_roundtrip() {
    let Some(tools) = binutils() else { return };
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
    sete al
    setnz [rdi]
    setnbe byte ptr [rsi + 4]
    cmovl eax, ebx
    cmovnge rax, [rbx + 8]
    cmovz cx, dx
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
    check_roundtrip(&tools, "tricky", src);
}

/// AST -> IR -> normalized listing must assemble to the same object code as
/// the original. This checks the IR's operand semantics (memory vs.
/// immediate, inferred sizes, equate ordering, numeric labels) against GNU as.
fn check_ir_roundtrip(tools: &(String, String), name: &str, src: &str) {
    let parsed = l2w::parser::parse(src);
    assert!(!parsed.has_errors(), "{name}: {:#?}", parsed.diagnostics);
    let lowered = l2w::ir::lower(&parsed.program);
    assert!(!lowered.has_errors(), "{name}: {:#?}", lowered.diagnostics);
    let listing = lowered.module.to_string();
    let a = assemble_dump(tools, src, &format!("{name}-ir-orig"));
    let b = assemble_dump(tools, &listing, &format!("{name}-ir-listing"));
    assert_eq!(
        a, b,
        "{name}: object code differs after IR round trip\n--- listing ---\n{listing}"
    );
}

#[test]
fn ir_listing_assembles_identically() {
    let Some(tools) = binutils() else { return };
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    for entry in std::fs::read_dir(examples).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        if path.extension().is_some_and(|e| e == "s") && !name.starts_with("errors") {
            check_ir_roundtrip(&tools, &name, &std::fs::read_to_string(&path).unwrap());
        }
    }
    let src = r#"
.intel_syntax noprefix
.data
early: .ascii "abc"
early_len = . - early
konst = 42
.text
.globl f
f:
    mov rdx, early_len          # absolute equate defined before -> immediate
    mov rdx, late_len           # defined after -> memory (GAS semantics)
    mov rcx, konst
    push konst
    mov rax, early              # bare symbol -> memory
    mov rax, offset early       # address
    mov rax, offset late_len    # value via offset
    imul rax, rbx, offset (late_len + 1)
    add rax, early_len * 2
    mov [rbx], ecx              # size inferred from register
    add qword ptr [rbx + rsi*4 + 8], 7
    inc byte ptr [rip + early]
    push [rax]
    pop [rax]
    call [rax]
    movzx eax, byte ptr [rdi]
    movsx rcx, word ptr [rdi]
    movsxd rax, [rdi]
    imul rax, rbx, 10
    imul rcx, 3
    shl rax, cl
    sar dword ptr [rdi], 2
    cdqe
    cqo
    idiv rbx
    xchg rax, [rdi]
    lock add dword ptr [rip + early], 1
    test al, 1
    setz al                     # 8-bit only; memory defaults to byte
    setnz [rdi]
    setnbe byte ptr [rdi + 1]
    cmovnge rax, [rbx + 8]      # size from the register
    cmovz cx, word ptr [rdi]
1:  dec ecx
    jnz 1b
    jz 2f
    jnle 1b
2:  call g
    leave
    ret
g:  ret 8
.data
late: .ascii "defg"
late_len = . - late
.text
    jmp f
.section .rodata
ro: .quad f, 1b, 2b
.bss
buf: .zero 16
.lcomm lbuf, 32
.comm cbuf, 8, 8
"#;
    check_ir_roundtrip(&tools, "ir-tricky", src);
}
