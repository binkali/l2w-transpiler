//! Abstract syntax tree for GAS Intel-syntax (`noprefix`) x86-64 assembly.
//!
//! The AST is a faithful, *syntactic* representation of the source. It does
//! not yet decide platform semantics; that is the job of the IR and the
//! transpiler. Notably, Intel syntax is ambiguous for bare symbols:
//!
//! * `mov rax, msg`        — loads the 8 bytes **stored at** `msg`
//! * `mov rax, offset msg` — loads the **address** of `msg`
//! * `jmp msg` / `call f`  — `msg` / `f` is a **branch target**
//!
//! The parser therefore keeps bare symbolic expressions as
//! [`OperandKind::Symbolic`] and lets later phases decide (based on the
//! mnemonic and the symbol table) what they mean.
//!
//! Every type implements [`std::fmt::Display`], printing normalized GAS
//! Intel syntax that re-parses to the same AST.

use crate::diagnostics::Span;
use std::fmt;

// ---------------------------------------------------------------------------
// Sizes and registers
// ---------------------------------------------------------------------------

/// Operand size as written with `byte ptr`, `qword ptr`, ... or implied by a register.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Size {
    Byte,
    Word,
    Dword,
    Qword,
    Tbyte,
    Xmmword,
}

impl Size {
    pub fn bytes(self) -> u32 {
        match self {
            Size::Byte => 1,
            Size::Word => 2,
            Size::Dword => 4,
            Size::Qword => 8,
            Size::Tbyte => 10,
            Size::Xmmword => 16,
        }
    }

    /// Parses a size keyword (`byte`, `word`, `dword`, `qword`, `tbyte`,
    /// `xmmword`/`oword`), case-insensitively.
    pub fn from_keyword(s: &str) -> Option<Size> {
        Some(match s.to_ascii_lowercase().as_str() {
            "byte" => Size::Byte,
            "word" => Size::Word,
            "dword" => Size::Dword,
            "qword" => Size::Qword,
            "tbyte" => Size::Tbyte,
            "xmmword" | "oword" => Size::Xmmword,
            _ => return None,
        })
    }

    pub fn keyword(self) -> &'static str {
        match self {
            Size::Byte => "byte",
            Size::Word => "word",
            Size::Dword => "dword",
            Size::Qword => "qword",
            Size::Tbyte => "tbyte",
            Size::Xmmword => "xmmword",
        }
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.keyword())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Segment {
    Es,
    Cs,
    Ss,
    Ds,
    Fs,
    Gs,
}

/// Register class plus hardware encoding number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegKind {
    /// General-purpose register; the number is the hardware encoding
    /// (0 = rax, 1 = rcx, 2 = rdx, 3 = rbx, 4 = rsp, 5 = rbp, 6 = rsi,
    /// 7 = rdi, 8..=15 = r8..r15).
    Gpr(u8),
    /// Legacy high-byte registers `ah`, `ch`, `dh`, `bh` (number 0..=3 of
    /// the containing register rax, rcx, rdx, rbx).
    HighByte(u8),
    /// Instruction pointer (only valid as a memory base).
    Rip,
    Segment(Segment),
    Xmm(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Register {
    pub kind: RegKind,
    pub size: Size,
}

const GPR64: [&str; 16] = [
    "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13",
    "r14", "r15",
];
const GPR32: [&str; 16] = [
    "eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "r8d", "r9d", "r10d", "r11d", "r12d",
    "r13d", "r14d", "r15d",
];
const GPR16: [&str; 16] = [
    "ax", "cx", "dx", "bx", "sp", "bp", "si", "di", "r8w", "r9w", "r10w", "r11w", "r12w", "r13w",
    "r14w", "r15w",
];
const GPR8: [&str; 16] = [
    "al", "cl", "dl", "bl", "spl", "bpl", "sil", "dil", "r8b", "r9b", "r10b", "r11b", "r12b",
    "r13b", "r14b", "r15b",
];
const HIGH8: [&str; 4] = ["ah", "ch", "dh", "bh"];
const SEGMENTS: [(&str, Segment); 6] = [
    ("es", Segment::Es),
    ("cs", Segment::Cs),
    ("ss", Segment::Ss),
    ("ds", Segment::Ds),
    ("fs", Segment::Fs),
    ("gs", Segment::Gs),
];
const XMM: [&str; 16] = [
    "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7", "xmm8", "xmm9", "xmm10",
    "xmm11", "xmm12", "xmm13", "xmm14", "xmm15",
];

impl Register {
    pub const fn gpr(number: u8, size: Size) -> Self {
        Register {
            kind: RegKind::Gpr(number),
            size,
        }
    }

    pub const RAX: Register = Register::gpr(0, Size::Qword);
    pub const RCX: Register = Register::gpr(1, Size::Qword);
    pub const RDX: Register = Register::gpr(2, Size::Qword);
    pub const RSP: Register = Register::gpr(4, Size::Qword);
    pub const RSI: Register = Register::gpr(6, Size::Qword);
    pub const RDI: Register = Register::gpr(7, Size::Qword);

    /// Parses a register name case-insensitively (`rax`, `R8D`, `xmm3`, `fs`, ...).
    pub fn parse(name: &str) -> Option<Register> {
        let lower = name.to_ascii_lowercase();
        let n = lower.as_str();
        match n {
            "rip" => {
                return Some(Register {
                    kind: RegKind::Rip,
                    size: Size::Qword,
                })
            }
            "eip" => {
                return Some(Register {
                    kind: RegKind::Rip,
                    size: Size::Dword,
                })
            }
            _ => {}
        }
        for (table, size) in [
            (&GPR64, Size::Qword),
            (&GPR32, Size::Dword),
            (&GPR16, Size::Word),
            (&GPR8, Size::Byte),
        ] {
            if let Some(i) = table.iter().position(|r| *r == n) {
                return Some(Register::gpr(i as u8, size));
            }
        }
        // `r8l`..`r15l` are accepted aliases of `r8b`..`r15b`.
        if let Some(i) = GPR8[8..]
            .iter()
            .position(|r| r.strip_suffix('b').map(|s| format!("{s}l")).as_deref() == Some(n))
        {
            return Some(Register::gpr(i as u8 + 8, Size::Byte));
        }
        if let Some(i) = HIGH8.iter().position(|r| *r == n) {
            return Some(Register {
                kind: RegKind::HighByte(i as u8),
                size: Size::Byte,
            });
        }
        if let Some((_, seg)) = SEGMENTS.iter().find(|(s, _)| *s == n) {
            return Some(Register {
                kind: RegKind::Segment(*seg),
                size: Size::Word,
            });
        }
        if let Some(i) = XMM.iter().position(|r| *r == n) {
            return Some(Register {
                kind: RegKind::Xmm(i as u8),
                size: Size::Xmmword,
            });
        }
        None
    }

    pub fn name(&self) -> &'static str {
        match self.kind {
            RegKind::Gpr(n) => {
                let table = match self.size {
                    Size::Qword => &GPR64,
                    Size::Dword => &GPR32,
                    Size::Word => &GPR16,
                    _ => &GPR8,
                };
                table[n as usize]
            }
            RegKind::HighByte(n) => HIGH8[n as usize],
            RegKind::Rip => {
                if self.size == Size::Dword {
                    "eip"
                } else {
                    "rip"
                }
            }
            RegKind::Segment(seg) => SEGMENTS.iter().find(|(_, s)| *s == seg).unwrap().0,
            RegKind::Xmm(n) => XMM[n as usize],
        }
    }

    pub fn is_gpr(&self) -> bool {
        matches!(self.kind, RegKind::Gpr(_) | RegKind::HighByte(_))
    }

    pub fn is_segment(&self) -> bool {
        matches!(self.kind, RegKind::Segment(_))
    }

    /// The same physical register viewed with a different width
    /// (only meaningful for [`RegKind::Gpr`]).
    pub fn with_size(self, size: Size) -> Register {
        Register { size, ..self }
    }
}

impl fmt::Display for Register {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

/// A reference to a symbol, optionally with a relocation modifier such as
/// `puts@PLT` or `var@GOTPCREL`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SymbolRef {
    pub name: String,
    pub modifier: Option<String>,
}

impl SymbolRef {
    pub fn new(name: impl Into<String>) -> Self {
        SymbolRef {
            name: name.into(),
            modifier: None,
        }
    }
}

/// Formats a symbol name for GAS output, quoting it when it is not a plain
/// identifier (or would be mistaken for a register or keyword).
pub fn symbol_name(name: &str) -> std::borrow::Cow<'_, str> {
    let mut chars = name.chars();
    let plain = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, '_' | '.'))
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '$'))
        && name != "."
        && Register::parse(name).is_none()
        && Size::from_keyword(name).is_none()
        && !name.eq_ignore_ascii_case("ptr")
        && !name.eq_ignore_ascii_case("offset");
    if plain {
        std::borrow::Cow::Borrowed(name)
    } else {
        std::borrow::Cow::Owned(format!("\"{name}\""))
    }
}

fn symbol_list(names: &[String]) -> String {
    names
        .iter()
        .map(|n| symbol_name(n))
        .collect::<Vec<_>>()
        .join(", ")
}

impl fmt::Display for SymbolRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&symbol_name(&self.name))?;
        match &self.modifier {
            Some(m) => write!(f, "@{m}"),
            None => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Shl,
    Shr,
    And,
    Or,
    Xor,
}

impl BinaryOp {
    /// Binding strength; higher binds tighter.
    pub fn precedence(self) -> u8 {
        match self {
            BinaryOp::Or => 1,
            BinaryOp::Xor => 2,
            BinaryOp::And => 3,
            BinaryOp::Shl | BinaryOp::Shr => 4,
            BinaryOp::Add | BinaryOp::Sub => 5,
            BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => 6,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
            BinaryOp::Shl => "<<",
            BinaryOp::Shr => ">>",
            BinaryOp::And => "&",
            BinaryOp::Or => "|",
            BinaryOp::Xor => "^",
        }
    }
}

/// An assembler expression (immediates, displacements, data values).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Int(i128),
    Symbol(SymbolRef),
    /// `.` — the current location counter.
    Here,
    /// GAS numeric local label reference: `1b` (backward) / `1f` (forward).
    LocalLabelRef {
        number: u32,
        forward: bool,
    },
    Unary(UnaryOp, Box<Expr>),
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
}

impl Expr {
    pub fn symbol(name: impl Into<String>) -> Expr {
        Expr::Symbol(SymbolRef::new(name))
    }

    pub fn binary(op: BinaryOp, l: Expr, r: Expr) -> Expr {
        Expr::Binary(op, Box::new(l), Box::new(r))
    }

    /// `true` if the expression contains no symbols, labels or `.`.
    pub fn is_constant(&self) -> bool {
        match self {
            Expr::Int(_) => true,
            Expr::Symbol(_) | Expr::Here | Expr::LocalLabelRef { .. } => false,
            Expr::Unary(_, e) => e.is_constant(),
            Expr::Binary(_, l, r) => l.is_constant() && r.is_constant(),
        }
    }

    /// Evaluates a constant expression with 64-bit two's complement
    /// wrap-around semantics. Returns `None` for symbolic expressions or
    /// division by zero.
    pub fn eval_constant(&self) -> Option<i64> {
        Some(match self {
            Expr::Int(v) => *v as i64,
            Expr::Unary(UnaryOp::Neg, e) => e.eval_constant()?.wrapping_neg(),
            Expr::Unary(UnaryOp::Not, e) => !e.eval_constant()?,
            Expr::Binary(op, l, r) => {
                let (l, r) = (l.eval_constant()?, r.eval_constant()?);
                match op {
                    BinaryOp::Add => l.wrapping_add(r),
                    BinaryOp::Sub => l.wrapping_sub(r),
                    BinaryOp::Mul => l.wrapping_mul(r),
                    BinaryOp::Div => l.checked_div(r)?,
                    BinaryOp::Mod => l.checked_rem(r)?,
                    BinaryOp::Shl => l.wrapping_shl(r as u32),
                    BinaryOp::Shr => ((l as u64).wrapping_shr(r as u32)) as i64,
                    BinaryOp::And => l & r,
                    BinaryOp::Or => l | r,
                    BinaryOp::Xor => l ^ r,
                }
            }
            Expr::Symbol(_) | Expr::Here | Expr::LocalLabelRef { .. } => return None,
        })
    }

    /// All symbols referenced by this expression.
    pub fn symbols(&self) -> Vec<&SymbolRef> {
        let mut out = Vec::new();
        self.collect_symbols(&mut out);
        out
    }

    fn collect_symbols<'a>(&'a self, out: &mut Vec<&'a SymbolRef>) {
        match self {
            Expr::Symbol(s) => out.push(s),
            Expr::Unary(_, e) => e.collect_symbols(out),
            Expr::Binary(_, l, r) => {
                l.collect_symbols(out);
                r.collect_symbols(out);
            }
            _ => {}
        }
    }

    fn precedence(&self) -> u8 {
        match self {
            Expr::Binary(op, ..) => op.precedence(),
            _ => u8::MAX,
        }
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Int(v) => write!(f, "{v}"),
            Expr::Symbol(s) => write!(f, "{s}"),
            Expr::Here => f.write_str("."),
            Expr::LocalLabelRef { number, forward } => {
                write!(f, "{number}{}", if *forward { 'f' } else { 'b' })
            }
            Expr::Unary(op, e) => {
                f.write_str(match op {
                    UnaryOp::Neg => "-",
                    UnaryOp::Not => "~",
                })?;
                if matches!(**e, Expr::Binary(..) | Expr::Unary(..)) {
                    write!(f, "({e})")
                } else {
                    write!(f, "{e}")
                }
            }
            Expr::Binary(op, l, r) => {
                let p = op.precedence();
                if l.precedence() < p {
                    write!(f, "({l})")?;
                } else {
                    write!(f, "{l}")?;
                }
                write!(f, " {} ", op.symbol())?;
                if r.precedence() <= p {
                    write!(f, "({r})")
                } else {
                    write!(f, "{r}")
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Operands and instructions
// ---------------------------------------------------------------------------

/// A memory operand `size ptr seg:[base + index*scale + disp]`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemoryOperand {
    pub size: Option<Size>,
    pub segment: Option<Register>,
    pub base: Option<Register>,
    pub index: Option<Register>,
    /// 1, 2, 4 or 8. Meaningful only when `index` is set.
    pub scale: u8,
    pub disp: Option<Expr>,
}

impl MemoryOperand {
    /// `true` for `[rip + sym]` style addressing.
    pub fn is_rip_relative(&self) -> bool {
        matches!(
            self.base,
            Some(Register {
                kind: RegKind::Rip,
                ..
            })
        )
    }
}

impl fmt::Display for MemoryOperand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(size) = self.size {
            write!(f, "{size} ptr ")?;
        }
        if let Some(seg) = self.segment {
            write!(f, "{seg}:")?;
        }
        f.write_str("[")?;
        let mut first = true;
        if let Some(base) = self.base {
            write!(f, "{base}")?;
            first = false;
        }
        if let Some(index) = self.index {
            if !first {
                f.write_str(" + ")?;
            }
            write!(f, "{index}*{}", self.scale)?;
            first = false;
        }
        if let Some(disp) = &self.disp {
            if first {
                write!(f, "{disp}")?;
            } else {
                // The parser accumulates `+`/`-` terms left-associatively, so a
                // left-leaning additive chain can be printed without parentheses.
                // Lower-precedence operators (`|`, `&`, shifts, ...) and a
                // negated sum must keep them.
                let add = BinaryOp::Add.precedence();
                match disp {
                    Expr::Unary(UnaryOp::Neg, inner) => match &**inner {
                        Expr::Binary(op, ..) if op.precedence() <= add => {
                            write!(f, " - ({inner})")?
                        }
                        _ => write!(f, " - {inner}")?,
                    },
                    Expr::Binary(op, ..) if op.precedence() < add => write!(f, " + ({disp})")?,
                    _ => write!(f, " + {disp}")?,
                }
            }
        }
        f.write_str("]")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperandKind {
    Register(Register),
    /// A constant expression (`42`, `-1`, `'A'`, `(4 * 8)`).
    Immediate(Expr),
    /// `offset sym` — the address of `sym` used as an immediate.
    Offset(Expr),
    /// An explicit memory operand.
    Memory(MemoryOperand),
    /// A bare expression referencing symbols, e.g. `msg` or `loop_start`.
    /// Branch target for jumps/calls; memory reference for data
    /// instructions (Intel syntax semantics). Resolved in later phases.
    Symbolic(Expr),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operand {
    pub kind: OperandKind,
    pub span: Span,
}

impl fmt::Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            OperandKind::Register(r) => write!(f, "{r}"),
            OperandKind::Immediate(e) | OperandKind::Symbolic(e) => write!(f, "{e}"),
            OperandKind::Offset(e) => write!(f, "offset {e}"),
            OperandKind::Memory(m) => write!(f, "{m}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    /// `rep`, `repe`, `repne`, `lock`, ... (lowercase).
    pub prefix: Option<String>,
    /// Lowercase mnemonic.
    pub mnemonic: String,
    pub operands: Vec<Operand>,
    /// Span of the mnemonic.
    pub span: Span,
}

impl fmt::Display for Instruction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(p) = &self.prefix {
            write!(f, "{p} ")?;
        }
        f.write_str(&self.mnemonic)?;
        for (i, op) in self.operands.iter().enumerate() {
            f.write_str(if i == 0 { " " } else { ", " })?;
            write!(f, "{op}")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Directives
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignKind {
    /// `.align` — on x86 ELF GAS the argument is a byte count.
    Align,
    /// `.balign` — byte count.
    Balign,
    /// `.p2align` — power of two.
    P2align,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringKind {
    /// `.ascii` — no terminator.
    Ascii,
    /// `.asciz` — NUL terminated.
    Asciz,
    /// `.string` — NUL terminated (same as `.asciz` on x86).
    String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolType {
    Function,
    Object,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Directive {
    /// `.intel_syntax noprefix`
    IntelSyntax,
    Text,
    Data,
    Bss,
    /// `.section name[, "flags"[, @type[, extra...]]]`
    Section {
        name: String,
        flags: Option<String>,
        /// Section type including its prefix character, e.g. `@progbits`.
        kind: Option<String>,
        /// Remaining raw arguments (e.g. comdat information).
        extra: Option<String>,
    },
    /// `.global` / `.globl`
    Global(Vec<String>),
    Extern(Vec<String>),
    Local(Vec<String>),
    Align {
        kind: AlignKind,
        alignment: Expr,
        fill: Option<Expr>,
        max: Option<Expr>,
    },
    Strings {
        kind: StringKind,
        values: Vec<Vec<u8>>,
    },
    /// `.byte` (1), `.word`/`.short`/`.2byte` (2), `.long`/`.int`/`.4byte` (4),
    /// `.quad`/`.8byte` (8).
    Values {
        width: u8,
        values: Vec<Expr>,
    },
    Zero(Expr),
    /// `.skip` / `.space`
    Skip {
        size: Expr,
        fill: Option<Expr>,
    },
    Type {
        symbol: String,
        kind: SymbolType,
    },
    Size {
        symbol: String,
        value: Expr,
    },
    /// `.set sym, expr`, `.equ sym, expr` and `sym = expr`
    Set {
        symbol: String,
        value: Expr,
    },
    /// `.comm` / `.lcomm sym, size[, align]`
    Comm {
        symbol: String,
        size: Expr,
        align: Option<Expr>,
        local: bool,
    },
    /// Debug / tooling metadata without program semantics
    /// (`.file`, `.ident`, `.loc`, `.cfi_*`, ...). Arguments kept raw.
    Metadata {
        name: String,
        args: String,
    },
}

impl Directive {
    pub fn data_name(width: u8) -> &'static str {
        match width {
            1 => ".byte",
            2 => ".word",
            4 => ".long",
            _ => ".quad",
        }
    }
}

/// Escapes bytes for a GAS string literal (octal escapes for non-printables,
/// because `\x` in GAS consumes an unbounded number of hex digits).
pub fn escape_string(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() + 2);
    s.push('"');
    for &b in bytes {
        match b {
            b'"' => s.push_str("\\\""),
            b'\\' => s.push_str("\\\\"),
            b'\n' => s.push_str("\\n"),
            b'\t' => s.push_str("\\t"),
            b'\r' => s.push_str("\\r"),
            0x20..=0x7e => s.push(b as char),
            _ => s.push_str(&format!("\\{b:03o}")),
        }
    }
    s.push('"');
    s
}

fn join_exprs(values: &[Expr]) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

impl fmt::Display for Directive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Directive::IntelSyntax => f.write_str(".intel_syntax noprefix"),
            Directive::Text => f.write_str(".text"),
            Directive::Data => f.write_str(".data"),
            Directive::Bss => f.write_str(".bss"),
            Directive::Section {
                name,
                flags,
                kind,
                extra,
            } => {
                write!(f, ".section {name}")?;
                if flags.is_some() || kind.is_some() || extra.is_some() {
                    write!(
                        f,
                        ", {}",
                        escape_string(flags.as_deref().unwrap_or("").as_bytes())
                    )?;
                }
                if let Some(k) = kind {
                    write!(f, ", {k}")?;
                }
                if let Some(e) = extra {
                    write!(f, ", {e}")?;
                }
                Ok(())
            }
            Directive::Global(s) => write!(f, ".globl {}", symbol_list(s)),
            Directive::Extern(s) => write!(f, ".extern {}", symbol_list(s)),
            Directive::Local(s) => write!(f, ".local {}", symbol_list(s)),
            Directive::Align {
                kind,
                alignment,
                fill,
                max,
            } => {
                let name = match kind {
                    AlignKind::Align => ".align",
                    AlignKind::Balign => ".balign",
                    AlignKind::P2align => ".p2align",
                };
                write!(f, "{name} {alignment}")?;
                match (fill, max) {
                    (Some(fill), Some(max)) => write!(f, ", {fill}, {max}"),
                    (None, Some(max)) => write!(f, ",, {max}"),
                    (Some(fill), None) => write!(f, ", {fill}"),
                    (None, None) => Ok(()),
                }
            }
            Directive::Strings { kind, values } => {
                let name = match kind {
                    StringKind::Ascii => ".ascii",
                    StringKind::Asciz => ".asciz",
                    StringKind::String => ".string",
                };
                let vals: Vec<String> = values.iter().map(|v| escape_string(v)).collect();
                write!(f, "{name} {}", vals.join(", "))
            }
            Directive::Values { width, values } => {
                write!(f, "{} {}", Directive::data_name(*width), join_exprs(values))
            }
            Directive::Zero(e) => write!(f, ".zero {e}"),
            Directive::Skip { size, fill } => match fill {
                Some(fill) => write!(f, ".skip {size}, {fill}"),
                None => write!(f, ".skip {size}"),
            },
            Directive::Type { symbol, kind } => {
                let k = match kind {
                    SymbolType::Function => "function",
                    SymbolType::Object => "object",
                    SymbolType::Other(o) => o.as_str(),
                };
                write!(f, ".type {}, @{k}", symbol_name(symbol))
            }
            Directive::Size { symbol, value } => {
                write!(f, ".size {}, {value}", symbol_name(symbol))
            }
            Directive::Set { symbol, value } => write!(f, ".set {}, {value}", symbol_name(symbol)),
            Directive::Comm {
                symbol,
                size,
                align,
                local,
            } => {
                let name = if *local { ".lcomm" } else { ".comm" };
                write!(f, "{name} {}, {size}", symbol_name(symbol))?;
                if let Some(a) = align {
                    write!(f, ", {a}")?;
                }
                Ok(())
            }
            Directive::Metadata { name, args } => {
                if args.is_empty() {
                    f.write_str(name)
                } else {
                    write!(f, "{name} {args}")
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Statements and program
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementKind {
    Label(String),
    /// GAS numeric local label `1:`.
    NumericLabel(u32),
    Instruction(Instruction),
    Directive(Directive),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub kind: StatementKind,
    pub span: Span,
}

impl fmt::Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            StatementKind::Label(l) => write!(f, "{}:", symbol_name(l)),
            StatementKind::NumericLabel(n) => write!(f, "{n}:"),
            StatementKind::Instruction(i) => write!(f, "    {i}"),
            StatementKind::Directive(d) => write!(f, "    {d}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Program {
    pub statements: Vec<Statement>,
}

impl Program {
    pub fn instructions(&self) -> impl Iterator<Item = &Instruction> {
        self.statements.iter().filter_map(|s| match &s.kind {
            StatementKind::Instruction(i) => Some(i),
            _ => None,
        })
    }
}

impl fmt::Display for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for s in &self.statements {
            writeln!(f, "{s}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_roundtrip() {
        for name in [
            "rax", "r8", "r15d", "ax", "r9w", "al", "sil", "r12b", "ah", "rip", "fs", "xmm7",
        ] {
            let r = Register::parse(name).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(r.name(), name);
        }
        assert_eq!(Register::parse("R10L").unwrap().name(), "r10b");
        assert_eq!(Register::parse("RAX"), Some(Register::RAX));
        assert!(Register::parse("rax2").is_none());
        assert!(Register::parse("xmm16").is_none());
    }

    #[test]
    fn expr_eval_and_display() {
        let e = Expr::binary(
            BinaryOp::Mul,
            Expr::binary(BinaryOp::Add, Expr::Int(2), Expr::Int(3)),
            Expr::Int(4),
        );
        assert_eq!(e.eval_constant(), Some(20));
        assert_eq!(e.to_string(), "(2 + 3) * 4");
        let s = Expr::binary(BinaryOp::Sub, Expr::Here, Expr::symbol("msg"));
        assert!(!s.is_constant());
        assert_eq!(s.to_string(), ". - msg");
    }

    #[test]
    fn string_escaping() {
        assert_eq!(escape_string(b"Hi\n\"x\"\0"), r#""Hi\n\"x\"\000""#);
    }
}
