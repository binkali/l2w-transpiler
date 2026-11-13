//! Shared helpers for the integration tests: tool detection, translation,
//! re-assembling translated (COFF) code for Linux, and a Win32 mock that
//! lets translated programs run on Linux.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Standard output of `tool arg`, if the tool runs and succeeds.
fn tool_output(tool: &str, arg: &str) -> Option<String> {
    let out = Command::new(tool).arg(arg).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn available(tool: &str) -> bool {
    tool_output(tool, "--version").is_some()
}

/// Candidate names for GNU `as` / `objdump` targeting x86-64 ELF: the
/// native tools on Linux, or cross binutils elsewhere (Homebrew's
/// `x86_64-elf-binutils` on macOS, `binutils-x86-64-linux-gnu` on Debian).
pub const GNU_AS: &[&str] = &["as", "x86_64-linux-gnu-as", "x86_64-elf-as"];
pub const GNU_OBJDUMP: &[&str] = &["objdump", "x86_64-linux-gnu-objdump", "x86_64-elf-objdump"];

/// The first of `candidates` that is a GNU binutils tool. Existence is not
/// enough: on macOS `as` and `objdump` are LLVM tools that accept neither
/// `--64` nor GAS directives such as `.intel_syntax noprefix`.
pub fn gnu_tool(candidates: &[&str]) -> Option<String> {
    candidates
        .iter()
        .find(|c| tool_output(c, "--version").is_some_and(|v| v.starts_with("GNU")))
        .map(|c| c.to_string())
}

/// A C compiler that *targets* x86-64 Linux (its `-dumpmachine` says so),
/// native or cross. Apple clang behind the name `gcc` is not one: it emits
/// Mach-O assembly that l2w does not accept.
pub fn linux_target_gcc() -> Option<String> {
    ["gcc", "x86_64-linux-gnu-gcc"]
        .into_iter()
        .find(|c| {
            tool_output(c, "-dumpmachine")
                .is_some_and(|m| m.starts_with("x86_64") && m.contains("linux"))
        })
        .map(str::to_string)
}

/// A C compiler whose executables run on this host, which must be x86-64
/// Linux: the behavioral tests build and *run* the original Linux program.
pub fn linux_gcc() -> Option<String> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return None;
    }
    linux_target_gcc()
}

pub const LINUX_HOST_HINT: &str = "needs an x86-64 Linux host with gcc";

pub fn dir() -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join("windows");
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn translate(src: &str, name: &str) -> String {
    let p = l2w::parser::parse(src);
    assert!(!p.has_errors(), "{name}: {:#?}", p.diagnostics);
    let l = l2w::ir::lower(&p.program);
    assert!(!l.has_errors(), "{name}: {:#?}", l.diagnostics);
    let t = l2w::transpiler::transpile(&l.module);
    assert!(!t.has_errors(), "{name}: {:#?}", t.diagnostics);
    l2w::codegen::windows::emit(&t, name)
}

pub fn run(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
    assert!(
        out.status.success(),
        "{cmd:?} failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Maps the COFF section directives of a translation back to ELF ones so the
/// translated instructions can be executed on the Linux test host.
pub fn coff_to_elf_sections(asm: &str) -> String {
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

/// Renames the symbol `_start` (not `__l2w_sysv__start`) so the translated
/// entry point does not clash with the C runtime's `_start`.
pub fn rename_entry(asm: &str) -> String {
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
pub fn run_in(exe: &Path, cwd: &Path) -> (String, i32) {
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

pub const WIN32_MOCK: &str = r#"
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
    case ESPIPE: return 1;      /* ERROR_INVALID_FUNCTION: seek on a pipe */
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

/// Result of running a program: (stdout, exit status).
pub type Outcome = (String, i32);

/// Builds `asm` twice and runs both builds in fresh directories:
///
/// * **Linux**: the original assembly as a static `-nostdlib` executable,
///   plus `c_ext` (freestanding C, `ABI` defined empty = System V);
/// * **Windows (emulated)**: the l2w translation, re-assembled for Linux,
///   linked with [`WIN32_MOCK`] and `c_ext` compiled with
///   `ABI = __attribute__((ms_abi))`, i.e. the external functions use the
///   Windows calling convention exactly as real Windows DLL functions would.
///
/// Returns `None` unless this is an x86-64 Linux host with gcc.
pub fn linux_and_windows(name: &str, asm: &str, c_ext: &str) -> Option<(Outcome, Outcome)> {
    let Some(gcc) = linux_gcc() else {
        eprintln!("skipping {name}: {LINUX_HOST_HINT}");
        return None;
    };
    let d = dir().join("spec").join(name);
    std::fs::create_dir_all(&d).unwrap();
    let ext = d.join("ext.c");
    std::fs::write(&ext, format!("#include <stdint.h>\n{c_ext}")).unwrap();
    let cflags = [
        "-O0",
        "-fno-omit-frame-pointer",
        "-fno-stack-protector",
        "-fcf-protection=none",
        "-fno-asynchronous-unwind-tables",
    ];

    let orig_s = d.join("prog.s");
    std::fs::write(&orig_s, asm).unwrap();
    let orig = d.join("prog-linux");
    run(Command::new(&gcc)
        .args([
            "-nostdlib",
            "-static",
            "-no-pie",
            "-ffreestanding",
            "-DABI=",
        ])
        .args(cflags)
        .arg("-o")
        .arg(&orig)
        .arg(&orig_s)
        .arg(&ext));

    let mock = d.join("win32_mock.c");
    std::fs::write(&mock, WIN32_MOCK).unwrap();
    let tr_s = d.join("prog-win.s");
    std::fs::write(
        &tr_s,
        rename_entry(&coff_to_elf_sections(&translate(asm, name))),
    )
    .unwrap();
    let tr = d.join("prog-win");
    run(Command::new(&gcc)
        .args(["-no-pie", "-DWIN", "-DABI=__attribute__((ms_abi))"])
        .args(cflags)
        .arg("-o")
        .arg(&tr)
        .arg(&mock)
        .arg(&ext)
        .arg(&tr_s));

    Some((
        run_in(&orig, &d.join("run-linux")),
        run_in(&tr, &d.join("run-win")),
    ))
}

/// Asserts that the builds behave identically — Linux original, translation
/// on the Win32 mock and, if available, the translated `.exe` under Wine —
/// and returns the outcome.
pub fn same_behavior(name: &str, asm: &str, c_ext: &str) -> Option<Outcome> {
    let (linux, windows) = linux_and_windows(name, asm, c_ext)?;
    assert_eq!(
        linux, windows,
        "{name}: Linux original vs. translated build (Win32 mock)"
    );
    if let Some(wine) = wine_outcome(name, asm, c_ext) {
        assert_eq!(
            linux, wine,
            "{name}: Linux original vs. translated .exe under Wine"
        );
    }
    Some(linux)
}

/// `true` if Windows executables can be built (MinGW-w64) and run (Wine).
pub fn wine_available() -> bool {
    available("x86_64-w64-mingw32-gcc")
        && Command::new("wine")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
}

/// Builds the l2w translation of `asm` as a real Windows executable
/// (MinGW-w64, `-nostdlib -e _start -lkernel32`, `c_ext` compiled natively
/// for Windows) and runs it under Wine in a fresh directory. Returns
/// `None` if MinGW-w64 or Wine is missing.
pub fn wine_outcome(name: &str, asm: &str, c_ext: &str) -> Option<Outcome> {
    if !wine_available() {
        eprintln!("skipping Wine run of {name}: MinGW-w64 or Wine not available");
        return None;
    }
    let d = dir().join("wine").join(name);
    std::fs::create_dir_all(&d).unwrap();
    let ext = d.join("ext.c");
    std::fs::write(&ext, format!("#include <stdint.h>\n{c_ext}")).unwrap();
    let s = d.join("prog.s");
    std::fs::write(&s, translate(asm, name)).unwrap();
    let exe = d.join("prog.exe");
    run(Command::new("x86_64-w64-mingw32-gcc")
        .args([
            "-nostdlib",
            "-e",
            "_start",
            "-O0",
            "-fno-omit-frame-pointer",
            "-ffreestanding",
            "-DWIN",
            "-DABI=",
        ])
        .args([
            "-fno-stack-protector",
            "-fno-asynchronous-unwind-tables",
            "-o",
        ])
        .arg(&exe)
        .arg(&s)
        .arg(&ext)
        .arg("-lkernel32"));
    let cwd = d.join("run");
    let _ = std::fs::remove_dir_all(&cwd);
    std::fs::create_dir_all(&cwd).unwrap();
    let out = Command::new("wine")
        .arg(&exe)
        .current_dir(&cwd)
        .env("WINEDEBUG", "-all")
        .output()
        .unwrap();
    Some((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code().expect("wine exited normally"),
    ))
}
