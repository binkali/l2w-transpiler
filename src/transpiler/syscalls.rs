//! System call translation pass.
//!
//! Every `syscall` whose number is statically known and supported is
//! replaced by a call to a Win32-based shim (see
//! [`crate::compatibility::syscalls`]); the shims and the file-descriptor
//! table are appended to the program. Unknown or unsupported system calls
//! are errors.

use super::bridge::snippet_items;
use super::Transpiler;
use crate::compatibility::syscalls as sys;
use crate::diagnostics::Diagnostic;
use crate::ir::*;

fn describe(n: i64) -> String {
    match sys::linux_syscall_name(n) {
        Some(name) => format!("`{name}` ({n})"),
        None => format!("{n}"),
    }
}

impl Transpiler {
    pub(super) fn translate_syscalls(&mut self) {
        let sites = self.module.platform.syscalls.clone();
        let mut plan: Vec<(Loc, &'static str)> = Vec::new();
        for sc in &sites {
            if !self.is_kept(sc.at.section) {
                continue;
            }
            let Value::Const(n) = sc.number else {
                self.diags.push(
                    Diagnostic::error(
                        "the system call number in `rax` cannot be determined statically",
                        sc.span,
                    )
                    .with_help(
                        "load the number with `mov eax, N` before `syscall`, in the same basic block (no label or call in between)",
                    ),
                );
                continue;
            };
            let Some(shim) = sys::shim_for(n) else {
                self.diags.push(
                    Diagnostic::error(
                        format!(
                            "Linux system call {} has no Windows translation",
                            describe(n)
                        ),
                        sc.span,
                    )
                    .with_help(format!("supported system calls: {}", sys::SUPPORTED)),
                );
                continue;
            };
            if shim.key == "open" {
                if let Value::Const(flags) = sc.args[1] {
                    let bad = flags & !sys::OPEN_SUPPORTED_FLAGS;
                    if bad != 0 || flags & sys::O_ACCMODE == sys::O_ACCMODE {
                        self.diags.push(
                            Diagnostic::error(
                                format!("`open` flags {flags:#o} cannot be translated (unsupported bits {bad:#o})"),
                                sc.span,
                            )
                            .with_help("supported: O_RDONLY, O_WRONLY, O_RDWR, O_CREAT, O_EXCL, O_TRUNC, O_APPEND, O_NOCTTY, O_LARGEFILE, O_CLOEXEC"),
                        );
                        continue;
                    }
                }
            }
            self.notes.push(format!(
                "syscall: {} at line {} -> {} ({})",
                describe(n),
                sc.span.line,
                sys::shim_name(shim.key),
                shim.win32
            ));
            plan.push((sc.at, shim.key));
        }

        let mut keys: Vec<&str> = Vec::new();
        for (_, k) in &plan {
            if !keys.contains(k) {
                keys.push(k);
            }
        }
        if keys.is_empty() {
            return;
        }
        for name in sys::WIN32_IMPORTS {
            if self
                .module
                .symbols
                .get(name)
                .is_some_and(|s| s.is_defined())
            {
                self.diags.push(Diagnostic::global_error(format!(
                    "the program defines `{name}`, which the system call runtime imports from the Windows API"
                )));
            }
        }

        // Rewrite from the back so that pending locations stay valid. The
        // replacement goes *before* the syscall item (whose slot receives
        // the last instruction), so it always stays inside its chunk.
        plan.sort_by_key(|(at, _)| std::cmp::Reverse((at.section, at.item)));
        for (at, key) in plan {
            let span = match &self.module.sections[at.section].items[at.item] {
                Item::Instr(ins) => ins.span,
                other => unreachable!("syscall site is not an instruction: {other:?}"),
            };
            let mut items = snippet_items(&sys::call_site(key));
            for item in &mut items {
                if let Item::Instr(ins) = item {
                    ins.span = span;
                    ins.operand_spans.iter_mut().for_each(|s| *s = span);
                }
            }
            let last = items.pop().expect("call site has instructions");
            self.module.sections[at.section].items[at.item] = last;
            self.module.insert_items(at, items);
        }

        let code = sys::runtime_code(&keys)
            .into_iter()
            .map(|(name, src)| (name, src, false))
            .collect();
        self.append_code(code);
        if let Some(data) = sys::runtime_data(&keys) {
            self.append_data(&data);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::ir::lower;
    use crate::parser::parse;
    use crate::transpiler::{transpile, Translation};

    fn translate(src: &str) -> Translation {
        let p = parse(src);
        assert!(!p.has_errors(), "{:#?}", p.diagnostics);
        let l = lower(&p.program);
        assert!(!l.has_errors(), "{:#?}", l.diagnostics);
        transpile(&l.module)
    }

    fn errors(src: &str) -> Vec<String> {
        translate(src)
            .diagnostics
            .iter()
            .filter(|d| d.is_error())
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn syscalls_become_shim_calls() {
        let t = translate("_start:\n mov eax, 1\n mov edi, 1\n lea rsi, [rip + m]\n mov edx, 2\n syscall\n mov eax, 60\n xor edi, edi\n syscall\n.data\nm: .ascii \"hi\"\n");
        assert!(!t.has_errors(), "{:#?}", t.diagnostics);
        let out = crate::codegen::windows::emit(&t, "t.s");
        assert!(out.contains("    mov edx, 2\n    lea rsp, [rsp - 128]\n    call __l2w_sys_write\n    lea rsp, [rsp + 128]\n    mov eax, 60\n"), "{out}");
        assert!(
            out.contains("    call __l2w_sys_exit\n    lea rsp, [rsp + 128]\n"),
            "{out}"
        );
        assert!(!out.contains("syscall"), "{out}");
        for f in [
            "__l2w_sys_write:",
            "__l2w_sys_exit:",
            "__l2w_fd_lookup:",
            "__l2w_errno:",
            "call WriteFile",
            "call ExitProcess",
            "__l2w_fd_flags:\n    .byte 155, 155, 155",
        ] {
            assert!(out.contains(f), "missing {f}:\n{out}");
        }
        assert!(!out.contains("__l2w_sys_read"), "{out}");
        assert!(
            t.notes.iter().any(
                |n| n.contains("syscall: `write` (1) at line 6 -> __l2w_sys_write (WriteFile)")
            ),
            "{:?}",
            t.notes
        );
    }

    #[test]
    fn untranslatable_syscalls_are_errors() {
        let e = errors("f:\n mov eax, 9\n syscall\n ret\n");
        assert!(
            e[0].contains("Linux system call `mmap` (9) has no Windows translation"),
            "{e:?}"
        );
        let e = errors("f:\n mov rax, rdi\n syscall\n ret\n");
        assert!(e[0].contains("cannot be determined statically"), "{e:?}");
        let e = errors("f:\n mov eax, 2\n mov esi, 0x10000\n syscall\n ret\n");
        assert!(
            e[0].contains("`open` flags 0o200000 cannot be translated"),
            "{e:?}"
        );
        let ok = errors("f:\n mov eax, 2\n mov esi, 0x241\n mov edx, 0x1a4\n syscall\n ret\n");
        assert!(ok.is_empty(), "{ok:?}");
        let e = errors(".globl WriteFile\nWriteFile: ret\nf:\n mov eax, 1\n syscall\n ret\n");
        assert!(e.iter().any(|m| m.contains("defines `WriteFile`")), "{e:?}");
    }
}
