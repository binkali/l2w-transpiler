use l2w::cli::{self, Command, Mode, Options};
use l2w::diagnostics::{Diagnostic, SourceFile};
use l2w::{ast::StatementKind, codegen, ir, lexer, linker, parser, transpiler};
use std::process::ExitCode;

/// Exit codes: 0 = success, 1 = errors in the input (including assembler or
/// linker errors), 2 = usage, I/O or toolchain error.
const EXIT_INPUT_ERROR: u8 = 1;
const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    match cli::parse_args(std::env::args().skip(1)) {
        Ok(Command::Help) => {
            print!("{}", cli::USAGE);
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("l2w {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Command::Run(opts)) => run(&opts),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

fn run(opts: &Options) -> ExitCode {
    let name = opts.input.display().to_string();
    let text = match std::fs::read_to_string(&opts.input) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: cannot read `{name}`: {e}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let file = SourceFile::new(name, text);
    let verbose = |msg: &str| {
        if opts.verbose {
            eprintln!("[l2w] {msg}");
        }
    };
    verbose(&format!(
        "input: {} ({} bytes), target: {}",
        file.name,
        file.text.len(),
        opts.target
    ));

    if opts.mode == Mode::DumpTokens {
        let lexed = lexer::tokenize(&file.text);
        for t in &lexed.tokens {
            println!("{:>4}:{:<3} {}", t.span.line, t.span.col, t.kind);
        }
        return report(&file, &lexed.diagnostics);
    }

    let parsed = parser::parse(&file.text);
    verbose(&format!("lexed {} tokens", parsed.token_count));
    if opts.verbose {
        let (mut labels, mut instrs, mut dirs) = (0, 0, 0);
        for s in &parsed.program.statements {
            match s.kind {
                StatementKind::Label(_) | StatementKind::NumericLabel(_) => labels += 1,
                StatementKind::Instruction(_) => instrs += 1,
                StatementKind::Directive(_) => dirs += 1,
            }
        }
        verbose(&format!(
            "parsed {} statements: {instrs} instructions, {labels} labels, {dirs} directives",
            parsed.program.statements.len()
        ));
    }
    let status = report(&file, &parsed.diagnostics);
    if parsed.has_errors() {
        return status;
    }

    if opts.mode == Mode::DumpAst {
        println!("{:#?}", parsed.program);
        return ExitCode::SUCCESS;
    }

    let lowered = ir::lower(&parsed.program);
    if opts.verbose {
        let m = &lowered.module;
        verbose(&format!(
            "IR: {} sections, {} symbols, {} functions, {} calls, {} syscalls, {} externals",
            m.sections.len(),
            m.symbols.len(),
            m.functions.len(),
            m.platform.calls.len(),
            m.platform.syscalls.len(),
            m.platform.externals.len()
        ));
    }
    let status = report(&file, &lowered.diagnostics);
    if lowered.has_errors() {
        return status;
    }

    if opts.mode == Mode::DumpIr {
        println!("{}", lowered.module.summary());
        println!("# normalized listing");
        print!("{}", lowered.module);
        return ExitCode::SUCCESS;
    }

    let translation = transpiler::transpile(&lowered.module);
    for note in &translation.notes {
        verbose(note);
    }
    let status = report(&file, &translation.diagnostics);
    if translation.has_errors() {
        return status;
    }

    match opts.mode {
        Mode::Check => {
            eprintln!("{}: ok (translatable to {})", file.name, opts.target);
            ExitCode::SUCCESS
        }
        Mode::Transpile => {
            let asm = codegen::windows::emit(&translation, &file.name);
            match &opts.output {
                Some(path) => {
                    if let Err(e) = std::fs::write(path, &asm) {
                        eprintln!("error: cannot write `{}`: {e}", path.display());
                        return ExitCode::from(EXIT_USAGE);
                    }
                    verbose(&format!("wrote {} ({} bytes)", path.display(), asm.len()));
                }
                None => print!("{asm}"),
            }
            ExitCode::SUCCESS
        }
        Mode::Compile => {
            let exe = opts
                .output
                .clone()
                .expect("--compile always has an output path");
            let plan = match linker::plan(&translation, &opts.libs) {
                Ok(p) => p,
                Err(d) => return report(&file, &[d]),
            };
            let asm = codegen::windows::emit(&translation, &file.name);
            let kept = opts.keep_asm.then(|| exe.with_extension("s"));
            if kept.as_deref() == Some(opts.input.as_path()) {
                eprintln!(
                    "error: --keep-asm would overwrite the input `{}`; choose another -o",
                    opts.input.display()
                );
                return ExitCode::from(EXIT_USAGE);
            }
            match linker::build(&asm, &plan, &exe, kept.as_deref(), &verbose) {
                Ok(()) => {
                    if let Some(k) = &kept {
                        verbose(&format!("kept {}", k.display()));
                    }
                    let size = std::fs::metadata(&exe).map(|m| m.len()).unwrap_or(0);
                    verbose(&format!("wrote {} ({size} bytes)", exe.display()));
                    ExitCode::SUCCESS
                }
                Err(linker::LinkError::Build(d)) => report(&file, &[d]),
                Err(linker::LinkError::Tool(d)) => {
                    eprint!("{}", d.render(&file));
                    ExitCode::from(EXIT_USAGE)
                }
            }
        }
        Mode::DumpTokens | Mode::DumpAst | Mode::DumpIr => unreachable!(),
    }
}

/// Prints diagnostics to stderr and returns the matching exit code.
fn report(file: &SourceFile, diags: &[Diagnostic]) -> ExitCode {
    for d in diags {
        eprint!("{}", d.render(file));
    }
    let errors = diags.iter().filter(|d| d.is_error()).count();
    if errors > 0 {
        eprintln!(
            "error: aborting due to {errors} error{}",
            if errors == 1 { "" } else { "s" }
        );
        ExitCode::from(EXIT_INPUT_ERROR)
    } else {
        ExitCode::SUCCESS
    }
}
