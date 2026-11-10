//! Recursive-descent parser: token stream -> [`ast::Program`].
//!
//! Grammar (informal):
//!
//! ```text
//! line        := label* statement? NEWLINE
//! label       := IDENT ':' | INTEGER ':'
//! statement   := directive | assignment | instruction
//! assignment  := IDENT '=' expr
//! instruction := [prefix] MNEMONIC [operand (',' operand)*]
//! operand     := REGISTER
//!              | SIZE ['ptr'] memory
//!              | 'offset' ['FLAT' ':'] expr
//!              | memory
//!              | expr                      (immediate or symbolic)
//! memory      := [SEGREG ':'] [expr] '[' term (('+'|'-') term)* ']'
//!              | [SEGREG ':'] expr        (only after SIZE ptr)
//! term        := REGISTER ['*' scale] | scale '*' REGISTER | mul_expr
//! ```
//!
//! Expression precedence follows GAS (lowest first): `|`, `^`, `&`,
//! `<<`/`>>`, `+`/`-`, `*`/`/`/`%`, unary `-`/`~`/`+`.
//!
//! The parser recovers from errors by skipping to the end of the current
//! line, so several independent errors are reported in one run.

use crate::ast::*;
use crate::diagnostics::{has_errors, Diagnostic, Span};
use crate::lexer::{tokenize, Token, TokenKind};

/// Instruction prefixes accepted before a mnemonic.
const PREFIXES: &[&str] = &["lock", "rep", "repe", "repz", "repne", "repnz"];

/// Directives that carry no program semantics (debug info, toolchain
/// identification). They are kept as [`Directive::Metadata`].
const METADATA_DIRECTIVES: &[&str] = &[".file", ".ident", ".loc", ".addrsig", ".addrsig_sym"];

/// All directives the parser understands (used for help messages).
pub const SUPPORTED_DIRECTIVES: &[&str] = &[
    ".intel_syntax",
    ".text",
    ".data",
    ".bss",
    ".section",
    ".global",
    ".globl",
    ".extern",
    ".local",
    ".align",
    ".balign",
    ".p2align",
    ".ascii",
    ".asciz",
    ".string",
    ".byte",
    ".word",
    ".short",
    ".2byte",
    ".value",
    ".long",
    ".int",
    ".4byte",
    ".quad",
    ".8byte",
    ".zero",
    ".skip",
    ".space",
    ".type",
    ".size",
    ".set",
    ".equ",
    ".comm",
    ".lcomm",
    ".file",
    ".ident",
    ".loc",
    ".cfi_*",
];

#[derive(Debug)]
pub struct ParseOutput {
    pub program: Program,
    /// Lexer and parser diagnostics, in source order.
    pub diagnostics: Vec<Diagnostic>,
    /// Number of tokens produced by the lexer (for `--verbose`).
    pub token_count: usize,
}

impl ParseOutput {
    pub fn has_errors(&self) -> bool {
        has_errors(&self.diagnostics)
    }
}

/// Lexes and parses a complete source file.
pub fn parse(src: &str) -> ParseOutput {
    let lexed = tokenize(src);
    let token_count = lexed.tokens.len();
    let mut p = Parser {
        src,
        tokens: lexed.tokens,
        pos: 0,
        diagnostics: lexed.diagnostics,
    };
    let program = p.program();
    let mut diagnostics = p.diagnostics;
    diagnostics.sort_by_key(|d| d.span.map(|s| (s.line, s.col)));
    ParseOutput {
        program,
        diagnostics,
        token_count,
    }
}

type PResult<T> = Result<T, Diagnostic>;

struct Parser<'a> {
    src: &'a str,
    tokens: Vec<Token>,
    pos: usize,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Parser<'a> {
    // -- token helpers ------------------------------------------------------

    fn peek(&self) -> &TokenKind {
        self.peek_at(0)
    }

    fn peek_at(&self, n: usize) -> &TokenKind {
        let i = (self.pos + n).min(self.tokens.len() - 1);
        &self.tokens[i].kind
    }

    fn tok(&self) -> &Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn span(&self) -> Span {
        self.tok().span
    }

    fn prev_span(&self) -> Span {
        self.tokens[self.pos.saturating_sub(1)].span
    }

    fn bump(&mut self) -> Token {
        let t = self.tok().clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn at_end_of_statement(&self) -> bool {
        matches!(self.peek(), TokenKind::Newline | TokenKind::Eof)
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.peek() == kind {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &TokenKind, context: &str) -> PResult<Token> {
        if self.peek() == kind {
            Ok(self.bump())
        } else {
            Err(self.unexpected(&format!("{kind} {context}")))
        }
    }

    fn unexpected(&self, expected: &str) -> Diagnostic {
        Diagnostic::error(
            format!("expected {expected}, found {}", self.peek()),
            self.span(),
        )
    }

    fn peek_ident_is(&self, n: usize, word: &str) -> bool {
        matches!(self.peek_at(n), TokenKind::Ident(s) if s.eq_ignore_ascii_case(word))
    }

    fn recover(&mut self) {
        while !self.at_end_of_statement() {
            self.bump();
        }
        self.bump();
    }

    // -- program structure --------------------------------------------------

    fn program(&mut self) -> Program {
        let mut statements = Vec::new();
        loop {
            match self.peek() {
                TokenKind::Eof => break,
                TokenKind::Newline => {
                    self.bump();
                }
                _ => {
                    if let Err(d) = self.line(&mut statements) {
                        self.diagnostics.push(d);
                        self.recover();
                    }
                }
            }
        }
        Program { statements }
    }

    fn line(&mut self, out: &mut Vec<Statement>) -> PResult<()> {
        // Leading labels (any number per line).
        loop {
            match (self.peek().clone(), self.peek_at(1)) {
                (TokenKind::Ident(name), TokenKind::Colon) => {
                    let span = self.span();
                    self.bump();
                    self.bump();
                    if Size::from_keyword(&name).is_some() || name.eq_ignore_ascii_case("ptr") {
                        return Err(Diagnostic::error(
                            format!("`{name}` is a reserved keyword and cannot be used as a label"),
                            span,
                        ));
                    }
                    out.push(Statement {
                        kind: StatementKind::Label(name),
                        span,
                    });
                }
                (TokenKind::Str(s), TokenKind::Colon) => {
                    let span = self.span();
                    self.bump();
                    self.bump();
                    out.push(Statement {
                        kind: StatementKind::Label(quoted_symbol_name(&s, span)?),
                        span,
                    });
                }
                (TokenKind::Integer(n), TokenKind::Colon) => {
                    let span = self.span();
                    let n = u32::try_from(n)
                        .map_err(|_| Diagnostic::error("numeric local label is too large", span))?;
                    self.bump();
                    self.bump();
                    out.push(Statement {
                        kind: StatementKind::NumericLabel(n),
                        span,
                    });
                }
                (TokenKind::Register(r), TokenKind::Colon) if !r.is_segment() => {
                    return Err(Diagnostic::error(
                        format!("register name `{r}` cannot be used as a label"),
                        self.span(),
                    ));
                }
                _ => break,
            }
        }

        let start = self.span();
        let kind = match self.peek().clone() {
            TokenKind::Newline | TokenKind::Eof => {
                self.bump();
                return Ok(());
            }
            TokenKind::Directive(name) => {
                self.bump();
                StatementKind::Directive(self.directive(&name, start)?)
            }
            TokenKind::Ident(name) if self.peek_at(1) == &TokenKind::Equals => {
                self.bump();
                self.bump();
                let value = self.expr()?;
                StatementKind::Directive(Directive::Set {
                    symbol: name,
                    value,
                })
            }
            TokenKind::Ident(_) => StatementKind::Instruction(self.instruction()?),
            TokenKind::Register(r) => {
                return Err(Diagnostic::error(
                    format!("expected an instruction or directive, found register `{r}`"),
                    start,
                ))
            }
            _ => return Err(self.unexpected("a label, instruction or directive")),
        };
        let span = start.to(self.prev_span());
        out.push(Statement { kind, span });
        self.end_of_statement()
    }

    fn end_of_statement(&mut self) -> PResult<()> {
        if self.at_end_of_statement() {
            self.bump();
            Ok(())
        } else {
            Err(self.unexpected("end of line"))
        }
    }

    // -- instructions -------------------------------------------------------

    fn instruction(&mut self) -> PResult<Instruction> {
        let first = self.bump();
        let TokenKind::Ident(name) = first.kind else {
            unreachable!()
        };
        let mut span = first.span;
        let mut mnemonic = name.to_ascii_lowercase();
        let mut prefix = None;
        if PREFIXES.contains(&mnemonic.as_str()) {
            if let TokenKind::Ident(next) = self.peek().clone() {
                span = self.span();
                self.bump();
                prefix = Some(mnemonic);
                mnemonic = next.to_ascii_lowercase();
            }
        }
        if Size::from_keyword(&mnemonic).is_some() || matches!(mnemonic.as_str(), "ptr" | "offset")
        {
            return Err(Diagnostic::error(
                format!("expected an instruction mnemonic, found keyword `{mnemonic}`"),
                span,
            ));
        }
        let mut operands = Vec::new();
        if !self.at_end_of_statement() {
            loop {
                operands.push(self.operand()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        Ok(Instruction {
            prefix,
            mnemonic,
            operands,
            span,
        })
    }

    fn operand(&mut self) -> PResult<Operand> {
        let start = self.span();
        let kind = self.operand_kind()?;
        Ok(Operand {
            kind,
            span: start.to(self.prev_span()),
        })
    }

    fn operand_kind(&mut self) -> PResult<OperandKind> {
        match self.peek().clone() {
            TokenKind::Ident(word) if Size::from_keyword(&word).is_some() => {
                let size = Size::from_keyword(&word);
                self.bump();
                if self.peek_ident_is(0, "ptr") {
                    self.bump();
                }
                let mut mem = self.memory(true)?;
                mem.size = size;
                Ok(OperandKind::Memory(mem))
            }
            TokenKind::Ident(word) if word.eq_ignore_ascii_case("offset") => {
                self.bump();
                // gcc -masm=intel emits `OFFSET FLAT:sym`.
                if self.peek_ident_is(0, "flat") && self.peek_at(1) == &TokenKind::Colon {
                    self.bump();
                    self.bump();
                }
                Ok(OperandKind::Offset(self.expr()?))
            }
            TokenKind::Register(r) if r.is_segment() && self.peek_at(1) == &TokenKind::Colon => {
                Ok(OperandKind::Memory(self.memory(true)?))
            }
            TokenKind::Register(r) => {
                self.bump();
                Ok(OperandKind::Register(r))
            }
            TokenKind::LBracket => Ok(OperandKind::Memory(self.memory(false)?)),
            TokenKind::Percent if matches!(self.peek_at(1), TokenKind::Register(_)) => {
                Err(Diagnostic::error("register prefix `%` is not supported", self.span())
                    .with_help("this transpiler expects `.intel_syntax noprefix`; write `rax` instead of `%rax`"))
            }
            _ => {
                let e = self.expr()?;
                if self.peek() == &TokenKind::LBracket {
                    // `disp[base + index*scale]`, e.g. `msg[rip]`
                    let mut mem = MemoryOperand {
                        disp: Some(e),
                        ..MemoryOperand::default()
                    };
                    let span = self.span();
                    self.bracket(&mut mem)?;
                    validate_memory(&mem, span)?;
                    Ok(OperandKind::Memory(mem))
                } else if e.is_constant() {
                    Ok(OperandKind::Immediate(e))
                } else {
                    Ok(OperandKind::Symbolic(e))
                }
            }
        }
    }

    /// Parses `[seg:] [disp] [ '[' ... ']' ]`. With `allow_bare`, a bare
    /// expression without brackets is accepted as an absolute/symbolic
    /// memory reference (`qword ptr counter`).
    fn memory(&mut self, allow_bare: bool) -> PResult<MemoryOperand> {
        let start = self.span();
        let mut mem = MemoryOperand::default();
        if let TokenKind::Register(r) = self.peek().clone() {
            if r.is_segment() && self.peek_at(1) == &TokenKind::Colon {
                self.bump();
                self.bump();
                mem.segment = Some(r);
            }
        }
        if self.peek() != &TokenKind::LBracket {
            if let TokenKind::Register(r) = self.peek() {
                return Err(Diagnostic::error(
                    format!(
                        "register `{r}` must be enclosed in `[...]` to be used as a memory operand"
                    ),
                    self.span(),
                ));
            }
            mem.disp = Some(self.expr()?);
            if self.peek() != &TokenKind::LBracket {
                if !allow_bare && mem.segment.is_none() {
                    return Err(self.unexpected("`[`"));
                }
                return Ok(mem);
            }
        }
        self.bracket(&mut mem)?;
        validate_memory(&mem, start.to(self.prev_span()))?;
        Ok(mem)
    }

    fn bracket(&mut self, mem: &mut MemoryOperand) -> PResult<()> {
        self.expect(&TokenKind::LBracket, "to start a memory operand")?;
        let mut negative = match self.peek() {
            TokenKind::Minus => {
                self.bump();
                true
            }
            TokenKind::Plus => {
                self.bump();
                false
            }
            _ => false,
        };
        loop {
            self.mem_term(mem, negative)?;
            match self.peek() {
                TokenKind::Plus => {
                    self.bump();
                    negative = false;
                }
                TokenKind::Minus => {
                    self.bump();
                    negative = true;
                }
                TokenKind::RBracket => {
                    self.bump();
                    return Ok(());
                }
                _ => return Err(self.unexpected("`+`, `-` or `]` in memory operand")),
            }
        }
    }

    fn mem_term(&mut self, mem: &mut MemoryOperand, negative: bool) -> PResult<()> {
        let span = self.span();
        let reg_term = match (
            self.peek().clone(),
            self.peek_at(1).clone(),
            self.peek_at(2).clone(),
        ) {
            (TokenKind::Register(r), TokenKind::Star, _) => {
                self.bump();
                self.bump();
                let scale = self.scale()?;
                Some((r, Some(scale)))
            }
            (TokenKind::Integer(_), TokenKind::Star, TokenKind::Register(r)) => {
                let scale = self.scale()?;
                self.bump(); // '*'
                self.bump(); // register
                Some((r, Some(scale)))
            }
            (TokenKind::Register(r), _, _) => {
                self.bump();
                Some((r, None))
            }
            _ => None,
        };

        match reg_term {
            Some((r, scale)) => {
                let span = span.to(self.prev_span());
                if negative {
                    return Err(Diagnostic::error(
                        format!("register `{r}` cannot be subtracted in a memory operand"),
                        span,
                    ));
                }
                if r.is_segment() {
                    return Err(Diagnostic::error(
                        "segment override must be written before `[`, e.g. `fs:[rax]`",
                        span,
                    ));
                }
                match scale {
                    None if mem.base.is_none() => mem.base = Some(r),
                    _ if mem.index.is_none() => {
                        mem.index = Some(r);
                        mem.scale = scale.unwrap_or(1);
                    }
                    _ => {
                        return Err(Diagnostic::error(
                            "too many registers in memory operand (at most a base and an index)",
                            span,
                        ))
                    }
                }
            }
            None => {
                let e = self.mul_expr()?;
                mem.disp = Some(match (mem.disp.take(), negative) {
                    (None, false) => e,
                    (None, true) => Expr::Unary(UnaryOp::Neg, Box::new(e)),
                    (Some(d), false) => Expr::binary(BinaryOp::Add, d, e),
                    (Some(d), true) => Expr::binary(BinaryOp::Sub, d, e),
                });
            }
        }
        Ok(())
    }

    fn scale(&mut self) -> PResult<u8> {
        let span = self.span();
        match self.peek().clone() {
            TokenKind::Integer(v @ (1 | 2 | 4 | 8)) => {
                self.bump();
                Ok(v as u8)
            }
            TokenKind::Integer(v) => Err(Diagnostic::error(
                format!("invalid scale factor `{v}`"),
                span,
            )
            .with_help("the index scale must be 1, 2, 4 or 8")),
            _ => Err(self.unexpected("a scale factor (1, 2, 4 or 8)")),
        }
    }

    // -- expressions --------------------------------------------------------

    fn expr(&mut self) -> PResult<Expr> {
        self.binary_level(0)
    }

    fn mul_expr(&mut self) -> PResult<Expr> {
        self.binary_level(5)
    }

    /// Precedence climbing over the GAS binary operator levels.
    fn binary_level(&mut self, level: usize) -> PResult<Expr> {
        const LEVELS: &[&[(TokenKind, BinaryOp)]] = &[
            &[(TokenKind::Pipe, BinaryOp::Or)],
            &[(TokenKind::Caret, BinaryOp::Xor)],
            &[(TokenKind::Amp, BinaryOp::And)],
            &[
                (TokenKind::Shl, BinaryOp::Shl),
                (TokenKind::Shr, BinaryOp::Shr),
            ],
            &[
                (TokenKind::Plus, BinaryOp::Add),
                (TokenKind::Minus, BinaryOp::Sub),
            ],
            &[
                (TokenKind::Star, BinaryOp::Mul),
                (TokenKind::Slash, BinaryOp::Div),
                (TokenKind::Percent, BinaryOp::Mod),
            ],
        ];
        if level == LEVELS.len() {
            return self.unary();
        }
        let mut lhs = self.binary_level(level + 1)?;
        'outer: loop {
            for (tok, op) in LEVELS[level] {
                if self.peek() == tok {
                    self.bump();
                    let rhs = self.binary_level(level + 1)?;
                    lhs = Expr::binary(*op, lhs, rhs);
                    continue 'outer;
                }
            }
            return Ok(lhs);
        }
    }

    fn unary(&mut self) -> PResult<Expr> {
        match self.peek() {
            TokenKind::Minus => {
                self.bump();
                Ok(Expr::Unary(UnaryOp::Neg, Box::new(self.unary()?)))
            }
            TokenKind::Tilde => {
                self.bump();
                Ok(Expr::Unary(UnaryOp::Not, Box::new(self.unary()?)))
            }
            TokenKind::Plus => {
                self.bump();
                self.unary()
            }
            _ => self.atom(),
        }
    }

    fn atom(&mut self) -> PResult<Expr> {
        let span = self.span();
        match self.peek().clone() {
            TokenKind::Integer(v) => {
                self.bump();
                Ok(Expr::Int(v))
            }
            TokenKind::LocalLabelRef { number, forward } => {
                self.bump();
                Ok(Expr::LocalLabelRef { number, forward })
            }
            TokenKind::Dot => {
                self.bump();
                Ok(Expr::Here)
            }
            TokenKind::LParen => {
                self.bump();
                let e = self.expr()?;
                self.expect(&TokenKind::RParen, "to close `(`")?;
                Ok(e)
            }
            TokenKind::Ident(name) => {
                if Size::from_keyword(&name).is_some()
                    || name.eq_ignore_ascii_case("ptr")
                    || name.eq_ignore_ascii_case("offset")
                {
                    return Err(Diagnostic::error(
                        format!("unexpected keyword `{name}` in expression"),
                        span,
                    ));
                }
                self.bump();
                Ok(Expr::Symbol(split_symbol(&name, span)?))
            }
            TokenKind::Register(r) => Err(Diagnostic::error(
                format!("register `{r}` cannot be used in an expression here"),
                span,
            )
            .with_help("registers in address computations must be inside `[...]`")),
            TokenKind::Str(s) => {
                // GAS allows quoted symbol names: `call "printf"@PLT`.
                let end = self.bump().end;
                let name = quoted_symbol_name(&s, span)?;
                let mut modifier = None;
                if let TokenKind::Ident(m) = self.peek().clone() {
                    if self.tok().start == end && m.len() > 1 && m.starts_with('@') {
                        self.bump();
                        modifier = Some(m[1..].to_string());
                    }
                }
                Ok(Expr::Symbol(SymbolRef { name, modifier }))
            }
            _ => Err(self.unexpected("an expression")),
        }
    }

    // -- directives ---------------------------------------------------------

    fn directive(&mut self, name: &str, span: Span) -> PResult<Directive> {
        if name.starts_with(".cfi_") || METADATA_DIRECTIVES.contains(&name) {
            return Ok(Directive::Metadata {
                name: name.to_string(),
                args: self.raw_rest(),
            });
        }
        Ok(match name {
            ".intel_syntax" => {
                match self.peek().clone() {
                    TokenKind::Ident(arg) if arg.eq_ignore_ascii_case("noprefix") => {
                        self.bump();
                    }
                    TokenKind::Ident(arg) if arg.eq_ignore_ascii_case("prefix") => {
                        return Err(Diagnostic::error(
                            "`.intel_syntax prefix` is not supported",
                            self.span(),
                        )
                        .with_help("use `.intel_syntax noprefix` and write registers without `%`"))
                    }
                    TokenKind::Newline | TokenKind::Eof => {
                        self.diagnostics.push(
                            Diagnostic::warning(
                                "`.intel_syntax` without argument requires `%` register prefixes in GNU as",
                                span,
                            )
                            .with_help("write `.intel_syntax noprefix`; the input is parsed as noprefix"),
                        );
                    }
                    _ => return Err(self.unexpected("`noprefix`")),
                }
                Directive::IntelSyntax
            }
            ".att_syntax" => {
                return Err(Diagnostic::error("AT&T syntax is not supported", span)
                    .with_help("convert the source to `.intel_syntax noprefix`"))
            }
            ".text" | ".data" | ".bss" => {
                if !self.at_end_of_statement() {
                    return Err(Diagnostic::error(
                        format!("subsections (`{name} N`) are not supported"),
                        self.span(),
                    ));
                }
                match name {
                    ".text" => Directive::Text,
                    ".data" => Directive::Data,
                    _ => Directive::Bss,
                }
            }
            ".section" => self.section()?,
            ".global" | ".globl" => Directive::Global(self.symbol_list()?),
            ".extern" => Directive::Extern(self.symbol_list()?),
            ".local" => Directive::Local(self.symbol_list()?),
            ".align" | ".balign" | ".p2align" => {
                let kind = match name {
                    ".align" => AlignKind::Align,
                    ".balign" => AlignKind::Balign,
                    _ => AlignKind::P2align,
                };
                let alignment = self.expr()?;
                let (mut fill, mut max) = (None, None);
                if self.eat(&TokenKind::Comma) {
                    if self.peek() != &TokenKind::Comma && !self.at_end_of_statement() {
                        fill = Some(self.expr()?);
                    }
                    if self.eat(&TokenKind::Comma) {
                        max = Some(self.expr()?);
                    }
                }
                Directive::Align {
                    kind,
                    alignment,
                    fill,
                    max,
                }
            }
            ".ascii" | ".asciz" | ".string" => {
                let kind = match name {
                    ".ascii" => StringKind::Ascii,
                    ".asciz" => StringKind::Asciz,
                    _ => StringKind::String,
                };
                let mut values = Vec::new();
                loop {
                    match self.peek().clone() {
                        TokenKind::Str(s) => {
                            self.bump();
                            values.push(s);
                        }
                        _ => {
                            return Err(self.unexpected(&format!("a string literal after `{name}`")))
                        }
                    }
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                Directive::Strings { kind, values }
            }
            ".byte" | ".word" | ".short" | ".2byte" | ".value" | ".long" | ".int" | ".4byte"
            | ".quad" | ".8byte" => {
                let width = match name {
                    ".byte" => 1,
                    ".word" | ".short" | ".2byte" | ".value" => 2,
                    ".long" | ".int" | ".4byte" => 4,
                    _ => 8,
                };
                let values = self.expr_list()?;
                if values.is_empty() {
                    self.diagnostics.push(Diagnostic::warning(
                        format!("`{name}` without values emits nothing"),
                        span,
                    ));
                }
                Directive::Values { width, values }
            }
            ".zero" => Directive::Zero(self.expr()?),
            ".skip" | ".space" => {
                let size = self.expr()?;
                let fill = if self.eat(&TokenKind::Comma) {
                    Some(self.expr()?)
                } else {
                    None
                };
                Directive::Skip { size, fill }
            }
            ".type" => {
                let symbol = self.symbol_name()?;
                self.expect(&TokenKind::Comma, "after symbol in `.type`")?;
                let kind = self.type_name()?;
                Directive::Type { symbol, kind }
            }
            ".size" => {
                let symbol = self.symbol_name()?;
                self.expect(&TokenKind::Comma, "after symbol in `.size`")?;
                Directive::Size {
                    symbol,
                    value: self.expr()?,
                }
            }
            ".set" | ".equ" => {
                let symbol = self.symbol_name()?;
                self.expect(&TokenKind::Comma, &format!("after symbol in `{name}`"))?;
                Directive::Set {
                    symbol,
                    value: self.expr()?,
                }
            }
            ".comm" | ".lcomm" => {
                let symbol = self.symbol_name()?;
                self.expect(&TokenKind::Comma, &format!("after symbol in `{name}`"))?;
                let size = self.expr()?;
                let align = if self.eat(&TokenKind::Comma) {
                    Some(self.expr()?)
                } else {
                    None
                };
                Directive::Comm {
                    symbol,
                    size,
                    align,
                    local: name == ".lcomm",
                }
            }
            _ => {
                return Err(
                    Diagnostic::error(format!("unsupported directive `{name}`"), span).with_help(
                        format!("supported directives: {}", SUPPORTED_DIRECTIVES.join(" ")),
                    ),
                )
            }
        })
    }

    fn section(&mut self) -> PResult<Directive> {
        let name = match self.peek().clone() {
            TokenKind::Str(s) => {
                self.bump();
                String::from_utf8_lossy(&s).into_owned()
            }
            TokenKind::Ident(_) | TokenKind::Dot => {
                // Section names like `.note.GNU-stack` are lexed as several
                // tokens; join tokens that are directly adjacent in the source.
                let start = self.tok().start;
                let mut end = self.bump().end;
                while !self.at_end_of_statement()
                    && self.peek() != &TokenKind::Comma
                    && self.tok().start == end
                {
                    end = self.bump().end;
                }
                self.src[start..end].to_string()
            }
            _ => return Err(self.unexpected("a section name")),
        };
        let (mut flags, mut kind, mut extra) = (None, None, None);
        if self.eat(&TokenKind::Comma) {
            match self.peek().clone() {
                TokenKind::Str(s) => {
                    self.bump();
                    flags = Some(String::from_utf8_lossy(&s).into_owned());
                }
                _ => return Err(self.unexpected("a section flags string")),
            }
            if self.eat(&TokenKind::Comma) {
                kind = Some(format!("@{}", self.prefixed_name()?));
                if self.eat(&TokenKind::Comma) {
                    extra = Some(self.raw_rest());
                }
            }
        }
        Ok(Directive::Section {
            name,
            flags,
            kind,
            extra,
        })
    }

    /// `@name`, `%name` or `"name"` (used for section and symbol types).
    fn prefixed_name(&mut self) -> PResult<String> {
        match self.peek().clone() {
            TokenKind::Ident(s) if s.starts_with('@') && s.len() > 1 => {
                self.bump();
                Ok(s[1..].to_string())
            }
            TokenKind::Percent => {
                self.bump();
                match self.peek().clone() {
                    TokenKind::Ident(s) => {
                        self.bump();
                        Ok(s)
                    }
                    _ => Err(self.unexpected("a type name after `%`")),
                }
            }
            TokenKind::Str(s) => {
                self.bump();
                Ok(String::from_utf8_lossy(&s).into_owned())
            }
            TokenKind::Ident(s) => {
                self.bump();
                Ok(s)
            }
            _ => Err(self.unexpected("a type such as `@function` or `@progbits`")),
        }
    }

    fn type_name(&mut self) -> PResult<SymbolType> {
        let name = self.prefixed_name()?;
        Ok(match name.as_str() {
            "function" | "STT_FUNC" => SymbolType::Function,
            "object" | "STT_OBJECT" => SymbolType::Object,
            _ => SymbolType::Other(name),
        })
    }

    fn symbol_name(&mut self) -> PResult<String> {
        match self.peek().clone() {
            TokenKind::Ident(s) if !s.contains('@') => {
                self.bump();
                Ok(s)
            }
            TokenKind::Str(s) => {
                let span = self.span();
                self.bump();
                quoted_symbol_name(&s, span)
            }
            TokenKind::Register(r) => Err(Diagnostic::error(
                format!("register name `{r}` cannot be used as a symbol"),
                self.span(),
            )),
            _ => Err(self.unexpected("a symbol name")),
        }
    }

    fn symbol_list(&mut self) -> PResult<Vec<String>> {
        let mut v = vec![self.symbol_name()?];
        while self.eat(&TokenKind::Comma) {
            v.push(self.symbol_name()?);
        }
        Ok(v)
    }

    fn expr_list(&mut self) -> PResult<Vec<Expr>> {
        let mut v = Vec::new();
        if self.at_end_of_statement() {
            return Ok(v);
        }
        loop {
            v.push(self.expr()?);
            if !self.eat(&TokenKind::Comma) {
                return Ok(v);
            }
        }
    }

    /// Consumes the rest of the statement and returns its source text.
    fn raw_rest(&mut self) -> String {
        if self.at_end_of_statement() {
            return String::new();
        }
        let start = self.tok().start;
        let mut end = start;
        while !self.at_end_of_statement() {
            end = self.bump().end;
        }
        self.src[start..end].trim().to_string()
    }
}

/// Validates a quoted symbol name (`"main"`).
fn quoted_symbol_name(bytes: &[u8], span: Span) -> PResult<String> {
    match std::str::from_utf8(bytes) {
        Ok(s) if !s.is_empty() && !s.contains(['"', '\\', '\n', '\0']) => Ok(s.to_string()),
        Ok("") => Err(Diagnostic::error("empty symbol name", span)),
        _ => Err(Diagnostic::error(
            "invalid characters in quoted symbol name",
            span,
        )),
    }
}

/// Splits `name@MODIFIER` (e.g. `puts@PLT`).
fn split_symbol(text: &str, span: Span) -> PResult<SymbolRef> {
    match text.split_once('@') {
        None => Ok(SymbolRef::new(text)),
        Some((name, modifier))
            if !name.is_empty() && !modifier.is_empty() && !modifier.contains('@') =>
        {
            Ok(SymbolRef {
                name: name.to_string(),
                modifier: Some(modifier.to_string()),
            })
        }
        _ => Err(Diagnostic::error(format!("invalid symbol `{text}`"), span)),
    }
}

/// Structural validation of x86-64 addressing modes.
fn validate_memory(mem: &MemoryOperand, span: Span) -> PResult<()> {
    let err = |msg: String| Err(Diagnostic::error(msg, span));
    if let Some(base) = mem.base {
        match base.kind {
            RegKind::Rip => {
                if mem.index.is_some() {
                    return err("RIP-relative addressing cannot use an index register".into());
                }
            }
            RegKind::Gpr(_) if matches!(base.size, Size::Qword | Size::Dword) => {}
            _ => {
                return err(format!(
                    "`{base}` cannot be used as a base register in 64-bit mode"
                ))
            }
        }
    }
    if let Some(index) = mem.index {
        match index.kind {
            RegKind::Gpr(4) => {
                return err(format!("`{index}` cannot be used as an index register"));
            }
            RegKind::Gpr(_) if matches!(index.size, Size::Qword | Size::Dword) => {}
            RegKind::Rip => return err("`rip` cannot be used as an index register".into()),
            _ => {
                return err(format!(
                    "`{index}` cannot be used as an index register in 64-bit mode"
                ))
            }
        }
        if let Some(base) = mem.base {
            if base.kind != RegKind::Rip && base.size != index.size {
                return err(format!(
                    "base `{base}` and index `{index}` must have the same size"
                ));
            }
        }
    }
    if mem.base.is_none() && mem.index.is_none() && mem.disp.is_none() {
        return err("empty memory operand".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(src: &str) -> Program {
        let out = parse(src);
        assert!(
            !out.has_errors(),
            "unexpected errors: {:#?}",
            out.diagnostics
        );
        out.program
    }

    fn single_instruction(src: &str) -> Instruction {
        let p = parse_ok(src);
        let i = p.instructions().next().expect("no instruction").clone();
        i
    }

    fn errors(src: &str) -> Vec<String> {
        parse(src)
            .diagnostics
            .iter()
            .filter(|d| d.is_error())
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn labels_and_instructions() {
        let p = parse_ok("_start:\n  mov rax, 60\nloop: inc rcx\n");
        assert_eq!(p.statements.len(), 4);
        assert_eq!(p.statements[0].kind, StatementKind::Label("_start".into()));
        assert_eq!(p.statements[2].kind, StatementKind::Label("loop".into()));
        let StatementKind::Instruction(i) = &p.statements[3].kind else {
            panic!()
        };
        assert_eq!(i.mnemonic, "inc");
        assert_eq!(i.span, Span::new(3, 7, 3));
    }

    #[test]
    fn register_and_immediate_operands() {
        let i = single_instruction("MOV EAX, -1");
        assert_eq!(i.mnemonic, "mov");
        assert_eq!(
            i.operands[0].kind,
            OperandKind::Register(Register::gpr(0, Size::Dword))
        );
        let OperandKind::Immediate(e) = &i.operands[1].kind else {
            panic!()
        };
        assert_eq!(e.eval_constant(), Some(-1));
    }

    #[test]
    fn memory_operands() {
        let i = single_instruction("mov qword ptr [rbp + rcx*8 - 16], rax");
        let OperandKind::Memory(m) = &i.operands[0].kind else {
            panic!()
        };
        assert_eq!(m.size, Some(Size::Qword));
        assert_eq!(m.base, Some(Register::gpr(5, Size::Qword)));
        assert_eq!(m.index, Some(Register::RCX));
        assert_eq!(m.scale, 8);
        assert_eq!(m.disp.as_ref().unwrap().eval_constant(), Some(-16));

        let i = single_instruction("lea rsi, [rip + msg]");
        let OperandKind::Memory(m) = &i.operands[1].kind else {
            panic!()
        };
        assert!(m.is_rip_relative());
        assert_eq!(m.disp, Some(Expr::symbol("msg")));

        let i = single_instruction("lea rsi, msg[rip]");
        assert!(matches!(&i.operands[1].kind, OperandKind::Memory(m) if m.is_rip_relative()));

        let i = single_instruction("mov eax, dword ptr [4*rbx]");
        let OperandKind::Memory(m) = &i.operands[1].kind else {
            panic!()
        };
        assert_eq!(
            (m.base, m.index, m.scale),
            (None, Some(Register::gpr(3, Size::Qword)), 4)
        );

        let i = single_instruction("mov rax, qword ptr fs:0x28");
        let OperandKind::Memory(m) = &i.operands[1].kind else {
            panic!()
        };
        assert!(m.segment.is_some());
        assert_eq!(m.disp, Some(Expr::Int(0x28)));

        let i = single_instruction("inc dword ptr counter");
        assert!(matches!(&i.operands[0].kind, OperandKind::Memory(m) if m.base.is_none()));
    }

    #[test]
    fn symbolic_and_offset_operands() {
        let i = single_instruction("mov rsi, offset msg");
        assert_eq!(i.operands[1].kind, OperandKind::Offset(Expr::symbol("msg")));
        let i = single_instruction("mov edi, OFFSET FLAT:.LC0");
        assert_eq!(
            i.operands[1].kind,
            OperandKind::Offset(Expr::symbol(".LC0"))
        );
        let i = single_instruction("jmp .Lloop");
        assert_eq!(
            i.operands[0].kind,
            OperandKind::Symbolic(Expr::symbol(".Lloop"))
        );
        let i = single_instruction("call puts@PLT");
        let OperandKind::Symbolic(Expr::Symbol(s)) = &i.operands[0].kind else {
            panic!()
        };
        assert_eq!(
            (s.name.as_str(), s.modifier.as_deref()),
            ("puts", Some("PLT"))
        );
        let i = single_instruction("jne 1b");
        assert_eq!(
            i.operands[0].kind,
            OperandKind::Symbolic(Expr::LocalLabelRef {
                number: 1,
                forward: false
            })
        );
    }

    #[test]
    fn prefixes() {
        let i = single_instruction("rep movsb");
        assert_eq!(i.prefix.as_deref(), Some("rep"));
        assert_eq!(i.mnemonic, "movsb");
        assert!(i.operands.is_empty());
    }

    #[test]
    fn data_directives() {
        let p = parse_ok(
            r#"
            .intel_syntax noprefix
            .section .rodata
msg:        .asciz "Hello\n"
            len = . - msg
            .data
nums:       .byte 1, 2, 'x'
            .word 0x1234
            .long -1
            .quad nums, 0
            .bss
buf:        .zero 64
            .skip 16, 0
            .comm big, 4096, 32
            .section .note.GNU-stack,"",@progbits
            .type main, @function
            .size main, .-main
            .p2align 4,,15
            .globl main, _start
            .cfi_startproc
"#,
        );
        let dirs: Vec<&Directive> = p
            .statements
            .iter()
            .filter_map(|s| match &s.kind {
                StatementKind::Directive(d) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(dirs[0], &Directive::IntelSyntax);
        assert!(matches!(dirs[1], Directive::Section { name, .. } if name == ".rodata"));
        assert_eq!(
            dirs[2],
            &Directive::Strings {
                kind: StringKind::Asciz,
                values: vec![b"Hello\n".to_vec()]
            }
        );
        assert!(matches!(dirs[3], Directive::Set { symbol, .. } if symbol == "len"));
        assert!(matches!(dirs[5], Directive::Values { width: 1, values } if values.len() == 3));
        assert!(matches!(dirs[6], Directive::Values { width: 2, .. }));
        assert!(
            matches!(dirs[8], Directive::Values { width: 8, values } if !values[0].is_constant())
        );
        assert!(matches!(dirs[11], Directive::Skip { fill: Some(_), .. }));
        assert!(matches!(
            dirs[12],
            Directive::Comm {
                local: false,
                align: Some(_),
                ..
            }
        ));
        assert_eq!(
            dirs[13],
            &Directive::Section {
                name: ".note.GNU-stack".into(),
                flags: Some(String::new()),
                kind: Some("@progbits".into()),
                extra: None
            }
        );
        assert_eq!(
            dirs[14],
            &Directive::Type {
                symbol: "main".into(),
                kind: SymbolType::Function
            }
        );
        assert!(matches!(
            dirs[16],
            Directive::Align {
                kind: AlignKind::P2align,
                fill: None,
                max: Some(_),
                ..
            }
        ));
        assert_eq!(
            dirs[17],
            &Directive::Global(vec!["main".into(), "_start".into()])
        );
        assert!(matches!(dirs[18], Directive::Metadata { name, .. } if name == ".cfi_startproc"));
    }

    #[test]
    fn error_messages_with_locations() {
        let e = errors("mov rax, [rbx*3]");
        assert_eq!(e.len(), 1);
        assert!(
            e[0].starts_with("1:15: error: invalid scale factor `3`"),
            "{e:?}"
        );

        let e = errors("mov rax, [rsp*2]");
        assert!(
            e[0].contains("`rsp` cannot be used as an index register"),
            "{e:?}"
        );

        let e = errors("mov rax, [rbx - rcx]");
        assert!(e[0].contains("cannot be subtracted"), "{e:?}");

        let e = errors("  .weird 1");
        assert!(
            e[0].starts_with("1:3: error: unsupported directive `.weird`"),
            "{e:?}"
        );

        let e = errors(".att_syntax");
        assert!(e[0].contains("AT&T syntax is not supported"));

        let e = errors("mov rax, rbx rcx");
        assert!(
            e[0].contains("expected end of line, found register `rcx`"),
            "{e:?}"
        );

        let e = errors("mov rax, [rbx + eax]");
        assert!(e[0].contains("same size"), "{e:?}");

        let e = errors("mov %rax, 1");
        assert!(e[0].contains("register prefix `%`"), "{e:?}");
    }

    #[test]
    fn recovers_and_reports_multiple_errors() {
        let out = parse("mov rax, [rbx*3]\nmov rax, 1\n.bogus\nadd rax, (1\n");
        let e: Vec<_> = out.diagnostics.iter().filter(|d| d.is_error()).collect();
        assert_eq!(e.len(), 3, "{e:#?}");
        assert_eq!(e[0].span.unwrap().line, 1);
        assert_eq!(e[1].span.unwrap().line, 3);
        assert_eq!(e[2].span.unwrap().line, 4);
        assert_eq!(out.program.instructions().count(), 1);
    }

    #[test]
    fn quoted_symbols() {
        let p = parse_ok(
            "\t.globl \"main\"\n\"main\":\n\tcall \"printf\"@PLT\n\tlea rcx, \"tbl\"[rip]\n\t.size \"main\", .-\"main\"\n\"my sym\": nop\n",
        );
        assert_eq!(
            p.statements[0].kind,
            StatementKind::Directive(Directive::Global(vec!["main".into()]))
        );
        assert_eq!(p.statements[1].kind, StatementKind::Label("main".into()));
        let call = p.instructions().next().unwrap();
        assert_eq!(
            call.operands[0].kind,
            OperandKind::Symbolic(Expr::Symbol(SymbolRef {
                name: "printf".into(),
                modifier: Some("PLT".into())
            }))
        );
        let printed = p.to_string();
        assert!(printed.contains("main:\n"), "{printed}");
        assert!(printed.contains("\"my sym\":"), "{printed}");
        assert_eq!(parse_ok(&printed).to_string(), printed);
    }

    #[test]
    fn display_roundtrip_is_stable() {
        let src = r#"
.intel_syntax noprefix
.text
.globl _start
_start:
    mov qword ptr [rbp + rcx*8 - 16], rax
    lea rsi, [rip + msg + 4]
    mov rdx, (len * 2) | 1
    mov eax, dword ptr fs:[rax]
    rep stosb
1:  jne 1b
.section .rodata, "a", @progbits
msg: .ascii "a\"b\\c\n\001"
len = . - msg
.p2align 4,,15
"#;
        let first = parse_ok(src).to_string();
        let second = parse_ok(&first).to_string();
        assert_eq!(first, second);
    }
}
