//! AST -> IR lowering.
//!
//! 1. **Numeric local labels** (`1:` / `1b` / `1f`) are renamed to unique
//!    local labels `.Ll2w_<n>_<k>`.
//! 2. **Pass A** assigns every statement to a section, records label,
//!    equate, `.globl`/`.extern`/`.type`/`.size`/`.comm` information.
//! 3. Equates are classified (absolute / relocatable / unknown) like GAS.
//! 4. **Pass B** builds section items, lowers and validates instructions
//!    (mnemonic, operand forms, operand sizes, immediate ranges) and data.
//! 5. [`super::analysis`] derives functions, calls and system calls.

use super::*;
use crate::ast::{self, Directive, OperandKind, StatementKind};
use crate::diagnostics::{has_errors, Diagnostic, Span};
use std::collections::HashMap;

pub struct LowerOutput {
    pub module: Module,
    pub diagnostics: Vec<Diagnostic>,
}

impl LowerOutput {
    pub fn has_errors(&self) -> bool {
        has_errors(&self.diagnostics)
    }
}

type LResult<T> = Result<T, Diagnostic>;

/// Lowers a parsed program into the IR.
pub fn lower(program: &ast::Program) -> LowerOutput {
    let mut diagnostics = Vec::new();
    let program = rename_numeric_labels(program, &mut diagnostics);
    let mut l = Lowerer {
        module: Module::default(),
        diags: diagnostics,
        section_index: HashMap::new(),
        stmt_section: Vec::new(),
        equate_stmt: HashMap::new(),
    };
    l.pass_a(&program);
    l.classify_equates();
    l.pass_b(&program);
    l.finish();
    let mut module = l.module;
    super::analysis::analyze(&mut module);
    let mut diagnostics = l.diags;
    diagnostics.sort_by_key(|d| d.span.map(|s| (s.line, s.col)));
    LowerOutput {
        module,
        diagnostics,
    }
}

// ---------------------------------------------------------------------------
// Numeric local labels
// ---------------------------------------------------------------------------

fn numeric_label_name(n: u32, k: usize) -> String {
    format!(".Ll2w_{n}_{k}")
}

/// Replaces numeric labels and `Nb`/`Nf` references by unique names.
fn rename_numeric_labels(program: &ast::Program, diags: &mut Vec<Diagnostic>) -> ast::Program {
    let mut total: HashMap<u32, usize> = HashMap::new();
    for s in &program.statements {
        if let StatementKind::NumericLabel(n) = s.kind {
            *total.entry(n).or_default() += 1;
        }
    }
    let mut seen: HashMap<u32, usize> = HashMap::new();
    let mut out = program.clone();
    for stmt in &mut out.statements {
        let span = stmt.span;
        if let StatementKind::NumericLabel(n) = stmt.kind {
            let k = seen.entry(n).or_default();
            stmt.kind = StatementKind::Label(numeric_label_name(n, *k));
            *k += 1;
            continue;
        }
        let mut resolve = |e: &mut Expr| {
            rewrite_local_refs(e, &mut |number, forward| {
                let k = seen.get(&number).copied().unwrap_or(0);
                if forward {
                    if k < total.get(&number).copied().unwrap_or(0) {
                        Ok(numeric_label_name(number, k))
                    } else {
                        Err(format!(
                            "no local label `{number}:` after this reference (`{number}f`)"
                        ))
                    }
                } else if k > 0 {
                    Ok(numeric_label_name(number, k - 1))
                } else {
                    Err(format!(
                        "no local label `{number}:` before this reference (`{number}b`)"
                    ))
                }
            })
            .map_err(|m| diags.push(Diagnostic::error(m, span)))
            .ok();
        };
        for_each_expr_mut(stmt, &mut resolve);
    }
    out
}

fn rewrite_local_refs(
    e: &mut Expr,
    f: &mut dyn FnMut(u32, bool) -> Result<String, String>,
) -> Result<(), String> {
    match e {
        Expr::LocalLabelRef { number, forward } => {
            *e = Expr::symbol(f(*number, *forward)?);
            Ok(())
        }
        Expr::Unary(_, x) => rewrite_local_refs(x, f),
        Expr::Binary(_, l, r) => {
            rewrite_local_refs(l, f)?;
            rewrite_local_refs(r, f)
        }
        _ => Ok(()),
    }
}

fn for_each_expr_mut(stmt: &mut ast::Statement, f: &mut dyn FnMut(&mut Expr)) {
    match &mut stmt.kind {
        StatementKind::Instruction(ins) => {
            for op in &mut ins.operands {
                match &mut op.kind {
                    OperandKind::Immediate(e)
                    | OperandKind::Offset(e)
                    | OperandKind::Symbolic(e) => f(e),
                    OperandKind::Memory(m) => {
                        if let Some(d) = &mut m.disp {
                            f(d)
                        }
                    }
                    OperandKind::Register(_) => {}
                }
            }
        }
        StatementKind::Directive(d) => match d {
            Directive::Values { values, .. } => values.iter_mut().for_each(f),
            Directive::Align {
                alignment,
                fill,
                max,
                ..
            } => {
                f(alignment);
                fill.iter_mut().chain(max.iter_mut()).for_each(f);
            }
            Directive::Zero(e) => f(e),
            Directive::Skip { size, fill } => {
                f(size);
                fill.iter_mut().for_each(f);
            }
            Directive::Size { value, .. } | Directive::Set { value, .. } => f(value),
            Directive::Comm { size, align, .. } => {
                f(size);
                align.iter_mut().for_each(f);
            }
            _ => {}
        },
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Lowerer
// ---------------------------------------------------------------------------

struct Lowerer {
    module: Module,
    diags: Vec<Diagnostic>,
    section_index: HashMap<String, usize>,
    /// Section of every statement (pass A).
    stmt_section: Vec<usize>,
    /// Statement index at which each equate is defined.
    equate_stmt: HashMap<String, usize>,
}

/// Instruction operand together with its source span.
struct Op {
    op: Operand,
    span: Span,
}

impl Lowerer {
    fn error(&mut self, msg: impl Into<String>, span: Span) {
        self.diags.push(Diagnostic::error(msg, span));
    }

    fn section(
        &mut self,
        name: &str,
        flags: Option<String>,
        ty: Option<String>,
        span: Span,
    ) -> usize {
        if let Some(&i) = self.section_index.get(name) {
            let sec = &self.module.sections[i];
            if (flags.is_some() && flags != sec.flags) || (ty.is_some() && ty != sec.elf_type) {
                self.diags.push(Diagnostic::warning(
                    format!("ignoring changed attributes of section `{name}`"),
                    span,
                ));
            }
            return i;
        }
        let kind = SectionKind::classify(name, flags.as_deref(), ty.as_deref());
        self.module.sections.push(Section {
            name: name.to_string(),
            kind,
            flags,
            elf_type: ty,
            items: Vec::new(),
            span: Some(span),
        });
        let i = self.module.sections.len() - 1;
        self.section_index.insert(name.to_string(), i);
        i
    }

    // -- pass A ---------------------------------------------------------------

    fn pass_a(&mut self, program: &ast::Program) {
        let mut current: Option<usize> = None;
        for (i, stmt) in program.statements.iter().enumerate() {
            let span = stmt.span;
            // Section switches take effect for the directive itself.
            if let StatementKind::Directive(d) = &stmt.kind {
                match d {
                    Directive::Text => current = Some(self.section(".text", None, None, span)),
                    Directive::Data => current = Some(self.section(".data", None, None, span)),
                    Directive::Bss => current = Some(self.section(".bss", None, None, span)),
                    Directive::Section {
                        name, flags, kind, ..
                    } => {
                        let (n, fl, k) = (name.clone(), flags.clone(), kind.clone());
                        current = Some(self.section(&n, fl, k, span))
                    }
                    _ => {}
                }
            }
            let sec = match current {
                Some(s) => s,
                None => {
                    // GAS starts in `.text`.
                    let s = self.section(".text", None, None, span);
                    current = Some(s);
                    s
                }
            };
            self.stmt_section.push(sec);

            match &stmt.kind {
                StatementKind::Label(name) => self.define(
                    name,
                    SymbolDef::Label(Loc {
                        section: sec,
                        item: 0,
                    }),
                    span,
                ),
                StatementKind::Directive(d) => match d {
                    Directive::Set { symbol, value } => {
                        if self.equate_stmt.contains_key(symbol) {
                            self.diags.push(
                                Diagnostic::error(
                                    format!("redefinition of equate `{symbol}` is not supported"),
                                    span,
                                )
                                .with_help("use a different name for each value"),
                            );
                            continue;
                        }
                        self.equate_stmt.insert(symbol.clone(), i);
                        self.define(
                            symbol,
                            SymbolDef::Equate {
                                value: value.clone(),
                                class: ValueClass::Unknown,
                                at: Loc {
                                    section: sec,
                                    item: 0,
                                },
                            },
                            span,
                        );
                    }
                    Directive::Global(names) => {
                        for n in names {
                            self.module.symbols.entry(n).global = true;
                        }
                    }
                    Directive::Extern(names) => {
                        for n in names {
                            self.module.symbols.entry(n).declared_extern = true;
                        }
                    }
                    Directive::Type { symbol, kind } => {
                        self.module.symbols.entry(symbol).ty = Some(kind.clone());
                    }
                    Directive::Size { symbol, value } => {
                        self.module.symbols.entry(symbol).size = Some(value.clone());
                    }
                    Directive::Comm { symbol, local, .. } => {
                        self.define(symbol, SymbolDef::Common { local: *local }, span)
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    fn define(&mut self, name: &str, def: SymbolDef, span: Span) {
        let sym = self.module.symbols.entry(name);
        if sym.is_defined() {
            let first = sym.def_span;
            let mut d = Diagnostic::error(format!("symbol `{name}` is already defined"), span);
            if let Some(f) = first {
                d = d.with_help(format!(
                    "first definition is at line {}, column {}",
                    f.line, f.col
                ));
            }
            self.diags.push(d);
            return;
        }
        sym.def = def;
        sym.def_span = Some(span);
    }

    // -- equate classification ---------------------------------------------------

    fn classify_equates(&mut self) {
        let names: Vec<String> = self
            .module
            .symbols
            .iter()
            .filter(|s| matches!(s.def, SymbolDef::Equate { .. }))
            .map(|s| s.name.clone())
            .collect();
        let mut memo: HashMap<String, (ValueClass, u32)> = HashMap::new();
        for name in names {
            let mut visiting = Vec::new();
            let class = self.equate_class(&name, &mut visiting, &mut memo);
            if let Some(SymbolDef::Equate { class: c, .. }) =
                self.module.symbols.get_mut(&name).map(|s| &mut s.def)
            {
                *c = class;
            }
        }
    }

    fn equate_class(
        &mut self,
        name: &str,
        visiting: &mut Vec<String>,
        memo: &mut HashMap<String, (ValueClass, u32)>,
    ) -> ValueClass {
        if let Some((c, _)) = memo.get(name) {
            return *c;
        }
        if visiting.len() > crate::ir::MAX_EQUATE_CHAIN as usize {
            return self.chain_too_long(name, memo);
        }
        if visiting.iter().any(|v| v == name) {
            let span = self.def_span(name);
            self.error(
                format!("equate `{name}` is defined in terms of itself"),
                span,
            );
            return ValueClass::Unknown;
        }
        let Some(SymbolDef::Equate { value, at, .. }) =
            self.module.symbols.get(name).map(|s| s.def.clone())
        else {
            return ValueClass::Unknown;
        };
        visiting.push(name.to_string());
        let c = self.expr_class(&value, at.section, visiting, memo);
        visiting.pop();
        // Chain depth: 1 + the deepest equate referenced by the value.
        let depth = value
            .symbols()
            .iter()
            .filter_map(|r| memo.get(&r.name).map(|(_, d)| *d))
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        if depth > crate::ir::MAX_EQUATE_CHAIN && depth != u32::MAX {
            return self.chain_too_long(name, memo);
        }
        memo.insert(name.to_string(), (c, depth));
        c
    }

    fn def_span(&self, name: &str) -> Span {
        self.module
            .symbols
            .get(name)
            .and_then(|s| s.def_span)
            .unwrap_or_default()
    }

    /// Reports an equate chain longer than [`crate::ir::MAX_EQUATE_CHAIN`]
    /// and marks `name` as unresolvable (depth `u32::MAX`) so the error is
    /// reported once per chain.
    fn chain_too_long(
        &mut self,
        name: &str,
        memo: &mut HashMap<String, (ValueClass, u32)>,
    ) -> ValueClass {
        let span = self.def_span(name);
        self.error(
            format!(
                "equate `{name}` ends a chain of more than {} equates defined in terms of each other",
                crate::ir::MAX_EQUATE_CHAIN
            ),
            span,
        );
        memo.insert(name.to_string(), (ValueClass::Unknown, u32::MAX));
        ValueClass::Unknown
    }

    fn expr_class(
        &mut self,
        e: &Expr,
        here: usize,
        visiting: &mut Vec<String>,
        memo: &mut HashMap<String, (ValueClass, u32)>,
    ) -> ValueClass {
        use crate::ast::BinaryOp;
        use ValueClass::*;
        match e {
            Expr::Int(_) => Absolute,
            Expr::Here => Relocatable(here),
            Expr::LocalLabelRef { .. } => Unknown,
            Expr::Symbol(s) => {
                if s.modifier.is_some() {
                    return Unknown;
                }
                match self.module.symbols.get(&s.name).map(|s| s.def.clone()) {
                    Some(SymbolDef::Label(loc)) => Relocatable(loc.section),
                    Some(SymbolDef::Equate { .. }) => self.equate_class(&s.name, visiting, memo),
                    _ => Unknown,
                }
            }
            Expr::Unary(_, x) => match self.expr_class(x, here, visiting, memo) {
                Absolute => Absolute,
                _ => Unknown,
            },
            Expr::Binary(op, l, r) => {
                let (l, r) = (
                    self.expr_class(l, here, visiting, memo),
                    self.expr_class(r, here, visiting, memo),
                );
                match (op, l, r) {
                    (_, Absolute, Absolute) => Absolute,
                    (BinaryOp::Add, Relocatable(s), Absolute)
                    | (BinaryOp::Add, Absolute, Relocatable(s)) => Relocatable(s),
                    (BinaryOp::Sub, Relocatable(s), Absolute) => Relocatable(s),
                    (BinaryOp::Sub, Relocatable(a), Relocatable(b)) if a == b => Absolute,
                    _ => Unknown,
                }
            }
        }
    }

    // -- pass B ---------------------------------------------------------------

    fn pass_b(&mut self, program: &ast::Program) {
        let mut chunk: Option<Chunk> = None;
        for (i, stmt) in program.statements.iter().enumerate() {
            let sec = self.stmt_section[i];
            let len = self.module.sections[sec].items.len();
            match &mut chunk {
                Some(c) if c.section == sec => {}
                _ => {
                    if let Some(c) = chunk.take() {
                        self.close_chunk(c);
                    }
                    chunk = Some(Chunk {
                        section: sec,
                        start: len,
                        end: len,
                    });
                }
            }
            if let Some(item) = self.lower_statement(stmt, i, sec) {
                let loc = Loc {
                    section: sec,
                    item: self.module.sections[sec].items.len(),
                };
                match &item {
                    Item::Label { name, .. } => {
                        if let Some(s) = self.module.symbols.get_mut(name) {
                            if let SymbolDef::Label(l) = &mut s.def {
                                if l.section == sec && s.def_span == Some(stmt.span) {
                                    *l = loc;
                                }
                            }
                        }
                    }
                    Item::Set { symbol, .. } => {
                        if let Some(SymbolDef::Equate { at, .. }) =
                            self.module.symbols.get_mut(symbol).map(|s| &mut s.def)
                        {
                            *at = loc;
                        }
                    }
                    _ => {}
                }
                self.module.sections[sec].items.push(item);
            }
        }
        if let Some(c) = chunk {
            self.close_chunk(c);
        }
    }

    fn close_chunk(&mut self, mut c: Chunk) {
        c.end = self.module.sections[c.section].items.len();
        if c.end > c.start {
            self.module.chunks.push(c);
        }
    }

    fn lower_statement(&mut self, stmt: &ast::Statement, index: usize, sec: usize) -> Option<Item> {
        let span = stmt.span;
        let kind = self.module.sections[sec].kind;
        match &stmt.kind {
            StatementKind::Label(name) => Some(Item::Label {
                name: name.clone(),
                span,
            }),
            StatementKind::NumericLabel(_) => unreachable!("renamed before lowering"),
            StatementKind::Instruction(ins) => match self.lower_instruction(ins, index) {
                Ok(ins) => {
                    if !matches!(kind, SectionKind::Text | SectionKind::Other) {
                        self.diags.push(Diagnostic::warning(
                            format!(
                                "instruction in {} section `{}`",
                                kind.describe(),
                                self.module.sections[sec].name
                            ),
                            span,
                        ));
                    }
                    Some(Item::Instr(ins))
                }
                Err(d) => {
                    self.diags.push(d);
                    None
                }
            },
            StatementKind::Directive(d) => self.lower_directive(d, span, kind),
        }
    }

    fn lower_directive(&mut self, d: &Directive, span: Span, kind: SectionKind) -> Option<Item> {
        let bss_check = |this: &mut Self, what: &str| -> bool {
            if kind == SectionKind::Bss {
                this.diags.push(
                    Diagnostic::error(format!("{what} in a `.bss` section"), span)
                        .with_help("`.bss` holds zero-initialized data only; use `.zero`/`.skip` or move the data to `.data`"),
                );
                false
            } else {
                true
            }
        };
        match d {
            Directive::Values { width, values } => {
                if !bss_check(self, "initialized data") {
                    return None;
                }
                for v in values {
                    self.reference_symbols(v, span);
                    if let Some(x) = self.module.symbols.eval(v) {
                        if !fits(x, imm_range(width_size(*width), ImmMode::Full)) {
                            self.diags.push(Diagnostic::warning(
                                format!("value `{v}` = {x} does not fit in {} byte(s) and will be truncated", width),
                                span,
                            ));
                        }
                    }
                }
                Some(Item::Data {
                    data: Data::Values {
                        width: *width,
                        values: values.clone(),
                    },
                    span,
                })
            }
            Directive::Strings { kind: sk, values } => {
                if !bss_check(self, "string data") {
                    return None;
                }
                let mut bytes = Vec::new();
                for v in values {
                    bytes.extend_from_slice(v);
                    if *sk != ast::StringKind::Ascii {
                        bytes.push(0);
                    }
                }
                Some(Item::Data {
                    data: Data::Bytes(bytes),
                    span,
                })
            }
            Directive::Zero(size) => {
                self.check_fill_size(size, span);
                Some(Item::Data {
                    data: Data::Fill {
                        size: size.clone(),
                        fill: None,
                    },
                    span,
                })
            }
            Directive::Skip { size, fill } => {
                self.check_fill_size(size, span);
                if let Some(f) = fill {
                    if kind == SectionKind::Bss && self.module.symbols.eval(f) != Some(0) {
                        bss_check(self, "non-zero fill");
                        return None;
                    }
                }
                Some(Item::Data {
                    data: Data::Fill {
                        size: size.clone(),
                        fill: fill.clone(),
                    },
                    span,
                })
            }
            Directive::Align {
                kind: ak,
                alignment,
                fill,
                max,
            } => {
                let Some(v) = self.module.symbols.eval(alignment) else {
                    self.error("alignment must be a constant", span);
                    return None;
                };
                let bytes = match ak {
                    ast::AlignKind::P2align => {
                        if !(0..=30).contains(&v) {
                            self.error(
                                format!("alignment exponent {v} out of range (0..=30)"),
                                span,
                            );
                            return None;
                        }
                        1u64 << v
                    }
                    _ => {
                        if v <= 0 || (v as u64).count_ones() != 1 {
                            self.error(format!("alignment {v} is not a power of two"), span);
                            return None;
                        }
                        v as u64
                    }
                };
                Some(Item::Align {
                    bytes,
                    fill: fill.clone(),
                    max: max.clone(),
                    span,
                })
            }
            Directive::Set { symbol, value } => {
                self.reference_symbols(value, span);
                Some(Item::Set {
                    symbol: symbol.clone(),
                    value: value.clone(),
                    span,
                })
            }
            Directive::Comm {
                symbol,
                size,
                align,
                local,
            } => Some(Item::Common {
                symbol: symbol.clone(),
                size: size.clone(),
                align: align.clone(),
                local: *local,
                span,
            }),
            Directive::Metadata { .. } => {
                self.module.platform.dropped_metadata += 1;
                None
            }
            // Handled in pass A or carry no items.
            Directive::IntelSyntax
            | Directive::Text
            | Directive::Data
            | Directive::Bss
            | Directive::Section { .. }
            | Directive::Global(_)
            | Directive::Extern(_)
            | Directive::Local(_)
            | Directive::Type { .. }
            | Directive::Size { .. } => None,
        }
    }

    fn check_fill_size(&mut self, size: &Expr, span: Span) {
        self.reference_symbols(size, span);
        if let Some(v) = self.module.symbols.eval(size) {
            if v < 0 {
                self.error(format!("negative size {v}"), span);
            }
        }
    }

    /// Records references to symbols (creating external entries for
    /// undefined ones) and ELF relocation modifiers.
    fn reference_symbols(&mut self, e: &Expr, span: Span) {
        for s in e.symbols() {
            let sym = self.module.symbols.entry(&s.name);
            if sym.first_ref.is_none() {
                sym.first_ref = Some(span);
            }
            if let Some(m) = &s.modifier {
                self.module.platform.elf_relocs.push(ElfReloc {
                    symbol: s.name.clone(),
                    modifier: m.clone(),
                    span,
                });
            }
        }
    }

    fn finish(&mut self) {
        let mut externals = Vec::new();
        let mut warnings = Vec::new();
        for sym in self.module.symbols.iter() {
            if sym.is_defined() {
                continue;
            }
            if sym.first_ref.is_some() || sym.declared_extern || sym.global {
                externals.push(sym.name.clone());
            }
            if sym.first_ref.is_none() && (sym.ty.is_some() || sym.size.is_some()) {
                warnings.push(format!(
                    "`.type`/`.size` given for undefined symbol `{}`",
                    sym.name
                ));
            }
        }
        for w in warnings {
            self.diags.push(Diagnostic {
                severity: crate::diagnostics::Severity::Warning,
                message: w,
                span: None,
                help: Vec::new(),
            });
        }
        self.module.platform.externals = externals;
    }

    // -- instructions -------------------------------------------------------

    fn lower_instruction(&mut self, ins: &ast::Instruction, stmt: usize) -> LResult<Instr> {
        let span = ins.span;
        let mnemonic = Mnemonic::parse(&ins.mnemonic).ok_or_else(|| {
            Diagnostic::error(format!("unsupported instruction `{}`", ins.mnemonic), span)
                .with_help(format!("supported instructions: {SUPPORTED_MNEMONICS}"))
        })?;
        let mut lock = false;
        if let Some(p) = &ins.prefix {
            if p == "lock" {
                lock = true;
            } else {
                return Err(Diagnostic::error(
                    format!("prefix `{p}` is not supported with `{}`", ins.mnemonic),
                    span,
                ));
            }
        }
        let mut ops = Vec::with_capacity(ins.operands.len());
        for op in &ins.operands {
            let lowered = self.lower_operand(op, mnemonic.is_branch(), stmt)?;
            ops.push(Op {
                op: lowered,
                span: op.span,
            });
        }
        let size = validate(&self.module.symbols, mnemonic, &mut ops, span)?;
        check_high_byte_rex(&ops)?;
        if lock {
            use Mnemonic::*;
            let ok = matches!(
                mnemonic,
                Add | Sub | And | Or | Xor | Inc | Dec | Not | Neg | Xchg
            ) && matches!(
                ops.first(),
                Some(Op {
                    op: Operand::Mem(_),
                    ..
                })
            );
            if !ok {
                return Err(Diagnostic::error(
                    format!("`lock` cannot be used with this form of `{}`", ins.mnemonic),
                    span,
                )
                .with_help(
                    "`lock` requires a memory destination and an arithmetic/logic instruction",
                ));
            }
        }
        let operand_spans = ops.iter().map(|o| o.span).collect();
        Ok(Instr {
            mnemonic,
            lock,
            operands: ops.into_iter().map(|o| o.op).collect(),
            size,
            span,
            operand_spans,
        })
    }

    fn lower_operand(&mut self, op: &ast::Operand, branch: bool, stmt: usize) -> LResult<Operand> {
        let span = op.span;
        Ok(match &op.kind {
            OperandKind::Register(r) => match r.kind {
                RegKind::Segment(_) => {
                    return Err(Diagnostic::error(
                        format!("segment register `{r}` is not supported as an operand"),
                        span,
                    ))
                }
                RegKind::Rip => {
                    return Err(Diagnostic::error(
                        "`rip` can only be used as a memory base, e.g. `[rip + sym]`",
                        span,
                    ))
                }
                _ => Operand::Reg(*r),
            },
            OperandKind::Immediate(e) => Operand::Imm(e.clone()),
            OperandKind::Offset(e) => {
                if branch {
                    return Err(Diagnostic::error(
                        "`offset` cannot be used as a branch target",
                        span,
                    ));
                }
                self.reference_symbols(e, span);
                if self.is_absolute(e, None) {
                    Operand::Imm(e.clone())
                } else {
                    Operand::Addr(e.clone())
                }
            }
            OperandKind::Memory(m) => {
                if let Some(d) = &m.disp {
                    self.reference_symbols(d, span);
                }
                if m.segment.is_some() {
                    self.module.platform.segment_accesses.push(span);
                }
                Operand::Mem(Mem {
                    size: m.size,
                    segment: m.segment,
                    base: m.base,
                    index: m.index,
                    scale: if m.index.is_some() { m.scale } else { 1 },
                    disp: m.disp.clone(),
                })
            }
            OperandKind::Symbolic(e) => {
                self.reference_symbols(e, span);
                if branch {
                    Operand::Target(e.clone())
                } else if self.is_absolute(e, Some(stmt)) {
                    Operand::Imm(e.clone())
                } else {
                    // GNU as semantics: a bare symbol is a memory reference.
                    for s in e.symbols() {
                        if let Some(&def) = self.equate_stmt.get(&s.name) {
                            let absolute = matches!(
                                self.module.symbols.get(&s.name).map(|s| &s.def),
                                Some(SymbolDef::Equate {
                                    class: ValueClass::Absolute,
                                    ..
                                })
                            );
                            if def > stmt && absolute {
                                self.diags.push(
                                    Diagnostic::warning(
                                        format!(
                                            "`{}` is defined after this use, so GNU as treats this operand as a memory access to address `{}`, not as the value",
                                            s.name, s.name
                                        ),
                                        span,
                                    )
                                    .with_help(format!(
                                        "move the definition of `{}` before its first use, or write `offset {}` to use the value",
                                        s.name, s.name
                                    )),
                                );
                            }
                        }
                    }
                    Operand::Mem(Mem {
                        size: None,
                        disp: Some(e.clone()),
                        scale: 1,
                        ..Mem::default()
                    })
                }
            }
        })
    }

    /// `true` if `e` only consists of constants and absolute equates. With
    /// `before = Some(stmt)` the equates must be defined before statement
    /// `stmt` — GNU as decides bare-symbol operands in a single pass, while
    /// `offset expr` is resolved at the end of assembly.
    fn is_absolute(&self, e: &Expr, before: Option<usize>) -> bool {
        match e {
            Expr::Int(_) => true,
            Expr::Here | Expr::LocalLabelRef { .. } => false,
            Expr::Symbol(s) => {
                s.modifier.is_none()
                    && self
                        .equate_stmt
                        .get(&s.name)
                        .is_some_and(|&d| before.map_or(true, |stmt| d < stmt))
                    && matches!(
                        self.module.symbols.get(&s.name).map(|s| &s.def),
                        Some(SymbolDef::Equate {
                            class: ValueClass::Absolute,
                            ..
                        })
                    )
            }
            Expr::Unary(_, x) => self.is_absolute(x, before),
            Expr::Binary(_, l, r) => self.is_absolute(l, before) && self.is_absolute(r, before),
        }
    }
}

// ---------------------------------------------------------------------------
// Instruction validation
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImmMode {
    /// Full operand width (mov r64, imm64).
    Full,
    /// Sign-extended 32-bit for 64-bit operations.
    Sext32,
    /// 8-bit unsigned (shift counts).
    U8,
    /// 16-bit unsigned (`ret imm16`).
    U16,
}

fn width_size(width: u8) -> Size {
    match width {
        1 => Size::Byte,
        2 => Size::Word,
        4 => Size::Dword,
        _ => Size::Qword,
    }
}

fn imm_range(size: Size, mode: ImmMode) -> (i128, i128) {
    match (mode, size) {
        (ImmMode::U8, _) => (0, 255),
        (ImmMode::U16, _) => (0, 65535),
        (_, Size::Byte) => (-128, 255),
        (_, Size::Word) => (-32768, 65535),
        (_, Size::Dword) => (i32::MIN as i128, u32::MAX as i128),
        (ImmMode::Sext32, _) => (i32::MIN as i128, i32::MAX as i128),
        _ => (i64::MIN as i128, u64::MAX as i128),
    }
}

/// Literals above `i64::MAX` arrive wrapped (two's complement), so every
/// 64-bit pattern lies within the full 64-bit range `i64::MIN..=u64::MAX`.
fn fits(v: i64, (lo, hi): (i128, i128)) -> bool {
    (lo..=hi).contains(&(v as i128))
}

fn reg_desc(r: &Register) -> String {
    format!("`{r}` is {}-bit", r.size.bytes() * 8)
}

fn size_desc(s: Size) -> String {
    format!("{}-bit", s.bytes() * 8)
}

fn err(msg: impl Into<String>, span: Span) -> Diagnostic {
    Diagnostic::error(msg, span)
}

fn arity(m: Mnemonic, ops: &[Op], allowed: &[usize], span: Span) -> LResult<()> {
    if allowed.contains(&ops.len()) {
        return Ok(());
    }
    let expected = match allowed {
        [n] => format!("{n} operand{}", if *n == 1 { "" } else { "s" }),
        _ => format!(
            "{} operands",
            allowed
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" or ")
        ),
    };
    Err(err(
        format!("`{}` expects {expected}, found {}", m.name(), ops.len()),
        span,
    ))
}

/// Register or memory operand (`r/m`); rejects non-GPR registers.
fn require_rm(m: Mnemonic, op: &Op) -> LResult<()> {
    match &op.op {
        Operand::Reg(r) if r.is_gpr() => Ok(()),
        Operand::Reg(r) => Err(err(
            format!("register `{r}` cannot be used with `{}`", m.name()),
            op.span,
        )),
        Operand::Mem(_) => Ok(()),
        other => Err(err(
            format!(
                "`{}` expects a register or memory operand here, found {}",
                m.name(),
                describe(other)
            ),
            op.span,
        )),
    }
}

fn require_reg(m: Mnemonic, op: &Op) -> LResult<Register> {
    match &op.op {
        Operand::Reg(r) if r.is_gpr() => Ok(*r),
        other => Err(err(
            format!(
                "`{}` expects a general-purpose register here, found {}",
                m.name(),
                describe(other)
            ),
            op.span,
        )),
    }
}

fn describe(op: &Operand) -> String {
    match op {
        Operand::Reg(r) => format!("register `{r}`"),
        Operand::Imm(e) => format!("immediate `{e}`"),
        Operand::Addr(e) => format!("address `offset {e}`"),
        Operand::Mem(m) => format!("memory operand `{m}`"),
        Operand::Target(e) => format!("label `{e}`"),
    }
}

/// Unifies operand sizes of the given operands, fills in memory sizes and
/// returns the operation size.
fn unify(m: Mnemonic, ops: &mut [Op], idx: &[usize], span: Span) -> LResult<Size> {
    let mut found: Option<(Size, usize)> = None;
    for &i in idx {
        let s = match &ops[i].op {
            Operand::Reg(r) => r.size,
            Operand::Mem(Mem { size: Some(s), .. }) => *s,
            _ => continue,
        };
        match found {
            Some((fs, fi)) if fs != s => {
                let d = |o: &Operand, sz: Size| match o {
                    Operand::Reg(r) => reg_desc(r),
                    _ => format!("the memory operand is {}", size_desc(sz)),
                };
                return Err(err(
                    format!(
                        "operand size mismatch for `{}`: {}, but {}",
                        m.name(),
                        d(&ops[fi].op, fs),
                        d(&ops[i].op, s)
                    ),
                    ops[i].span,
                ));
            }
            None => found = Some((s, i)),
            _ => {}
        }
    }
    let Some((size, _)) = found else {
        return Err(err(format!("ambiguous operand size for `{}`", m.name()), span)
            .with_help("specify the size of the memory operand, e.g. `qword ptr [rax]` or `dword ptr [rax]`"));
    };
    for &i in idx {
        if let Operand::Mem(mem) = &mut ops[i].op {
            mem.size = Some(size);
        }
    }
    Ok(size)
}

fn check_imm(symbols: &SymbolTable, op: &Op, size: Size, mode: ImmMode) -> LResult<()> {
    if let Operand::Imm(e) = &op.op {
        if let Some(v) = symbols.eval(e) {
            let range = imm_range(size, mode);
            if !fits(v, range) {
                let what = match mode {
                    ImmMode::U8 => "an 8-bit unsigned immediate".to_string(),
                    ImmMode::U16 => "a 16-bit unsigned immediate".to_string(),
                    ImmMode::Sext32 if size == Size::Qword => {
                        "a sign-extended 32-bit immediate (64-bit operations only take imm32)"
                            .to_string()
                    }
                    _ if size == Size::Byte => "an 8-bit operand".to_string(),
                    _ => format!("a {} operand", size_desc(size)),
                };
                let mut d = err(
                    if e.to_string() == v.to_string() {
                        format!("immediate `{e}` does not fit in {what}")
                    } else {
                        format!("immediate `{e}` ({v}) does not fit in {what}")
                    },
                    op.span,
                );
                if mode == ImmMode::Sext32 && size == Size::Qword {
                    d = d.with_help("load the value into a register with `mov` first");
                }
                return Err(d);
            }
        }
    }
    Ok(())
}

fn default_mem_size(op: &mut Op, size: Size) {
    if let Operand::Mem(m) = &mut op.op {
        m.size.get_or_insert(size);
    }
}

/// Validates operand forms for `m`, fills in sizes and returns the operation size.
fn validate(
    symbols: &SymbolTable,
    m: Mnemonic,
    ops: &mut [Op],
    span: Span,
) -> LResult<Option<Size>> {
    use Mnemonic::*;
    let is = |o: &Op, f: fn(&Operand) -> bool| f(&o.op);
    let imm_like = |o: &Operand| matches!(o, Operand::Imm(_) | Operand::Addr(_));
    let mem = |o: &Operand| matches!(o, Operand::Mem(_));
    let target_err = |o: &Op| {
        err(
            format!("`{}` cannot be used with a label operand here", m.name()),
            o.span,
        )
    };
    for o in ops.iter() {
        if matches!(o.op, Operand::Target(_)) && !m.is_branch() {
            return Err(target_err(o));
        }
    }

    Ok(match m {
        Mov | Add | Sub | And | Or | Xor | Cmp => {
            arity(m, ops, &[2], span)?;
            require_rm(m, &ops[0])?;
            if !is(&ops[1], imm_like) {
                require_rm(m, &ops[1])?;
            }
            if is(&ops[0], mem) && is(&ops[1], mem) {
                return Err(err(
                    format!("`{}` cannot have two memory operands", m.name()),
                    ops[1].span,
                )
                .with_help("load one of them into a register first"));
            }
            let size = unify(m, ops, &[0, 1], span)?;
            let mode = if m == Mov && matches!(ops[0].op, Operand::Reg(_)) {
                ImmMode::Full
            } else {
                ImmMode::Sext32
            };
            check_imm(symbols, &ops[1], size, mode)?;
            Some(size)
        }
        Test => {
            arity(m, ops, &[2], span)?;
            require_rm(m, &ops[0])?;
            if !is(&ops[1], imm_like) {
                require_rm(m, &ops[1])?;
            }
            if is(&ops[0], mem) && is(&ops[1], mem) {
                return Err(err("`test` cannot have two memory operands", ops[1].span));
            }
            let size = unify(m, ops, &[0, 1], span)?;
            check_imm(symbols, &ops[1], size, ImmMode::Sext32)?;
            Some(size)
        }
        Lea => {
            arity(m, ops, &[2], span)?;
            let r = require_reg(m, &ops[0])?;
            if r.size == Size::Byte {
                return Err(err("`lea` cannot target an 8-bit register", ops[0].span));
            }
            match &mut ops[1].op {
                Operand::Mem(mm) => mm.size = None,
                other => {
                    return Err(err(
                        format!(
                            "`lea` requires a memory operand as source, found {}",
                            describe(other)
                        ),
                        ops[1].span,
                    )
                    .with_help("write e.g. `lea rax, [rip + label]`"))
                }
            }
            Some(r.size)
        }
        Imul => {
            arity(m, ops, &[1, 2, 3], span)?;
            match ops.len() {
                1 => {
                    require_rm(m, &ops[0])?;
                    Some(unify(m, ops, &[0], span)?)
                }
                _ => {
                    let r = require_reg(m, &ops[0])?;
                    if r.size == Size::Byte {
                        return Err(err(
                            "`imul` with two or three operands cannot use 8-bit registers",
                            ops[0].span,
                        ));
                    }
                    let imm_at = if ops.len() == 3 {
                        require_rm(m, &ops[1])?;
                        if !matches!(ops[2].op, Operand::Imm(_)) {
                            return Err(err(
                                "third operand of `imul` must be an immediate",
                                ops[2].span,
                            ));
                        }
                        Some(2)
                    } else if matches!(ops[1].op, Operand::Imm(_)) {
                        Some(1)
                    } else {
                        require_rm(m, &ops[1])?;
                        None
                    };
                    let size = if imm_at == Some(1) {
                        unify(m, ops, &[0], span)?
                    } else {
                        unify(m, ops, &[0, 1], span)?
                    };
                    if let Some(i) = imm_at {
                        check_imm(symbols, &ops[i], size, ImmMode::Sext32)?;
                    }
                    Some(size)
                }
            }
        }
        Mul | Idiv | Div | Not | Neg | Inc | Dec => {
            arity(m, ops, &[1], span)?;
            require_rm(m, &ops[0])?;
            Some(unify(m, ops, &[0], span)?)
        }
        Shl | Shr | Sar => {
            arity(m, ops, &[1, 2], span)?;
            require_rm(m, &ops[0])?;
            let size = unify(m, ops, &[0], span)?;
            if ops.len() == 2 {
                match &ops[1].op {
                    Operand::Imm(_) => check_imm(symbols, &ops[1], Size::Byte, ImmMode::U8)?,
                    Operand::Reg(r) if *r == Register::gpr(1, Size::Byte) => {}
                    _ => {
                        return Err(err(
                            format!("shift count of `{}` must be an immediate or `cl`", m.name()),
                            ops[1].span,
                        ))
                    }
                }
            }
            Some(size)
        }
        Movzx | Movsx => {
            arity(m, ops, &[2], span)?;
            let dst = require_reg(m, &ops[0])?;
            require_rm(m, &ops[1])?;
            let src = match &ops[1].op {
                Operand::Reg(r) => r.size,
                Operand::Mem(Mem { size: Some(s), .. }) => *s,
                _ => {
                    return Err(err(
                        format!("ambiguous source size for `{}`", m.name()),
                        ops[1].span,
                    )
                    .with_help("write `byte ptr [...]` or `word ptr [...]`"))
                }
            };
            if !matches!(src, Size::Byte | Size::Word) || src >= dst.size || dst.size == Size::Byte
            {
                return Err(err(
                    format!(
                        "`{}` needs an 8- or 16-bit source and a larger destination, found {} source and {}",
                        m.name(),
                        size_desc(src),
                        reg_desc(&dst)
                    ),
                    span,
                ));
            }
            Some(dst.size)
        }
        Movsxd => {
            arity(m, ops, &[2], span)?;
            let dst = require_reg(m, &ops[0])?;
            require_rm(m, &ops[1])?;
            default_mem_size(&mut ops[1], Size::Dword);
            let src = match &ops[1].op {
                Operand::Reg(r) => r.size,
                Operand::Mem(Mem { size: Some(s), .. }) => *s,
                _ => unreachable!(),
            };
            if dst.size != Size::Qword || src != Size::Dword {
                return Err(err(
                    "`movsxd` needs a 64-bit destination and a 32-bit source",
                    span,
                ));
            }
            Some(Size::Qword)
        }
        Xchg => {
            arity(m, ops, &[2], span)?;
            require_rm(m, &ops[0])?;
            require_rm(m, &ops[1])?;
            if is(&ops[0], mem) && is(&ops[1], mem) {
                return Err(err("`xchg` cannot have two memory operands", ops[1].span));
            }
            Some(unify(m, ops, &[0, 1], span)?)
        }
        Setcc(_) => {
            arity(m, ops, &[1], span)?;
            require_rm(m, &ops[0])?;
            default_mem_size(&mut ops[0], Size::Byte);
            let size = unify(m, ops, &[0], span)?;
            if size != Size::Byte {
                return Err(err(
                    format!(
                        "`{}` needs an 8-bit operand, found {}",
                        m.name(),
                        size_desc(size)
                    ),
                    ops[0].span,
                )
                .with_help("write e.g. `sete al` and widen the result with `movzx eax, al`"));
            }
            Some(Size::Byte)
        }
        Cmovcc(_) => {
            arity(m, ops, &[2], span)?;
            let dst = require_reg(m, &ops[0])?;
            if dst.size == Size::Byte {
                return Err(err(
                    format!("`{}` cannot use 8-bit registers", m.name()),
                    ops[0].span,
                ));
            }
            require_rm(m, &ops[1])?;
            Some(unify(m, ops, &[0, 1], span)?)
        }
        Push | Pop => {
            arity(m, ops, &[1], span)?;
            match &ops[0].op {
                Operand::Imm(_) | Operand::Addr(_) if m == Push => {
                    check_imm(symbols, &ops[0], Size::Qword, ImmMode::Sext32)?;
                    Some(Size::Qword)
                }
                Operand::Reg(_) | Operand::Mem(_) => {
                    require_rm(m, &ops[0])?;
                    default_mem_size(&mut ops[0], Size::Qword);
                    let size = unify(m, ops, &[0], span)?;
                    if !matches!(size, Size::Qword | Size::Word) {
                        return Err(err(
                            format!(
                                "`{}` only supports 64-bit (or 16-bit) operands in 64-bit mode",
                                m.name()
                            ),
                            ops[0].span,
                        )
                        .with_help("use the 64-bit register, e.g. `rax` instead of `eax`"));
                    }
                    Some(size)
                }
                other => {
                    return Err(err(
                        format!("invalid operand for `{}`: {}", m.name(), describe(other)),
                        ops[0].span,
                    ))
                }
            }
        }
        Jmp | Call => {
            arity(m, ops, &[1], span)?;
            match &ops[0].op {
                Operand::Target(_) => None,
                Operand::Reg(r) if r.is_gpr() && r.size == Size::Qword => None,
                Operand::Mem(_) => {
                    default_mem_size(&mut ops[0], Size::Qword);
                    if unify(m, ops, &[0], span)? != Size::Qword {
                        return Err(err(
                            format!("indirect `{}` requires a 64-bit memory operand", m.name()),
                            ops[0].span,
                        ));
                    }
                    None
                }
                other => {
                    return Err(err(
                        format!("invalid target for `{}`: {}", m.name(), describe(other)),
                        ops[0].span,
                    )
                    .with_help("use a label, a 64-bit register or a qword memory operand"))
                }
            }
        }
        Jcc(_) => {
            arity(m, ops, &[1], span)?;
            match &ops[0].op {
                Operand::Target(_) => None,
                other => {
                    return Err(err(
                        format!(
                            "conditional jumps need a label target, found {}",
                            describe(other)
                        ),
                        ops[0].span,
                    ))
                }
            }
        }
        Ret => {
            arity(m, ops, &[0, 1], span)?;
            if let Some(op) = ops.first() {
                if !matches!(op.op, Operand::Imm(_)) {
                    return Err(err("`ret` only takes an immediate operand", op.span));
                }
                check_imm(symbols, op, Size::Word, ImmMode::U16)?;
            }
            None
        }
        Nop => {
            arity(m, ops, &[0, 1], span)?;
            if !ops.is_empty() {
                require_rm(m, &ops[0])?;
                let s = unify(m, ops, &[0], span)?;
                if !matches!(s, Size::Word | Size::Dword) {
                    return Err(err(
                        "multi-byte `nop` takes a 16- or 32-bit operand",
                        ops[0].span,
                    ));
                }
                Some(s)
            } else {
                None
            }
        }
        Cbw | Cwde | Cdqe | Cwd | Cdq | Cqo | Leave | Pushfq | Popfq | Syscall => {
            arity(m, ops, &[0], span)?;
            None
        }
    })
}

/// `ah`/`bh`/`ch`/`dh` cannot be encoded together with a REX prefix.
fn check_high_byte_rex(ops: &[Op]) -> LResult<()> {
    let Some(high) = ops.iter().find(|o| {
        matches!(
            o.op,
            Operand::Reg(Register {
                kind: RegKind::HighByte(_),
                ..
            })
        )
    }) else {
        return Ok(());
    };
    let needs_rex = |r: &Register| match r.kind {
        RegKind::Gpr(n) => {
            n >= 8 || r.size == Size::Qword || (r.size == Size::Byte && (4..8).contains(&n))
        }
        _ => false,
    };
    for o in ops {
        let rex = match &o.op {
            Operand::Reg(r) => needs_rex(r),
            Operand::Mem(m) => [m.base, m.index]
                .iter()
                .flatten()
                .any(|r| matches!(r.kind, RegKind::Gpr(n) if n >= 8)),
            _ => false,
        };
        if rex {
            return Err(err(
                format!(
                    "`{}` cannot be used in an instruction that requires a REX prefix",
                    high.op
                ),
                o.span,
            )
            .with_help("use `al`/`bl`/`cl`/`dl` or a register without r8–r15/spl/bpl/sil/dil"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn lower_src(src: &str) -> LowerOutput {
        let p = parse(src);
        assert!(!p.has_errors(), "parse errors: {:#?}", p.diagnostics);
        lower(&p.program)
    }

    fn lower_ok(src: &str) -> Module {
        let out = lower_src(src);
        assert!(!out.has_errors(), "lowering errors: {:#?}", out.diagnostics);
        out.module
    }

    fn first_error(src: &str) -> String {
        let out = lower_src(src);
        out.diagnostics
            .iter()
            .find(|d| d.is_error())
            .map(ToString::to_string)
            .unwrap_or_else(|| panic!("expected an error for {src:?}"))
    }

    fn instrs(m: &Module) -> Vec<&Instr> {
        m.instructions().map(|(_, i)| i).collect()
    }

    #[test]
    fn operand_semantics_follow_gas() {
        let m = lower_ok(
            ".data\nmsg: .ascii \"hi\"\nlen = . - msg\nk = 5\n.text\n\
             mov rdx, len\nmov rdx, msg\nmov rsi, offset msg\nlea rsi, [rip + msg]\nmov eax, k\njmp done\ndone: ret\n",
        );
        let i = instrs(&m);
        assert_eq!(i[0].operands[1], Operand::Imm(Expr::symbol("len")));
        assert!(
            matches!(&i[1].operands[1], Operand::Mem(mm) if mm.size == Some(Size::Qword) && mm.is_absolute())
        );
        assert_eq!(i[2].operands[1], Operand::Addr(Expr::symbol("msg")));
        assert_eq!(i[2].operands[1].to_string(), "offset msg");
        assert_eq!(i[0].operands[1].to_string(), "offset len");
        assert!(
            matches!(&i[3].operands[1], Operand::Mem(mm) if mm.size.is_none() && mm.is_rip_relative())
        );
        assert_eq!(i[4].operands[1], Operand::Imm(Expr::symbol("k")));
        assert_eq!(i[5].operands[0], Operand::Target(Expr::symbol("done")));
    }

    #[test]
    fn forward_equate_is_memory_with_warning() {
        let out = lower_src(".text\nmov rdx, later\n.data\nlater = 7\n");
        assert!(!out.has_errors());
        let w = &out.diagnostics[0];
        assert!(w.message.contains("defined after this use"), "{w}");
        let m = out.module;
        assert!(matches!(&instrs(&m)[0].operands[1], Operand::Mem(_)));
        // `offset` is resolved at the end of assembly: a constant.
        let m = lower_ok(".text\nmov rdx, offset later\n.data\nlater = 7\n");
        assert_eq!(
            instrs(&m)[0].operands[1],
            Operand::Imm(Expr::symbol("later"))
        );
    }

    #[test]
    fn sizes_are_inferred_and_checked() {
        let m =
            lower_ok("mov qword ptr [rax], 1\nmov [rbx], ecx\ninc byte ptr [rdi]\npush [rax]\n");
        let i = instrs(&m);
        assert_eq!(i[0].size, Some(Size::Qword));
        assert!(matches!(&i[1].operands[0], Operand::Mem(mm) if mm.size == Some(Size::Dword)));
        assert_eq!(i[2].size, Some(Size::Byte));
        assert_eq!(i[3].size, Some(Size::Qword));

        assert!(first_error("mov rax, ebx")
            .contains("operand size mismatch for `mov`: `rax` is 64-bit, but `ebx` is 32-bit"));
        assert!(first_error("mov [rax], 1").contains("ambiguous operand size"));
        assert!(first_error("inc [rax]").contains("ambiguous operand size"));
        assert!(first_error("mov dword ptr [rax], rbx").contains("mismatch"));
        assert!(first_error("push eax").contains("only supports 64-bit"));
    }

    #[test]
    fn immediates_are_range_checked() {
        lower_ok("mov rax, 0x1122334455667788\nmov eax, 0xFFFFFFFF\nadd rax, -0x80000000\nmov al, 255\nmov al, -128\n");
        assert!(first_error("add rax, 0xFFFFFFFF").contains("sign-extended 32-bit"));
        assert!(first_error("mov al, 256").contains("does not fit in an 8-bit operand"));
        assert!(first_error("mov qword ptr [rax], 0x100000000").contains("sign-extended"));
        assert!(first_error("shl rax, 300").contains("8-bit unsigned"));
        assert!(first_error("k = 300\nmov bl, k").contains("(300) does not fit"));
    }

    #[test]
    fn invalid_forms() {
        assert!(first_error("mov [rax], [rbx]").contains("two memory operands"));
        assert!(first_error("lea rax, rbx").contains("requires a memory operand"));
        assert!(first_error("mov rax").contains("expects 2 operands, found 1"));
        assert!(first_error("je rax").contains("need a label target"));
        assert!(first_error("shl rax, rbx").contains("must be an immediate or `cl`"));
        assert!(first_error("movzx eax, [rbx]").contains("ambiguous source size"));
        assert!(first_error("mov ah, sil").contains("REX prefix"));
        assert!(first_error("lock add rax, 1").contains("`lock` cannot be used"));
        assert!(first_error("mov fs, ax").contains("segment register"));
        assert!(first_error("mov rax, 5\n jmp offset x").contains("branch target"));
        assert!(first_error("mov 5, rax").contains("register or memory operand"));
    }

    #[test]
    fn unsupported_instruction_has_location_and_help() {
        let out = lower_src("nop\n  cpuid\n");
        let d = out.diagnostics.iter().find(|d| d.is_error()).unwrap();
        assert_eq!(d.message, "unsupported instruction `cpuid`");
        assert_eq!(d.span, Some(Span::new(2, 3, 5)));
        assert!(d.help[0].contains("supported instructions"));
    }

    #[test]
    fn setcc_and_cmovcc_forms() {
        let m = lower_ok(
            "cmp rdi, rsi\nsete al\nsetnz [rdx]\nsetnbe byte ptr [rdx + 1]\nsetg r9b\n\
             cmovl eax, esi\ncmovnge rax, [rdx]\ncmovz cx, dx\n",
        );
        let listing: Vec<String> = instrs(&m).iter().map(ToString::to_string).collect();
        assert_eq!(
            listing[1..],
            [
                "sete al",
                "setne byte ptr [rdx]",
                "seta byte ptr [rdx + 1]",
                "setg r9b",
                "cmovl eax, esi",
                "cmovl rax, qword ptr [rdx]",
                "cmove cx, dx",
            ]
        );
        assert_eq!(instrs(&m)[1].size, Some(Size::Byte));
        assert_eq!(instrs(&m)[6].size, Some(Size::Qword));
        assert!(first_error("sete eax").contains("needs an 8-bit operand"));
        assert!(first_error("sete word ptr [rax]").contains("needs an 8-bit operand"));
        assert!(first_error("sete 1").contains("register or memory operand"));
        assert!(first_error("sete al, bl").contains("expects 1 operand"));
        assert!(first_error("cmove al, bl").contains("cannot use 8-bit registers"));
        assert!(first_error("cmove eax, 1").contains("register or memory operand"));
        assert!(first_error("cmove [rax], ebx").contains("general-purpose register"));
        assert!(first_error("cmove rax, ebx").contains("size mismatch"));
        assert!(first_error("setx al").contains("unsupported instruction `setx`"));
        assert!(first_error("cmov eax, ebx").contains("unsupported instruction `cmov`"));
    }

    #[test]
    fn jcc_aliases_are_canonical() {
        let m = lower_ok(
            "a: jz a\njnz a\njnle a\njnge a\njnl a\njng a\njnbe a\njnae a\njnb a\njna a\n",
        );
        let names: Vec<String> = instrs(&m)
            .iter()
            .map(|i| i.mnemonic.name().into_owned())
            .collect();
        assert_eq!(
            names,
            ["je", "jne", "jg", "jl", "jge", "jle", "ja", "jb", "jae", "jbe"]
        );
    }

    #[test]
    fn numeric_labels_are_renamed() {
        let m = lower_ok("1: dec ecx\njnz 1b\njmp 1f\n1: ret\n");
        let i = instrs(&m);
        assert_eq!(i[1].operands[0], Operand::Target(Expr::symbol(".Ll2w_1_0")));
        assert_eq!(i[2].operands[0], Operand::Target(Expr::symbol(".Ll2w_1_1")));
        assert!(first_error("jmp 2b").contains("no local label `2:` before"));
        assert!(first_error("jmp 3f\n").contains("no local label `3:` after"));
    }

    #[test]
    fn sections_symbols_and_chunks() {
        let m = lower_ok(
            ".section .rodata\nmsg: .asciz \"x\"\n.text\n.globl _start\n_start: nop\n.data\nv: .quad msg\n.text\n ret\n.bss\nbuf: .zero 8\n.comm big, 64, 8\n",
        );
        let kinds: Vec<_> = m
            .sections
            .iter()
            .map(|s| (s.name.as_str(), s.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                (".rodata", SectionKind::ReadOnly),
                (".text", SectionKind::Text),
                (".data", SectionKind::Data),
                (".bss", SectionKind::Bss)
            ]
        );
        // .text is entered twice -> two chunks of the same section
        assert_eq!(m.chunks.iter().filter(|c| c.section == 1).count(), 2);
        assert!(m.symbols.get("_start").unwrap().global);
        assert!(matches!(
            m.symbols.get("msg").unwrap().def,
            SymbolDef::Label(Loc {
                section: 0,
                item: 0
            })
        ));
        assert!(matches!(
            m.symbols.get("big").unwrap().def,
            SymbolDef::Common { local: false }
        ));
        assert_eq!(
            m.sections[0].items[1],
            Item::Data {
                data: Data::Bytes(b"x\0".to_vec()),
                span: Span::new(2, 6, 10)
            }
        );
    }

    #[test]
    fn data_errors() {
        assert!(first_error(".bss\n.byte 1").contains("initialized data in a `.bss` section"));
        assert!(first_error("x: nop\nx: nop").contains("symbol `x` is already defined"));
        assert!(first_error(".align 3").contains("not a power of two"));
        assert!(first_error("a = b\nb = a\n").contains("defined in terms of itself"));
        let m = lower_ok(".p2align 4\n.align 8\n");
        assert!(matches!(
            m.sections[0].items[0],
            Item::Align { bytes: 16, .. }
        ));
        assert!(matches!(
            m.sections[0].items[1],
            Item::Align { bytes: 8, .. }
        ));
    }

    #[test]
    fn externals_and_elf_relocs() {
        let m = lower_ok(".extern exit\nmain: call puts@PLT\nmov rax, qword ptr [rip + stdout@GOTPCREL]\nmov rax, fs:0x28\ncall exit\n");
        assert_eq!(m.platform.externals, ["exit", "puts", "stdout"]);
        assert_eq!(m.platform.elf_relocs.len(), 2);
        assert_eq!(m.platform.elf_relocs[0].modifier, "PLT");
        assert_eq!(m.platform.segment_accesses.len(), 1);
    }
}
