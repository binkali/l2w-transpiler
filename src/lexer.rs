//! Lexer for GAS Intel-syntax x86-64 assembly.
//!
//! Recognized lexical elements:
//!
//! * identifiers (mnemonics, labels, symbols, keywords such as `qword`/`ptr`)
//! * registers (`rax`, `r8d`, `al`, `xmm0`, `fs`, ... — reserved in `noprefix` mode)
//! * directives (`.text`, `.globl`, ...) — a dotted name at the **start of a
//!   statement** that is not followed by `:` (that would be a label like
//!   `.Lloop:`) or `=`. Dotted names elsewhere (`jmp .Lloop`,
//!   `.section .rodata`) are identifiers.
//! * integers: decimal, `0x` hex, `0b` binary, leading-`0` octal, `h`-suffixed
//!   hex and character literals (`'A'`)
//! * numeric local label references `1b` / `1f`
//! * strings with GAS escapes (`\n`, `\t`, `\\`, `\"`, octal, `\x` hex)
//! * comments: `#` and `//` to end of line, `/* ... */` blocks
//! * punctuation and operators; `;` separates statements like a newline.

use crate::ast::Register;
use crate::diagnostics::{Diagnostic, Span};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Ident(String),
    Register(Register),
    /// Lowercased directive name including the leading dot.
    Directive(String),
    Integer(i128),
    LocalLabelRef {
        number: u32,
        forward: bool,
    },
    Str(Vec<u8>),
    Comma,
    Colon,
    LBracket,
    RBracket,
    LParen,
    RParen,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Equals,
    Tilde,
    Amp,
    Pipe,
    Caret,
    Shl,
    Shr,
    /// A lone `.` (location counter).
    Dot,
    Newline,
    Eof,
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenKind::Ident(s) => write!(f, "identifier `{s}`"),
            TokenKind::Register(r) => write!(f, "register `{r}`"),
            TokenKind::Directive(d) => write!(f, "directive `{d}`"),
            TokenKind::Integer(v) => write!(f, "integer `{v}`"),
            TokenKind::LocalLabelRef { number, forward } => {
                write!(
                    f,
                    "local label reference `{number}{}`",
                    if *forward { 'f' } else { 'b' }
                )
            }
            TokenKind::Str(_) => f.write_str("string literal"),
            TokenKind::Newline => f.write_str("end of line"),
            TokenKind::Eof => f.write_str("end of file"),
            other => {
                let s = match other {
                    TokenKind::Comma => ",",
                    TokenKind::Colon => ":",
                    TokenKind::LBracket => "[",
                    TokenKind::RBracket => "]",
                    TokenKind::LParen => "(",
                    TokenKind::RParen => ")",
                    TokenKind::Plus => "+",
                    TokenKind::Minus => "-",
                    TokenKind::Star => "*",
                    TokenKind::Slash => "/",
                    TokenKind::Percent => "%",
                    TokenKind::Equals => "=",
                    TokenKind::Tilde => "~",
                    TokenKind::Amp => "&",
                    TokenKind::Pipe => "|",
                    TokenKind::Caret => "^",
                    TokenKind::Shl => "<<",
                    TokenKind::Shr => ">>",
                    TokenKind::Dot => ".",
                    _ => unreachable!(),
                };
                write!(f, "`{s}`")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    /// Byte offsets into the source (`start..end`).
    pub start: usize,
    pub end: usize,
}

/// Result of lexing: tokens (always terminated by `Newline`, `Eof`) plus
/// any lexical errors. Lexing continues after errors so that as many
/// problems as possible are reported in a single run.
#[derive(Debug)]
pub struct LexOutput {
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn tokenize(src: &str) -> LexOutput {
    let mut lx = Lexer {
        src,
        pos: 0,
        line: 1,
        col: 1,
        tokens: Vec::new(),
        diagnostics: Vec::new(),
        stmt_len: 0,
    };
    lx.run();
    LexOutput {
        tokens: lx.tokens,
        diagnostics: lx.diagnostics,
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || matches!(c, '_' | '.' | '@')
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '$' | '@')
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    line: u32,
    col: u32,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
    /// Number of tokens emitted in the current statement (0 = statement start).
    stmt_len: usize,
}

#[derive(Clone, Copy)]
struct Mark {
    pos: usize,
    line: u32,
    col: u32,
}

impl<'a> Lexer<'a> {
    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn peek_nth(&self, n: usize) -> Option<char> {
        self.src[self.pos..].chars().nth(n)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn mark(&self) -> Mark {
        Mark {
            pos: self.pos,
            line: self.line,
            col: self.col,
        }
    }

    fn span_from(&self, m: Mark) -> Span {
        let len = if self.line == m.line {
            self.col - m.col
        } else {
            1
        };
        Span::new(m.line, m.col, len.max(1))
    }

    fn push(&mut self, kind: TokenKind, m: Mark) {
        let span = self.span_from(m);
        match kind {
            TokenKind::Newline => self.stmt_len = 0,
            // A colon directly after the first token of a statement ends a
            // label; what follows starts a new statement.
            TokenKind::Colon
                if self.stmt_len == 1
                    && matches!(
                        self.tokens.last().map(|t| &t.kind),
                        Some(TokenKind::Ident(_) | TokenKind::Integer(_) | TokenKind::Str(_))
                    ) =>
            {
                self.stmt_len = 0
            }
            _ => self.stmt_len += 1,
        }
        self.tokens.push(Token {
            kind,
            span,
            start: m.pos,
            end: self.pos,
        });
    }

    fn error(&mut self, msg: impl Into<String>, span: Span) {
        self.diagnostics.push(Diagnostic::error(msg, span));
    }

    fn run(&mut self) {
        while let Some(c) = self.peek() {
            let m = self.mark();
            match c {
                ' ' | '\t' | '\r' | '\x0c' => {
                    self.bump();
                }
                '\n' | ';' => {
                    self.bump();
                    self.push(TokenKind::Newline, m);
                }
                '#' => self.skip_line_comment(),
                '/' if self.peek_nth(1) == Some('/') => self.skip_line_comment(),
                '/' if self.peek_nth(1) == Some('*') => self.skip_block_comment(),
                '"' => self.string(),
                '\'' => self.char_literal(),
                '0'..='9' => self.number(),
                '.' if !self.peek_nth(1).is_some_and(is_ident_continue) => {
                    self.bump();
                    self.push(TokenKind::Dot, m);
                }
                c if is_ident_start(c) => self.ident(),
                '<' if self.peek_nth(1) == Some('<') => {
                    self.bump();
                    self.bump();
                    self.push(TokenKind::Shl, m);
                }
                '>' if self.peek_nth(1) == Some('>') => {
                    self.bump();
                    self.bump();
                    self.push(TokenKind::Shr, m);
                }
                _ => {
                    self.bump();
                    let kind = match c {
                        ',' => TokenKind::Comma,
                        ':' => TokenKind::Colon,
                        '[' => TokenKind::LBracket,
                        ']' => TokenKind::RBracket,
                        '(' => TokenKind::LParen,
                        ')' => TokenKind::RParen,
                        '+' => TokenKind::Plus,
                        '-' => TokenKind::Minus,
                        '*' => TokenKind::Star,
                        '/' => TokenKind::Slash,
                        '%' => TokenKind::Percent,
                        '=' => TokenKind::Equals,
                        '~' => TokenKind::Tilde,
                        '&' => TokenKind::Amp,
                        '|' => TokenKind::Pipe,
                        '^' => TokenKind::Caret,
                        '$' => {
                            let span = self.span_from(m);
                            self.diagnostics.push(
                                Diagnostic::error("unexpected `$`", span).with_help(
                                    "`$` marks immediates in AT&T syntax; in Intel syntax write the value directly",
                                ),
                            );
                            continue;
                        }
                        _ => {
                            let span = self.span_from(m);
                            self.error(
                                format!("unexpected character `{}`", c.escape_default()),
                                span,
                            );
                            continue;
                        }
                    };
                    self.push(kind, m);
                }
            }
        }
        let m = self.mark();
        if !matches!(
            self.tokens.last().map(|t| &t.kind),
            Some(TokenKind::Newline) | None
        ) {
            self.push(TokenKind::Newline, m);
        }
        self.push(TokenKind::Eof, m);
    }

    fn skip_line_comment(&mut self) {
        while let Some(c) = self.peek() {
            if c == '\n' {
                break;
            }
            self.bump();
        }
    }

    fn skip_block_comment(&mut self) {
        let m = self.mark();
        self.bump();
        self.bump();
        loop {
            match self.peek() {
                None => {
                    self.error("unterminated block comment", Span::new(m.line, m.col, 2));
                    return;
                }
                Some('*') if self.peek_nth(1) == Some('/') => {
                    self.bump();
                    self.bump();
                    return;
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    fn ident(&mut self) {
        let m = self.mark();
        while self.peek().is_some_and(is_ident_continue) {
            self.bump();
        }
        let text = &self.src[m.pos..self.pos];
        let kind =
            if text.starts_with('.') && self.stmt_len == 0 && !self.next_nonblank_is(&[':', '=']) {
                TokenKind::Directive(text.to_ascii_lowercase())
            } else if let Some(reg) = Register::parse(text) {
                TokenKind::Register(reg)
            } else {
                TokenKind::Ident(text.to_string())
            };
        self.push(kind, m);
    }

    fn next_nonblank_is(&self, chars: &[char]) -> bool {
        self.src[self.pos..]
            .chars()
            .find(|c| !matches!(c, ' ' | '\t'))
            .is_some_and(|c| chars.contains(&c))
    }

    fn number(&mut self) {
        let m = self.mark();
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            self.bump();
        }
        let text = &self.src[m.pos..self.pos];
        let span = self.span_from(m);
        match parse_number(text) {
            Ok(kind) => self.push(kind, m),
            Err(msg) => {
                self.error(msg, span);
                // Emit a placeholder so the parser does not report a second,
                // confusing error for the same location.
                self.push(TokenKind::Integer(0), m);
            }
        }
    }

    fn char_literal(&mut self) {
        let m = self.mark();
        self.bump(); // opening quote
        let value = match self.peek() {
            None | Some('\n') => {
                let span = self.span_from(m);
                self.error("empty character literal", span);
                return;
            }
            Some('\\') => {
                self.bump();
                match self.escape() {
                    Some(b) => b as i128,
                    None => return,
                }
            }
            Some(c) => {
                self.bump();
                if !c.is_ascii() {
                    let span = self.span_from(m);
                    self.error("character literal must be ASCII", span);
                }
                c as i128
            }
        };
        // GAS allows the closing quote to be omitted.
        if self.peek() == Some('\'') {
            self.bump();
        }
        self.push(TokenKind::Integer(value), m);
    }

    fn string(&mut self) {
        let m = self.mark();
        self.bump();
        let mut bytes = Vec::new();
        loop {
            match self.peek() {
                None | Some('\n') => {
                    let span = Span::new(m.line, m.col, 1);
                    self.diagnostics.push(
                        Diagnostic::error("unterminated string literal", span)
                            .with_help("add a closing `\"` before the end of the line"),
                    );
                    self.push(TokenKind::Str(bytes), m);
                    return;
                }
                Some('"') => {
                    self.bump();
                    break;
                }
                Some('\\') => {
                    self.bump();
                    if let Some(b) = self.escape() {
                        bytes.push(b);
                    }
                }
                Some(c) => {
                    self.bump();
                    let mut buf = [0u8; 4];
                    bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
        self.push(TokenKind::Str(bytes), m);
    }

    /// Parses an escape sequence; the backslash has already been consumed.
    fn escape(&mut self) -> Option<u8> {
        let m = self.mark();
        let c = self.bump()?;
        Some(match c {
            'n' => b'\n',
            't' => b'\t',
            'r' => b'\r',
            'b' => 0x08,
            'f' => 0x0c,
            'v' => 0x0b,
            'a' => 0x07,
            'e' => 0x1b,
            '\\' => b'\\',
            '"' => b'"',
            '\'' => b'\'',
            '0'..='7' => {
                let mut v = c.to_digit(8).unwrap();
                for _ in 0..2 {
                    match self.peek().and_then(|d| d.to_digit(8)) {
                        Some(d) => {
                            v = v * 8 + d;
                            self.bump();
                        }
                        None => break,
                    }
                }
                if v > 0xff {
                    let span = self.span_from(m);
                    self.error("octal escape out of range (max \\377)", span);
                }
                v as u8
            }
            'x' => {
                let mut v: u32 = 0;
                let mut n = 0;
                while let Some(d) = self.peek().and_then(|d| d.to_digit(16)) {
                    v = (v << 4 | d) & 0xff; // GAS keeps the low byte
                    n += 1;
                    self.bump();
                }
                if n == 0 {
                    let span = self.span_from(m);
                    self.error("`\\x` escape without hex digits", span);
                }
                v as u8
            }
            '\n' => {
                let span = self.span_from(m);
                self.error("unterminated string literal", span);
                return None;
            }
            other => {
                let span = self.span_from(m);
                self.diagnostics.push(Diagnostic::warning(
                    format!(
                        "unknown escape sequence `\\{}`, treated as `{}`",
                        other, other
                    ),
                    span,
                ));
                let mut buf = [0u8; 4];
                other.encode_utf8(&mut buf);
                buf[0]
            }
        })
    }
}

/// Interprets a numeric token: decimal, `0x` hex, `0b` binary, leading-zero
/// octal, `h`-suffixed hex, or a local label reference (`1b`, `2f`).
fn parse_number(text: &str) -> Result<TokenKind, String> {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let last = *bytes.last().unwrap();
    let head = &lower[..lower.len() - 1];

    if lower.len() >= 2 && matches!(last, b'b' | b'f') && head.bytes().all(|b| b.is_ascii_digit()) {
        let number = head
            .parse::<u32>()
            .map_err(|_| format!("local label number `{head}` is too large"))?;
        return Ok(TokenKind::LocalLabelRef {
            number,
            forward: last == b'f',
        });
    }

    let (digits, radix) = if let Some(rest) = lower.strip_prefix("0x") {
        (rest, 16)
    } else if let Some(rest) = lower.strip_prefix("0b") {
        (rest, 2)
    } else if last == b'h' {
        (head, 16)
    } else if lower.len() > 1 && lower.starts_with('0') {
        (&lower[1..], 8)
    } else {
        (lower.as_str(), 10)
    };
    if digits.is_empty() {
        return Err(format!("invalid integer literal `{text}`: missing digits"));
    }
    let value = u128::from_str_radix(digits, radix).map_err(|_| {
        let kind = match radix {
            16 => "hexadecimal",
            8 => "octal",
            2 => "binary",
            _ => "decimal",
        };
        format!("invalid {kind} integer literal `{text}`")
    })?;
    if value > u64::MAX as u128 {
        return Err(format!("integer literal `{text}` does not fit in 64 bits"));
    }
    Ok(TokenKind::Integer(value as i128))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Size;

    fn kinds(src: &str) -> Vec<TokenKind> {
        let out = tokenize(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        out.tokens.into_iter().map(|t| t.kind).collect()
    }

    fn ident(s: &str) -> TokenKind {
        TokenKind::Ident(s.into())
    }

    #[test]
    fn instruction_with_memory_operand() {
        use TokenKind::*;
        assert_eq!(
            kinds("mov qword ptr [rbp - 8], 0x10 # comment"),
            vec![
                ident("mov"),
                ident("qword"),
                ident("ptr"),
                LBracket,
                Register(crate::ast::Register::gpr(5, Size::Qword)),
                Minus,
                Integer(8),
                RBracket,
                Comma,
                Integer(16),
                Newline,
                Eof
            ]
        );
    }

    #[test]
    fn directives_vs_dotted_labels() {
        use TokenKind::*;
        let k = kinds(".text\n.Lloop: jmp .Lloop\n.section .rodata\nfoo: .byte 1\nlen = . - msg");
        assert_eq!(k[0], Directive(".text".into()));
        assert_eq!(k[2], ident(".Lloop"));
        assert_eq!(k[3], Colon);
        assert_eq!(k[5], ident(".Lloop"));
        assert_eq!(k[7], Directive(".section".into()));
        assert_eq!(k[8], ident(".rodata"));
        // label then directive on the same line
        assert_eq!(k[12], Directive(".byte".into()));
        assert!(k.contains(&Dot));
        assert!(k.contains(&Equals));
    }

    #[test]
    fn number_formats() {
        use TokenKind::*;
        assert_eq!(
            kinds("10 0x1F 0b101 017 0FFh 'A' '\\n' 1b 2f"),
            vec![
                Integer(10),
                Integer(31),
                Integer(5),
                Integer(15),
                Integer(255),
                Integer(65),
                Integer(10),
                LocalLabelRef {
                    number: 1,
                    forward: false
                },
                LocalLabelRef {
                    number: 2,
                    forward: true
                },
                Newline,
                Eof
            ]
        );
    }

    #[test]
    fn strings_and_escapes() {
        let k = kinds(r#".asciz "Hi\n\t\"q\"\101\x42""#);
        assert_eq!(k[1], TokenKind::Str(b"Hi\n\t\"q\"AB".to_vec()));
    }

    #[test]
    fn comments_and_separators() {
        use TokenKind::*;
        let k = kinds("nop // c1\n/* multi\nline */ nop; ret # end");
        assert_eq!(
            k,
            vec![
                ident("nop"),
                Newline,
                ident("nop"),
                Newline,
                ident("ret"),
                Newline,
                Eof
            ]
        );
    }

    #[test]
    fn symbol_with_modifier() {
        assert_eq!(kinds("call puts@PLT")[1], ident("puts@PLT"));
    }

    #[test]
    fn errors_have_positions() {
        let out = tokenize("mov rax, 1\n  mov rbx, \"abc\n  mov rcx, 0x\n  mov $1, ?");
        let msgs: Vec<String> = out.diagnostics.iter().map(|d| d.to_string()).collect();
        assert!(
            msgs[0].starts_with("2:12: error: unterminated string"),
            "{msgs:?}"
        );
        assert!(
            msgs[1].starts_with("3:12: error: invalid integer literal `0x`"),
            "{msgs:?}"
        );
        assert!(
            msgs[2].starts_with("4:7: error: unexpected `$`"),
            "{msgs:?}"
        );
        assert!(
            msgs[3].starts_with("4:11: error: unexpected character `?`"),
            "{msgs:?}"
        );
    }

    #[test]
    fn too_large_integer() {
        let out = tokenize(".quad 0x10000000000000000");
        assert!(out.diagnostics[0]
            .message
            .contains("does not fit in 64 bits"));
    }
}
