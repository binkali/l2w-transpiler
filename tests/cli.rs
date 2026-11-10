//! End-to-end tests of the `l2w` binary.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn l2w(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_l2w"))
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run l2w")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn write_temp(name: &str, content: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("cli");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, content).unwrap();
    p
}

#[test]
fn check_accepts_examples() {
    for ex in [
        "examples/hello.s",
        "examples/loop.s",
        "examples/functions.s",
    ] {
        let o = l2w(&[ex, "--check"]);
        assert!(o.status.success(), "{ex}: {}", stderr(&o));
        assert!(stderr(&o).contains(": ok"));
    }
}

#[test]
fn check_reports_errors_with_line_and_column() {
    let o = l2w(&["examples/errors.s", "--check"]);
    assert_eq!(o.status.code(), Some(1));
    let e = stderr(&o);
    assert!(e.contains("error: invalid scale factor `3`"), "{e}");
    assert!(e.contains("--> examples/errors.s:3:19"), "{e}");
    assert!(e.contains("error: unsupported directive `.weak`"), "{e}");
    assert!(e.contains("--> examples/errors.s:5:5"), "{e}");
    assert!(e.contains("aborting due to 3 errors"), "{e}");
}

#[test]
fn verbose_prints_statistics() {
    let o = l2w(&["examples/hello.s", "--check", "--verbose"]);
    assert!(o.status.success());
    let e = stderr(&o);
    assert!(e.contains("[l2w] lexed"), "{e}");
    assert!(e.contains("IR: 2 sections"), "{e}");
    assert!(e.contains("2 syscalls"), "{e}");
    assert!(e.contains("8 instructions, 2 labels, 6 directives"), "{e}");
}

#[test]
fn dump_ast_and_tokens() {
    let o = l2w(&["examples/hello.s", "--dump-ast"]);
    assert!(o.status.success());
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("mnemonic: \"syscall\""), "{out}");

    let o = l2w(&["examples/hello.s", "--dump-tokens"]);
    assert!(o.status.success());
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("directive `.intel_syntax`"), "{out}");
    assert!(out.contains("register `rax`"), "{out}");
}

#[test]
fn usage_errors_exit_with_code_2() {
    let o = l2w(&[]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("no input file"));

    let o = l2w(&["examples/hello.s", "--target", "macos"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("unsupported target"));

    let o = l2w(&["does/not/exist.s", "--check"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("cannot read"));
}

#[test]
fn help_and_version() {
    let o = l2w(&["--help"]);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("--compile"));
    let o = l2w(&["--version"]);
    assert!(String::from_utf8_lossy(&o.stdout).starts_with("l2w "));
}

#[test]
fn transpile_is_rejected_until_implemented() {
    let o = l2w(&["examples/hello.s", "-o", "/dev/null"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("not implemented yet"));
}

#[test]
fn lexer_errors_are_reported() {
    let p = write_temp("lexerr.s", "mov rax, \"unterminated\nmov $1, rax\n");
    let o = l2w(&[p.to_str().unwrap(), "--check"]);
    assert_eq!(o.status.code(), Some(1));
    let e = stderr(&o);
    assert!(e.contains("unterminated string literal"), "{e}");
    assert!(e.contains("lexerr.s:1:10"), "{e}");
    assert!(e.contains("unexpected `$`"), "{e}");
}

#[test]
fn check_reports_semantic_errors() {
    let o = l2w(&["examples/errors_semantic.s", "--check"]);
    assert_eq!(o.status.code(), Some(1));
    let e = stderr(&o);
    for (msg, loc) in [
        (
            "operand size mismatch for `mov`: `rax` is 64-bit, but `ebx` is 32-bit",
            "errors_semantic.s:6:14",
        ),
        ("ambiguous operand size for `inc`", "errors_semantic.s:7:5"),
        (
            "does not fit in a sign-extended 32-bit immediate",
            "errors_semantic.s:8:14",
        ),
        ("unsupported instruction `cpuid`", "errors_semantic.s:9:5"),
        (
            "no local label `1:` before this reference",
            "errors_semantic.s:10:5",
        ),
        ("`len` is defined after this use", "errors_semantic.s:11:14"),
    ] {
        assert!(e.contains(msg), "missing `{msg}` in:\n{e}");
        assert!(e.contains(loc), "missing `{loc}` in:\n{e}");
    }
    assert!(e.contains("aborting due to 5 errors"), "{e}");
}

#[test]
fn dump_ir_shows_analysis_and_listing() {
    let o = l2w(&["examples/hello.s", "--dump-ir"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("entry: _start (Start)"), "{out}");
    assert!(
        out.contains("rax=1 (write) rdi=1 rsi=&msg rdx=len"),
        "{out}"
    );
    assert!(out.contains("rax=60 (exit) rdi=0"), "{out}");
    assert!(
        out.contains("len                  equate (absolute) = . - msg"),
        "{out}"
    );
    assert!(out.contains("    mov rdx, offset len"), "{out}");
}
