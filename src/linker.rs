//! Assembling and linking with MinGW-w64 (`--compile`).
//!
//! The translated assembly is assembled and linked by a Windows-targeting
//! GCC driver (`x86_64-w64-mingw32-gcc`, or the compiler named by the
//! environment variable `L2W_CC`). Before it is used, the driver is checked
//! with `-dumpmachine`: a compiler that does not target Windows (e.g. the
//! host `gcc` on Linux) is rejected instead of producing a Linux binary.
//!
//! | Entry point | Link command |
//! |---|---|
//! | `main` | `gcc prog.s -o prog.exe` (MinGW C runtime calls `main`) |
//! | `_start` | `gcc -nostdlib -e _start prog.s -o prog.exe [-lmsvcrt] -lkernel32` |
//!
//! `_start` programs do not get the C runtime startup code; the system call
//! shims only need kernel32. If such a program calls external functions
//! (e.g. `puts`), msvcrt is linked as well: `msvcrt.dll` initializes itself
//! when it is loaded. Additional libraries are passed with `-l`.

use crate::diagnostics::Diagnostic;
use crate::ir::EntryKind;
use crate::transpiler::Translation;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Environment variable that overrides the compiler driver.
pub const CC_ENV: &str = "L2W_CC";

/// Default MinGW-w64 driver names, tried in order.
pub fn default_drivers() -> &'static [&'static str] {
    if cfg!(windows) {
        &["x86_64-w64-mingw32-gcc", "gcc"]
    } else {
        &["x86_64-w64-mingw32-gcc"]
    }
}

pub const INSTALL_HINT: &str = "install MinGW-w64:\n\
    \x20 Debian/Ubuntu/Kali:  sudo apt install gcc-mingw-w64-x86-64\n\
    \x20 Fedora:              sudo dnf install mingw64-gcc\n\
    \x20 Arch:                sudo pacman -S mingw-w64-gcc\n\
    \x20 macOS (Homebrew):    brew install mingw-w64\n\
    \x20 Windows (MSYS2):     pacman -S mingw-w64-x86_64-gcc\n\
    or set L2W_CC to a Windows-targeting gcc";

/// How the executable is linked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkPlan {
    /// `Some(symbol)` for a raw entry point (`_start`), `None` for `main`.
    pub raw_entry: Option<String>,
    /// Libraries in link order (without `-l`).
    pub libs: Vec<String>,
}

/// Decides how to link a translation.
pub fn plan(t: &Translation, user_libs: &[String]) -> Result<LinkPlan, Diagnostic> {
    let platform = &t.module.platform;
    let Some(entry) = &platform.entry else {
        return Err(Diagnostic::global_error(
            "cannot build an executable: the program has no entry point (`_start` or `main`)",
        )
        .with_help("define `_start` or a global `main`, or use `-o out.s` to get assembly for linking into another program"));
    };
    let mut libs: Vec<String> = user_libs.to_vec();
    match entry.kind {
        EntryKind::Main => Ok(LinkPlan {
            raw_entry: None,
            libs,
        }),
        EntryKind::Start => {
            if !platform.externals.is_empty() {
                libs.push("msvcrt".into());
            }
            libs.push("kernel32".into());
            Ok(LinkPlan {
                raw_entry: Some(entry.symbol.clone()),
                libs,
            })
        }
    }
}

/// Driver arguments for linking `asm` into `exe`.
pub fn link_args(plan: &LinkPlan, asm: &Path, exe: &Path) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(entry) = &plan.raw_entry {
        args.extend(["-nostdlib".to_string(), "-e".to_string(), entry.clone()]);
    }
    args.push(asm.display().to_string());
    args.extend(["-o".to_string(), exe.display().to_string()]);
    args.extend(plan.libs.iter().map(|l| format!("-l{l}")));
    args
}

/// Failure while building the executable.
#[derive(Debug)]
pub enum LinkError {
    /// The toolchain is missing or unusable (exit code 2).
    Tool(Diagnostic),
    /// The assembler or linker rejected the program (exit code 1).
    Build(Diagnostic),
}

/// A usable compiler driver.
#[derive(Debug, Clone)]
pub struct Driver {
    pub program: String,
    pub machine: String,
}

/// Finds a Windows-targeting compiler driver.
pub fn find_driver() -> Result<Driver, LinkError> {
    let candidates: Vec<String> = match std::env::var(CC_ENV) {
        Ok(cc) if !cc.is_empty() => vec![cc],
        _ => default_drivers().iter().map(|s| s.to_string()).collect(),
    };
    let mut rejected = Vec::new();
    for program in &candidates {
        let Ok(out) = Command::new(program).arg("-dumpmachine").output() else {
            continue;
        };
        let machine = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && targets_windows(&machine) {
            return Ok(Driver {
                program: program.clone(),
                machine,
            });
        }
        rejected.push(format!("`{program}` targets `{machine}`"));
    }
    let msg = if rejected.is_empty() {
        format!(
            "no MinGW-w64 compiler found (tried {})",
            candidates
                .iter()
                .map(|c| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        format!(
            "no Windows-targeting compiler found: {}",
            rejected.join(", ")
        )
    };
    Err(LinkError::Tool(
        Diagnostic::global_error(msg).with_help(INSTALL_HINT),
    ))
}

/// `true` for x86-64 Windows target triples (`x86_64-w64-mingw32`, ...).
pub fn targets_windows(machine: &str) -> bool {
    machine.starts_with("x86_64") && (machine.contains("mingw") || machine.contains("windows"))
}

/// Symbols from `undefined reference to `sym'` linker messages.
pub fn undefined_references(stderr: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in stderr.lines() {
        if let Some(rest) = line.split("undefined reference to `").nth(1) {
            if let Some(sym) = rest.split('\'').next() {
                if !out.iter().any(|s| s == sym) {
                    out.push(sym.to_string());
                }
            }
        }
    }
    out
}

/// Assembles and links `asm` (Windows assembly text) into `exe`.
///
/// The assembly is written to `asm_path` if given (kept), otherwise to a
/// temporary file that is removed afterwards. `log` receives progress
/// messages (verbose output).
pub fn build(
    asm: &str,
    plan: &LinkPlan,
    exe: &Path,
    asm_path: Option<&Path>,
    log: &dyn Fn(&str),
) -> Result<(), LinkError> {
    let driver = find_driver()?;
    log(&format!(
        "linker: {} (target {})",
        driver.program, driver.machine
    ));
    let (path, temporary) = match asm_path {
        Some(p) => (p.to_path_buf(), false),
        None => (temp_path(exe), true),
    };
    std::fs::write(&path, asm).map_err(|e| {
        LinkError::Tool(Diagnostic::global_error(format!(
            "cannot write `{}`: {e}",
            path.display()
        )))
    })?;
    let args = link_args(plan, &path, exe);
    log(&format!("running: {} {}", driver.program, args.join(" ")));
    let result = Command::new(&driver.program).args(&args).output();
    if temporary {
        let _ = std::fs::remove_file(&path);
    }
    let out = result.map_err(|e| {
        LinkError::Tool(Diagnostic::global_error(format!(
            "cannot run `{}`: {e}",
            driver.program
        )))
    })?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim_end().to_string();
    let mut d = Diagnostic::global_error(format!(
        "`{}` failed to assemble or link the translated program:\n{stderr}",
        driver.program
    ));
    let undefined = undefined_references(&stderr);
    if !undefined.is_empty() {
        let names: Vec<String> = undefined.iter().map(|s| format!("`{s}`")).collect();
        d = d.with_help(format!(
            "{} {} not defined by the program or the linked libraries ({}); pass the library that provides {} with `-l <name>`",
            names.join(", "),
            if names.len() == 1 { "is" } else { "are" },
            plan.libs.iter().map(|l| format!("-l{l}")).collect::<Vec<_>>().join(" ").if_empty("C runtime defaults"),
            if names.len() == 1 { "it" } else { "them" },
        ));
    }
    Err(LinkError::Build(d))
}

trait IfEmpty {
    fn if_empty(self, default: &str) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, default: &str) -> String {
        if self.is_empty() {
            default.to_string()
        } else {
            self
        }
    }
}

fn temp_path(exe: &Path) -> PathBuf {
    let stem = exe
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".into());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("l2w-{}-{nanos}-{stem}.s", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translation(src: &str) -> Translation {
        let p = crate::parser::parse(src);
        let l = crate::ir::lower(&p.program);
        crate::transpiler::transpile(&l.module)
    }

    #[test]
    fn plans() {
        let t = translation(".globl _start\n_start:\n mov eax, 60\n xor edi, edi\n syscall\n");
        let p = plan(&t, &[]).unwrap();
        assert_eq!(p.raw_entry.as_deref(), Some("_start"));
        assert_eq!(p.libs, ["kernel32"]);
        assert_eq!(
            link_args(&p, Path::new("a.s"), Path::new("a.exe")),
            [
                "-nostdlib",
                "-e",
                "_start",
                "a.s",
                "-o",
                "a.exe",
                "-lkernel32"
            ]
        );

        let t = translation(".globl _start\n_start:\n call puts\n mov eax, 60\n syscall\n");
        assert_eq!(
            plan(&t, &["user32".into()]).unwrap().libs,
            ["user32", "msvcrt", "kernel32"]
        );

        let t = translation(".globl main\nmain:\n xor eax, eax\n ret\n");
        let p = plan(&t, &[]).unwrap();
        assert_eq!(p.raw_entry, None);
        assert_eq!(
            link_args(&p, Path::new("a.s"), Path::new("a.exe")),
            ["a.s", "-o", "a.exe"]
        );

        let t = translation(".globl f\nf:\n ret\n");
        assert!(plan(&t, &[])
            .unwrap_err()
            .message
            .contains("no entry point"));
    }

    #[test]
    fn triples_and_linker_output() {
        assert!(targets_windows("x86_64-w64-mingw32"));
        assert!(targets_windows("x86_64-pc-windows-gnu"));
        assert!(!targets_windows("x86_64-linux-gnu"));
        assert!(!targets_windows("i686-w64-mingw32"));
        let err = "/usr/bin/ld: a.o:fake:(.text+0x5): undefined reference to `frob'\n\
                   /usr/bin/ld: a.o:fake:(.text+0x9): undefined reference to `frob'\n\
                   ld: undefined reference to `knob'\n";
        assert_eq!(undefined_references(err), ["frob", "knob"]);
    }
}
