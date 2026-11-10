# l2w — Linux-to-Windows x86-64 Assembly Transpiler

`l2w` translates **Linux x86-64 assembly** (GNU Assembler, `.intel_syntax noprefix`,
System V ABI, ELF directives, Linux system calls) into **Windows x86-64 assembly**
(GNU Assembler, Intel syntax, Windows x64 ABI, COFF sections, Win32 API calls),
so that it can be built into a Windows `.exe` with MinGW-w64.

It is **not** a regex-based text converter. The pipeline is a real compiler front
end (lexer → parser → AST → IR) followed by semantic translation passes. Whenever a
construct can't be translated while keeping its meaning, `l2w` stops with a
precise diagnostic instead of producing wrong code.

```text
source ─lexer─▶ tokens ─parser─▶ AST ─▶ IR ─transpiler─▶ Windows IR ─codegen─▶ .s ─MinGW─▶ .exe
```

## Status

| Phase | Content | Status |
|---|---|---|
| 1 | Rust project, CLI, lexer, parser, diagnostics | ✅ done |
| 2 | AST → intermediate representation | ⏳ planned |
| 3 | Translating ordinary instructions and GAS directives, ELF → COFF sections | ⏳ planned |
| 4 | Calling-convention compatibility (System V ↔ Windows x64) | ⏳ planned |
| 5 | Linux system calls → Windows API runtime layer | ⏳ planned |
| 6 | Windows code generation, cross-compiling with MinGW-w64 | ⏳ planned |
| 7 | More tests, error handling, documentation | ⏳ planned |

Right now, `--check`, `--dump-tokens` and `--dump-ast` are fully working.
Translating (`-o`) and `--compile` stop with a clear "not implemented yet" error
until phases 3 and 6 are done.

## Building

Requires Rust (stable, ≥ 1.75). No external crates are used.

```bash
cargo build --release
./target/release/l2w --help
```

## Usage

```bash
l2w input.s -o output.s                    # translate (phase 3+)
l2w input.s -o output.s --target windows   # same; windows is the default
l2w input.s --compile -o output.exe        # translate + assemble + link (phase 6+)
l2w input.s --check                        # parse/analyze only, report diagnostics
l2w input.s --check --verbose              # with progress information
l2w input.s --dump-tokens                  # print the lexer's token stream
l2w input.s --dump-ast                     # print the parsed AST
```

Exit codes: `0` success, `1` errors in the input, `2` usage or I/O error.

### Example diagnostics

```text
$ l2w examples/errors.s --check
error: invalid scale factor `3`
  --> examples/errors.s:3:19
   |
 3 |     mov rax, [rbx*3]
   |                   ^
  = help: the index scale must be 1, 2, 4 or 8
error: unsupported directive `.weak`
  --> examples/errors.s:5:5
   |
 5 |     .weak foo
   |     ^^^^^
error: `rsp` cannot be used as an index register
  --> examples/errors.s:6:14
   ...
error: aborting due to 3 errors
```

After an error, the parser skips to the end of the line and keeps going, so one run
reports every independent error.

## Project structure

```text
src/
├── main.rs                  CLI entry point, pipeline driver
├── lib.rs                   library crate (used by the binary and the tests)
├── cli.rs                   argument parsing (no dependencies)
├── lexer.rs                 tokenizer
├── parser.rs                recursive-descent parser → AST
├── ast.rs                   AST types + normalized pretty-printer
├── diagnostics.rs           spans, errors/warnings, rustc-style rendering
├── ir.rs                    intermediate representation            (phase 2)
├── transpiler.rs            Linux → Windows passes                 (phases 3–5)
├── codegen/{mod,windows}.rs Windows/COFF GAS emitter               (phase 6)
├── compatibility/
│   ├── syscalls.rs          Linux syscalls → Win32 API             (phase 5)
│   ├── calling_convention.rs System V ↔ Windows x64                (phase 4)
│   └── directives.rs        ELF → COFF sections/directives         (phase 3)
└── linker.rs                MinGW-w64 assembler/linker driver      (phase 6)
tests/
├── cli.rs                   end-to-end tests of the binary
└── roundtrip.rs             parse → print → `as` object comparison
examples/                    sample programs (hello, loop, functions, errors)
```

## Supported input (phase 1)

### Lexer

| Element | Examples |
|---|---|
| Instructions / identifiers | `mov`, `MOV`, `_start`, `.Lloop`, `puts@PLT`, `"quoted name"` |
| Registers (reserved in `noprefix` mode) | `rax`…`r15`, `eax`…`r15d`, `ax`…`r15w`, `al`…`r15b`/`r15l`, `ah`/`bh`/`ch`/`dh`, `rip`, `es`…`gs`, `xmm0`…`xmm15` |
| Integers | `42`, `0x2A`, `0b101010`, `052` (octal), `2Ah`, `'A'`, `'\n'` |
| Local label references | `1b`, `2f` |
| Strings | `"Hi\n"` with `\n \t \r \b \f \v \a \e \\ \" \'`, octal `\101`, hex `\x41` |
| Comments | `# …`, `// …`, `/* … */` |
| Statement separators | newline, `;` |
| Operators | `+ - * / % << >> & \| ^ ~ ( ) [ ] , : =` and `.` (location counter) |

**Directive vs. label:** a dotted name at the start of a statement is a directive
unless `:` (label, e.g. `.Lloop:`) or `=` (assignment) follows. In any other
position (`jmp .Lloop`, `.section .rodata`) it's a symbol.

### Parser

**Statements:** labels (`name:`, `"name":`, numeric `1:`), several labels per line,
instructions with an optional prefix (`rep`, `repe/repz`, `repne/repnz`, `lock`),
directives, and assignments `sym = expr`.

**Operands:**

| Syntax | AST |
|---|---|
| `rax` | `Register` |
| `42`, `-1`, `(4*8)` | `Immediate` (constant expression) |
| `offset msg`, `OFFSET FLAT:msg` | `Offset` (address as immediate) |
| `[rbp + rcx*8 - 16]`, `[4*rbx]`, `[rip + msg]`, `msg[rip]` | `Memory` |
| `qword ptr [rax]`, `dword ptr counter`, `qword ptr fs:0x28`, `fs:[rax]` | `Memory` with size / segment |
| `msg`, `.Lloop`, `puts@PLT`, `1b` | `Symbolic` |

> **Intel-syntax semantics:** `mov rax, msg` **loads from memory** at `msg`, while
> `jmp msg` uses `msg` as a **branch target**. The parser keeps both as `Symbolic`
> and doesn't guess. Later phases resolve them from the mnemonic and symbol table.

**Addressing modes are checked structurally:** the scale must be 1/2/4/8; `rsp`
can't be an index; RIP-relative addressing can't take an index; base and index must
be the same width (64- or 32-bit); 8/16-bit address registers are rejected; registers
can't be subtracted.

**Expressions** use GAS precedence (lowest first): `|`, `^`, `&`, `<< >>`, `+ -`,
`* / %`, unary `- ~ +`.

**Directives:**

| Directive | Notes |
|---|---|
| `.intel_syntax noprefix` | `prefix` → error; no argument → warning |
| `.att_syntax` | error: AT&T not supported |
| `.text`, `.data`, `.bss` | subsections → error |
| `.section name[, "flags"[, @type[, …]]]` | names like `.note.GNU-stack` supported |
| `.global` / `.globl`, `.extern`, `.local` | symbol lists |
| `.align`, `.balign`, `.p2align` | including the `.p2align 4,,15` form |
| `.ascii`, `.asciz`, `.string` | several strings per directive |
| `.byte` (1), `.word`/`.short`/`.2byte`/`.value` (2), `.long`/`.int`/`.4byte` (4), `.quad`/`.8byte` (8) | expressions, including symbols |
| `.zero`, `.skip`/`.space` | optional fill byte |
| `.type sym, @function/@object/%function/STT_FUNC` | |
| `.size sym, expr` | |
| `.set`/`.equ sym, expr`, `sym = expr` | |
| `.comm`, `.lcomm` | |
| `.file`, `.ident`, `.loc`, `.addrsig`, `.cfi_*` | kept as metadata |
| anything else | error: unsupported directive (with a list of supported ones) |

The parser handles assembly produced by `gcc -S -masm=intel` (`-O0`/`-O2`),
including quoted symbol names.

### Not supported (yet)

- AT&T syntax and `.intel_syntax prefix` (`%rax`)
- Macros (`.macro`, `.rept`, `.irp`), conditional assembly (`.if`), `.include`
- Subsections, `.weak`, `.pushsection`/`.popsection`
- Floating-point literals
- `ymm`/`zmm`, mask and control registers

## Tests

```bash
cargo test
```

- **Unit tests** cover the lexer, parser, AST printer, CLI parsing and diagnostic
  rendering.
- **`tests/cli.rs`** runs the binary end to end: `--check`, `--verbose`, dumps,
  exit codes, and error positions.
- **`tests/roundtrip.rs`** is a semantic check. It parses the input, prints it back
  as normalized assembly, assembles **both** versions with GNU `as`, and requires
  identical object contents and relocations. This shows the parser and printer keep
  the meaning of the code. The test is skipped if `as`/`objdump` aren't installed.

## Planned translation rules (outline for later phases)

- **Sections:** `.text` → `.text`, `.data` → `.data`, `.bss` → `.bss`,
  `.rodata` → `.rdata`. `.note.GNU-stack` is dropped. `.type`/`.size` become COFF
  `.def`/`.scl`/`.type`/`.endef` or are dropped.
- **Syscalls:** `read(0)` → `ReadFile`, `write(1)` → `WriteFile`,
  `open(2)` → `CreateFileA`, `close(3)` → `CloseHandle`,
  `getpid(39)` → `GetCurrentProcessId`, `exit(60)` → `ExitProcess`. This goes
  through a small runtime layer that maps Linux file descriptors to Windows
  `HANDLE`s and converts return values and error codes (`-errno`).
- **Calling convention:** argument registers `rdi, rsi, rdx, rcx, r8, r9` →
  `rcx, rdx, r8, r9` + stack, 32 bytes of shadow space, 16-byte stack alignment, and
  saving `rsi`/`rdi`/`xmm6–15` (callee-saved on Windows only).
- If a syscall number isn't statically known, or the semantics can't be preserved,
  `l2w` reports an error.
