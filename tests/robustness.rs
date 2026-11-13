//! Error-handling robustness: no input may crash l2w.
//!
//! Deterministic mutation fuzzing: the examples are mutated (bytes
//! deleted, duplicated, replaced by interesting tokens, lines swapped) and
//! every mutant is run through the whole pipeline (lexer, parser, IR,
//! transpiler, code generation, diagnostics rendering). Each stage must
//! return diagnostics instead of panicking, and every error must either
//! have a valid source position or be a global error.

use l2w::diagnostics::SourceFile;
use std::panic;

const EXAMPLES: &[(&str, &str)] = &[
    ("hello", include_str!("../examples/hello.s")),
    ("hello_world", include_str!("../examples/hello_world.s")),
    ("loop", include_str!("../examples/loop.s")),
    ("functions", include_str!("../examples/functions.s")),
    ("library", include_str!("../examples/library.s")),
    ("errors", include_str!("../examples/errors.s")),
    (
        "errors_semantic",
        include_str!("../examples/errors_semantic.s"),
    ),
];

const TOKENS: &[&str] = &[
    "[",
    "]",
    "+",
    "-",
    "*",
    ",",
    ":",
    "\"",
    "'",
    "\\",
    ".",
    "rsp",
    "rip",
    "ah",
    "r12b",
    "qword ptr",
    "byte ptr",
    "offset",
    "@PLT",
    "@GOTPCREL",
    "0x",
    "0b",
    "1f",
    "1b",
    "9999999999999999999999",
    "-128",
    ".text",
    ".data",
    ".bss",
    ".section .rodata",
    ".globl",
    ".set x,",
    ".comm",
    ".quad",
    ".byte",
    ".zero",
    ".align 3",
    "syscall",
    "call",
    "jmp",
    "ret",
    "push",
    "pop",
    "lea",
    "mov",
    "xchg",
    "imul",
    "\n",
    ";",
    "#",
    "/*",
    "*/",
    "_start:",
    "main:",
    "1:",
    ".L0:",
    "fs:",
    "%rax",
    "$1",
];

/// xorshift64*: small, deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn mutate(src: &str, rng: &mut Rng) -> String {
    let mut chars: Vec<char> = src.chars().collect();
    for _ in 0..1 + rng.below(4) {
        let len = chars.len();
        let at = rng.below(len + 1);
        match rng.below(5) {
            0 if len > 0 => {
                let n = 1 + rng.below(8);
                chars.drain(at.min(len - 1)..(at + n).min(len));
            }
            1 if len > 0 => {
                let end = (at + 1 + rng.below(30)).min(len);
                let piece: Vec<char> = chars[at.min(end)..end].to_vec();
                chars.splice(at..at, piece);
            }
            2 => {
                let tok: Vec<char> = TOKENS[rng.below(TOKENS.len())].chars().collect();
                chars.splice(at..at, tok);
            }
            3 if len > 0 => {
                let tok: Vec<char> = TOKENS[rng.below(TOKENS.len())].chars().collect();
                let end = (at + 1 + rng.below(6)).min(len);
                chars.splice(at.min(end)..end, tok);
            }
            _ => {
                let mut lines: Vec<String> = chars
                    .iter()
                    .collect::<String>()
                    .lines()
                    .map(String::from)
                    .collect();
                if lines.len() > 1 {
                    let (a, b) = (rng.below(lines.len()), rng.below(lines.len()));
                    lines.swap(a, b);
                }
                chars = (lines.join("\n") + "\n").chars().collect();
            }
        }
    }
    chars.into_iter().collect()
}

/// Runs the whole pipeline; panics (with the input) only on a crash or an
/// invalid diagnostic position.
fn pipeline(name: &str, src: &str) {
    let file = SourceFile::new(name.to_string(), src.to_string());
    let lines: Vec<&str> = src.split('\n').collect();
    let check = |diags: &[l2w::diagnostics::Diagnostic]| {
        for d in diags {
            if let Some(span) = d.span {
                let (line, col) = (span.line as usize, span.col as usize);
                assert!(
                    line >= 1 && line <= lines.len(),
                    "{name}: bad line in {d:?}\n---\n{src}"
                );
                let width = lines[line - 1].chars().count();
                assert!(
                    col >= 1 && col <= width + 1,
                    "{name}: bad column in {d:?}\n---\n{src}"
                );
            }
            let _ = d.render(&file);
        }
    };
    let lexed = l2w::lexer::tokenize(src);
    check(&lexed.diagnostics);
    let parsed = l2w::parser::parse(src);
    check(&parsed.diagnostics);
    if parsed.has_errors() {
        return;
    }
    let lowered = l2w::ir::lower(&parsed.program);
    check(&lowered.diagnostics);
    let _ = lowered.module.summary();
    let _ = lowered.module.to_string();
    if lowered.has_errors() {
        return;
    }
    let t = l2w::transpiler::transpile(&lowered.module);
    check(&t.diagnostics);
    if !t.has_errors() {
        let _ = l2w::codegen::windows::emit(&t, name);
        let _ = l2w::linker::plan(&t, &[]);
    }
}

#[test]
fn mutated_examples_never_crash() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut failures = Vec::new();
    let iterations = std::env::var("L2W_FUZZ_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    for i in 0..iterations {
        let (name, src) = EXAMPLES[i % EXAMPLES.len()];
        let mutant = mutate(src, &mut rng);
        if let Err(e) = panic::catch_unwind(|| pipeline(name, &mutant)) {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            failures.push(format!(
                "#{i} ({name}): {}\n--- input ---\n{mutant}",
                msg.lines().next().unwrap_or("")
            ));
            if failures.len() >= 5 {
                break;
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} crash(es):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn pathological_inputs() {
    for src in [
        "",
        "\n\n\n",
        "\0",
        "\u{feff}mov rax, 1\n",
        "mov rax, 1",
        "é: mov rax, 1\n",
        "mov rax, \"\n",
        "/* never closed",
        "[[[[[[[[",
        "mov rax, ((((((((((((((((((((((((((((((((((1))))))))))))))))))))))))))))))))))\n",
        "mov rax, 1 << 200\n",
        "mov rax, -9223372036854775808\n",
        ".set a, b\n.set b, a\nmov rax, a\n",
        ".set a, a + 1\nmov rax, a\n",
        ".zero -1\n",
        ".zero 99999999999999\n",
        ".align 63\n",
        ".p2align 200\n",
        "x: x: ret\n",
        "jmp 1f\n",
        "1:\n1:\njmp 1b\n",
        "call [rip + x]\n",
        "lea rsp, [rsp - 128]\n",
        "_start:\n_start:\n",
        ".globl main\nmain:\n",
        ".globl _start\n_start: jmp _start\n",
        "mov eax, 1\nsyscall\n",
    ] {
        pipeline("pathological", src);
    }
    let deep = format!("mov rax, {}1{}\n", "(".repeat(5000), ")".repeat(5000));
    pipeline("deep", &deep);
    let long = "add rax, 1\n".repeat(20000);
    pipeline("long", &long);
    let mut chain = String::from(".set a0, 1\n");
    for i in 1..5000 {
        chain += &format!(".set a{i}, a{} + 1\n", i - 1);
    }
    chain += "mov rax, a4999\nmov rbx, offset a4999\n.data\n.quad a4999\n";
    pipeline("chain", &chain);
    // Chains within the limit are evaluated exactly; longer ones are errors.
    let chain = |n: usize| {
        let mut s = String::from(".set a0, 1\n");
        for i in 1..n {
            s += &format!(".set a{i}, a{} + 1\n", i - 1);
        }
        s + &format!("mov rax, a{}\n", n - 1)
    };
    let l = l2w::ir::lower(&l2w::parser::parse(&chain(90)).program);
    assert!(!l.has_errors(), "{:#?}", l.diagnostics);
    assert!(l.module.to_string().contains("mov rax, offset a89"));
    let a89 = l2w::ast::Expr::Symbol(l2w::ast::SymbolRef {
        name: "a89".into(),
        modifier: None,
    });
    assert_eq!(l.module.symbols.eval(&a89), Some(90));
    let l = l2w::ir::lower(&l2w::parser::parse(&chain(150)).program);
    assert!(l
        .diagnostics
        .iter()
        .any(|d| d.message.contains("chain of more than 100 equates")));
}
