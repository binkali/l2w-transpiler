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
    let o = l2w(&["examples/library.s", "--check"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stderr(&o).contains(": ok (translatable to windows)"));
}

#[test]
fn syscalls_are_translated_or_reported() {
    for ex in [
        "examples/hello.s",
        "examples/loop.s",
        "examples/functions.s",
    ] {
        let o = l2w(&[ex, "--check"]);
        assert_eq!(o.status.code(), Some(0), "{ex}: {}", stderr(&o));
    }
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    let path = dir.join("cli-mmap.s");
    std::fs::write(
        &path,
        ".globl _start\n_start:\n    mov eax, 9\n    syscall\n",
    )
    .unwrap();
    let o = l2w(&[path.to_str().unwrap(), "--check"]);
    assert_eq!(o.status.code(), Some(1));
    let err = stderr(&o);
    assert!(
        err.contains(":4:5") && err.contains("`mmap` (9) has no Windows translation"),
        "{err}"
    );
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
    let o = l2w(&["examples/hello.s", "--dump-ir", "--verbose"]);
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
fn transpile_writes_windows_assembly() {
    let out = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("cli")
        .join("library.win.s");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let o = l2w(&[
        "examples/library.s",
        "-o",
        out.to_str().unwrap(),
        "--target",
        "windows",
        "--verbose",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    let asm = std::fs::read_to_string(&out).unwrap();
    assert!(asm.contains(".section .rdata,\"dr\""), "{asm}");
    assert!(asm.contains("mov rax, qword ptr [rip + counter]"), "{asm}");
    assert!(asm.contains("lea rax, [rip + greeting]"), "{asm}");
    assert!(
        asm.contains(".def fib_at; .scl 2; .type 32; .endef"),
        "{asm}"
    );
    assert!(
        !asm.contains("@function") && !asm.contains(".note"),
        "{asm}"
    );
    let e = stderr(&o);
    assert!(e.contains("dropped section `.note.GNU-stack`"), "{e}");
    assert!(e.contains("common symbol `shared` defined in .bss"), "{e}");

    // Without -o the assembly goes to stdout.
    let o = l2w(&["examples/library.s"]);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains(".intel_syntax noprefix"));
}

fn tool_works(tool: &str, arg: &str) -> bool {
    Command::new(tool)
        .arg(arg)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn l2w_env(args: &[&str], key: &str, value: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_l2w"))
        .args(args)
        .env(key, value)
        .output()
        .expect("run l2w")
}

/// PE imports (DLL names) of an executable, via MinGW objdump.
fn imported_dlls(exe: &Path) -> Option<Vec<String>> {
    let out = Command::new("x86_64-w64-mingw32-objdump")
        .arg("-p")
        .arg(exe)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    Some(
        text.lines()
            .filter_map(|l| l.trim().strip_prefix("DLL Name: "))
            .map(|s| s.to_ascii_uppercase())
            .collect(),
    )
}

#[test]
fn compile_builds_windows_executables() {
    if !tool_works("x86_64-w64-mingw32-gcc", "-dumpmachine") {
        eprintln!("skipping: MinGW-w64 not installed");
        return;
    }
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("compile");
    std::fs::create_dir_all(&dir).unwrap();
    let wine = tool_works("wine", "--version");
    for (ex, expected) in [
        ("hello", "Hello, World!\n"),
        ("loop", ""),
        ("functions", ""),
    ] {
        let exe = dir.join(format!("{ex}.exe"));
        let _ = std::fs::remove_file(&exe);
        let o = l2w(&[
            &format!("examples/{ex}.s"),
            "--compile",
            "-o",
            exe.to_str().unwrap(),
        ]);
        assert_eq!(o.status.code(), Some(0), "{ex}: {}", stderr(&o));
        let bytes = std::fs::read(&exe).unwrap();
        assert_eq!(&bytes[..2], b"MZ", "{ex}");
        let pe = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
        assert_eq!(&bytes[pe..pe + 4], b"PE\0\0", "{ex}");
        assert_eq!(
            u16::from_le_bytes([bytes[pe + 4], bytes[pe + 5]]),
            0x8664,
            "{ex}: AMD64 machine"
        );
        if let Some(dlls) = imported_dlls(&exe) {
            assert_eq!(dlls, ["KERNEL32.DLL"], "{ex}");
        }
        if wine && !expected.is_empty() {
            let out = Command::new("wine").arg(&exe).output().unwrap();
            assert_eq!(
                String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"),
                expected
            );
        }
    }

    // --keep-asm keeps the Windows assembly next to the executable.
    let exe = dir.join("kept.exe");
    let o = l2w(&[
        "examples/hello.s",
        "--compile",
        "-o",
        exe.to_str().unwrap(),
        "--keep-asm",
    ]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let asm = std::fs::read_to_string(dir.join("kept.s")).unwrap();
    assert!(asm.contains("call __l2w_sys_write"), "{asm}");

    // A `main` program using the C runtime (gcc -O0 output with printf).
    if tool_works("gcc", "--version") {
        let c = dir.join("prog.c");
        std::fs::write(&c, "#include <stdio.h>\nstatic long t[4];\nint add(int a, int b) { int c = a + b; return c; }\n\
                            int main(void) { for (int i = 0; i < 4; i++) t[i] = add(i, i); printf(\"%ld\\n\", t[3]); return 0; }\n").unwrap();
        let s = dir.join("prog.s");
        let gcc = Command::new("gcc")
            .args([
                "-O0",
                "-S",
                "-masm=intel",
                "-fno-asynchronous-unwind-tables",
                "-fcf-protection=none",
                "-fno-stack-protector",
                "-o",
            ])
            .arg(&s)
            .arg(&c)
            .output()
            .unwrap();
        assert!(gcc.status.success());
        let exe = dir.join("prog.exe");
        let o = l2w(&[
            s.to_str().unwrap(),
            "--compile",
            "-o",
            exe.to_str().unwrap(),
        ]);
        assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
        if let Some(dlls) = imported_dlls(&exe) {
            assert!(
                dlls.iter()
                    .any(|d| d.contains("MSVCRT") || d.starts_with("API-MS-WIN-CRT")),
                "{dlls:?}"
            );
        }
        if wine {
            let out = Command::new("wine").arg(&exe).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "6");
        }
    }

    // Undefined external: linker error with a hint, exit code 1.
    let p = write_temp(
        "undef.s",
        ".globl _start\n_start:\n    call frobnicate\n    mov eax, 60\n    syscall\n",
    );
    let o = l2w(&[
        p.to_str().unwrap(),
        "--compile",
        "-o",
        dir.join("undef.exe").to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(1));
    let e = stderr(&o);
    assert!(
        e.contains("undefined reference to `frobnicate'")
            && e.contains("`frobnicate` is not defined"),
        "{e}"
    );
}

/// LP64 -> LLP64: `long` values must reach the Windows C runtime at full
/// width (`labs` -> `_abs64`, `%ld` -> `%I64d`, glibc `__isoc23_` aliases).
const LP64_PROGRAM: &str = r#"#include <stdio.h>
#include <stdlib.h>
#include <string.h>
int main(void) {
    volatile int sh = 40;
    long big = 1L << sh;
    unsigned long ub = ((unsigned long)0xfedcba98u << (sh - 8)) | 0x76543210u;
    char buf[128];
    long parsed = 0;
    printf("%ld %lu %lx %lX\n", big, ub, ub, big);
    printf("%5ld|%-14ld|%+ld|%lld|%zu|%%ld|%d\n", -big, big, big, -big, (size_t)big, 42);
    printf("%s %c %li %lo\n", "str", 'c', -big, big);
    printf("labs %ld\n", labs(-big));
    printf("strtol %ld\n", strtol("1099511627776", NULL, 10));
    printf("strtoul %lu\n", strtoul("18364758544493064720", NULL, 10));
    printf("atol %ld\n", atol("-1099511627777"));
    snprintf(buf, sizeof buf, "%ld/%lu", -big, ub);
    puts(buf);
    sprintf(buf, "%ld", big * 3);
    puts(buf);
    sscanf("1099511627779", "%ld", &parsed);
    printf("sscanf %ld\n", parsed);
    printf("%ld %ld %ld %ld %ld %ld %ld\n", big+1, big+2, big+3, big+4, big+5, big+6, big+7);
    return 0;
}
"#;

#[test]
fn long_values_survive_the_windows_c_runtime() {
    if !tool_works("x86_64-w64-mingw32-gcc", "-dumpmachine") || !tool_works("gcc", "--version") {
        eprintln!("skipping: gcc or MinGW-w64 not installed");
        return;
    }
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("lp64");
    std::fs::create_dir_all(&dir).unwrap();
    let c = dir.join("lp64.c");
    std::fs::write(&c, LP64_PROGRAM).unwrap();
    let flags = [
        "-O0",
        "-fno-builtin",
        "-fcf-protection=none",
        "-fno-stack-protector",
    ];
    let s = dir.join("lp64.s");
    let gcc = Command::new("gcc")
        .args(flags)
        .args(["-S", "-masm=intel", "-fno-asynchronous-unwind-tables", "-o"])
        .arg(&s)
        .arg(&c)
        .output()
        .unwrap();
    assert!(
        gcc.status.success(),
        "{}",
        String::from_utf8_lossy(&gcc.stderr)
    );
    let native = dir.join("lp64");
    assert!(Command::new("gcc")
        .args(flags)
        .arg("-o")
        .arg(&native)
        .arg(&c)
        .status()
        .unwrap()
        .success());
    let expected = Command::new(&native).output().unwrap();
    let exe = dir.join("lp64.exe");
    let o = l2w(&[
        s.to_str().unwrap(),
        "--compile",
        "-o",
        exe.to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    if tool_works("wine", "--version") {
        let out = Command::new("wine").arg(&exe).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"),
            String::from_utf8_lossy(&expected.stdout)
        );
    }
}

#[test]
fn compile_reports_toolchain_and_entry_problems() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("compile");
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("never.exe");
    let exe = exe.to_str().unwrap();
    // Missing compiler: exit code 2 with installation hints.
    let o = l2w_env(
        &["examples/hello.s", "--compile", "-o", exe],
        "L2W_CC",
        "/nonexistent/l2w-cc",
    );
    assert_eq!(o.status.code(), Some(2));
    let e = stderr(&o);
    assert!(
        e.contains("no MinGW-w64 compiler found") && e.contains("apt install gcc-mingw-w64-x86-64"),
        "{e}"
    );
    // A compiler that does not target Windows is refused.
    if tool_works("gcc", "-dumpmachine") {
        let o = l2w_env(
            &["examples/hello.s", "--compile", "-o", exe],
            "L2W_CC",
            "gcc",
        );
        assert_eq!(o.status.code(), Some(2));
        assert!(
            stderr(&o).contains("no Windows-targeting compiler found: `gcc` targets"),
            "{}",
            stderr(&o)
        );
    }
    // No entry point.
    let o = l2w(&["examples/library.s", "--compile", "-o", exe]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("no entry point"));
    assert!(!Path::new(exe).exists());
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
