//! Linux -> Windows translation passes over the IR.
//!
//! Phase 3 implements the platform-independent part: sections, symbols,
//! data and ordinary instructions. Everything that depends on the OS ABI
//! (system calls, calls into external code) is rejected with a diagnostic
//! until the corresponding layers exist (phases 4 and 5).
//!
//! # Translation rules
//!
//! **Sections** are mapped by [`compatibility::directives::map_section`]
//! (`.rodata` -> `.rdata`, `.text.X` -> `.text$X`, `.note*` dropped, ...).
//! Symbols defined in a dropped section must not be referenced.
//!
//! **Common symbols** (`.comm`/`.lcomm`) become explicit, aligned `.bss`
//! definitions (equivalent for a single translation unit; COFF common
//! alignment semantics differ from ELF).
//!
//! **Addressing.** A Windows x64 image is loaded above 4 GiB (MinGW
//! default image base `0x140000000`, plus ASLR), so 32-bit absolute
//! relocations (`R_X86_64_32S`/`IMAGE_REL_AMD64_ADDR32`) cannot be used:
//!
//! | Linux (non-PIE) | Windows |
//! |---|---|
//! | `mov rax, [sym]` / `mov rax, sym` | `mov rax, qword ptr [rip + sym]` (same address) |
//! | `mov rax, offset sym` | `lea rax, [rip + sym]` (same value, flags untouched by both) |
//! | `mov rax, [rip + sym@GOTPCREL]` | `lea rax, [rip + sym]` (GOT slot holds the address) |
//! | `call f@PLT` | `call f` |
//! | `.quad sym` | unchanged (`IMAGE_REL_AMD64_ADDR64`) |
//! | `.long sym - .` | unchanged (PC-relative) |
//! | `mov eax, offset sym`, `push offset sym`, `[rbx + sym]`, `.long sym` | **error** — needs a 32-bit absolute address |
//!
//! **Other semantic differences** that are rejected: `fs:`/`gs:` accesses
//! (Linux TLS and stack protector; `gs:` is the TEB on Windows) and memory
//! below `rsp` (the System V red zone does not exist on Windows).

use crate::ast::{Expr, RegKind, Register, Segment, Size, SymbolRef, UnaryOp};
use crate::compatibility::directives::{map_section, SectionMapping};
use crate::diagnostics::{has_errors, Diagnostic, Span};
use crate::ir::*;

mod bridge;
mod syscalls;

/// Result of translating a module to Windows.
#[derive(Debug, Clone)]
pub struct Translation {
    /// The IR rewritten to Windows semantics.
    pub module: Module,
    /// COFF mapping for every IR section (same indices).
    pub sections: Vec<SectionMapping>,
    pub diagnostics: Vec<Diagnostic>,
    /// Informational notes for `--verbose`.
    pub notes: Vec<String>,
}

impl Translation {
    pub fn has_errors(&self) -> bool {
        has_errors(&self.diagnostics)
    }
}

pub fn transpile(module: &Module) -> Translation {
    let mut t = Transpiler {
        module: module.clone(),
        sections: Vec::new(),
        diags: Vec::new(),
        notes: Vec::new(),
    };
    t.map_sections();
    t.lower_commons();
    t.rewrite_items();
    t.bridge_calling_conventions();
    t.translate_syscalls();
    t.diags.sort_by_key(|d| d.span.map(|s| (s.line, s.col)));
    Translation {
        module: t.module,
        sections: t.sections,
        diagnostics: t.diags,
        notes: t.notes,
    }
}

struct Transpiler {
    module: Module,
    sections: Vec<SectionMapping>,
    diags: Vec<Diagnostic>,
    notes: Vec<String>,
}

fn item_span(item: &Item) -> Span {
    match item {
        Item::Label { span, .. }
        | Item::Data { span, .. }
        | Item::Align { span, .. }
        | Item::Set { span, .. }
        | Item::Common { span, .. } => *span,
        Item::Instr(i) => i.span,
    }
}

impl Transpiler {
    // -- sections -------------------------------------------------------------

    fn map_sections(&mut self) {
        for sec in &self.module.sections {
            let first_span = sec.items.first().map(item_span);
            let mapping = match map_section(&sec.name, sec.kind, sec.flags.as_deref()) {
                Ok(m) => m,
                Err(msg) => {
                    let d = match first_span {
                        Some(s) => Diagnostic::error(msg, s),
                        None => Diagnostic::global_error(msg),
                    };
                    self.diags.push(d);
                    SectionMapping::Drop("unsupported")
                }
            };
            if let SectionMapping::Drop(reason) = &mapping {
                self.notes
                    .push(format!("dropped section `{}` ({reason})", sec.name));
                for item in &sec.items {
                    if let Item::Label { name, span } = item {
                        if self
                            .module
                            .symbols
                            .get(name)
                            .is_some_and(|s| s.first_ref.is_some())
                        {
                            self.diags.push(Diagnostic::error(
                                format!("symbol `{name}` is referenced but its section `{}` has no Windows equivalent", sec.name),
                                *span,
                            ));
                        }
                    }
                }
            }
            self.sections.push(mapping);
        }
    }

    fn is_kept(&self, section: usize) -> bool {
        matches!(self.sections[section], SectionMapping::Keep(_))
    }

    // -- commons --------------------------------------------------------------

    /// Turns `.comm`/`.lcomm` into explicit `.bss` definitions.
    fn lower_commons(&mut self) {
        let mut commons = Vec::new();
        for sec in &self.module.sections {
            for item in &sec.items {
                if let Item::Common {
                    symbol,
                    size,
                    align,
                    local,
                    span,
                } = item
                {
                    commons.push((symbol.clone(), size.clone(), align.clone(), *local, *span));
                }
            }
        }
        if commons.is_empty() {
            return;
        }
        let bss = match self.module.sections.iter().position(|s| s.name == ".bss") {
            Some(i) => i,
            None => {
                self.module.sections.push(Section {
                    name: ".bss".into(),
                    kind: SectionKind::Bss,
                    flags: None,
                    elf_type: None,
                    items: Vec::new(),
                });
                self.sections
                    .push(map_section(".bss", SectionKind::Bss, None).expect(".bss maps"));
                self.module.sections.len() - 1
            }
        };
        let start = self.module.sections[bss].items.len();
        for (symbol, size, align, local, span) in commons {
            let bytes = match &align {
                Some(a) => match self.module.symbols.eval(a) {
                    Some(v) if v > 0 && (v as u64).is_power_of_two() => v as u64,
                    _ => {
                        self.diags.push(Diagnostic::error(
                            format!("alignment of common symbol `{symbol}` must be a constant power of two"),
                            span,
                        ));
                        continue;
                    }
                },
                // Natural alignment for the size, at most 16 bytes.
                None => match self.module.symbols.eval(&size) {
                    Some(v) if v > 0 => 1u64 << (63 - (v as u64).leading_zeros()).min(4),
                    _ => 16,
                },
            };
            let items = &mut self.module.sections[bss].items;
            items.push(Item::Align {
                bytes,
                fill: None,
                max: None,
                span,
            });
            let loc = Loc {
                section: bss,
                item: items.len(),
            };
            items.push(Item::Label {
                name: symbol.clone(),
                span,
            });
            items.push(Item::Data {
                data: Data::Fill { size, fill: None },
                span,
            });
            let sym = self.module.symbols.entry(&symbol);
            sym.def = SymbolDef::Label(loc);
            if !local {
                // ELF common symbols are global.
                sym.global = true;
            }
            self.notes.push(format!(
                "common symbol `{symbol}` defined in .bss (align {bytes})"
            ));
        }
        let end = self.module.sections[bss].items.len();
        self.module.chunks.push(Chunk {
            section: bss,
            start,
            end,
        });
    }

    // -- items ----------------------------------------------------------------

    fn rewrite_items(&mut self) {
        for s in 0..self.module.sections.len() {
            if !self.is_kept(s) {
                continue;
            }
            for i in 0..self.module.sections[s].items.len() {
                let mut item = self.module.sections[s].items[i].clone();
                let result = match &mut item {
                    Item::Instr(ins) => self.rewrite_instr(ins),
                    Item::Data {
                        data: Data::Values { width, values },
                        span,
                    } => self.check_data(*width, values, *span),
                    Item::Set { value, span, .. } => self.check_modifiers(value, *span),
                    _ => Ok(()),
                };
                match result {
                    Ok(()) => self.module.sections[s].items[i] = item,
                    Err(d) => self.diags.push(d),
                }
            }
        }
    }

    /// Number of addresses (relocatable terms) in an expression: 0 for
    /// constants and differences, 1 for `sym + const`. `None` for
    /// expressions that are not linear in addresses.
    fn address_terms(&self, e: &Expr, depth: u32) -> Option<i32> {
        use crate::ast::BinaryOp::*;
        if depth > 32 {
            return None;
        }
        Some(match e {
            Expr::Int(_) => 0,
            Expr::Here | Expr::LocalLabelRef { .. } => 1,
            Expr::Symbol(s) => match self.module.symbols.get(&s.name).map(|s| &s.def) {
                Some(SymbolDef::Equate {
                    class: ValueClass::Absolute,
                    ..
                }) => 0,
                Some(SymbolDef::Equate {
                    class: ValueClass::Relocatable(_),
                    ..
                }) => 1,
                Some(SymbolDef::Equate { value, .. }) => self.address_terms(value, depth + 1)?,
                _ => 1,
            },
            Expr::Unary(op, x) => match (op, self.address_terms(x, depth + 1)?) {
                (_, 0) => 0,
                (UnaryOp::Neg, n) => -n,
                _ => return None,
            },
            Expr::Binary(op, l, r) => {
                let (l, r) = (
                    self.address_terms(l, depth + 1)?,
                    self.address_terms(r, depth + 1)?,
                );
                match op {
                    Add => l + r,
                    Sub => l - r,
                    _ if l == 0 && r == 0 => 0,
                    _ => return None,
                }
            }
        })
    }

    /// Rejects ELF relocation modifiers (`@GOT`, `@TPOFF`, ...).
    fn check_modifiers(&self, e: &Expr, span: Span) -> Result<(), Diagnostic> {
        match e.symbols().into_iter().find(|s| s.modifier.is_some()) {
            Some(s) => Err(Diagnostic::error(
                format!(
                    "ELF relocation `{}@{}` has no Windows equivalent here",
                    s.name,
                    s.modifier.as_deref().unwrap()
                ),
                span,
            )),
            None => Ok(()),
        }
    }

    fn check_data(&self, width: u8, values: &[Expr], span: Span) -> Result<(), Diagnostic> {
        for v in values {
            self.check_modifiers(v, span)?;
            match self.address_terms(v, 0) {
                Some(0) => {}
                Some(1) if width == 8 => {}
                Some(1) => {
                    return Err(Diagnostic::error(
                        format!(
                            "{width}-byte absolute address `{v}` cannot be represented on Windows x64 (images are loaded above 4 GiB)"
                        ),
                        span,
                    )
                    .with_help("store pointers with `.quad`, or use a relative offset such as `.long sym - .`"))
                }
                _ => {
                    return Err(Diagnostic::error(
                        format!("unsupported relocation expression `{v}` in data"),
                        span,
                    ))
                }
            }
        }
        Ok(())
    }

    fn rewrite_instr(&self, ins: &mut Instr) -> Result<(), Diagnostic> {
        let span_of = |i: usize| ins.operand_spans.get(i).copied().unwrap_or(ins.span);
        let rip = Register {
            kind: RegKind::Rip,
            size: Size::Qword,
        };
        let is_r64 =
            |o: &Operand| matches!(o, Operand::Reg(r) if r.is_gpr() && r.size == Size::Qword);

        if ins.mnemonic == Mnemonic::Mov && ins.operands.len() == 2 && is_r64(&ins.operands[0]) {
            // `mov r64, [rip + sym@GOTPCREL]` loads the address of `sym`.
            if let Operand::Mem(m) = &ins.operands[1] {
                if let Some(Expr::Symbol(SymbolRef {
                    name,
                    modifier: Some(md),
                })) = &m.disp
                {
                    if md == "GOTPCREL"
                        && m.is_rip_relative()
                        && m.index.is_none()
                        && m.segment.is_none()
                    {
                        ins.mnemonic = Mnemonic::Lea;
                        ins.operands[1] = Operand::Mem(Mem {
                            base: Some(rip),
                            disp: Some(Expr::symbol(name.clone())),
                            scale: 1,
                            ..Mem::default()
                        });
                        return Ok(());
                    }
                }
            }
            // `mov r64, offset sym` -> `lea r64, [rip + sym]`.
            if let Operand::Addr(e) = &ins.operands[1] {
                if self.address_terms(e, 0) == Some(1) {
                    self.check_modifiers(e, span_of(1))?;
                    let disp = Some(e.clone());
                    ins.mnemonic = Mnemonic::Lea;
                    ins.operands[1] = Operand::Mem(Mem {
                        base: Some(rip),
                        disp,
                        scale: 1,
                        ..Mem::default()
                    });
                    return Ok(());
                }
            }
        }

        for i in 0..ins.operands.len() {
            let span = span_of(i);
            match &mut ins.operands[i] {
                Operand::Reg(_) => {}
                Operand::Imm(e) => self.check_modifiers(e, span)?,
                Operand::Target(e) => strip_plt(e, span)?,
                Operand::Addr(e) => {
                    return Err(Diagnostic::error(
                        format!(
                            "`offset {e}` needs a 32-bit absolute address, which is not available on Windows x64 (images are loaded above 4 GiB)"
                        ),
                        span,
                    )
                    .with_help(format!("load the address into a 64-bit register first: `lea rax, [rip + {e}]`")))
                }
                Operand::Mem(m) => self.rewrite_mem(m, span)?,
            }
        }
        Ok(())
    }

    fn rewrite_mem(&self, m: &mut Mem, span: Span) -> Result<(), Diagnostic> {
        if let Some(
            seg @ Register {
                kind: RegKind::Segment(Segment::Fs | Segment::Gs),
                ..
            },
        ) = m.segment
        {
            return Err(Diagnostic::error(
                format!("`{seg}:` memory access (Linux thread-local storage / stack protector) has no equivalent on Windows"),
                span,
            )
            .with_help("on Windows `gs:` points to the thread environment block; for C code compile with `-fno-stack-protector` and avoid `__thread`"));
        }
        if let Some(base) = m.base {
            if matches!(base.kind, RegKind::Gpr(4)) {
                if let Some(v) = m.disp.as_ref().and_then(|d| self.module.symbols.eval(d)) {
                    if v < 0 {
                        return Err(Diagnostic::error(
                            format!("access below the stack pointer (`{base} - {}`) relies on the System V red zone, which Windows does not have", -v),
                            span,
                        )
                        .with_help("reserve the space first with `sub rsp, N` and address it with a non-negative offset"));
                    }
                }
            }
        }
        let Some(disp) = &m.disp else { return Ok(()) };
        self.check_modifiers(disp, span).map_err(|d| {
            if disp.symbols().iter().any(|s| s.modifier.as_deref() == Some("GOTPCREL")) {
                d.with_help("only `mov r64, [rip + sym@GOTPCREL]` (load the address of `sym`) is translated")
            } else {
                d
            }
        })?;
        match self.address_terms(disp, 0) {
            Some(0) => Ok(()),
            Some(1) if m.is_rip_relative() => Ok(()),
            Some(1) if m.base.is_none() && m.index.is_none() => {
                // Same address, encoded relative to rip instead of absolute.
                m.base = Some(Register { kind: RegKind::Rip, size: Size::Qword });
                Ok(())
            }
            Some(1) => Err(Diagnostic::error(
                format!("address `{disp}` combined with registers needs a 32-bit absolute address, which is not available on Windows x64"),
                span,
            )
            .with_help(format!("load the address first, e.g. `lea r11, [rip + {disp}]`, then index it: `[r11 + ...]`"))),
            _ => Err(Diagnostic::error(format!("unsupported address expression `{disp}`"), span)),
        }
    }

    // -- platform -------------------------------------------------------------
}

/// `call f@PLT` -> `call f`; other modifiers on branch targets are errors.
fn strip_plt(e: &mut Expr, span: Span) -> Result<(), Diagnostic> {
    match e {
        Expr::Symbol(s) => match s.modifier.as_deref() {
            None => Ok(()),
            Some("PLT") => {
                s.modifier = None;
                Ok(())
            }
            Some(m) => Err(Diagnostic::error(
                format!("unsupported relocation `@{m}` on branch target"),
                span,
            )),
        },
        Expr::Unary(_, x) => strip_plt(x, span),
        Expr::Binary(_, l, r) => {
            strip_plt(l, span)?;
            strip_plt(r, span)
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn translate(src: &str) -> Translation {
        let p = parse(src);
        assert!(!p.has_errors(), "{:#?}", p.diagnostics);
        let l = lower(&p.program);
        assert!(!l.has_errors(), "{:#?}", l.diagnostics);
        transpile(&l.module)
    }

    fn translate_ok(src: &str) -> Translation {
        let t = translate(src);
        assert!(!t.has_errors(), "{:#?}", t.diagnostics);
        t
    }

    fn instrs(t: &Translation) -> Vec<String> {
        t.module
            .instructions()
            .map(|(_, i)| i.to_string())
            .collect()
    }

    fn first_error(src: &str) -> String {
        translate(src)
            .diagnostics
            .iter()
            .find(|d| d.is_error())
            .map(ToString::to_string)
            .unwrap_or_else(|| panic!("expected an error for {src:?}"))
    }

    #[test]
    fn absolute_addressing_becomes_rip_relative() {
        let t = translate_ok(
            ".data\nv: .quad 1\nmsg: .ascii \"x\"\n.text\nf:\nmov rax, v\nmov rax, [v + 8]\ninc qword ptr v\n\
             mov rsi, offset msg\nlea rdi, [msg]\nmov rcx, [rip + v]\nmov rax, [rbx + 8]\nret\n",
        );
        assert_eq!(
            instrs(&t),
            [
                "mov rax, qword ptr [rip + v]",
                "mov rax, qword ptr [rip + v + 8]",
                "inc qword ptr [rip + v]",
                "lea rsi, [rip + msg]",
                "lea rdi, [rip + msg]",
                "mov rcx, qword ptr [rip + v]",
                "mov rax, qword ptr [rbx + 8]",
                "ret"
            ]
        );
    }

    #[test]
    fn elf_relocation_modifiers() {
        let t = translate_ok(".text\nf: ret\ng:\ncall f@PLT\nmov rax, qword ptr [rip + v@GOTPCREL]\nret\n.data\nv: .quad 0\n");
        assert_eq!(instrs(&t)[1..3], ["call f", "lea rax, [rip + v]"]);
        assert!(
            first_error("mov eax, dword ptr [rip + v@GOTPCREL]\n.data\nv: .long 0")
                .contains("`v@GOTPCREL`")
        );
        assert!(
            first_error("mov rax, qword ptr [rip + v@TPOFF]\n.data\nv: .long 0")
                .contains("no Windows equivalent")
        );
    }

    #[test]
    fn constants_are_untouched() {
        let t = translate_ok(".data\nm: .ascii \"ab\"\nlen = . - m\n.text\nf:\nmov rdx, len\nmov rdx, offset len\nadd rax, [rbx + len]\nret\n");
        assert_eq!(
            instrs(&t)[..3],
            [
                "mov rdx, offset len",
                "mov rdx, offset len",
                "add rax, qword ptr [rbx + len]"
            ]
        );
    }

    #[test]
    fn rejected_addressing() {
        assert!(first_error(".data\nm: .byte 0\n.text\nmov eax, offset m")
            .contains("32-bit absolute address"));
        assert!(first_error(".data\nm: .byte 0\n.text\npush offset m")
            .contains("32-bit absolute address"));
        assert!(
            first_error(".data\nt: .quad 0\n.text\nmov rax, [t + rcx*8]")
                .contains("combined with registers")
        );
        assert!(first_error(".data\nt: .quad 0\n.text\nmov rax, [rbx + t]")
            .contains("combined with registers"));
        assert!(first_error("mov rax, fs:0x28").contains("thread-local storage"));
        assert!(first_error("mov qword ptr [rsp - 8], rax").contains("red zone"));
        let e = first_error(".data\nm: .byte 0\np: .long m\n");
        assert!(e.contains("4-byte absolute address `m`"), "{e}");
    }

    #[test]
    fn data_relocations() {
        translate_ok(".data\nm: .byte 0\np: .quad m, m + 4, 0\nd: .long m - ., p - m\n");
    }

    #[test]
    fn sections_are_mapped() {
        let t = translate_ok(
            ".section .rodata\nr: .byte 1\n.section .text.hot,\"ax\",@progbits\nf: ret\n\
             .section .note.GNU-stack,\"\",@progbits\n.section .data.rel.ro\np: .quad r\n",
        );
        let names: Vec<String> = t
            .sections
            .iter()
            .map(|m| match m {
                SectionMapping::Keep(c) => c.name.clone(),
                SectionMapping::Drop(_) => "-".into(),
            })
            .collect();
        assert!(names.contains(&".rdata".to_string()), "{names:?}");
        assert!(names.contains(&".text$hot".to_string()), "{names:?}");
        assert!(names.contains(&"-".to_string()), "{names:?}");
        assert!(t.notes.iter().any(|n| n.contains(".note.GNU-stack")));
        assert!(first_error(".section .init_array,\"aw\"\n.quad 0\n").contains("constructors"));
    }

    #[test]
    fn commons_become_bss() {
        let t = translate_ok(".text\nf: ret\n.comm buf, 64, 32\n.lcomm small, 4\n");
        let bss = t
            .module
            .sections
            .iter()
            .position(|s| s.name == ".bss")
            .unwrap();
        let items: Vec<String> = t.module.sections[bss]
            .items
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            items,
            [
                "    .balign 32",
                "buf:",
                "    .skip 64",
                "    .balign 4",
                "small:",
                "    .skip 4"
            ]
        );
        assert!(t.module.symbols.get("buf").unwrap().global);
        assert!(!t.module.symbols.get("small").unwrap().global);
    }

    #[test]
    fn os_dependent_constructs_are_reported() {
        // Known system calls and external calls are translated (phases 4, 5).
        let t = translate("_start:\nmov rax, 60\nxor edi, edi\nsyscall\ncall puts\n");
        assert!(!t.has_errors(), "{:#?}", t.diagnostics);
        let t = translate("_start:\nmov rax, 57\nsyscall\n");
        let e: Vec<String> = t.diagnostics.iter().map(ToString::to_string).collect();
        assert!(
            e[0].contains("`fork` (57) has no Windows translation"),
            "{e:?}"
        );
    }
}
