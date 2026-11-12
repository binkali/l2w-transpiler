//! LP64 (Linux) -> LLP64 (Windows) data-model bridging for C library calls.
//!
//! `long` is 64 bits on Linux and 32 bits on Windows. Assembly carries no
//! types: a C `long` is just a 64-bit register, and the bridges already pass
//! every register at full width. The bits are lost *inside* the Windows C
//! runtime, whose functions were compiled with a 32-bit `long` (`labs` reads
//! `ecx`, `printf("%ld")` fetches 4 bytes). That code cannot be changed, so
//! l2w fixes the known cases at the boundary, in the System V -> Windows
//! thunk:
//!
//! * functions with a 64-bit counterpart in the Windows C runtime are
//!   called under that name ([`replacement`]: `labs` -> `_abs64`, ...);
//! * `printf`/`scanf`-family functions get a thunk that first copies the
//!   format string to the stack and rewrites the length modifiers of
//!   integer conversions to `I64` (`%ld` -> `%I64d`, also `ll`, `z`, `j`,
//!   `t`) — see [`format_thunk`]. The variadic values themselves are
//!   already forwarded at full width;
//! * other functions known to depend on `long` or on Linux-only layouts
//!   produce a warning ([`unbridged`]).
//!
//! Functions l2w does not know remain a limitation: their signatures are
//! not visible in assembly.

use super::calling_convention::{thunk_name, FORWARDED_ARGS, SHADOW_SPACE, SYSV_ARGS, WIN64_ARGS};
use crate::ast::symbol_name;

/// Linux functions with a `long`/`off_t` parameter or result and the
/// Windows C runtime function with the same semantics on 64-bit values
/// (exported by both `msvcrt.dll` and the UCRT).
const REPLACEMENTS: &[(&str, &str)] = &[
    ("labs", "_abs64"),
    ("llabs", "_abs64"),
    ("atol", "_atoi64"),
    ("atoll", "_atoi64"),
    ("strtol", "_strtoi64"),
    ("strtoll", "_strtoi64"),
    ("strtoul", "_strtoui64"),
    ("strtoull", "_strtoui64"),
    ("wcstol", "_wcstoi64"),
    ("wcstoul", "_wcstoui64"),
    ("ftell", "_ftelli64"),
    ("ftello", "_ftelli64"),
    ("fseek", "_fseeki64"),
    ("fseeko", "_fseeki64"),
    ("lseek", "_lseeki64"),
];

/// Standard name of a glibc alias: C23/C99 headers redirect `strtol`,
/// `sscanf`, ... to `__isoc23_strtol`, `__isoc99_sscanf`, ..., which do not
/// exist on Windows.
pub fn canonical_name(f: &str) -> &str {
    ["__isoc23_", "__isoc99_"]
        .iter()
        .find_map(|p| f.strip_prefix(p))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(f)
}

/// Windows replacement for the external function `f` (a standard name, see
/// [`canonical_name`]), if `f` uses `long`.
pub fn replacement(f: &str) -> Option<&'static str> {
    REPLACEMENTS.iter().find(|(l, _)| *l == f).map(|(_, w)| *w)
}

/// `printf`/`scanf`-family functions and the index of their format argument.
const FORMAT_FUNCTIONS: &[(&str, usize)] = &[
    ("printf", 0),
    ("scanf", 0),
    ("fprintf", 1),
    ("sprintf", 1),
    ("fscanf", 1),
    ("sscanf", 1),
    ("snprintf", 2),
];

/// Index of the format argument if `f` takes a `printf`/`scanf` format.
pub fn format_argument(f: &str) -> Option<usize> {
    FORMAT_FUNCTIONS
        .iter()
        .find(|(n, _)| *n == f)
        .map(|(_, k)| *k)
}

/// Known functions whose `long`/layout dependence l2w cannot bridge, with
/// the reason (reported as a warning).
const UNBRIDGED: &[(&str, &str)] = &[
    ("ldiv", "returns `ldiv_t`, a struct of two `long`s (16 bytes on Linux, 8 on Windows)"),
    ("lldiv", "returns a 16-byte struct, which System V returns in rdx:rax and Windows through a hidden pointer"),
    ("gettimeofday", "`struct timeval` has `long` fields (16 bytes on Linux, 8 on Windows)"),
    ("vprintf", "takes a `va_list`, whose System V layout differs from Windows"),
    ("vfprintf", "takes a `va_list`, whose System V layout differs from Windows"),
    ("vsprintf", "takes a `va_list`, whose System V layout differs from Windows"),
    ("vsnprintf", "takes a `va_list`, whose System V layout differs from Windows"),
    ("vscanf", "takes a `va_list`, whose System V layout differs from Windows"),
    ("vsscanf", "takes a `va_list`, whose System V layout differs from Windows"),
    ("vfscanf", "takes a `va_list`, whose System V layout differs from Windows"),
];

/// Reason why calls to `f` are not correct on Windows, if `f` is known
/// to depend on the Linux data model in a way l2w cannot bridge.
pub fn unbridged(f: &str) -> Option<&'static str> {
    UNBRIDGED.iter().find(|(n, _)| *n == f).map(|(_, r)| *r)
}

/// Size of the stack buffer for rewritten format strings. Longer formats
/// are passed through unchanged.
pub const FORMAT_BUFFER: u32 = 512;

/// System V -> Windows thunk for the `printf`/`scanf`-family function `f`
/// whose format is argument `fmt_arg`: rewrites the format into a stack
/// buffer, then calls the Windows function `target` exactly like
/// [`super::calling_convention::sysv_to_win_thunk`].
///
/// Rewriting, inside a conversion specification (after `%`, skipping
/// flags, width, precision and `$`): `l`, `ll`, `z`, `j`, `t` before
/// `d i o u x X n` become `I64`; before any other conversion (`%lc`, `%ls`,
/// `%lf`) the modifier is kept. `%%` is copied unchanged.
///
/// Only rax, r10 and r11 are used as scratch registers (none of them carries
/// an argument), so the argument registers reach the Windows call intact.
pub fn format_thunk(f: &str, target: &str, fmt_arg: usize) -> String {
    assert!(fmt_arg < WIN64_ARGS.len().min(SYSV_ARGS.len()));
    let call_frame = SHADOW_SPACE + 8 * (FORWARDED_ARGS - WIN64_ARGS.len() as u32); // 80
    let frame = call_frame + FORMAT_BUFFER; // multiple of 16
    let fmt = SYSV_ARGS[fmt_arg];
    let thunk = thunk_name(f);
    let l = |s: &str| symbol_name(&format!("{thunk}.{s}")).into_owned();
    let (copy, spec, ell, one, wide, keep, done, call) = (
        l("copy"),
        l("spec"),
        l("ell"),
        l("one"),
        l("wide"),
        l("keep"),
        l("done"),
        l("call"),
    );
    let buf = call_frame;
    // The longest expansion per input byte is `l` -> `I64` (+2), so 4 free
    // bytes are always enough for one step (including the terminator).
    let limit = buf + FORMAT_BUFFER - 4;
    let target = symbol_name(target);
    let name = symbol_name(&thunk);
    let mut s = format!(
        "{name}:\n\
         \x20   push rbp\n\
         \x20   mov rbp, rsp\n\
         \x20   sub rsp, {frame}\n\
         \x20   and rsp, -16\n\
         \x20   mov rax, {fmt}\n\
         \x20   test rax, rax\n\
         \x20   jz {call}\n\
         \x20   lea r10, [rsp + {buf}]\n\
         {copy}:\n\
         \x20   lea r11, [rsp + {limit}]\n\
         \x20   cmp r10, r11\n\
         \x20   jae {call}\n\
         \x20   movzx r11d, byte ptr [rax]\n\
         \x20   mov byte ptr [r10], r11b\n\
         \x20   inc rax\n\
         \x20   inc r10\n\
         \x20   test r11d, r11d\n\
         \x20   jz {done}\n\
         \x20   cmp r11d, 37\n\
         \x20   jne {copy}\n\
         {spec}:\n\
         \x20   lea r11, [rsp + {limit}]\n\
         \x20   cmp r10, r11\n\
         \x20   jae {call}\n\
         \x20   movzx r11d, byte ptr [rax]\n\
         \x20   test r11d, r11d\n\
         \x20   jz {copy}\n\
         \x20   cmp r11d, 108\n\
         \x20   je {ell}\n\
         \x20   cmp r11d, 122\n\
         \x20   je {one}\n\
         \x20   cmp r11d, 106\n\
         \x20   je {one}\n\
         \x20   cmp r11d, 116\n\
         \x20   je {one}\n\
         \x20   mov byte ptr [r10], r11b\n\
         \x20   inc rax\n\
         \x20   inc r10\n"
    );
    // flags, width, precision, `*`, `$`: still inside the specification
    for c in *b"-+ #.*$" {
        s += &format!("    cmp r11d, {c}\n    je {spec}\n");
    }
    s += &format!(
        "    cmp r11d, 48\n\
         \x20   jb {copy}\n\
         \x20   cmp r11d, 57\n\
         \x20   jbe {spec}\n\
         \x20   jmp {copy}\n\
         {ell}:\n\
         \x20   cmp byte ptr [rax + 1], 108\n\
         \x20   jne {one}\n\
         \x20   inc rax\n\
         {one}:\n\
         \x20   movzx r11d, byte ptr [rax + 1]\n"
    );
    for c in *b"diouxXn" {
        s += &format!("    cmp r11d, {c}\n    je {wide}\n");
    }
    s += &format!(
        "{keep}:\n\
         \x20   movzx r11d, byte ptr [rax]\n\
         \x20   mov byte ptr [r10], r11b\n\
         \x20   inc rax\n\
         \x20   inc r10\n\
         \x20   jmp {spec}\n\
         {wide}:\n\
         \x20   mov byte ptr [r10], 73\n\
         \x20   mov byte ptr [r10 + 1], 54\n\
         \x20   mov byte ptr [r10 + 2], 52\n\
         \x20   add r10, 3\n\
         \x20   inc rax\n\
         \x20   jmp {spec}\n\
         {done}:\n\
         \x20   lea {fmt}, [rsp + {buf}]\n\
         {call}:\n\
         \x20   mov qword ptr [rsp + 32], r8\n\
         \x20   mov qword ptr [rsp + 40], r9\n"
    );
    for i in 0..(FORWARDED_ARGS - SYSV_ARGS.len() as u32) {
        s += &format!(
            "    mov rax, qword ptr [rbp + {}]\n    mov qword ptr [rsp + {}], rax\n",
            16 + 8 * i,
            48 + 8 * i
        );
    }
    s += &format!(
        "    mov r9, rcx\n\
         \x20   mov r8, rdx\n\
         \x20   mov rdx, rsi\n\
         \x20   mov rcx, rdi\n\
         \x20   call {target}\n\
         \x20   leave\n\
         \x20   ret\n"
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables() {
        assert_eq!(replacement("labs"), Some("_abs64"));
        assert_eq!(replacement("strtoul"), Some("_strtoui64"));
        assert_eq!(replacement("puts"), None);
        assert_eq!(format_argument("printf"), Some(0));
        assert_eq!(format_argument("snprintf"), Some(2));
        assert_eq!(format_argument("puts"), None);
        assert!(unbridged("ldiv").is_some());
        assert!(unbridged("labs").is_none());
        assert_eq!(canonical_name("__isoc23_strtol"), "strtol");
        assert_eq!(canonical_name("__isoc99_sscanf"), "sscanf");
        assert_eq!(canonical_name("__isoc23_"), "__isoc23_");
        assert_eq!(canonical_name("puts"), "puts");
        for (f, _) in REPLACEMENTS {
            assert!(format_argument(f).is_none() && unbridged(f).is_none());
        }
    }

    #[test]
    fn format_thunk_shape() {
        let t = format_thunk("fprintf", "fprintf", 1);
        assert!(t.starts_with("__l2w_thunk_fprintf:\n"));
        assert!(t.contains("sub rsp, 592\n"));
        assert!(t.contains("mov rax, rsi\n"));
        assert!(t.contains("lea rsi, [rsp + 80]\n"));
        assert!(t.contains("call fprintf\n    leave\n    ret\n"));
    }
}
