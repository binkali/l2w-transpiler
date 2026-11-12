//! System V AMD64 <-> Windows x64 calling convention bridging.
//!
//! # The two conventions
//!
//! | | System V (Linux) | Windows x64 |
//! |---|---|---|
//! | integer arguments | rdi, rsi, rdx, rcx, r8, r9, then stack | rcx, rdx, r8, r9, then stack |
//! | stack arguments at callee entry | `[rsp + 8]`, `[rsp + 16]`, ... | `[rsp + 40]`, ... (after 32 bytes shadow space) |
//! | shadow space | none | 32 bytes, reserved by the caller |
//! | red zone | 128 bytes below rsp | none |
//! | alignment | rsp ≡ 0 (mod 16) before `call` | same |
//! | return value | rax (rdx:rax) | rax |
//! | callee-saved | rbx, rbp, r12–r15 | rbx, rbp, **rdi, rsi**, r12–r15, **xmm6–xmm15** |
//! | system calls | `syscall`, number in rax, args rdi, rsi, rdx, **r10**, r8, r9 | (no stable syscall ABI; use the Win32 API) |
//!
//! # Bridging strategy
//!
//! Translated code keeps the System V convention internally, so calls
//! between translated functions are unchanged. Conversions happen only at
//! the boundaries:
//!
//! * **System V -> Windows thunk** ([`sysv_to_win_thunk`]) for every
//!   external function the program calls or tail-jumps to.
//! * **Windows -> System V wrapper** ([`win_to_sysv_wrapper`]) under the
//!   public name of every exported function and of `main`.
//! * **Entry wrapper** ([`entry_wrapper`]) for `_start`.
//!
//! Without type information the number of arguments is unknown, so bridges
//! always forward [`FORWARDED_ARGS`] integer arguments. Surplus arguments
//! are harmless in both conventions (the caller owns and cleans up the
//! argument area); reading surplus stack slots reads the caller's frame.
//! Floating-point arguments cannot occur because l2w does not accept SSE
//! instructions; for the same reason translated code never modifies
//! xmm6–xmm15, so wrappers do not need to save them.

use crate::ast::symbol_name;

/// System V integer argument registers.
pub const SYSV_ARGS: [&str; 6] = ["rdi", "rsi", "rdx", "rcx", "r8", "r9"];
/// Windows x64 integer argument registers.
pub const WIN64_ARGS: [&str; 4] = ["rcx", "rdx", "r8", "r9"];
/// Registers the Windows callee must preserve but System V code may clobber.
pub const WIN64_ONLY_CALLEE_SAVED: [&str; 2] = ["rsi", "rdi"];
/// Windows shadow space in bytes.
pub const SHADOW_SPACE: u32 = 32;
/// Size of the System V red zone in bytes.
pub const RED_ZONE: u32 = 128;
/// Integer arguments forwarded by every bridge (6 in registers + 4 on the stack).
pub const FORWARDED_ARGS: u32 = 10;

/// Name of the System V -> Windows thunk for external function `f`.
pub fn thunk_name(f: &str) -> String {
    format!("__l2w_thunk_{f}")
}

/// Name of the System V body of an exported function `f`.
pub fn sysv_body_name(f: &str) -> String {
    format!("__l2w_sysv_{f}")
}

/// System V -> Windows x64 thunk: called with the System V convention,
/// calls `target` with the Windows convention and returns its result.
///
/// Stack at thunk entry (System V): `[rsp]` return address, arguments 7..10
/// at `[rsp + 8]`..`[rsp + 32]`, i.e. `[rbp + 16]`.. after `push rbp`.
/// The Windows call frame is 32 bytes shadow space + arguments 5..10.
pub fn sysv_to_win_thunk(target: &str) -> String {
    sysv_to_win_thunk_to(target, target)
}

/// Thunk for external `f` (named [`thunk_name`]`(f)`) that calls the
/// Windows function `target`, e.g. a 64-bit replacement
/// (see [`super::data_model`]).
pub fn sysv_to_win_thunk_to(f: &str, target: &str) -> String {
    let stack_args = FORWARDED_ARGS - WIN64_ARGS.len() as u32; // 6: args 5..10
    let frame = SHADOW_SPACE + 8 * stack_args; // 80, multiple of 16
    let mut s = format!(
        "{name}:\n\
         \x20   push rbp\n\
         \x20   mov rbp, rsp\n\
         \x20   sub rsp, {frame}\n\
         \x20   and rsp, -16\n\
         \x20   mov qword ptr [rsp + 32], r8\n\
         \x20   mov qword ptr [rsp + 40], r9\n",
        name = symbol_name(&thunk_name(f))
    );
    let target = symbol_name(target);
    // System V arguments 7..10 -> Windows stack slots 7..10.
    for i in 0..(FORWARDED_ARGS - SYSV_ARGS.len() as u32) {
        s += &format!(
            "    mov rax, qword ptr [rbp + {}]\n    mov qword ptr [rsp + {}], rax\n",
            16 + 8 * i,
            48 + 8 * i
        );
    }
    // Order matters: every register is read before it is overwritten.
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

/// Windows x64 -> System V wrapper: called with the Windows convention
/// under the public name `public`, calls the System V body `body`.
///
/// Stack at wrapper entry (Windows): `[rsp]` return address, shadow space
/// `[rsp + 8]`..`[rsp + 40]`, argument 5 at `[rsp + 40]` = `[rbp + 48]`
/// after `push rbp`. rsi and rdi are callee-saved on Windows only and are
/// preserved by the wrapper.
pub fn win_to_sysv_wrapper(public: &str, body: &str) -> String {
    let (public, body) = (symbol_name(public), symbol_name(body));
    let stack_args = FORWARDED_ARGS - SYSV_ARGS.len() as u32; // 4: args 7..10
                                                              // entry ≡ 8 (mod 16); push rbp, rsi, rdi -> ≡ 0; 32 bytes keep it ≡ 0.
    let mut s = format!(
        "{public}:\n\
         \x20   push rbp\n\
         \x20   mov rbp, rsp\n\
         \x20   push rsi\n\
         \x20   push rdi\n\
         \x20   sub rsp, {}\n",
        8 * stack_args
    );
    for i in 0..stack_args {
        let slot = if i == 0 {
            "[rsp]".to_string()
        } else {
            format!("[rsp + {}]", 8 * i)
        };
        s += &format!(
            "    mov rax, qword ptr [rbp + {}]\n    mov qword ptr {slot}, rax\n",
            64 + 8 * i
        );
    }
    s += &format!(
        "    mov rdi, rcx\n\
         \x20   mov rsi, rdx\n\
         \x20   mov rdx, r8\n\
         \x20   mov rcx, r9\n\
         \x20   mov r8, qword ptr [rbp + 48]\n\
         \x20   mov r9, qword ptr [rbp + 56]\n\
         \x20   call {body}\n\
         \x20   lea rsp, [rbp - 16]\n\
         \x20   pop rdi\n\
         \x20   pop rsi\n\
         \x20   pop rbp\n\
         \x20   ret\n"
    );
    s
}

/// Entry wrapper for `_start`. The Windows loader calls the entry point
/// like a function (rsp ≡ 8 mod 16), while Linux enters `_start` with
/// rsp ≡ 0 (mod 16) and no return address. Linux `_start` code never
/// returns, so the wrapper only restores the alignment and jumps.
pub fn entry_wrapper(public: &str, body: &str) -> String {
    format!(
        "{}:\n    sub rsp, 8\n    jmp {}\n",
        symbol_name(public),
        symbol_name(body)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thunk_shape() {
        let t = sysv_to_win_thunk("puts");
        assert!(t.starts_with("__l2w_thunk_puts:\n"));
        assert!(t.contains("sub rsp, 80\n"));
        assert!(t.contains("mov qword ptr [rsp + 32], r8"));
        assert!(t.contains("mov rax, qword ptr [rbp + 40]\n    mov qword ptr [rsp + 72], rax"));
        let order: Vec<usize> = [
            "mov r9, rcx",
            "mov r8, rdx",
            "mov rdx, rsi",
            "mov rcx, rdi",
            "call puts",
        ]
        .iter()
        .map(|p| t.find(p).unwrap())
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn wrapper_shape() {
        let w = win_to_sysv_wrapper("main", "__l2w_sysv_main");
        assert!(w.starts_with("main:\n"));
        assert!(w.contains("mov r8, qword ptr [rbp + 48]"));
        assert!(w.contains("mov rax, qword ptr [rbp + 88]\n    mov qword ptr [rsp + 24], rax"));
        assert!(w.contains("call __l2w_sysv_main"));
        assert!(w.contains("pop rdi\n    pop rsi\n    pop rbp\n    ret"));
    }
}
