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
| 2 | AST → intermediate representation, semantic validation, analysis | ✅ done |
| 3 | Translating ordinary instructions and GAS directives, ELF → COFF sections | ✅ done |
| 4 | Calling-convention compatibility (System V ↔ Windows x64) | ✅ done |
| 5 | Linux system calls → Windows API runtime layer | ✅ done |
| 6 | Windows code generation, cross-compiling with MinGW-w64 | ✅ done |
| 7 | More tests, error handling, documentation | ✅ done |

Everything up to a Windows executable works: translation (`-o`), `--compile`, `--check` (parsing, IR validation and checking that the
code can be translated to Windows), `--dump-tokens`, `--dump-ast` and `--dump-ir` all work.
Calls to external (Windows/CRT) functions and Windows calls into the program (`main`,
exported functions) are bridged, and Linux **system calls** are implemented on top of the
Win32 API. `--compile` assembles and links with MinGW-w64 into a `.exe`.

## Quick start

```bash
cargo build --release
./target/release/l2w examples/hello.s --check            # can it be translated?
./target/release/l2w examples/hello.s -o hello.win.s     # Windows assembly
./target/release/l2w examples/hello.s --compile -v       # hello.exe (needs MinGW-w64)
wine hello.exe                                           # or run it on Windows
```

## Feature matrix

| Area | Supported (translated, semantics preserved) | Rejected with a diagnostic | Documented differences |
|---|---|---|---|
| Syntax | GAS Intel syntax (`noprefix`), labels, numeric labels `1:`/`1b`/`1f`, equates, expressions, comments | AT&T syntax, `.intel_syntax prefix`, macros, `.if`, `.include`, floating-point literals | – |
| Instructions | `mov movzx movsx movsxd lea add sub imul mul idiv div and or xor not neg inc dec cmp test shl/sal shr sar xchg cbw cwde cdqe cwd cdq cqo set<cc> cmov<cc> jmp j<cc> call ret push pop pushfq popfq leave nop syscall`, `lock` prefix | everything else (SSE/AVX, string instructions, `movabs`, …) with the list of supported ones | – |
| Operands | registers (8–64 bit, `ah`–`dh`), immediates (range-checked), all memory forms, `offset`, `rip`-relative | invalid scale/index, size mismatches, ambiguous sizes, `ah` with REX, `fs:`/`gs:` (TLS) | – |
| Addressing | `[sym]` → `[rip + sym]`, `mov r64, offset sym` → `lea`, `@GOTPCREL`/`@PLT` resolved | `[reg + sym]`, `.long sym`, `push offset sym` (32-bit absolute addresses), red zone below `rsp` | – |
| Sections | `.text .data .bss .rodata(.X) .data.rel.ro` → `.text .data .bss .rdata`, `.comm`/`.lcomm` → `.bss`, notes/debug dropped | `.init_array`, `.fini_array`, `.ctors`, TLS (`.tdata`/`.tbss`), `.eh_frame`, hand-written `.got`/`.plt` | – |
| Calls | System V internally; thunks for external calls (≤ 10 integer args, shadow space, alignment); Windows wrappers for exported functions and `main`; `_start` entry wrapper; gcc `-O0` red zone in leaf functions | external symbols as data or address, reading argc/argv in `_start` | indirect calls assume System V targets (warning); no callbacks into Windows code, no struct-by-value or > 10 args; **`long` is 32-bit on Windows (LLP64)**: external C functions with `long` parameters/results (e.g. `printf("%ld")`, `labs`) see only the low 32 bits. Assembly has no types, so this can't be detected. |
| System calls | `read write open close lseek getpid exit exit_group` via Win32 shims (registers + RFLAGS preserved, `-errno`) | unknown syscall numbers, unsupported calls (`mmap`, `fork`, …), untranslatable static `open` flags | `close(0/1/2)` keeps the handle; no `SIGPIPE`; console `\r\n`; absolute paths are relative to the current drive |
| Build | `--compile` with MinGW-w64 (`main` → C runtime, `_start` → `-nostdlib -lkernel32`) | missing/non-Windows compiler (exit 2, install hints), linker errors (exit 1, hints) | – |
| Robustness | – | input that would exhaust resources: expressions nested > 64 levels or with > 1000 operators, equate chains > 100 | – |

## Building

Requires Rust (stable, ≥ 1.75). No external crates are used.

```bash
cargo build --release
./target/release/l2w --help
```

## Usage

```bash
l2w input.s -o output.s                    # translate to Windows assembly (stdout without -o)
l2w input.s -o output.s --target windows   # same; windows is the default
l2w input.s --compile -o output.exe        # translate + assemble + link with MinGW-w64
l2w input.s --compile                      # output: input.exe
l2w input.s --compile -l user32 --keep-asm # extra library; keep output.s next to the .exe
l2w input.s --check                        # parse/analyze only, report diagnostics
l2w input.s --check --verbose              # with progress information
l2w input.s --dump-tokens                  # print the lexer's token stream
l2w input.s --dump-ast                     # print the parsed AST
l2w input.s --dump-ir                      # print IR analysis + normalized listing
```

Exit codes: `0` success, `1` errors in the input (including assembler/linker errors such as
undefined references), `2` usage, I/O or toolchain error (MinGW-w64 missing).

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
  = help: supported directives: .intel_syntax .text .data .bss .section ...
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
├── ir.rs                    IR types + normalized listing printer
├── ir/lower.rs              AST → IR: semantics, validation, sizes
├── ir/analysis.rs           functions, calls, syscalls, register values
├── transpiler.rs            Linux → Windows passes (sections, addressing, data)
├── transpiler/bridge.rs     calling-convention bridging pass
├── transpiler/syscalls.rs   system call translation pass
├── codegen/{mod,windows}.rs Windows/COFF GAS emitter
├── compatibility/
│   ├── syscalls.rs          Linux syscalls → Win32 API shims, fd table
│   ├── calling_convention.rs System V ↔ Windows x64 ABI, thunks, wrappers
│   └── directives.rs        ELF → COFF section mapping
└── linker.rs                MinGW-w64 driver: discovery, entry point, link command
tests/
├── cli.rs                   end-to-end tests of the binary
├── roundtrip.rs             AST/IR → listing → `as` object comparison
└── windows.rs               MinGW assembly + behavioral equivalence
examples/                    sample programs (library, hello, hello_world, loop, functions, errors, errors_semantic)
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

## Intermediate representation (phase 2)

`ir::lower` turns the AST into an `ir::Module`. The module has two separate layers:

| Layer | Contents |
|---|---|
| **Architecture** | `Section`s (ELF name, classified as code / data / bss / read-only / note) holding `Item`s: labels, validated `Instr`s, data (`Values`, `Bytes`, `Fill`), `Align` (normalized to bytes), `Set` (equates, kept in place because `.` depends on position), `Common`. `Chunk`s keep the original order in which sections were entered. |
| **Symbols / structure** | `SymbolTable` (labels, equates with value class, commons, undefined = external; `.globl`, `.extern`, `.type`, `.size`), `Function`s |
| **Platform (Linux/ELF)** | entry point (`_start` → raw, `main` → C runtime), **syscall sites** with statically known register values, **call sites** (internal / external / indirect), externals, ELF relocation modifiers (`@PLT`, `@GOTPCREL`), `fs:`/`gs:` accesses, number of dropped metadata directives |

### Operand semantics (exactly as GNU as decides them)

I checked each of these rules against GNU as.

| Source | IR |
|---|---|
| `jmp L`, `jne 1b`, `call f` | `Target` |
| `5`, `'A'`, `mov rdx, len` where `len` is an **absolute equate defined earlier** | `Imm` |
| `offset len` (absolute equate, defined anywhere) | `Imm` |
| `offset msg` (address) | `Addr` |
| `mov rax, msg` (bare symbol) | `Mem` (absolute `[msg]`) |
| `mov rdx, len` where `len` is defined **later** | `Mem` + **warning**. GAS really does turn this into a memory read from address `len`. |

When the listing is printed, symbolic immediates get `offset` (`mov rdx, offset len`). That
way they stay immediates however the code is reordered later. GAS produces the identical
encoding for both forms.

### Validation

- **Instructions:** `mov movzx movsx movsxd lea add sub imul mul idiv div and or xor not neg inc dec cmp test shl/sal shr sar xchg cbw cwde cdqe cwd cdq cqo set<cc> cmov<cc> jmp j<cc> call ret push pop pushfq popfq leave nop syscall`.
  All condition-code aliases are accepted and normalized (`jz` → `je`, `setnz` → `setne`,
  `cmovnle` → `cmovg`, …). Anything else is an error (`unsupported instruction`), with line,
  column and the list of supported instructions.
- **Operand forms**, per instruction: arity; register/memory/immediate allowed or not; no
  memory-to-memory operands; `lea` needs memory; `jcc` needs a label; shift count must be an
  immediate or `cl`; `movzx`/`movsx` source sizes; `push`/`pop` only 64/16-bit; `setcc` takes
  an 8-bit register or memory operand (a memory operand defaults to `byte ptr`, as in GAS);
  `cmovcc` needs a 16/32/64-bit register destination and a source of the same size.
- **Operand sizes:** inferred from registers (`mov [rbx], ecx` → `dword ptr`); conflicts
  (`mov rax, ebx`) and ambiguous sizes (`inc [rdi]`, `mov [rax], 1`) are errors.
  `push`/`pop`/`jmp`/`call [mem]` default to `qword`, `movsxd` to `dword`, just like GAS.
- **Immediates:** range-checked per operand size. 64-bit operations take only
  sign-extended imm32 (`add rax, 0xFFFFFFFF` → error); `mov r64, imm64` is allowed. Equate
  values are evaluated where possible (`k = 300` / `mov bl, k` → error).
- **Encoding limits:** `ah/bh/ch/dh` with REX registers; `lock` only for read-modify-write
  instructions with a memory destination.
- **Symbols and data:** duplicate definitions, self-referencing equates, initialized data in
  `.bss`, alignment that isn't a power of two, data that doesn't fit (warning).
- **Numeric labels** (`1:` / `1b` / `1f`) are resolved to unique `.Ll2w_<n>_<k>` labels; a
  missing target is an error.
- **Reserved register names:** GAS reserves `k0–k7`, `st`, `mm*`, `tmm*`, `xmm16+`,
  `ymm*`, `zmm*`, `cr*`, `dr*`. Using one as a symbol is an error, because GAS would read it
  as a register.

### Analysis

- **Functions:** a label in a code section starts a function if it is the entry point, has
  `.type @function`, is global, or is the target of a direct `call`. It runs until the next
  function label. Local labels such as `.Lloop` stay inside the function.
- **Register values at `syscall`:** a conservative backward scan within the basic block. It
  understands `mov` (immediate, `offset`, copying from a register, 32-bit zero-extension),
  `xor/sub reg, reg`, `lea [rip + sym]`, and `add/sub/inc/dec` with constants. It stops at
  labels, `call`, `jmp`/`ret` and partial (8/16-bit) writes, including `setcc`; a `cmovcc`
  makes its destination unknown. Across a `syscall`, only `rax`, `rcx` and `r11` are treated
  as clobbered; on the fall-through path of a `jcc`, earlier values stay valid.

```text
$ l2w examples/hello.s --dump-ir
...
platform (linux/elf):
  entry: _start (Start)
  syscall line 15   rax=1 (write) rdi=1 rsi=&msg rdx=len in _start
  syscall line 19   rax=60 (exit) rdi=0 rsi=&msg rdx=len in _start
```

## Windows translation (phase 3)

```bash
l2w examples/library.s -o library.win.s --verbose
x86_64-w64-mingw32-as library.win.s -o library.obj
```

### Sections

| ELF | COFF |
|---|---|
| `.text`, `.text.X` | `.text`, `.text$X` (PE grouped section, merged into `.text` by the linker) |
| `.data`, `.data.X` | `.data`, `.data$X` |
| `.bss`, `.bss.X` | `.bss`, `.bss$X` |
| `.rodata`, `.rodata.X`, `.data.rel.ro*` | `.section .rdata,"dr"`, `.rdata$X` |
| `.note*` (e.g. `.note.GNU-stack`), `.comment`, `.debug*`, non-allocated sections | dropped (an error if a symbol in them is referenced) |
| `.init_array`, `.fini_array`, `.ctors`, `.dtors`, `.tdata`, `.tbss`, `.eh_frame`, `.got`, `.plt` | **error** |

### Symbols and directives

- `.globl` is kept. Every function gets a COFF symbol record
  `.def f; .scl 2; .type 32; .endef` (`.scl 3` for local functions).
- `.type`, `.size`, `.local`, `.file`, `.ident` and `.cfi_*` are ELF-only and are dropped.
- `.comm`/`.lcomm` become explicit, aligned definitions in `.bss` (global for `.comm`).
- Windows x64 adds no leading underscore to symbol names, so names stay unchanged.

### Addressing: why rewrites are needed

A Windows x64 image is loaded **above 4 GiB** (MinGW default image base `0x140000000`, plus
ASLR). Linux non-PIE code, however, often uses 32-bit absolute addresses
(`R_X86_64_32S`). I checked with MinGW `as`: those relocations would be emitted and would
then fail at link time. `l2w` therefore rewrites them into equivalent forms:

| Linux | Windows | Why it's equivalent |
|---|---|---|
| `mov rax, counter` / `mov rax, [counter]` | `mov rax, qword ptr [rip + counter]` | same address, encoded relative to `rip` |
| `mov rsi, offset msg` | `lea rsi, [rip + msg]` | same 64-bit value; neither instruction changes flags |
| `mov rax, [rip + v@GOTPCREL]` | `lea rax, [rip + v]` | the GOT slot holds exactly the address of `v` |
| `call f@PLT` | `call f` | direct call |
| `.quad sym`, `.long sym - .` | unchanged | 64-bit absolute / PC-relative relocation |

These are **rejected** because they can't be translated without changing semantics or
using extra registers:

- `mov eax, offset sym`, `push offset sym`, `add rax, offset sym` (32-bit absolute address);
- `[rbx + sym]`, `[sym + rcx*8]` (symbol address plus registers); the error message suggests
  `lea r11, [rip + sym]`;
- `.long sym`, `.word sym` (absolute address narrower than 64 bits);
- `fs:`/`gs:` accesses (Linux TLS and stack protector; on Windows `gs:` points to the TEB);
- accesses below `rsp` (`[rsp - 8]`): Windows has no System V red zone;
- other ELF relocations (`@GOT`, `@TPOFF`, `@GOTOFF`, …).

Ordinary instructions are copied unchanged, because they mean the same on both systems.
Constants stay constants (`mov rdx, offset len` for an absolute equate).

### Verification

`tests/windows.rs` checks every translation with real tools:

1. **MinGW-w64 `as`** assembles the output into a `pe-x86-64` object that contains **no**
   32-bit absolute relocations.
2. **Behavioral equivalence.** Phase 3 doesn't touch the calling convention yet, so the
   translated functions are still System V internally. The test reassembles the
   *translated* code for Linux (mapping back only the COFF section directives), calls it
   from a C harness, and requires output identical to the original code. This covers
   lookups, loops, data pointers, `offset`, `@GOTPCREL`, `@PLT`, commons and numeric labels.

## Calling conventions (phase 4)

| | System V (Linux) | Windows x64 |
|---|---|---|
| integer arguments | rdi, rsi, rdx, rcx, r8, r9, then stack | rcx, rdx, r8, r9, then stack |
| stack arguments at entry | `[rsp + 8]`, … | `[rsp + 40]`, … (after 32 bytes of shadow space) |
| shadow space | – | 32 bytes, reserved by the caller |
| red zone | 128 bytes below rsp | none |
| alignment before `call` | rsp ≡ 0 (mod 16) | rsp ≡ 0 (mod 16) |
| callee-saved | rbx, rbp, r12–r15 | rbx, rbp, **rsi, rdi**, r12–r15, **xmm6–15** |
| system calls | `syscall`, rax = number, args in rdi, rsi, rdx, **r10**, r8, r9 | not a stable ABI → Win32 API (see below) |

### Strategy: System V inside, bridges at the boundaries

Translated code keeps the System V convention **internally**. Calls between translated
functions don't change and stay correct. Code is converted only where it crosses between
the two conventions:

| Boundary | Generated code |
|---|---|
| `call puts` / `jmp puts` (tail call) to an **external** function | `call __l2w_thunk_puts`. The thunk moves arguments rdi, rsi, rdx, rcx → rcx, rdx, r8, r9; puts r8, r9 and stack arguments 7–10 into Windows stack slots 5–10; reserves the 32-byte shadow space; forces `and rsp, -16`. Windows callees preserve a superset of the System V callee-saved registers, so nothing else needs saving. |
| **exported** functions (`.globl`) and `main` (called by the CRT) | The public name becomes a **wrapper** with the Windows convention. It maps arguments back (incl. stack arguments 5–10), saves `rsi`/`rdi`, and calls the System V body `__l2w_sysv_<name>`. Internal calls, jumps and pointers (`.quad f`) are redirected to the body. |
| `_start` | Entry wrapper `sub rsp, 8; jmp __l2w_sysv__start`: Windows enters the entry point with rsp ≡ 8, Linux with rsp ≡ 0 (mod 16). Reading `argc/argv` from `[rsp]` or `pop` in `_start` is an **error**. |

The argument count is unknown without type information, so every bridge passes on **10
integer arguments** (6 registers + 4 stack slots). Extra arguments are harmless in both
conventions, because the caller owns the argument area. Floating-point arguments can't occur,
since SSE isn't supported. For the same reason translated code never changes `xmm6–15`,
so wrappers don't have to save them.

### Red zone

gcc `-O0` leaf functions keep locals below `rsp` (`push rbp; mov rbp, rsp;
mov [rbp-4], edi; …; pop rbp; ret`). On Windows, anything below `rsp` is volatile. When
`rsp == rbp` holds throughout the function (no other stack operations), `l2w` inserts
`sub rsp, N` after `mov rbp, rsp` (N = depth rounded up to 16) and replaces `pop rbp`
with `leave`. Since `rsp == rbp` at that point, `leave` is identical to `pop rbp`.
Other red-zone use is an **error**, with a hint.

### Rejected / warnings

- External symbols used as **data** or an **address** (`mov rax, [rip + stdout]`,
  `lea rdi, [rip + puts]`, `.quad malloc`): error. Windows imports these through `__imp_`
  pointers, and a Windows function called through a pointer expects the Windows convention.
- **Indirect calls** (`call rax`): warning. The target is assumed to be a System V function
  of the program.
- **Data model:** Linux is LP64, Windows is LLP64, so `long` is 64 bits on Linux and 32 bits on
  Windows. Registers are passed through unchanged, but a Windows C function declared with
  `long` only uses the low 32 bits (`printf("%ld")` prints `int`s). The assembly contains no
  type information, so `l2w` can't detect this. Use `long long`/`int64_t`/`%lld` functions
  for 64-bit values.
- Not handled: passing the address of a translated function to Windows code as a callback
  (e.g. for `qsort`), structs passed by value, functions with more than 10 arguments, and
  128-bit return values (rdx:rax).

### Verification on Linux, without Windows

gcc's `__attribute__((ms_abi))` generates Windows-convention calls and functions on Linux.
`tests/windows.rs` uses that to test the bridges by running them:

- the C harness calls the translated exports **in the Windows convention**, including
  `sum10` with 10 arguments (stack arguments);
- `call_ext10` calls an external function with 10 arguments that is written in Windows
  convention. That function **overwrites its shadow space** (this would crash without the
  space reserved by the thunk), checks the **stack alignment**, and clobbers r10/r11. The
  callee-saved `rbx` must survive;
- a probe calls `clobber` (which modifies rsi/rdi, as System V allows) in Windows
  convention and checks that **rsi/rdi are preserved**;
- also tested: the red-zone function `rz_add`, the tail call `jmp ext_twice`, and all
  phase 3 functions.

All outputs must match the original build. A deliberately broken thunk (no shadow space) is
caught by the test. On top of that, gcc `-O0` output that uses `printf` can be translated
and linked with `x86_64-w64-mingw32-gcc` into a working `PE32+` executable.

## System calls (phase 5)

Windows has no stable system-call interface. `l2w` replaces every `syscall` with a call to a
**shim** that implements the Linux call using the Win32 API (kernel32 only, no C runtime):

| Linux | rax | Win32 |
|---|---|---|
| `read(fd, buf, count)` | 0 | `ReadFile` |
| `write(fd, buf, count)` | 1 | `WriteFile` (+ `SetFilePointerEx` for `O_APPEND`) |
| `open(path, flags, mode)` | 2 | `CreateFileA` |
| `close(fd)` | 3 | `CloseHandle` |
| `lseek(fd, offset, whence)` | 8 | `SetFilePointerEx` (`SEEK_SET/CUR/END` = `FILE_BEGIN/CURRENT/END`; other `whence` values → `-EINVAL`) |
| `getpid()` | 39 | `GetCurrentProcessId` |
| `exit(status)`, `exit_group(status)` | 60, 231 | `ExitProcess(status & 0xff)` |

The system call number must be **statically known**. `l2w` tracks register values backwards
within the basic block (`mov eax, 1` … `syscall`). An unknown or unsupported number is an
**error** at the `syscall` line, which lists the supported calls. Statically known `open`
flags that can't be translated (e.g. `O_DIRECTORY`, `O_NONBLOCK`) are also an error; unknown
flags are checked at run time (`-EINVAL`).

### The shim contract: exactly like `syscall`

```asm
    lea rsp, [rsp - 128]        # skip the System V red zone (lea keeps the flags)
    call __l2w_sys_write
    lea rsp, [rsp + 128]
```

- Arguments in rdi, rsi, rdx; result in rax as a value or `-errno`.
- **All other registers and RFLAGS are preserved**, as with Linux `syscall`. Only rcx and
  r11 are clobbered. The shims save rdx, r8–r10 (volatile on Windows) and RFLAGS
  (`pushfq`/`popfq`). rsi/rdi are preserved by Windows itself.
- Data below rsp (red zone) stays untouched. The shim aligns the stack to 16 bytes and
  reserves shadow space and stack arguments.

### File descriptors and errors

- A table maps Linux fds (0–255) to Windows `HANDLE`s with flags. fds 0/1/2 are
  `GetStdHandle(STD_INPUT/OUTPUT/ERROR_HANDLE)`, fetched on first use.
- `open` returns the **lowest free fd**, like Linux (the first file is 3). Flags map to
  access (`GENERIC_READ`/`WRITE`) and disposition: `O_CREAT|O_EXCL` → `CREATE_NEW`,
  `O_CREAT|O_TRUNC` → `CREATE_ALWAYS`, `O_CREAT` → `OPEN_ALWAYS`, `O_TRUNC` →
  `TRUNCATE_EXISTING`, otherwise `OPEN_EXISTING`. Sharing is `READ|WRITE|DELETE`, since
  Linux has no mandatory locks. With `O_CREAT`, a `mode` without owner write permission
  gives `FILE_ATTRIBUTE_READONLY`.
- `O_APPEND`: every `write` seeks to the end first.
- `read` on a write-only fd, `write` on a read-only fd, and any access to a closed or
  invalid fd → `-EBADF`, like Linux.
- Win32 errors (`GetLastError`) → `-errno`: `ERROR_FILE_NOT_FOUND`/`PATH_NOT_FOUND` →
  `ENOENT`, `ACCESS_DENIED` → `EACCES`, `INVALID_HANDLE` → `EBADF`,
  `FILE_EXISTS`/`ALREADY_EXISTS` → `EEXIST`, `TOO_MANY_OPEN_FILES` → `EMFILE`,
  `INVALID_PARAMETER`/`INVALID_NAME`/`NEGATIVE_SEEK` → `EINVAL`, `INVALID_FUNCTION` →
  `ESPIPE` (seeking a pipe or console), `BROKEN_PIPE`/`NO_DATA` → `EPIPE`, `DISK_FULL` →
  `ENOSPC`, `FILENAME_EXCED_RANGE` → `ENAMETOOLONG`, `NOACCESS` → `EFAULT`, everything else
  → `EIO`. A broken pipe on `read` is end of file (0).
- `read`/`write` transfer at most `0x7ffff000` bytes per call, like Linux.

### Remaining differences (by design, documented)

- `close(0/1/2)` frees only the fd, not the Windows handle (stdout and stderr are often the
  **same** console handle on Windows).
- `write` to a broken pipe returns `-EPIPE`, but there is no `SIGPIPE` that would terminate
  the process.
- `lseek` on a pipe returns `-ESPIPE` like Linux; on a console handle the errno depends on
  the Win32 error code Windows reports (`ERROR_INVALID_FUNCTION` → `-ESPIPE`, other codes as
  mapped above).
- The console delivers `\r\n` line endings on `read`. Paths are passed to `CreateFileA` as
  they are: `/` works as a separator, but absolute Linux paths like `/tmp/x` refer to the
  root of the current drive.
- The symbol prefix `__l2w_` is reserved. A program must not define the imported Win32 names
  (`WriteFile`, …) itself.

### Verification

`tests/windows.rs` links the translated programs on Linux against a **Win32 mock**: a C
implementation (`ms_abi`) of the kernel32 functions above on top of POSIX. Every mock
function checks the **stack alignment** of the call, clobbers all volatile registers, and
writes its arguments into the caller's **shadow space** (gcc `-O0`). `examples/hello.s`,
`loop.s`, `functions.s`, and a file-I/O test program must produce the **same stdout and the
same exit status** as the original Linux binary. The file-I/O program covers lowest-free fd,
`O_TRUNC`, `O_APPEND`, `O_EXCL` → `EEXIST`, `ENOENT`, `EBADF` for wrong access mode or a
closed fd, `lseek` with all three `whence` values plus `-EINVAL` (negative position, bad
`whence`), `-ESPIPE` (stdout pipe) and `-EBADF`, register and flag preservation across
`syscall`, and `exit(384)` → status 128.
Deliberately broken shims (no `pushfq`/`popfq`, no call frame) are caught by the test.
The examples also link with MinGW into PE executables that import only `KERNEL32.dll`.

### Not supported

See also the [feature matrix](#feature-matrix).

- AT&T syntax and `.intel_syntax prefix` (`%rax`)
- Macros (`.macro`, `.rept`, `.irp`), conditional assembly (`.if`), `.include`
- Subsections, `.weak`, `.pushsection`/`.popsection`
- Floating-point literals
- `ymm`/`zmm`, mask and control registers
- SSE/AVX, string instructions (`movs`, `stos`, `rep`), `movabs`,
  `.set` redefinition, and `.lcomm` with an alignment argument (GAS/ELF doesn't accept it)

## Tests

```bash
cargo test                          # everything (~100 tests, a few seconds)
cargo test --test spec              # the specification test matrix
L2W_FUZZ_ITERATIONS=60000 cargo test --release --test robustness   # longer fuzzing
L2W_BLESS=1 cargo test --test spec golden   # regenerate the expected diagnostics
```

Tests that need external tools skip themselves with a message if the tool is missing, and
they check that the tool is the *right* one (on macOS, `as`/`gcc` are LLVM tools that don't
understand GAS syntax):

| Tests | Requirement | Found as |
|---|---|---|
| `roundtrip` | GNU `as`/`objdump` for x86-64 ELF | `as`, `x86_64-linux-gnu-as`, `x86_64-elf-as` (macOS: `brew install x86_64-elf-binutils`) |
| behavioral (`windows`, `spec`) | an **x86-64 Linux host** with `gcc` (the original Linux program is executed) | `gcc`, `x86_64-linux-gnu-gcc` |
| `--compile`, MinGW object checks | MinGW-w64 | `x86_64-w64-mingw32-gcc` (macOS: `brew install mingw-w64`) |
| running `.exe` files | Wine | `wine` |

**With Wine installed**, every behavioral test also builds the translation as a real `.exe`
(MinGW-w64, `-nostdlib -lkernel32`) and runs it under Wine, i.e. against the real
kernel32 implementation (PE loader, `CreateFileA`, `WriteFile`, Windows error codes). Its
output and exit status must match the Linux original as well: Linux original =
translation on the Win32 mock = translated `.exe` under Wine.

| Suite | What it shows |
|---|---|
| unit tests (`src/**`) | lexer, parser, IR lowering and validation, analysis, every transpiler pass, shim generation, link plans, CLI parsing, diagnostic rendering |
| `tests/roundtrip.rs` | AST and IR listings assemble **byte-identically** to the original with GNU `as`: the front end understands the source exactly like GAS |
| `tests/windows.rs` | MinGW assembles every translation without 32-bit absolute relocations; the translated code **behaves like the original** (C harness calls the exports with `ms_abi`; 10-argument calls both ways; rsi/rdi preservation; syscall shims against the Win32 mock) |
| `tests/spec.rs` | the test matrix of the specification (below) |
| `tests/cli.rs` | the binary end to end: all modes, exit codes, error positions, `--compile` (PE header, machine, imports; Wine run when available), toolchain errors |
| `tests/robustness.rs` | mutation fuzzing of the examples (3,000 mutants by default) and pathological inputs: no stage panics, every diagnostic position is valid |

### Specification test matrix (`tests/spec.rs`)

Each behavioral test builds its program up to three times: the **original** as a static
Linux executable; the **translation** linked against the Win32 mock, with external C functions
compiled with `ms_abi`, i.e. with real Windows calling-convention semantics; and, if Wine is
available, the translation as a **real Windows `.exe` under Wine**. All of them must produce
**the same stdout and exit status**.

| # | Requirement | Test | Covers |
|---|---|---|---|
| 1 | arithmetic with `mov`, `add`, `sub` | `spec_01_arithmetic` | also `imul idiv div and or xor not neg inc dec sar shr shl lea xchg`, 32-bit zero extension, 8-bit writes, overflow |
| 2 | conditions with `cmp` and `je` | `spec_02_conditions` | 20 `jcc` aliases (`je/jz … ja/jnbe, jb, js`) against a reference model, signed and unsigned edge values, `test`; the same masks computed branch-free with `setcc`/`cmovcc` |
| 3 | loops with labels and `jmp` | `spec_03_loops` | top-tested, nested, `dec/jnz`, numeric labels `1b`/`1f` |
| 4 | string output with `write` | `spec_04_write` | return values, partial and zero-length writes, invalid fd → `-EBADF` |
| 5 | program exit with `exit` | `spec_05_exit` | `exit`/`exit_group`, status 0…300 and −1 (truncated to 8 bits) |
| 6 | function calls, different argument counts | `spec_06_function_calls` | external calls with 0–10 arguments (weighted sums detect swapped arguments), `int` arguments, internal calls with stack arguments, recursion, tail call into an external function, callee-saved registers |
| 7 | `.data` and `.bss` | `spec_07_data_and_bss` | arrays, 8/16/32/64-bit data, read-modify-write, zeroed `.bss`, `.comm`, pointers in data, `.balign 64`, equates |
| 8 | faulty or unsupported instructions | `spec_08_diagnostics`, `spec_08_golden_diagnostics` | 17 cases with **exact line and column**; complete `--check` output of `examples/errors*.s` against `tests/expected/*.stderr` |
| 9 | stack alignment, shadow space | `spec_09_alignment_and_shadow_space`, `spec_09_red_zone_is_rejected` | external calls and syscalls from **every** stack alignment (the callee checks 16-byte alignment); callee overwrites its shadow space while caller data stays intact; red-zone use is rejected |
| 10 | Windows codegen, cross-compiling | `spec_10_cross_compilation` | `--compile` produces PE executables (run under Wine when available) |

### Error handling

- Every error has the **line and column** of the construct that caused it, rustc-style
  rendering with carets, and usually a `help:` line. Global errors (no source position) are
  used only when there is no single position, e.g. a missing entry point.
- Errors don't stop the analysis: all errors in a file are reported together. Translation
  and code generation only run if there were no errors.
- `l2w` never panics on input. Recursion is bounded (expression depth and size, equate
  chains), and fuzzing checks that every diagnostic position lies inside the file.
- Exit codes: `0` ok, `1` errors in the input (including assembler/linker errors),
  `2` usage, I/O, or toolchain errors.

## Building executables (phase 6)

`--compile` writes the Windows assembly to a temporary file (or, with `--keep-asm`, to
`<output>.s`) and calls the MinGW-w64 GCC driver:

| Entry point | Command |
|---|---|
| `main` (C runtime) | `x86_64-w64-mingw32-gcc prog.s -o prog.exe` |
| `_start` | `x86_64-w64-mingw32-gcc -nostdlib -e _start prog.s -o prog.exe [-lmsvcrt] -lkernel32` |

- `_start` programs get **no** C runtime startup. The system-call shims only need
  kernel32. If the program calls external functions (e.g. `puts`), `msvcrt` is linked
  too. `msvcrt.dll` initializes itself when it is loaded.
- No entry point (`_start` or `main`) → error with a hint (`-o out.s` for libraries).
- **Compiler discovery:** `$L2W_CC`, otherwise `x86_64-w64-mingw32-gcc` (on Windows also
  `gcc`). Each candidate is checked with `-dumpmachine`: a compiler that doesn't target
  `x86_64-*-mingw*`/`windows` (e.g. the host `gcc` on Linux) is **refused** instead of
  silently building a Linux binary. If none is found: exit code 2 with installation hints
  for Debian/Ubuntu/Kali, Fedora, Arch, macOS, and MSYS2.
- **Linker errors** are passed on. For `undefined reference to `x'`, `l2w` adds a hint
  saying which libraries were linked and that `-l <name>` adds more.
- `--verbose` shows the compiler found, the full command, and the size of the `.exe`.

`tests/cli.rs` builds `hello`, `loop`, `functions`, and a gcc `-O0` C program that uses
`printf`. It checks the PE header (`MZ`, `PE\0\0`, machine AMD64) and the imports
(`_start` programs: **only `KERNEL32.dll`**). If Wine is installed, the test also runs the
`.exe` files and compares their output. It also tests a missing compiler, a host `gcc`
given via `L2W_CC`, an undefined symbol, and a missing entry point.

## Possible extensions

More instructions (string instructions, `movabs`, SSE with `xmm6–15` saving in the
wrappers), more system calls (`fstat`, `mmap` → `VirtualAlloc`, `brk`), callbacks
(Windows → System V thunks for function pointers that are passed to Windows code), and
`.weak`/macros in the front end.
