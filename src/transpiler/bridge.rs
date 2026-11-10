//! Calling-convention bridging pass (System V <-> Windows x64).
//!
//! See [`crate::compatibility::calling_convention`] for the strategy. This
//! pass
//!
//! 1. reserves stack space for gcc-style red-zone use through `rbp` in leaf
//!    functions (`push rbp; mov rbp, rsp; mov [rbp - 4], edi; ...; pop rbp`)
//!    by inserting `sub rsp, N` and replacing `pop rbp` with the equivalent
//!    `leave`, or rejects red-zone use it cannot fix;
//! 2. rejects reads of the Linux initial process stack in `_start`;
//! 3. redirects direct branches (`call`/`jmp`/`jcc`) to external functions
//!    to System V -> Windows thunks and rejects all other references to
//!    external symbols;
//! 4. renames every exported function `f` to `__l2w_sysv_f` and gives the
//!    public name to a Windows -> System V wrapper (`_start` gets the
//!    alignment-fixing entry wrapper);
//! 5. appends thunks and wrappers to `.text`.

use super::Transpiler;
use crate::ast::{Expr, RegKind, Register, Size};
use crate::compatibility::calling_convention as cc;
use crate::compatibility::directives::{map_section, SectionMapping};
use crate::diagnostics::{Diagnostic, Span};
use crate::ir::*;

const RSP: Register = Register::RSP;
const RBP: Register = Register::gpr(5, Size::Qword);

fn instr(item: &Item) -> Option<&Instr> {
    match item {
        Item::Instr(i) => Some(i),
        _ => None,
    }
}

fn is_reg(op: Option<&Operand>, r: Register) -> bool {
    matches!(op, Some(Operand::Reg(x)) if *x == r)
}

/// `true` if the instruction changes rsp (explicitly or implicitly).
fn writes_rsp(ins: &Instr) -> bool {
    use Mnemonic::*;
    match ins.mnemonic {
        Push | Pop | Call | Leave | Ret => true,
        Cmp | Test | Jmp | Jcc(_) | Nop | Syscall => false,
        _ => matches!(ins.operands.first(), Some(Operand::Reg(r)) if r.kind == RegKind::Gpr(4)),
    }
}

/// Parses and validates a generated assembly snippet with l2w's own front
/// end and returns its items.
fn snippet_items(src: &str) -> Vec<Item> {
    let parsed = crate::parser::parse(src);
    let lowered = crate::ir::lower(&parsed.program);
    assert!(
        !parsed.has_errors() && !lowered.has_errors(),
        "internal error: invalid bridge snippet\n{src}\n{:#?}{:#?}",
        parsed.diagnostics,
        lowered.diagnostics
    );
    lowered
        .module
        .sections
        .into_iter()
        .next()
        .map(|s| s.items)
        .unwrap_or_default()
}

impl Transpiler {
    pub(super) fn bridge_calling_conventions(&mut self) {
        self.fix_red_zones();
        self.check_entry_stack();
        let externals = self.redirect_external_branches();
        self.check_external_references();
        self.warn_indirect_calls();

        let mut snippets = Vec::new();
        for f in self.exported_functions() {
            let body = cc::sysv_body_name(&f);
            if self.module.symbols.get(&body).is_some() {
                self.diags.push(Diagnostic::global_error(format!(
                    "cannot create the System V body `{body}` for `{f}`: the name is already in use"
                )));
                continue;
            }
            self.module.rename_symbol(&f, &body);
            if let Some(s) = self.module.symbols.get_mut(&body) {
                s.global = false;
            }
            for func in self
                .module
                .functions
                .iter_mut()
                .filter(|func| func.name == body)
            {
                func.global = false;
            }
            let is_start = self
                .module
                .platform
                .entry
                .as_ref()
                .is_some_and(|e| e.symbol == f && e.kind == EntryKind::Start);
            let (src, what) = if is_start {
                (
                    cc::entry_wrapper(&f, &body),
                    "entry wrapper (stack alignment)",
                )
            } else {
                (
                    cc::win_to_sysv_wrapper(&f, &body),
                    "Windows -> System V wrapper",
                )
            };
            self.notes
                .push(format!("bridge: {what} `{f}` for System V body `{body}`"));
            snippets.push((f, src, true));
        }
        for e in externals {
            self.notes.push(format!(
                "bridge: System V -> Windows thunk for external `{e}`"
            ));
            snippets.push((cc::thunk_name(&e), cc::sysv_to_win_thunk(&e), false));
        }
        if !snippets.is_empty() {
            self.append_code(snippets);
        }
    }

    // -- 1. red zone ----------------------------------------------------------

    fn fix_red_zones(&mut self) {
        for fi in 0..self.module.functions.len() {
            let f = self.module.functions[fi].clone();
            if !self.is_kept(f.section) {
                continue;
            }
            let items = &self.module.sections[f.section].items;
            let code: Vec<usize> = (f.start..f.end)
                .filter(|&i| instr(&items[i]).is_some())
                .collect();
            let at = |k: usize| code.get(k).map(|&i| instr(&items[i]).unwrap());
            let prologue = matches!(at(0), Some(i) if i.mnemonic == Mnemonic::Push && is_reg(i.operands.first(), RBP))
                && matches!(at(1), Some(i) if i.mnemonic == Mnemonic::Mov
                    && is_reg(i.operands.first(), RBP) && is_reg(i.operands.get(1), RSP));
            if !prologue {
                continue;
            }
            let alloc = match at(2) {
                Some(i) if i.mnemonic == Mnemonic::Sub && is_reg(i.operands.first(), RSP) => {
                    match i.operands.get(1) {
                        Some(Operand::Imm(e)) => self.module.symbols.eval(e).unwrap_or(0),
                        _ => 0,
                    }
                }
                _ => 0,
            };
            // Deepest access below rbp.
            let mut deepest: Option<(i64, Span)> = None;
            for &i in &code {
                let ins = instr(&items[i]).unwrap();
                for (k, op) in ins.operands.iter().enumerate() {
                    if let Operand::Mem(m) = op {
                        if m.base == Some(RBP) {
                            if let Some(d) =
                                m.disp.as_ref().and_then(|d| self.module.symbols.eval(d))
                            {
                                if d < 0 && deepest.map_or(true, |(x, _)| -d > x) {
                                    deepest = Some((
                                        -d,
                                        ins.operand_spans.get(k).copied().unwrap_or(ins.span),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            let Some((need, span)) = deepest else {
                continue;
            };
            if need <= alloc {
                continue;
            }
            let is_pop_rbp =
                |ins: &Instr| ins.mnemonic == Mnemonic::Pop && is_reg(ins.operands.first(), RBP);
            let other_rsp_writes = code[2..].iter().any(|&i| {
                let ins = instr(&items[i]).unwrap();
                writes_rsp(ins) && !is_pop_rbp(ins) && ins.mnemonic != Mnemonic::Ret
            });
            if alloc != 0 || other_rsp_writes || need > i64::from(cc::RED_ZONE) {
                self.diags.push(
                    Diagnostic::error(
                        format!(
                            "function `{}` accesses `[rbp - {need}]` below its stack frame (System V red zone), which is not safe on Windows",
                            f.name
                        ),
                        span,
                    )
                    .with_help("allocate the space in the prologue: `sub rsp, N` right after `mov rbp, rsp`"),
                );
                continue;
            }
            // Leaf function with frame pointer and rsp == rbp throughout:
            // reserve the red zone explicitly. `pop rbp` (with rsp == rbp)
            // becomes `leave` (rsp = rbp; pop rbp) — identical semantics.
            let reserve = (need + 15) / 16 * 16;
            let section = f.section;
            for &i in &code {
                if let Item::Instr(ins) = &mut self.module.sections[section].items[i] {
                    if is_pop_rbp(ins) {
                        ins.mnemonic = Mnemonic::Leave;
                        ins.operands.clear();
                        ins.operand_spans.clear();
                        ins.size = None;
                    }
                }
            }
            let mov_span = instr(&self.module.sections[section].items[code[1]])
                .unwrap()
                .span;
            let sub = Item::Instr(Instr {
                mnemonic: Mnemonic::Sub,
                lock: false,
                operands: vec![
                    Operand::Reg(RSP),
                    Operand::Imm(Expr::Int(i128::from(reserve))),
                ],
                size: Some(Size::Qword),
                span: mov_span,
                operand_spans: vec![mov_span, mov_span],
            });
            self.module.insert_items(
                Loc {
                    section,
                    item: code[1] + 1,
                },
                vec![sub],
            );
            self.notes.push(format!(
                "red zone: reserved {reserve} bytes in leaf function `{}`",
                f.name
            ));
        }
    }

    // -- 2. _start ------------------------------------------------------------

    fn check_entry_stack(&mut self) {
        let Some(entry) = &self.module.platform.entry else {
            return;
        };
        if entry.kind != EntryKind::Start {
            return;
        }
        let Some(f) = self
            .module
            .functions
            .iter()
            .find(|f| f.name == entry.symbol)
        else {
            return;
        };
        for item in &self.module.sections[f.section].items[f.start..f.end] {
            let Some(ins) = instr(item) else { continue };
            let reads_stack = ins.mnemonic == Mnemonic::Pop
                || ins.operands.iter().any(|o| matches!(o, Operand::Mem(m) if m.base.is_some_and(|b| b.kind == RegKind::Gpr(4))));
            if reads_stack {
                self.diags.push(
                    Diagnostic::error(
                        "`_start` reads the Linux initial process stack (argc/argv/envp), which does not exist on Windows",
                        ins.span,
                    )
                    .with_help("use `main(argc, argv)` with the C runtime instead of `_start`"),
                );
                return;
            }
            if writes_rsp(ins) || ins.mnemonic.is_branch() {
                return;
            }
        }
    }

    // -- 3. externals ---------------------------------------------------------

    fn is_external(&self, name: &str) -> bool {
        self.module
            .symbols
            .get(name)
            .map_or(true, |s| !s.is_defined())
    }

    /// Redirects `call ext` / `jmp ext` / `jcc ext` to thunks and returns
    /// the external functions in order of first use.
    fn redirect_external_branches(&mut self) -> Vec<String> {
        let mut externals: Vec<String> = Vec::new();
        for s in 0..self.module.sections.len() {
            if !self.is_kept(s) {
                continue;
            }
            for i in 0..self.module.sections[s].items.len() {
                let Item::Instr(ins) = &self.module.sections[s].items[i] else {
                    continue;
                };
                if !ins.mnemonic.is_branch() {
                    continue;
                }
                let Some(Operand::Target(e)) = ins.operands.first() else {
                    continue;
                };
                let name = match e {
                    Expr::Symbol(sym) if self.is_external(&sym.name) => sym.name.clone(),
                    Expr::Symbol(_) => continue,
                    other => {
                        if let Some(sym) = other
                            .symbols()
                            .into_iter()
                            .find(|s| self.is_external(&s.name))
                        {
                            let span = ins.operand_spans.first().copied().unwrap_or(ins.span);
                            self.diags.push(Diagnostic::error(
                                format!("branch to an offset from external symbol `{}` cannot be bridged", sym.name),
                                span,
                            ));
                        }
                        continue;
                    }
                };
                if let Item::Instr(ins) = &mut self.module.sections[s].items[i] {
                    ins.operands[0] = Operand::Target(Expr::symbol(cc::thunk_name(&name)));
                }
                if !externals.contains(&name) {
                    externals.push(name);
                }
            }
        }
        externals
    }

    /// Externals may only be used as direct branch targets.
    fn check_external_references(&mut self) {
        let mut errors = Vec::new();
        for (s, sec) in self.module.sections.iter().enumerate() {
            if !self.is_kept(s) {
                continue;
            }
            for item in &sec.items {
                let exprs: Vec<(&Expr, Span)> = match item {
                    Item::Instr(ins) => ins
                        .operands
                        .iter()
                        .enumerate()
                        .filter_map(|(k, op)| {
                            let span = ins.operand_spans.get(k).copied().unwrap_or(ins.span);
                            match op {
                                Operand::Imm(e) | Operand::Addr(e) => Some((e, span)),
                                Operand::Mem(m) => m.disp.as_ref().map(|d| (d, span)),
                                Operand::Target(_) | Operand::Reg(_) => None,
                            }
                        })
                        .collect(),
                    Item::Data {
                        data: Data::Values { values, .. },
                        span,
                    } => values.iter().map(|v| (v, *span)).collect(),
                    Item::Set { value, span, .. } => vec![(value, *span)],
                    _ => Vec::new(),
                };
                for (e, span) in exprs {
                    if let Some(sym) = e.symbols().into_iter().find(|s| self.is_external(&s.name)) {
                        errors.push(
                            Diagnostic::error(
                                format!("external symbol `{}` is used as data or as an address; only direct calls and jumps to external functions are bridged", sym.name),
                                span,
                            )
                            .with_help("Windows imports data and function addresses through `__imp_` pointers, and Windows functions expect the Windows calling convention when called through a pointer"),
                        );
                    }
                }
            }
        }
        self.diags.extend(errors);
    }

    fn warn_indirect_calls(&mut self) {
        for c in &self.module.platform.calls {
            if c.target == CallTarget::Indirect {
                self.diags.push(Diagnostic::warning(
                    "indirect call: the target is assumed to be a System V function of this program; calls into Windows code through pointers are not bridged",
                    c.span,
                ));
            }
        }
    }

    // -- 4./5. wrappers and thunks ------------------------------------------------

    /// Functions callable from Windows code: global functions and the entry point.
    fn exported_functions(&self) -> Vec<String> {
        let entry = self
            .module
            .platform
            .entry
            .as_ref()
            .map(|e| e.symbol.as_str());
        self.module
            .functions
            .iter()
            .filter(|f| self.is_kept(f.section) && (f.global || Some(f.name.as_str()) == entry))
            .map(|f| f.name.clone())
            .collect()
    }

    fn text_section(&mut self) -> usize {
        if let Some(i) = self.module.sections.iter().position(|s| s.name == ".text") {
            if self.is_kept(i) {
                return i;
            }
        }
        self.module.sections.push(Section {
            name: ".text".into(),
            kind: SectionKind::Text,
            flags: None,
            elf_type: None,
            items: Vec::new(),
        });
        self.sections
            .push(map_section(".text", SectionKind::Text, None).expect(".text maps"));
        debug_assert!(matches!(
            self.sections.last(),
            Some(SectionMapping::Keep(_))
        ));
        self.module.sections.len() - 1
    }

    /// Appends generated functions (`(name, source, exported)`) to `.text`.
    fn append_code(&mut self, snippets: Vec<(String, String, bool)>) {
        let text = self.text_section();
        let chunk_start = self.module.sections[text].items.len();
        for (name, src, exported) in snippets {
            let start = self.module.sections[text].items.len();
            let items = snippet_items(&src);
            let end = start + items.len();
            self.module.sections[text].items.extend(items);
            let sym = self.module.symbols.entry(&name);
            sym.def = SymbolDef::Label(Loc {
                section: text,
                item: start,
            });
            sym.global = exported;
            self.module.functions.push(Function {
                name,
                section: text,
                start,
                end,
                global: exported,
                reason: FunctionReason::Bridge,
            });
        }
        let end = self.module.sections[text].items.len();
        self.module.chunks.push(Chunk {
            section: text,
            start: chunk_start,
            end,
        });
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

    fn asm(src: &str) -> String {
        let t = translate(src);
        assert!(!t.has_errors(), "{:#?}", t.diagnostics);
        crate::codegen::windows::emit(&t, "t.s")
    }

    fn first_error(src: &str) -> String {
        let t = translate(src);
        t.diagnostics
            .iter()
            .find(|d| d.is_error())
            .map(ToString::to_string)
            .unwrap_or_else(|| panic!("expected error"))
    }

    #[test]
    fn external_calls_go_through_thunks() {
        let out = asm(".text\n.globl main\nmain:\n sub rsp, 8\n lea rdi, [rip + msg]\n call puts@PLT\n add rsp, 8\n jmp exit\n.data\nmsg: .asciz \"x\"\n");
        assert!(out.contains("    call __l2w_thunk_puts\n"), "{out}");
        assert!(out.contains("    jmp __l2w_thunk_exit\n"), "{out}");
        assert!(out.contains("__l2w_thunk_puts:\n    push rbp"), "{out}");
        assert!(out.contains("    call puts\n    leave\n    ret"), "{out}");
        assert!(out.contains("    call exit\n"), "{out}");
        // main -> Windows wrapper around the System V body
        assert!(out.contains(".globl main\n"), "{out}");
        assert!(!out.contains(".globl __l2w_sysv_main"), "{out}");
        assert!(out.contains("__l2w_sysv_main:\n    sub rsp, 8"), "{out}");
        assert!(
            out.contains("main:\n    push rbp\n    mov rbp, rsp\n    push rsi\n    push rdi"),
            "{out}"
        );
        assert!(out.contains(".def main; .scl 2; .type 32; .endef"), "{out}");
        assert!(out.contains(".def __l2w_sysv_main; .scl 3"), "{out}");
    }

    #[test]
    fn internal_calls_use_the_sysv_body() {
        let out = asm(
            ".text\n.globl f, g\nf:\n mov rax, rdi\n ret\ng:\n call f\n jmp f\n.data\np: .quad f\n",
        );
        assert!(
            out.contains("    call __l2w_sysv_f\n    jmp __l2w_sysv_f\n"),
            "{out}"
        );
        assert!(out.contains(".quad __l2w_sysv_f"), "{out}");
        assert!(out.contains("    call __l2w_sysv_g\n"), "{out}");
    }

    #[test]
    fn start_gets_entry_wrapper() {
        let out = asm(".globl _start\n_start:\n call work\n jmp _start\nwork: ret\n");
        assert!(
            out.contains("_start:\n    sub rsp, 8\n    jmp __l2w_sysv__start\n"),
            "{out}"
        );
        assert!(out.contains("    jmp __l2w_sysv__start\n"), "{out}");
        assert!(first_error("_start:\n mov rdi, [rsp]\n ret\n").contains("initial process stack"));
        assert!(first_error("_start:\n pop rdi\n ret\n").contains("initial process stack"));
    }

    #[test]
    fn red_zone_is_reserved_in_gcc_leaf_functions() {
        // gcc -O0: int add(int a, int b) { int c = a + b; return c; }
        let out = asm(".globl add\nadd:\n push rbp\n mov rbp, rsp\n mov dword ptr [rbp - 20], edi\n mov dword ptr [rbp - 24], esi\n \
                       mov edx, dword ptr [rbp - 20]\n mov eax, dword ptr [rbp - 24]\n add eax, edx\n mov dword ptr [rbp - 4], eax\n \
                       mov eax, dword ptr [rbp - 4]\n pop rbp\n ret\n");
        assert!(
            out.contains("    mov rbp, rsp\n    sub rsp, 32\n    mov dword ptr [rbp - 20], edi"),
            "{out}"
        );
        assert!(out.contains("    leave\n    ret\n"), "{out}");
        // only the Windows wrapper still pops rbp
        assert_eq!(out.matches("pop rbp").count(), 1, "{out}");
    }

    #[test]
    fn red_zone_errors() {
        let e = first_error(".type f, @function\nf:\n push rbp\n mov rbp, rsp\n mov qword ptr [rbp - 8], rdi\n call g\n pop rbp\n ret\ng: ret\n");
        assert!(
            e.contains("accesses `[rbp - 8]` below its stack frame"),
            "{e}"
        );
        let ok = translate(".type f, @function\nf:\n push rbp\n mov rbp, rsp\n sub rsp, 16\n mov qword ptr [rbp - 8], rdi\n leave\n ret\n");
        assert!(!ok.has_errors(), "{:#?}", ok.diagnostics);
    }

    #[test]
    fn external_data_references_are_rejected() {
        assert!(
            first_error("main:\n mov rax, qword ptr [rip + stdout]\n ret\n")
                .contains("external symbol `stdout` is used as data")
        );
        assert!(
            first_error("main:\n lea rdi, [rip + puts]\n ret\n").contains("external symbol `puts`")
        );
        assert!(first_error(".data\np: .quad malloc\n").contains("external symbol `malloc`"));
    }

    #[test]
    fn indirect_calls_warn() {
        let t = translate("main:\n call rax\n ret\n");
        assert!(!t.has_errors());
        assert!(t
            .diagnostics
            .iter()
            .any(|d| d.message.starts_with("indirect call")));
    }
}
