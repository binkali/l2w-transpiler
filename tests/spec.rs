//! Test matrix of the project specification (section 9).
//!
//! Every behavioral test builds its program twice and requires identical
//! stdout and exit status (see [`common::same_behavior`]):
//!
//! * the **original** Linux assembly, as a static Linux executable;
//! * the **l2w translation** (Windows x64 ABI, Win32 API), linked against a
//!   Win32 mock whose functions use the Windows calling convention and check
//!   stack alignment, and with external C functions compiled `ms_abi`.
//!
//! | # | Requirement | Test |
//! |---|---|---|
//! | 1 | arithmetic with `mov`, `add`, `sub` | `spec_01_arithmetic` |
//! | 2 | conditions with `cmp` and `je` | `spec_02_conditions` |
//! | 3 | loops with labels and `jmp` | `spec_03_loops` |
//! | 4 | string output with Linux `write` | `spec_04_write` |
//! | 5 | program exit with Linux `exit` | `spec_05_exit` |
//! | 6 | function calls with different argument counts | `spec_06_function_calls` |
//! | 7 | `.data` and `.bss` | `spec_07_data_and_bss` |
//! | 8 | faulty or unsupported instructions | `spec_08_diagnostics`, `spec_08_golden_diagnostics` |
//! | 9 | stack alignment and Windows shadow space | `spec_09_alignment_and_shadow_space` |
//! | 10 | Windows code generation and cross-compilation | `spec_10_cross_compilation` |

mod common;
use common::*;
use std::fmt::Write as _;
use std::process::Command;

/// `print_num(rdi)`: prints rdi as signed decimal + newline (Linux `write`).
const RUNTIME: &str = r#"
print_num:
    push rbx
    sub rsp, 48
    mov rax, rdi
    lea rsi, [rsp + 40]
    mov byte ptr [rsi], 10
    mov rbx, 10
    mov r8, rax
    test rax, rax
    jns .Lpn_digits
    neg rax
.Lpn_digits:
    xor edx, edx
    div rbx
    add dl, 48
    dec rsi
    mov byte ptr [rsi], dl
    test rax, rax
    jnz .Lpn_digits
    test r8, r8
    jns .Lpn_out
    dec rsi
    mov byte ptr [rsi], 45
.Lpn_out:
    lea rdx, [rsp + 41]
    sub rdx, rsi
    mov eax, 1
    mov edi, 1
    syscall
    add rsp, 48
    pop rbx
    ret
"#;

/// A complete program: `_start` runs `body`, then exits with status 0.
fn program(body: &str, functions: &str, data: &str) -> String {
    format!(
        ".intel_syntax noprefix\n.text\n.globl _start\n_start:\n{body}\n    mov eax, 60\n    xor edi, edi\n    syscall\n\
         {functions}\n{RUNTIME}\n{data}\n.section .note.GNU-stack,\"\",@progbits\n"
    )
}

/// `mov rdi, <src>; call print_num`
fn print(src: &str) -> String {
    format!("    mov rdi, {src}\n    call print_num\n")
}

fn lines(out: &str) -> Vec<&str> {
    out.lines().collect()
}

// -- 1 ------------------------------------------------------------------------

const ARITHMETIC: &str = "
    mov rax, 7
    add rax, 35
    mov rdi, rax
    call print_num              # 42
    mov rax, 10
    sub rax, 25
    mov rdi, rax
    call print_num              # -15
    mov rax, -6
    mov rbx, 7
    imul rax, rbx
    mov rdi, rax
    call print_num              # -42
    mov rax, -100
    cqo
    mov rcx, 7
    idiv rcx
    mov rbx, rdx
    mov rdi, rax
    call print_num              # -14
    mov rdi, rbx
    call print_num              # -2
    mov eax, 100
    xor edx, edx
    mov ecx, 7
    div ecx
    mov rbx, rdx
    mov rdi, rax
    call print_num              # 14
    mov rdi, rbx
    call print_num              # 2
    mov rax, 0xF0
    and rax, 0x3C
    or rax, 1
    xor rax, 0xFF
    mov rbx, rax
    mov rdi, rax
    call print_num              # 206
    not rbx
    mov rdi, rbx
    call print_num              # -207
    neg rbx
    mov rdi, rbx
    call print_num              # 207
    mov rax, -1
    inc rax
    dec rax
    dec rax
    mov rdi, rax
    call print_num              # -2
    mov rax, -16
    sar rax, 2
    mov rdi, rax
    call print_num              # -4
    mov rax, -16
    shr rax, 60
    shl rax, 3
    mov rdi, rax
    call print_num              # 120
    mov rax, -1
    add eax, 0                  # 32-bit writes zero-extend
    mov rdi, rax
    call print_num              # 4294967295
    mov rax, 0x1234
    mov al, 0xFF
    mov rbx, rax
    movzx ecx, al
    movsx rdx, al
    mov r12, rcx
    mov r13, rdx
    mov rdi, rbx
    call print_num              # 4863
    mov rdi, r12
    call print_num              # 255
    mov rdi, r13
    call print_num              # -1
    mov rbx, 7
    lea rax, [rbx + rbx*4 + 3]
    mov rdi, rax
    call print_num              # 38
    mov rax, 0x7FFFFFFFFFFFFFFF
    add rax, 1                  # wraps
    mov rdi, rax
    call print_num              # -9223372036854775808
    mov rax, 3
    mov rbx, 4
    xchg rax, rbx
    sub rax, rbx
    mov rdi, rax
    call print_num              # 1
";

#[test]
fn spec_01_arithmetic() {
    let Some((out, status)) = same_behavior("arithmetic", &program(ARITHMETIC, "", ""), "") else {
        return;
    };
    assert_eq!(status, 0);
    assert_eq!(
        lines(&out),
        [
            "42",
            "-15",
            "-42",
            "-14",
            "-2",
            "14",
            "2",
            "206",
            "-207",
            "207",
            "-2",
            "-4",
            "120",
            "4294967295",
            "4863",
            "255",
            "-1",
            "38",
            "-9223372036854775808",
            "1"
        ]
    );
}

// -- 2 ------------------------------------------------------------------------

const CONDITIONS: &[&str] = &[
    "e", "z", "ne", "nz", "g", "nle", "l", "nge", "ge", "nl", "le", "ng", "a", "nbe", "b", "nae",
    "ae", "be", "s", "ns",
];

/// `conds(a, b)` returns a bit mask: bit i is set iff `j<CONDITIONS[i]>` is
/// taken after `cmp a, b`; bit 31 is `je` after `test a, b`.
fn conditions_function() -> String {
    let mut s = String::from("conds:\n    xor eax, eax\n");
    for (i, c) in CONDITIONS.iter().enumerate() {
        let _ = write!(
            s,
            "    cmp rdi, rsi\n    j{c} .Lc{i}_taken\n    jmp .Lc{i}_done\n.Lc{i}_taken:\n    or eax, {}\n.Lc{i}_done:\n",
            1u32 << i
        );
    }
    s + "    test rdi, rsi\n    jne .Lct_done\n    or eax, 0x80000000\n.Lct_done:\n    ret\n"
}

/// Reference: which conditions hold for (a, b).
fn expected_mask(a: i64, b: i64) -> u32 {
    let (ua, ub) = (a as u64, b as u64);
    let sign = a.wrapping_sub(b) < 0;
    let mut m = 0u32;
    for (i, c) in CONDITIONS.iter().enumerate() {
        let taken = match *c {
            "e" | "z" => a == b,
            "ne" | "nz" => a != b,
            "g" | "nle" => a > b,
            "l" | "nge" => a < b,
            "ge" | "nl" => a >= b,
            "le" | "ng" => a <= b,
            "a" | "nbe" => ua > ub,
            "b" | "nae" => ua < ub,
            "ae" => ua >= ub,
            "be" => ua <= ub,
            "s" => sign,
            "ns" => !sign,
            _ => unreachable!(),
        };
        if taken {
            m |= 1 << i;
        }
    }
    if a & b == 0 {
        m |= 0x8000_0000;
    }
    m
}

#[test]
fn spec_02_conditions() {
    let pairs: &[(i64, i64)] = &[
        (1, 1),
        (1, 2),
        (2, 1),
        (-1, 1),
        (1, -1),
        (-5, -5),
        (-7, -3),
        (0, i64::MIN),
        (i64::MAX, -1),
        (0, 0),
        (6, 9),
    ];
    let mut body = String::new();
    for (a, b) in pairs {
        let _ = write!(body, "    mov rdi, {a}\n    mov rsi, {b}\n    call conds\n    mov rdi, rax\n    call print_num\n");
    }
    let Some((out, _)) = same_behavior(
        "conditions",
        &program(&body, &conditions_function(), ""),
        "",
    ) else {
        return;
    };
    let expected: Vec<String> = pairs
        .iter()
        .map(|(a, b)| expected_mask(*a, *b).to_string())
        .collect();
    assert_eq!(lines(&out), expected);
}

// -- 3 ------------------------------------------------------------------------

const LOOPS: &str = "
    xor eax, eax                # sum 1..100 (top-tested loop)
    mov ecx, 1
.Lsum_top:
    cmp ecx, 100
    jg .Lsum_done
    add eax, ecx
    inc ecx
    jmp .Lsum_top
.Lsum_done:
    mov rdi, rax
    call print_num              # 5050
    xor r12d, r12d              # nested: sum of i*j for i, j < 10
    xor ebx, ebx
.Louter:
    xor ecx, ecx
.Linner:
    mov rax, rbx
    imul rax, rcx
    add r12, rax
    inc rcx
    cmp rcx, 10
    jl .Linner
    inc rbx
    cmp rbx, 10
    jl .Louter
    mov rdi, r12
    call print_num              # 2025
    mov ecx, 10                 # countdown with numeric labels
    xor eax, eax
1:  add eax, ecx
    dec ecx
    jnz 1b
    jmp 1f
    mov eax, -1                 # skipped
1:  mov rdi, rax
    call print_num              # 55
    mov rax, 1                  # do-while: smallest power of two > 1000
2:  shl rax, 1
    cmp rax, 1000
    jbe 2b
    mov rdi, rax
    call print_num              # 1024
";

#[test]
fn spec_03_loops() {
    let Some((out, _)) = same_behavior("loops", &program(LOOPS, "", ""), "") else {
        return;
    };
    assert_eq!(lines(&out), ["5050", "2025", "55", "1024"]);
}

// -- 4 ------------------------------------------------------------------------

#[test]
fn spec_04_write() {
    let body = "
    mov eax, 1
    mov edi, 1
    lea rsi, [rip + greeting]
    mov edx, offset greeting_len
    syscall
    mov rbx, rax                # bytes written
    mov eax, 1
    mov edi, 1
    lea rsi, [rip + greeting]
    mov edx, 5                  # partial write
    syscall
    mov eax, 1
    mov edi, 1
    lea rsi, [rip + newline]
    mov edx, 1
    syscall
    mov eax, 1                  # zero-length write
    mov edi, 1
    lea rsi, [rip + greeting]
    xor edx, edx
    syscall
    mov r12, rax
    mov eax, 1                  # invalid fd
    mov edi, 9
    lea rsi, [rip + greeting]
    mov edx, 1
    syscall
    mov r13, rax
"
    .to_string()
        + &print("rbx")
        + &print("r12")
        + &print("r13");
    let data = ".section .rodata\ngreeting: .ascii \"Hello, Windows!\\n\"\ngreeting_len = . - greeting\nnewline: .ascii \"\\n\"\n";
    let Some((out, _)) = same_behavior("write", &program(&body, "", data), "") else {
        return;
    };
    assert_eq!(out, "Hello, Windows!\nHello\n16\n0\n-9\n");
}

// -- 5 ------------------------------------------------------------------------

#[test]
fn spec_05_exit() {
    // Linux reports status & 0xff; exit_group behaves like exit here.
    for (status, number, expected) in [
        (0, 60, 0),
        (1, 60, 1),
        (42, 60, 42),
        (255, 60, 255),
        (256, 60, 0),
        (300, 231, 44),
        (-1, 231, 255),
    ] {
        let src = format!(
            ".intel_syntax noprefix\n.text\n.globl _start\n_start:\n    mov edi, {status}\n    mov eax, {number}\n    syscall\n    hlt_unreachable:\n    jmp hlt_unreachable\n"
        );
        let Some((out, code)) = same_behavior(&format!("exit-{number}-{status}"), &src, "") else {
            return;
        };
        assert_eq!((out.as_str(), code), ("", expected), "exit({status})");
    }
}

// -- 6 ------------------------------------------------------------------------

/// External C functions (compiled ms_abi for the translated build). The
/// weights detect swapped or shifted arguments; `CHECK` makes a misaligned
/// call visible as a wrong result.
const EXT_FUNCTIONS: &str = r#"
#ifdef WIN
#define CHECK if ((uintptr_t)__builtin_frame_address(0) & 15) return -1000000
#else
#define CHECK
#endif
long ABI ext0(void) { CHECK; return 7; }
long ABI ext1(long a) { CHECK; return a; }
long ABI ext2(long a, long b) { CHECK; return a + 2 * b; }
long ABI ext4(long a, long b, long c, long d) { CHECK; return a + 2 * b + 3 * c + 4 * d; }
long ABI ext5(long a, long b, long c, long d, long e) { CHECK; return a + 2 * b + 3 * c + 4 * d + 5 * e; }
long ABI ext6(long a, long b, long c, long d, long e, long f) {
    CHECK; return a + 2 * b + 3 * c + 4 * d + 5 * e + 6 * f;
}
long ABI ext7(long a, long b, long c, long d, long e, long f, long g) {
    CHECK; return a + 2 * b + 3 * c + 4 * d + 5 * e + 6 * f + 7 * g;
}
long ABI ext10(long a, long b, long c, long d, long e, long f, long g, long h, long i, long j) {
    CHECK; return a + 2 * b + 3 * c + 4 * d + 5 * e + 6 * f + 7 * g + 8 * h + 9 * i + 10 * j;
}
int ABI ext_int(int a, int b) { CHECK; return a * b; }
"#;

/// Calls `f` with arguments 1..=n using the System V convention.
fn sysv_call(f: &str, n: usize) -> String {
    let regs = ["rdi", "rsi", "rdx", "rcx", "r8", "r9"];
    let stack = n.saturating_sub(6);
    let reserve = (stack * 8).div_ceil(16) * 16;
    let mut s = String::new();
    if reserve > 0 {
        let _ = writeln!(s, "    sub rsp, {reserve}");
    }
    for k in 0..stack {
        let _ = writeln!(s, "    mov qword ptr [rsp + {}], {}", 8 * k, 7 + k);
    }
    for (k, r) in regs.iter().enumerate().take(n.min(6)) {
        let _ = writeln!(s, "    mov {r}, {}", k + 1);
    }
    let _ = writeln!(s, "    call {f}");
    if reserve > 0 {
        let _ = writeln!(s, "    add rsp, {reserve}");
    }
    s + "    mov rdi, rax\n    call print_num\n"
}

/// Internal System V functions: weighted sums like the C ones.
const INTERNAL_FUNCTIONS: &str = "
int0:
    mov eax, 7
    ret
int3:
    lea rax, [rdi + rsi*2]
    lea rcx, [rdx + rdx*2]
    add rax, rcx
    ret
int8:                           # args 7, 8 on the stack
    lea rax, [rdi + rsi*2]
    imul rdx, rdx, 3
    add rax, rdx
    lea rax, [rax + rcx*4]
    imul r8, r8, 5
    add rax, r8
    imul r9, r9, 6
    add rax, r9
    mov rcx, qword ptr [rsp + 8]
    imul rcx, rcx, 7
    add rax, rcx
    mov rcx, qword ptr [rsp + 16]
    lea rax, [rax + rcx*8]
    ret
fib:                            # recursive, callee-saved rbx
    cmp rdi, 2
    jl .Lfib_base
    push rbx
    push r12
    sub rsp, 8
    mov rbx, rdi
    lea rdi, [rbx - 1]
    call fib
    mov r12, rax
    lea rdi, [rbx - 2]
    call fib
    add rax, r12
    add rsp, 8
    pop r12
    pop rbx
    ret
.Lfib_base:
    mov rax, rdi
    ret
tail10:                         # tail call into an external function
    jmp ext10
";

#[test]
fn spec_06_function_calls() {
    let mut body = String::new();
    for (f, n) in [
        ("ext0", 0),
        ("ext1", 1),
        ("ext2", 2),
        ("ext4", 4),
        ("ext5", 5),
        ("ext6", 6),
        ("ext7", 7),
        ("ext10", 10),
    ] {
        body += &sysv_call(f, n);
    }
    body += "    mov edi, -6\n    mov esi, 7\n    call ext_int\n    movsxd rdi, eax\n    call print_num\n";
    body += &sysv_call("int0", 0);
    body += &sysv_call("int3", 3);
    body += &sysv_call("int8", 8);
    body += "    mov edi, 20\n    call fib\n    mov rdi, rax\n    call print_num\n";
    body += &sysv_call("tail10", 10);
    // Callee-saved registers survive external calls.
    body += "    mov rbx, 11\n    mov r12, 12\n    mov r13, 13\n    mov r14, 14\n    mov r15, 15\n    call ext0\n\
             add rbx, r12\n    add rbx, r13\n    add rbx, r14\n    add rbx, r15\n";
    body += &print("rbx");
    let Some((out, _)) = same_behavior(
        "calls",
        &program(&body, INTERNAL_FUNCTIONS, ""),
        EXT_FUNCTIONS,
    ) else {
        return;
    };
    let w = |n: i64| (1..=n).map(|k| k * k).sum::<i64>().to_string();
    let expected = [
        "7".to_string(),
        w(1),
        w(2),
        w(4),
        w(5),
        w(6),
        w(7),
        w(10),
        "-42".into(),
        "7".into(),
        w(3),
        w(8),
        "6765".into(),
        w(10),
        "65".into(),
    ];
    assert_eq!(lines(&out), expected);
}

// -- 7 ------------------------------------------------------------------------

#[test]
fn spec_07_data_and_bss() {
    let body = "
    lea rsi, [rip + table]      # sum of a .data array
    xor eax, eax
    xor ecx, ecx
.Lt_loop:
    add rax, qword ptr [rsi + rcx*8]
    inc rcx
    cmp rcx, 5
    jl .Lt_loop
    mov rdi, rax
    call print_num              # 150
    movzx edi, byte ptr [rip + b1]
    call print_num              # 200
    movsx rdi, word ptr [rip + w1]
    call print_num              # -2
    mov edi, dword ptr [rip + d1]
    call print_num              # 4000000000
    add qword ptr [rip + counter], 5    # read-modify-write .data
    add qword ptr [rip + counter], 5
    mov rdi, qword ptr [rip + counter]
    call print_num              # 110
    lea rsi, [rip + zeros]      # .bss starts zeroed
    xor eax, eax
    xor ecx, ecx
.Lz_loop:
    or al, byte ptr [rsi + rcx]
    inc rcx
    cmp rcx, 4096
    jl .Lz_loop
    mov rdi, rax
    call print_num              # 0
    mov qword ptr [rip + zeros + 4088], 77
    mov rdi, qword ptr [rip + zeros + 4088]
    call print_num              # 77
    mov qword ptr [rip + shared + 8], 99    # .comm
    mov rdi, qword ptr [rip + shared + 8]
    call print_num              # 99
    mov rsi, qword ptr [rip + msg_ptr]      # pointer stored in .data
    movzx edi, byte ptr [rsi + 1]
    call print_num              # 105 ('i')
    lea rax, [rip + aligned]
    and eax, 63
    mov rdi, rax
    call print_num              # 0
    mov rdi, offset msg_len     # defined later: needs `offset` (GAS)
    call print_num              # 2
";
    let data = "
.data
table:   .quad 10, 20, 30, 40, 50
b1:      .byte 200
w1:      .word -2
d1:      .long 4000000000
counter: .quad 100
msg_ptr: .quad msg
.balign 64
aligned: .quad 1
.section .rodata
msg:     .ascii \"hi\"
msg_len = . - msg
.bss
zeros:   .zero 4096
.comm shared, 64, 16
";
    let Some((out, _)) = same_behavior("data", &program(body, "", data), "") else {
        return;
    };
    assert_eq!(
        lines(&out),
        [
            "150",
            "200",
            "-2",
            "4000000000",
            "110",
            "0",
            "77",
            "99",
            "105",
            "0",
            "2"
        ]
    );
}

// -- 8 ------------------------------------------------------------------------

/// `(source, marker, message)`: the first error must be reported at the
/// line and column of `marker` in `source` and contain `message`.
const DIAGNOSTICS: &[(&str, &str, &str)] = &[
    (
        "_start:\n    cpuid\n",
        "cpuid",
        "unsupported instruction `cpuid`",
    ),
    (
        "_start:\n    movaps xmm0, xmm1\n",
        "movaps",
        "unsupported instruction `movaps`",
    ),
    (
        "_start:\n    mov rax, %rbx\n",
        "%",
        "register prefix `%` is not supported",
    ),
    ("_start:\n    mov rax, $1\n", "$", "unexpected `$`"),
    (
        "_start:\n    mov eax, 9\n    syscall\n",
        "syscall",
        "`mmap` (9) has no Windows translation",
    ),
    (
        "_start:\n    mov rax, rdi\n    syscall\n",
        "syscall",
        "cannot be determined statically",
    ),
    (
        "_start:\n    mov al, 300\n",
        "300",
        "does not fit in an 8-bit operand",
    ),
    (
        "_start:\n    mov rax, qword ptr fs:[0x28]\n",
        "qword",
        "thread-local",
    ),
    (
        "_start:\n    mov rax, qword ptr [rbx + table]\n",
        "qword",
        "table",
    ),
    (
        "_start:\n    lea rdi, [rip + puts]\n",
        "[rip",
        "external symbol `puts`",
    ),
    (
        "_start:\n    mov rdi, qword ptr [rsp]\n",
        "mov rdi",
        "initial process stack",
    ),
    (
        "_start:\n    mov rax, qword ptr [rsp + rsp]\n",
        "[rsp + rsp]",
        "`rsp` cannot be used as an index register",
    ),
    ("_start:\n    mov eax, ebx, ecx\n", "mov", "operand"),
    ("_start:\n    jmp\n", "jmp", "operand"),
    (
        ".section .init_array,\"aw\"\n.quad f\n",
        ".section",
        "constructors",
    ),
    (
        ".data\nx: .long _start\n_start: ret\n",
        ".long",
        "4-byte absolute address `_start` cannot be represented",
    ),
    (".att_syntax\n", ".att_syntax", "AT&T"),
];

fn first_error(src: &str) -> Option<l2w::diagnostics::Diagnostic> {
    let p = l2w::parser::parse(src);
    if let Some(d) = p.diagnostics.into_iter().find(|d| d.is_error()) {
        return Some(d);
    }
    let l = l2w::ir::lower(&p.program);
    if let Some(d) = l.diagnostics.into_iter().find(|d| d.is_error()) {
        return Some(d);
    }
    l2w::transpiler::transpile(&l.module)
        .diagnostics
        .into_iter()
        .find(|d| d.is_error())
}

#[test]
fn spec_08_diagnostics() {
    let mut failures = Vec::new();
    for (src, marker, message) in DIAGNOSTICS {
        let (line_no, line) = src
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains(marker))
            .expect("marker in source");
        let col = line[..line.find(marker).unwrap()].chars().count() + 1;
        match first_error(src) {
            None => failures.push(format!("{src:?}: no error (would translate silently)")),
            Some(d) => {
                let at = d.span.map(|s| (s.line as usize, s.col as usize));
                if at != Some((line_no + 1, col)) || !d.message.contains(message) {
                    failures.push(format!(
                        "{src:?}: expected `{message}` at {}:{col}, got `{}` at {at:?}",
                        line_no + 1,
                        d.message
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Complete `l2w --check` output for the error examples, compared with the
/// files in `tests/expected/` (set `L2W_BLESS=1` to regenerate them).
#[test]
fn spec_08_golden_diagnostics() {
    for name in ["errors", "errors_semantic"] {
        let out = Command::new(env!("CARGO_BIN_EXE_l2w"))
            .args([format!("examples/{name}.s").as_str(), "--check"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{name}");
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let path = format!("tests/expected/{name}.stderr");
        if std::env::var_os("L2W_BLESS").is_some() {
            std::fs::create_dir_all("tests/expected").unwrap();
            std::fs::write(&path, &stderr).unwrap();
            continue;
        }
        let expected = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("missing {path}; run with L2W_BLESS=1"));
        assert_eq!(
            stderr, expected,
            "{name}: diagnostics changed (run with L2W_BLESS=1 if intended)"
        );
    }
}

// -- 9 ------------------------------------------------------------------------

/// Under the Windows ABI, a callee may overwrite its 32-byte shadow space.
const SHADOW_FUNCTIONS: &str = r#"
#ifdef WIN
#define CHECK if ((uintptr_t)__builtin_frame_address(0) & 15) return -1000000
#else
#define CHECK
#endif
long ABI scribble(long a, long b, long c, long d, long e) {
    CHECK;
    long r = a + 2 * b + 3 * c + 4 * d + 5 * e;
#ifdef WIN
    volatile long *home = (long *)__builtin_frame_address(0) + 2;
    home[0] = home[1] = home[2] = home[3] = -1;
#endif
    return r;
}
"#;

#[test]
fn spec_09_alignment_and_shadow_space() {
    let mut body = String::new();
    // External calls and system calls from every stack alignment: the thunks
    // and shims must align the stack for the Windows callee themselves.
    for pushes in 0..4 {
        for _ in 0..pushes {
            body += "    push 0\n";
        }
        body += "    push 5\n    mov edi, 1\n    mov esi, 2\n    mov edx, 3\n    mov ecx, 4\n    mov r8, 5\n";
        // System V passes the 5th argument in r8; the push is just padding.
        body += "    call scribble\n    add rsp, 8\n    mov rbx, rax\n";
        body += "    mov eax, 39\n    syscall\n";
        for _ in 0..pushes {
            body += "    add rsp, 8\n";
        }
        body += &print("rbx");
    }
    // Callee-saved data next to the thunk frame survives the scribbling.
    body += "    mov r12, 42\n    sub rsp, 16\n    mov qword ptr [rsp], 99\n    mov qword ptr [rsp + 8], 98\n\
             mov edi, 1\n    mov esi, 1\n    mov edx, 1\n    mov ecx, 1\n    mov r8d, 1\n    call scribble\n\
             mov rdi, qword ptr [rsp]\n    add rdi, qword ptr [rsp + 8]\n    add rdi, r12\n    add rsp, 16\n    call print_num\n";
    let Some((out, _)) = same_behavior("alignment", &program(&body, "", ""), SHADOW_FUNCTIONS)
    else {
        return;
    };
    assert_eq!(lines(&out), ["55", "55", "55", "55", "239"]);
}

/// Data below rsp (System V red zone) is volatile on Windows: such
/// accesses are rejected instead of being translated silently.
#[test]
fn spec_09_red_zone_is_rejected() {
    let src = "_start:\n    mov qword ptr [rsp - 8], 1234\n    mov eax, 60\n    syscall\n";
    let d = first_error(src).expect("red zone access must be an error");
    assert!(d.message.contains("System V red zone"), "{}", d.message);
    assert_eq!(d.span.map(|s| s.line), Some(2));
}

// -- 10 -----------------------------------------------------------------------

#[test]
fn spec_10_cross_compilation() {
    if !available("x86_64-w64-mingw32-gcc") {
        eprintln!("skipping: MinGW-w64 not installed");
        return;
    }
    let d = dir().join("spec").join("cross");
    std::fs::create_dir_all(&d).unwrap();
    let programs = [
        ("arithmetic", program(ARITHMETIC, "", "")),
        (
            "conditions",
            program(
                "    mov edi, 1\n    mov esi, 2\n    call conds\n",
                &conditions_function(),
                "",
            ),
        ),
        ("loops", program(LOOPS, "", "")),
    ];
    for (name, src) in &programs {
        let s = d.join(format!("{name}.s"));
        std::fs::write(&s, src).unwrap();
        let exe = d.join(format!("{name}.exe"));
        let out = Command::new(env!("CARGO_BIN_EXE_l2w"))
            .arg(&s)
            .args(["--compile", "-o"])
            .arg(&exe)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let bytes = std::fs::read(&exe).unwrap();
        assert_eq!(&bytes[..2], b"MZ");
        let pe = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
        assert_eq!(&bytes[pe..pe + 4], b"PE\0\0");
        if available("wine") {
            let run = Command::new("wine").arg(&exe).output().unwrap();
            assert!(run.status.success(), "{name} under wine");
        }
    }
}
