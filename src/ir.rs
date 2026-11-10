//! Intermediate representation.
//!
//! The IR is produced from the AST by [`lower()`] and is the single source of
//! truth for all translation passes. It separates two layers:
//!
//! * **Architecture layer** — [`Section`]s containing [`Item`]s: labels,
//!   validated [`Instr`]uctions with resolved operand semantics and sizes,
//!   data, alignment and equates. Nothing here is OS-specific.
//! * **Platform layer** — [`Platform`]: everything that ties the program to
//!   Linux/ELF: the entry point, system call sites (with statically known
//!   register values), calls to external functions, ELF relocation
//!   modifiers (`@PLT`, `@GOTPCREL`), thread-local `fs:`/`gs:` accesses and
//!   dropped ELF metadata.
//!
//! On top of both, [`SymbolTable`] and [`Function`]s describe the program
//! structure.
//!
//! # Operand semantics
//!
//! Lowering resolves the ambiguity of Intel syntax exactly as GNU `as` does:
//!
//! | Source | IR |
//! |---|---|
//! | `jmp L`, `call f` | [`Operand::Target`] |
//! | `mov rax, 5`, `mov rdx, len` (`len` an absolute equate defined *earlier*) | [`Operand::Imm`] |
//! | `mov rax, offset msg` | [`Operand::Addr`] |
//! | `mov rax, msg`, `mov rax, len` (`len` defined *later*) | [`Operand::Mem`] (absolute) |
//!
//! The last case is a well-known GNU `as` pitfall; lowering keeps GAS
//! semantics but emits a warning.
//!
//! [`Display`](fmt::Display) on [`Module`] prints a normalized GAS listing
//! that assembles to the same object code as the original input.

use crate::ast::{escape_string, symbol_name, Expr, RegKind, Register, Size, SymbolType};
use crate::diagnostics::Span;
use std::collections::HashMap;
use std::fmt;

mod analysis;
mod lower;

pub use lower::{lower, LowerOutput};

// ===========================================================================
// Architecture layer
// ===========================================================================

/// Condition codes of `jcc` (and later `setcc`/`cmovcc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cond {
    O,
    No,
    B,
    Ae,
    E,
    Ne,
    Be,
    A,
    S,
    Ns,
    P,
    Np,
    L,
    Ge,
    Le,
    G,
}

impl Cond {
    /// Parses a condition suffix including all aliases (`z`, `nae`, `nle`, ...).
    pub fn parse(s: &str) -> Option<Cond> {
        use Cond::*;
        Some(match s {
            "o" => O,
            "no" => No,
            "b" | "nae" | "c" => B,
            "ae" | "nb" | "nc" => Ae,
            "e" | "z" => E,
            "ne" | "nz" => Ne,
            "be" | "na" => Be,
            "a" | "nbe" => A,
            "s" => S,
            "ns" => Ns,
            "p" | "pe" => P,
            "np" | "po" => Np,
            "l" | "nge" => L,
            "ge" | "nl" => Ge,
            "le" | "ng" => Le,
            "g" | "nle" => G,
            _ => return None,
        })
    }

    /// Canonical suffix.
    pub fn suffix(self) -> &'static str {
        use Cond::*;
        match self {
            O => "o",
            No => "no",
            B => "b",
            Ae => "ae",
            E => "e",
            Ne => "ne",
            Be => "be",
            A => "a",
            S => "s",
            Ns => "ns",
            P => "p",
            Np => "np",
            L => "l",
            Ge => "ge",
            Le => "le",
            G => "g",
        }
    }

    pub fn negate(self) -> Cond {
        use Cond::*;
        match self {
            O => No,
            No => O,
            B => Ae,
            Ae => B,
            E => Ne,
            Ne => E,
            Be => A,
            A => Be,
            S => Ns,
            Ns => S,
            P => Np,
            Np => P,
            L => Ge,
            Ge => L,
            Le => G,
            G => Le,
        }
    }
}

/// Supported instructions. Anything else is rejected during lowering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mnemonic {
    Mov,
    Movzx,
    Movsx,
    Movsxd,
    Lea,
    Add,
    Sub,
    Imul,
    Mul,
    Idiv,
    Div,
    And,
    Or,
    Xor,
    Not,
    Neg,
    Inc,
    Dec,
    Cmp,
    Test,
    Shl,
    Shr,
    Sar,
    Xchg,
    /// `cbw`/`cwde`/`cdqe`: sign-extend al/ax/eax into ax/eax/rax.
    Cbw,
    Cwde,
    Cdqe,
    /// `cwd`/`cdq`/`cqo`: sign-extend ax/eax/rax into dx/edx/rdx.
    Cwd,
    Cdq,
    Cqo,
    Jmp,
    Jcc(Cond),
    Call,
    Ret,
    Push,
    Pop,
    Leave,
    /// `pushfq`/`popfq`: push/pop RFLAGS.
    Pushfq,
    Popfq,
    Nop,
    Syscall,
}

/// Maximum length of a chain of equates defined in terms of each other
/// (`.set a1, a0 + 1`, `.set a2, a1 + 1`, ...).
pub const MAX_EQUATE_CHAIN: u32 = 100;

/// Mnemonics accepted by [`Mnemonic::parse`] (for help messages).
pub const SUPPORTED_MNEMONICS: &str = "mov movzx movsx movsxd lea add sub imul mul idiv div and or xor not neg inc dec cmp test shl/sal shr sar xchg cbw cwde cdqe cwd cdq cqo jmp j<cc> call ret push pop pushfq popfq leave nop syscall";

impl Mnemonic {
    pub fn parse(s: &str) -> Option<Mnemonic> {
        use Mnemonic::*;
        Some(match s {
            "mov" => Mov,
            "movzx" => Movzx,
            "movsx" => Movsx,
            "movsxd" => Movsxd,
            "lea" => Lea,
            "add" => Add,
            "sub" => Sub,
            "imul" => Imul,
            "mul" => Mul,
            "idiv" => Idiv,
            "div" => Div,
            "and" => And,
            "or" => Or,
            "xor" => Xor,
            "not" => Not,
            "neg" => Neg,
            "inc" => Inc,
            "dec" => Dec,
            "cmp" => Cmp,
            "test" => Test,
            "shl" | "sal" => Shl,
            "shr" => Shr,
            "sar" => Sar,
            "xchg" => Xchg,
            "cbw" => Cbw,
            "cwde" => Cwde,
            "cdqe" => Cdqe,
            "cwd" => Cwd,
            "cdq" => Cdq,
            "cqo" => Cqo,
            "jmp" => Jmp,
            "call" => Call,
            "ret" => Ret,
            "push" => Push,
            "pop" => Pop,
            "leave" => Leave,
            "pushfq" | "pushf" => Pushfq,
            "popfq" | "popf" => Popfq,
            "nop" => Nop,
            "syscall" => Syscall,
            _ => return s.strip_prefix('j').and_then(Cond::parse).map(Jcc),
        })
    }

    pub fn name(self) -> std::borrow::Cow<'static, str> {
        use Mnemonic::*;
        std::borrow::Cow::Borrowed(match self {
            Mov => "mov",
            Movzx => "movzx",
            Movsx => "movsx",
            Movsxd => "movsxd",
            Lea => "lea",
            Add => "add",
            Sub => "sub",
            Imul => "imul",
            Mul => "mul",
            Idiv => "idiv",
            Div => "div",
            And => "and",
            Or => "or",
            Xor => "xor",
            Not => "not",
            Neg => "neg",
            Inc => "inc",
            Dec => "dec",
            Cmp => "cmp",
            Test => "test",
            Shl => "shl",
            Shr => "shr",
            Sar => "sar",
            Xchg => "xchg",
            Cbw => "cbw",
            Cwde => "cwde",
            Cdqe => "cdqe",
            Cwd => "cwd",
            Cdq => "cdq",
            Cqo => "cqo",
            Jmp => "jmp",
            Jcc(c) => return std::borrow::Cow::Owned(format!("j{}", c.suffix())),
            Call => "call",
            Ret => "ret",
            Push => "push",
            Pop => "pop",
            Leave => "leave",
            Pushfq => "pushfq",
            Popfq => "popfq",
            Nop => "nop",
            Syscall => "syscall",
        })
    }

    /// Direct/indirect jumps and calls (operands are branch targets).
    pub fn is_branch(self) -> bool {
        matches!(self, Mnemonic::Jmp | Mnemonic::Jcc(_) | Mnemonic::Call)
    }
}

/// A validated memory operand.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mem {
    /// Access size; `None` only for `lea` (no memory access).
    pub size: Option<Size>,
    pub segment: Option<Register>,
    pub base: Option<Register>,
    pub index: Option<Register>,
    pub scale: u8,
    pub disp: Option<Expr>,
}

impl Mem {
    pub fn is_rip_relative(&self) -> bool {
        matches!(
            self.base,
            Some(Register {
                kind: RegKind::Rip,
                ..
            })
        )
    }

    /// `[sym]` without base/index: an absolute 32-bit address (needs
    /// `R_X86_64_32S` on Linux; must become RIP-relative on Windows).
    pub fn is_absolute(&self) -> bool {
        self.base.is_none() && self.index.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operand {
    Reg(Register),
    /// Assembler-time constant: integers and absolute equates.
    Imm(Expr),
    /// Link-time address of a symbol used as an immediate (`offset sym`).
    Addr(Expr),
    Mem(Mem),
    /// Direct branch target of `jmp`/`jcc`/`call`.
    Target(Expr),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instr {
    pub mnemonic: Mnemonic,
    pub lock: bool,
    pub operands: Vec<Operand>,
    /// Operation size, where meaningful.
    pub size: Option<Size>,
    /// Span of the mnemonic.
    pub span: Span,
    /// Source span of each operand.
    pub operand_spans: Vec<Span>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Data {
    /// `.byte`/`.word`/`.long`/`.quad` values of `width` bytes each.
    Values { width: u8, values: Vec<Expr> },
    /// Raw bytes from `.ascii`/`.asciz`/`.string` (terminators included).
    Bytes(Vec<u8>),
    /// `.zero`/`.skip`/`.space`.
    Fill { size: Expr, fill: Option<Expr> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Label {
        name: String,
        span: Span,
    },
    Instr(Instr),
    Data {
        data: Data,
        span: Span,
    },
    /// Alignment normalized to a byte count (`.align`/`.balign` bytes,
    /// `.p2align` exponent).
    Align {
        bytes: u64,
        fill: Option<Expr>,
        max: Option<Expr>,
        span: Span,
    },
    /// Equate defined at this position (`.` in `value` refers to it).
    Set {
        symbol: String,
        value: Expr,
        span: Span,
    },
    /// `.comm` / `.lcomm`.
    Common {
        symbol: String,
        size: Expr,
        align: Option<Expr>,
        local: bool,
        span: Span,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    Text,
    Data,
    Bss,
    ReadOnly,
    /// `.note*` (e.g. `.note.GNU-stack`): ELF-only metadata.
    Note,
    Other,
}

impl SectionKind {
    /// Classifies an ELF section by name, flags and type.
    pub fn classify(name: &str, flags: Option<&str>, elf_type: Option<&str>) -> SectionKind {
        let prefixed = |p: &str| name == p || name.starts_with(&format!("{p}."));
        if prefixed(".text") {
            SectionKind::Text
        } else if prefixed(".bss") || elf_type == Some("@nobits") {
            SectionKind::Bss
        } else if prefixed(".rodata") {
            SectionKind::ReadOnly
        } else if prefixed(".data") {
            SectionKind::Data
        } else if name.starts_with(".note") {
            SectionKind::Note
        } else {
            match flags {
                Some(f) if f.contains('x') => SectionKind::Text,
                Some(f) if f.contains('w') => SectionKind::Data,
                Some(f) if f.contains('a') => SectionKind::ReadOnly,
                _ => SectionKind::Other,
            }
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            SectionKind::Text => "code",
            SectionKind::Data => "data",
            SectionKind::Bss => "bss",
            SectionKind::ReadOnly => "read-only data",
            SectionKind::Note => "ELF note",
            SectionKind::Other => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// Original (ELF) name.
    pub name: String,
    pub kind: SectionKind,
    pub flags: Option<String>,
    pub elf_type: Option<String>,
    pub items: Vec<Item>,
    /// Where the section was first entered (`None` for generated sections).
    pub span: Option<Span>,
}

/// A contiguous run of items of one section, in source order. Sections
/// can be entered several times; chunks preserve the original
/// interleaving, which matters for GAS (equates must stay ahead of uses).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    pub section: usize,
    pub start: usize,
    pub end: usize,
}

/// Position of an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Loc {
    pub section: usize,
    pub item: usize,
}

// ===========================================================================
// Symbols
// ===========================================================================

/// Static classification of an equate's value (as GAS sees it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueClass {
    /// Assembler-time constant (e.g. `5`, `. - msg`).
    Absolute,
    /// Address within a section (e.g. `msg + 4`).
    Relocatable(usize),
    /// Depends on undefined/common symbols.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolDef {
    Label(Loc),
    Equate {
        value: Expr,
        class: ValueClass,
        at: Loc,
    },
    Common {
        local: bool,
    },
    /// Referenced or declared but not defined in this file (external).
    Undefined,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub def: SymbolDef,
    /// `.globl`/`.global`
    pub global: bool,
    /// `.extern`
    pub declared_extern: bool,
    /// `.type`
    pub ty: Option<SymbolType>,
    /// `.size`
    pub size: Option<Expr>,
    pub def_span: Option<Span>,
    pub first_ref: Option<Span>,
}

impl Symbol {
    fn new(name: &str) -> Symbol {
        Symbol {
            name: name.to_string(),
            def: SymbolDef::Undefined,
            global: false,
            declared_extern: false,
            ty: None,
            size: None,
            def_span: None,
            first_ref: None,
        }
    }

    pub fn is_defined(&self) -> bool {
        !matches!(self.def, SymbolDef::Undefined)
    }
}

/// Symbols in order of first appearance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SymbolTable {
    symbols: Vec<Symbol>,
    index: HashMap<String, usize>,
}

impl SymbolTable {
    pub fn get(&self, name: &str) -> Option<&Symbol> {
        self.index.get(name).map(|&i| &self.symbols[i])
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Symbol> {
        self.index.get(name).map(|&i| &mut self.symbols[i])
    }

    /// Returns the symbol, creating an undefined entry if needed.
    pub fn entry(&mut self, name: &str) -> &mut Symbol {
        let i = match self.index.get(name) {
            Some(&i) => i,
            None => {
                self.symbols.push(Symbol::new(name));
                self.index.insert(name.to_string(), self.symbols.len() - 1);
                self.symbols.len() - 1
            }
        };
        &mut self.symbols[i]
    }

    pub fn iter(&self) -> impl Iterator<Item = &Symbol> {
        self.symbols.iter()
    }

    /// Renames a symbol table entry (references in the code are not touched;
    /// see [`Module::rename_symbol`]).
    pub fn rename(&mut self, old: &str, new: &str) {
        if let Some(i) = self.index.remove(old) {
            self.symbols[i].name = new.to_string();
            self.index.insert(new.to_string(), i);
        }
    }

    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    /// Evaluates an expression to a constant, substituting equates whose
    /// values are plain constants. `None` if the value is only known after
    /// layout (`. - msg`) or link time.
    pub fn eval(&self, e: &Expr) -> Option<i64> {
        self.eval_depth(e, 0)
    }

    /// `depth` counts equate hops; the expression nesting itself is bounded
    /// by the parser. Chains longer than [`MAX_EQUATE_CHAIN`] are rejected
    /// during lowering, so this limit is never reached for valid modules.
    fn eval_depth(&self, e: &Expr, depth: u32) -> Option<i64> {
        use crate::ast::{BinaryOp::*, UnaryOp};
        if depth > MAX_EQUATE_CHAIN {
            return None;
        }
        Some(match e {
            Expr::Int(v) => *v as i64,
            Expr::Symbol(s) => match &self.get(&s.name)?.def {
                SymbolDef::Equate { value, .. } if s.modifier.is_none() => {
                    self.eval_depth(value, depth + 1)?
                }
                _ => return None,
            },
            Expr::Unary(UnaryOp::Neg, x) => self.eval_depth(x, depth)?.wrapping_neg(),
            Expr::Unary(UnaryOp::Not, x) => !self.eval_depth(x, depth)?,
            Expr::Binary(op, l, r) => {
                let (l, r) = (self.eval_depth(l, depth)?, self.eval_depth(r, depth)?);
                match op {
                    Add => l.wrapping_add(r),
                    Sub => l.wrapping_sub(r),
                    Mul => l.wrapping_mul(r),
                    Div => l.checked_div(r)?,
                    Mod => l.checked_rem(r)?,
                    Shl => l.wrapping_shl(r as u32),
                    Shr => ((l as u64).wrapping_shr(r as u32)) as i64,
                    And => l & r,
                    Or => l | r,
                    Xor => l ^ r,
                }
            }
            Expr::Here | Expr::LocalLabelRef { .. } => return None,
        })
    }
}

// ===========================================================================
// Program structure and platform layer
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionReason {
    /// `.type f, @function`
    TypeDirective,
    /// Program entry point (`_start` / `main`).
    Entry,
    /// Target of a direct `call`.
    CallTarget,
    /// Global label in a code section.
    GlobalCode,
    /// Calling-convention thunk or wrapper generated by l2w.
    Bridge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    pub name: String,
    pub section: usize,
    /// Item index of the function's label.
    pub start: usize,
    /// One past the last item.
    pub end: usize,
    pub global: bool,
    pub reason: FunctionReason,
}

impl Function {
    pub fn contains(&self, loc: Loc) -> bool {
        loc.section == self.section && (self.start..self.end).contains(&loc.item)
    }
}

/// Statically known value of a register at a program point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Const(i64),
    /// Assembler-time constant without a known numeric value (`len`).
    AbsExpr(Expr),
    /// Address of a symbol (`lea rsi, [rip + msg]`, `mov rsi, offset msg`).
    Address(Expr),
    Unknown,
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Const(v) => write!(f, "{v}"),
            Value::AbsExpr(e) => write!(f, "{e}"),
            Value::Address(e) => write!(f, "&{e}"),
            Value::Unknown => f.write_str("?"),
        }
    }
}

/// Linux system call argument registers in order: rdi, rsi, rdx, r10, r8, r9.
pub const SYSCALL_ARG_REGS: [u8; 6] = [7, 6, 2, 10, 8, 9];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyscallSite {
    pub at: Loc,
    pub span: Span,
    pub function: Option<usize>,
    /// Value of `rax` (the system call number).
    pub number: Value,
    /// Values of rdi, rsi, rdx, r10, r8, r9.
    pub args: [Value; 6],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallTarget {
    /// A function defined in this file.
    Internal(String),
    /// An undefined (external) symbol, e.g. a libc function.
    External(String),
    /// `call rax`, `call [rbx + 8]`, ...
    Indirect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    pub at: Loc,
    pub span: Span,
    /// Calling function, if known.
    pub function: Option<usize>,
    pub target: CallTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// `_start`: raw entry, no C runtime.
    Start,
    /// `main`: entered through the C runtime.
    Main,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub symbol: String,
    pub kind: EntryKind,
}

/// Use of an ELF relocation modifier such as `puts@PLT` or `x@GOTPCREL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfReloc {
    pub symbol: String,
    pub modifier: String,
    pub span: Span,
}

/// Everything that ties the program to Linux / ELF.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Platform {
    pub entry: Option<Entry>,
    pub syscalls: Vec<SyscallSite>,
    pub calls: Vec<CallSite>,
    /// Undefined symbols (resolved by the linker), in order of appearance.
    pub externals: Vec<String>,
    pub elf_relocs: Vec<ElfReloc>,
    /// `fs:`/`gs:` memory accesses (thread-local storage, stack protector).
    pub segment_accesses: Vec<Span>,
    /// Number of dropped metadata directives (`.file`, `.cfi_*`, ...).
    pub dropped_metadata: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Module {
    pub sections: Vec<Section>,
    pub chunks: Vec<Chunk>,
    pub symbols: SymbolTable,
    pub functions: Vec<Function>,
    pub platform: Platform,
}

impl Module {
    pub fn item(&self, loc: Loc) -> &Item {
        &self.sections[loc.section].items[loc.item]
    }

    /// Index of the function containing `loc`.
    pub fn function_at(&self, loc: Loc) -> Option<usize> {
        self.functions.iter().position(|f| f.contains(loc))
    }

    /// All instructions with their locations.
    pub fn instructions(&self) -> impl Iterator<Item = (Loc, &Instr)> {
        self.sections.iter().enumerate().flat_map(|(s, sec)| {
            sec.items
                .iter()
                .enumerate()
                .filter_map(move |(i, it)| match it {
                    Item::Instr(ins) => Some((
                        Loc {
                            section: s,
                            item: i,
                        },
                        ins,
                    )),
                    _ => None,
                })
        })
    }

    /// Renames a symbol everywhere: symbol table, labels, equates, all
    /// expressions, functions and call sites.
    pub fn rename_symbol(&mut self, old: &str, new: &str) {
        fn in_expr(e: &mut Expr, old: &str, new: &str) {
            match e {
                Expr::Symbol(s) if s.name == old => s.name = new.to_string(),
                Expr::Unary(_, x) => in_expr(x, old, new),
                Expr::Binary(_, l, r) => {
                    in_expr(l, old, new);
                    in_expr(r, old, new);
                }
                _ => {}
            }
        }
        let fix = |name: &mut String| {
            if name == old {
                *name = new.to_string();
            }
        };
        self.symbols.rename(old, new);
        for sym in &mut self.symbols.symbols {
            if let SymbolDef::Equate { value, .. } = &mut sym.def {
                in_expr(value, old, new);
            }
            if let Some(size) = &mut sym.size {
                in_expr(size, old, new);
            }
        }
        for sec in &mut self.sections {
            for item in &mut sec.items {
                match item {
                    Item::Label { name, .. } => fix(name),
                    Item::Instr(ins) => {
                        for op in &mut ins.operands {
                            match op {
                                Operand::Imm(e) | Operand::Addr(e) | Operand::Target(e) => {
                                    in_expr(e, old, new)
                                }
                                Operand::Mem(m) => {
                                    if let Some(d) = &mut m.disp {
                                        in_expr(d, old, new)
                                    }
                                }
                                Operand::Reg(_) => {}
                            }
                        }
                    }
                    Item::Data { data, .. } => match data {
                        Data::Values { values, .. } => {
                            values.iter_mut().for_each(|v| in_expr(v, old, new))
                        }
                        Data::Fill { size, fill } => {
                            in_expr(size, old, new);
                            if let Some(f) = fill {
                                in_expr(f, old, new);
                            }
                        }
                        Data::Bytes(_) => {}
                    },
                    Item::Align { fill, max, .. } => {
                        for e in fill.iter_mut().chain(max.iter_mut()) {
                            in_expr(e, old, new);
                        }
                    }
                    Item::Set { symbol, value, .. } => {
                        fix(symbol);
                        in_expr(value, old, new);
                    }
                    Item::Common { symbol, size, .. } => {
                        fix(symbol);
                        in_expr(size, old, new);
                    }
                }
            }
        }
        for f in &mut self.functions {
            fix(&mut f.name);
        }
        for c in &mut self.platform.calls {
            if let CallTarget::Internal(n) | CallTarget::External(n) = &mut c.target {
                fix(n);
            }
        }
    }

    /// Inserts `items` before position `at`, keeping every stored location
    /// (chunks, symbols, functions, call and syscall sites) valid.
    pub fn insert_items(&mut self, at: Loc, items: Vec<Item>) {
        let n = items.len();
        if n == 0 {
            return;
        }
        let shift = |loc: &mut Loc| {
            if loc.section == at.section && loc.item >= at.item {
                loc.item += n;
            }
        };
        let sec = &mut self.sections[at.section].items;
        sec.splice(at.item..at.item, items);
        for c in &mut self.chunks {
            if c.section != at.section {
                continue;
            }
            if c.start <= at.item && at.item < c.end {
                c.end += n;
            } else if c.start >= at.item {
                c.start += n;
                c.end += n;
            }
        }
        for sym in &mut self.symbols.symbols {
            match &mut sym.def {
                SymbolDef::Label(loc) | SymbolDef::Equate { at: loc, .. } => shift(loc),
                _ => {}
            }
        }
        for f in &mut self.functions {
            if f.section == at.section {
                if f.start >= at.item {
                    f.start += n;
                }
                if f.end >= at.item {
                    f.end += n;
                }
            }
        }
        for c in &mut self.platform.calls {
            shift(&mut c.at);
        }
        for s in &mut self.platform.syscalls {
            shift(&mut s.at);
        }
    }

    /// Human-readable analysis report (`--dump-ir`).
    pub fn summary(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let _ = writeln!(s, "sections:");
        for (i, sec) in self.sections.iter().enumerate() {
            let _ = writeln!(
                s,
                "  [{i}] {:<16} {:<15} {} items{}",
                sec.name,
                sec.kind.describe(),
                sec.items.len(),
                sec.flags
                    .as_ref()
                    .map(|f| format!(", flags \"{f}\""))
                    .unwrap_or_default()
            );
        }
        let _ = writeln!(s, "symbols:");
        for sym in self.symbols.iter() {
            let def = match &sym.def {
                SymbolDef::Label(loc) => format!("label in {}", self.sections[loc.section].name),
                SymbolDef::Equate { value, class, .. } => {
                    let c = match class {
                        ValueClass::Absolute => "absolute".to_string(),
                        ValueClass::Relocatable(sec) => {
                            format!("address in {}", self.sections[*sec].name)
                        }
                        ValueClass::Unknown => "unknown".to_string(),
                    };
                    format!("equate ({c}) = {value}")
                }
                SymbolDef::Common { local } => {
                    if *local {
                        "local common".into()
                    } else {
                        "common".into()
                    }
                }
                SymbolDef::Undefined => "undefined (external)".into(),
            };
            let mut attrs = Vec::new();
            if sym.global {
                attrs.push("global".to_string());
            }
            if sym.declared_extern {
                attrs.push("extern".to_string());
            }
            match &sym.ty {
                Some(SymbolType::Function) => attrs.push("@function".into()),
                Some(SymbolType::Object) => attrs.push("@object".into()),
                Some(SymbolType::Other(o)) => attrs.push(format!("@{o}")),
                None => {}
            }
            let _ = writeln!(
                s,
                "  {:<20} {def}{}",
                symbol_name(&sym.name),
                if attrs.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", attrs.join(", "))
                }
            );
        }
        let _ = writeln!(s, "functions:");
        for f in &self.functions {
            let _ = writeln!(
                s,
                "  {:<20} {} items {}..{} ({:?}{})",
                f.name,
                self.sections[f.section].name,
                f.start,
                f.end,
                f.reason,
                if f.global { ", global" } else { "" }
            );
        }
        let fname = |f: Option<usize>| {
            f.map(|i| self.functions[i].name.clone())
                .unwrap_or_else(|| "-".into())
        };
        let _ = writeln!(s, "platform (linux/elf):");
        match &self.platform.entry {
            Some(e) => {
                let _ = writeln!(s, "  entry: {} ({:?})", e.symbol, e.kind);
            }
            None => {
                let _ = writeln!(s, "  entry: none");
            }
        }
        for c in &self.platform.calls {
            let t = match &c.target {
                CallTarget::Internal(n) => format!("{n} (internal)"),
                CallTarget::External(n) => format!("{n} (external)"),
                CallTarget::Indirect => "indirect".into(),
            };
            let _ = writeln!(
                s,
                "  call    line {:<4} {t} in {}",
                c.span.line,
                fname(c.function)
            );
        }
        for sc in &self.platform.syscalls {
            let name = match sc.number {
                Value::Const(n) => crate::compatibility::syscalls::linux_syscall_name(n)
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default(),
                _ => String::new(),
            };
            let regs = ["rdi", "rsi", "rdx", "r10", "r8", "r9"];
            let args: Vec<String> = regs
                .iter()
                .zip(&sc.args)
                .filter(|(_, v)| **v != Value::Unknown)
                .map(|(r, v)| format!("{r}={v}"))
                .collect();
            let _ = writeln!(
                s,
                "  syscall line {:<4} rax={}{name} {} in {}",
                sc.span.line,
                sc.number,
                args.join(" "),
                fname(sc.function)
            );
        }
        if !self.platform.externals.is_empty() {
            let _ = writeln!(s, "  externals: {}", self.platform.externals.join(", "));
        }
        for r in &self.platform.elf_relocs {
            let _ = writeln!(
                s,
                "  elf reloc line {:<4} {}@{}",
                r.span.line, r.symbol, r.modifier
            );
        }
        for sp in &self.platform.segment_accesses {
            let _ = writeln!(s, "  segment access line {}", sp.line);
        }
        if self.platform.dropped_metadata > 0 {
            let _ = writeln!(
                s,
                "  dropped metadata directives: {}",
                self.platform.dropped_metadata
            );
        }
        s
    }
}

// ===========================================================================
// Display: normalized GAS listing
// ===========================================================================

impl fmt::Display for Mem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ast = crate::ast::MemoryOperand {
            size: self.size,
            segment: self.segment,
            base: self.base,
            index: self.index,
            scale: self.scale,
            disp: self.disp.clone(),
        };
        write!(f, "{ast}")
    }
}

impl fmt::Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operand::Reg(r) => write!(f, "{r}"),
            // Symbolic immediates are printed with `offset`: GNU as then
            // always uses the value (same encoding), independent of whether
            // the equate is defined before or after this line.
            Operand::Imm(e) if e.symbols().is_empty() => write!(f, "{e}"),
            Operand::Imm(e) | Operand::Addr(e) => match e {
                Expr::Binary(..) => write!(f, "offset ({e})"),
                _ => write!(f, "offset {e}"),
            },
            Operand::Target(e) => write!(f, "{e}"),
            Operand::Mem(m) => write!(f, "{m}"),
        }
    }
}

impl fmt::Display for Instr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.lock {
            f.write_str("lock ")?;
        }
        f.write_str(&self.mnemonic.name())?;
        for (i, op) in self.operands.iter().enumerate() {
            write!(f, "{}{op}", if i == 0 { " " } else { ", " })?;
        }
        Ok(())
    }
}

impl fmt::Display for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let join = |v: &[Expr]| {
            v.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        match self {
            Item::Label { name, .. } => write!(f, "{}:", symbol_name(name)),
            Item::Instr(i) => write!(f, "    {i}"),
            Item::Data { data, .. } => match data {
                Data::Values { width, values } => {
                    write!(
                        f,
                        "    {} {}",
                        crate::ast::Directive::data_name(*width),
                        join(values)
                    )
                }
                Data::Bytes(b) => write!(f, "    .ascii {}", escape_string(b)),
                Data::Fill {
                    size,
                    fill: Some(v),
                } => write!(f, "    .skip {size}, {v}"),
                Data::Fill { size, fill: None } => write!(f, "    .skip {size}"),
            },
            Item::Align {
                bytes, fill, max, ..
            } => {
                write!(f, "    .balign {bytes}")?;
                match (fill, max) {
                    (Some(v), Some(m)) => write!(f, ", {v}, {m}"),
                    (None, Some(m)) => write!(f, ",, {m}"),
                    (Some(v), None) => write!(f, ", {v}"),
                    (None, None) => Ok(()),
                }
            }
            Item::Set { symbol, value, .. } => {
                write!(f, "    .set {}, {value}", symbol_name(symbol))
            }
            Item::Common {
                symbol,
                size,
                align,
                local,
                ..
            } => {
                write!(
                    f,
                    "    {} {}, {size}",
                    if *local { ".lcomm" } else { ".comm" },
                    symbol_name(symbol)
                )?;
                if let Some(a) = align {
                    write!(f, ", {a}")?;
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for Section {
    /// Prints the section switch directive.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.name.as_str(), &self.flags, &self.elf_type) {
            (".text" | ".data" | ".bss", None, None) => write!(f, "{}", self.name),
            (name, flags, ty) => {
                write!(f, ".section {name}")?;
                if flags.is_some() || ty.is_some() {
                    write!(
                        f,
                        ", {}",
                        escape_string(flags.as_deref().unwrap_or("").as_bytes())
                    )?;
                }
                if let Some(t) = ty {
                    write!(f, ", {t}")?;
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for Module {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, ".intel_syntax noprefix")?;
        for sym in self.symbols.iter() {
            if sym.global {
                writeln!(f, ".globl {}", symbol_name(&sym.name))?;
            }
        }
        for chunk in &self.chunks {
            let sec = &self.sections[chunk.section];
            writeln!(f, "{sec}")?;
            for item in &sec.items[chunk.start..chunk.end] {
                writeln!(f, "{item}")?;
            }
        }
        Ok(())
    }
}
