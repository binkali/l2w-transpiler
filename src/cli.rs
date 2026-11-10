//! Command-line argument parsing (dependency-free).
//!
//! ```text
//! l2w input.s -o output.s
//! l2w input.s -o output.s --target windows
//! l2w input.s --compile -o output.exe
//! l2w input.s --check
//! l2w input.s --verbose
//! ```

use std::fmt;
use std::path::PathBuf;

pub const USAGE: &str = "\
l2w - Linux-to-Windows x86-64 assembly transpiler

USAGE:
    l2w <INPUT> [OPTIONS]

ARGS:
    <INPUT>                 Linux x86-64 assembly (GAS, .intel_syntax noprefix)

OPTIONS:
    -o, --output <FILE>     Output file (.s when transpiling, .exe with --compile).
                            Without -o, transpiled assembly is written to stdout.
        --target <TARGET>   Target platform [default: windows] [possible: windows]
        --compile           Transpile, assemble and link into a Windows .exe (MinGW-w64)
        --check             Only parse and analyze the input, report diagnostics
        --dump-tokens       Print the lexer token stream
        --dump-ast          Print the parsed AST (debug format)
        --dump-ir           Print the IR analysis and normalized listing
    -v, --verbose           Print progress information to stderr
    -h, --help              Print this help
    -V, --version           Print version
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Windows,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Target::Windows => "windows",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Write Windows assembly (default).
    Transpile,
    /// Transpile, assemble and link to an executable.
    Compile,
    /// Parse/analyze only.
    Check,
    DumpTokens,
    DumpAst,
    DumpIr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub input: PathBuf,
    pub output: Option<PathBuf>,
    pub target: Target,
    pub mode: Mode,
    pub verbose: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Run(Options),
    Help,
    Version,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError(pub String);

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CliError {}

fn err<T>(msg: impl Into<String>) -> Result<T, CliError> {
    Err(CliError(msg.into()))
}

/// Parses arguments (without the program name).
pub fn parse_args<I, S>(args: I) -> Result<Command, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args = args.into_iter().map(Into::into);
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut target = Target::Windows;
    let mut modes: Vec<(Mode, &'static str)> = Vec::new();
    let mut verbose = false;

    while let Some(arg) = args.next() {
        // Support `--flag=value` as well as `--flag value`.
        let (flag, inline_value) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, CliError> {
            match inline_value.clone().or_else(|| args.next()) {
                Some(v) if !v.is_empty() => Ok(v),
                _ => err(format!("option `{name}` requires a value")),
            }
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "-o" | "--output" => {
                if output.is_some() {
                    return err("option `-o` given more than once");
                }
                output = Some(PathBuf::from(value(&flag)?));
            }
            "--target" => {
                let v = value("--target")?;
                target = match v.to_ascii_lowercase().as_str() {
                    "windows" | "win64" | "x86_64-windows" => Target::Windows,
                    other => {
                        return err(format!(
                            "unsupported target `{other}` (supported targets: windows)"
                        ))
                    }
                };
            }
            "--compile" => modes.push((Mode::Compile, "--compile")),
            "--check" => modes.push((Mode::Check, "--check")),
            "--dump-tokens" => modes.push((Mode::DumpTokens, "--dump-tokens")),
            "--dump-ast" => modes.push((Mode::DumpAst, "--dump-ast")),
            "--dump-ir" => modes.push((Mode::DumpIr, "--dump-ir")),
            "-v" | "--verbose" => verbose = true,
            "-" => return err("reading from stdin is not supported; pass a file path"),
            f if f.starts_with('-') => {
                return err(format!("unknown option `{f}` (see `l2w --help`)"))
            }
            _ => {
                if let Some(prev) = &input {
                    return err(format!(
                        "multiple input files given (`{}` and `{arg}`); only one is supported",
                        prev.display()
                    ));
                }
                input = Some(PathBuf::from(arg));
            }
        }
    }

    let Some(input) = input else {
        return err("no input file given (see `l2w --help`)");
    };
    if modes.len() > 1 {
        let names: Vec<_> = modes.iter().map(|(_, n)| *n).collect();
        return err(format!("conflicting options: {}", names.join(", ")));
    }
    let mode = modes.first().map_or(Mode::Transpile, |(m, _)| *m);
    let inspect_only = matches!(
        mode,
        Mode::Check | Mode::DumpTokens | Mode::DumpAst | Mode::DumpIr
    );
    if inspect_only && output.is_some() {
        return err("`-o` cannot be combined with --check or --dump-*");
    }
    let output = match (mode, output) {
        (Mode::Compile, None) => Some(input.with_extension("exe")),
        (_, o) => o,
    };
    Ok(Command::Run(Options {
        input,
        output,
        target,
        mode,
        verbose,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Options {
        match parse_args(args.iter().copied()).unwrap() {
            Command::Run(o) => o,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn basic_forms() {
        let o = run(&["in.s", "-o", "out.s"]);
        assert_eq!(o.input, PathBuf::from("in.s"));
        assert_eq!(o.output, Some(PathBuf::from("out.s")));
        assert_eq!(o.mode, Mode::Transpile);

        let o = run(&["in.s", "-o", "out.s", "--target", "windows"]);
        assert_eq!(o.target, Target::Windows);
        let o = run(&["in.s", "--target=windows"]);
        assert_eq!(o.target, Target::Windows);

        let o = run(&["in.s", "--compile", "-o", "out.exe"]);
        assert_eq!(o.mode, Mode::Compile);
        assert_eq!(o.output, Some(PathBuf::from("out.exe")));

        let o = run(&["dir/prog.s", "--compile"]);
        assert_eq!(o.output, Some(PathBuf::from("dir/prog.exe")));

        assert_eq!(run(&["in.s", "--check"]).mode, Mode::Check);
        assert!(run(&["in.s", "--verbose"]).verbose);
        assert!(run(&["-v", "in.s"]).verbose);
    }

    #[test]
    fn help_and_version() {
        assert_eq!(parse_args(["--help"]).unwrap(), Command::Help);
        assert_eq!(parse_args(["in.s", "-V"]).unwrap(), Command::Version);
    }

    #[test]
    fn errors() {
        let e = |a: &[&str]| parse_args(a.iter().copied()).unwrap_err().0;
        assert!(e(&[]).contains("no input file"));
        assert!(e(&["a.s", "b.s"]).contains("multiple input files"));
        assert!(e(&["a.s", "--target", "linux"]).contains("unsupported target `linux`"));
        assert!(e(&["a.s", "-o"]).contains("requires a value"));
        assert!(e(&["a.s", "--frobnicate"]).contains("unknown option"));
        assert!(e(&["a.s", "--check", "--compile"]).contains("conflicting options"));
        assert!(e(&["a.s", "--check", "-o", "x"]).contains("cannot be combined"));
    }
}
