//! Program structure and platform analysis on the IR.
//!
//! * **Functions** — a label in a code section starts a function if it is
//!   the entry point, has `.type @function`, is global, or is the target of
//!   a direct `call`. A function extends up to the next function label.
//! * **Calls** — every `call` is classified as internal, external
//!   (undefined symbol) or indirect.
//! * **System calls** — for every `syscall` the values of `rax` and the six
//!   argument registers are determined by a conservative backward scan
//!   within the basic block (see [`value_of`]).

use super::*;
use crate::ast::{BinaryOp, Expr};

pub(super) fn analyze(module: &mut Module) {
    module.platform.entry = find_entry(module);
    module.functions = find_functions(module);
    module.platform.calls = find_calls(module);
    module.platform.syscalls = find_syscalls(module);
}

fn is_label_in_code(module: &Module, name: &str) -> Option<Loc> {
    match module.symbols.get(name)?.def {
        SymbolDef::Label(loc) if module.sections[loc.section].kind == SectionKind::Text => {
            Some(loc)
        }
        _ => None,
    }
}

fn find_entry(module: &Module) -> Option<Entry> {
    [("_start", EntryKind::Start), ("main", EntryKind::Main)]
        .into_iter()
        .find(|(n, _)| is_label_in_code(module, n).is_some())
        .map(|(n, kind)| Entry {
            symbol: n.to_string(),
            kind,
        })
}

/// The symbol a direct branch target refers to (`f`, `f@PLT`, `f + 0`).
fn target_symbol(e: &Expr) -> Option<&str> {
    match e {
        Expr::Symbol(s) => Some(&s.name),
        _ => None,
    }
}

fn find_functions(module: &Module) -> Vec<Function> {
    let mut call_targets = std::collections::HashSet::new();
    for (_, ins) in module.instructions() {
        if ins.mnemonic == Mnemonic::Call {
            if let Some(Operand::Target(e)) = ins.operands.first() {
                if let Some(n) = target_symbol(e) {
                    call_targets.insert(n.to_string());
                }
            }
        }
    }
    let entry = module.platform.entry.as_ref().map(|e| e.symbol.as_str());
    let mut starts: Vec<(Loc, &Symbol, FunctionReason)> = Vec::new();
    for sym in module.symbols.iter() {
        let Some(loc) = is_label_in_code(module, &sym.name) else {
            continue;
        };
        let reason = if Some(sym.name.as_str()) == entry {
            FunctionReason::Entry
        } else if sym.ty == Some(SymbolType::Function) {
            FunctionReason::TypeDirective
        } else if call_targets.contains(&sym.name) {
            FunctionReason::CallTarget
        } else if sym.global {
            FunctionReason::GlobalCode
        } else {
            continue;
        };
        starts.push((loc, sym, reason));
    }
    starts.sort_by_key(|(loc, ..)| *loc);
    starts
        .iter()
        .enumerate()
        .map(|(i, (loc, sym, reason))| {
            let end = starts
                .get(i + 1)
                .filter(|(next, ..)| next.section == loc.section)
                .map(|(next, ..)| next.item)
                .unwrap_or(module.sections[loc.section].items.len());
            Function {
                name: sym.name.clone(),
                section: loc.section,
                start: loc.item,
                end,
                global: sym.global,
                reason: *reason,
            }
        })
        .collect()
}

fn find_calls(module: &Module) -> Vec<CallSite> {
    module
        .instructions()
        .filter(|(_, i)| i.mnemonic == Mnemonic::Call)
        .map(|(at, ins)| {
            let target = match ins.operands.first() {
                Some(Operand::Target(e)) => match target_symbol(e) {
                    Some(n) => match module.symbols.get(n).map(|s| &s.def) {
                        Some(SymbolDef::Undefined) | None => CallTarget::External(n.to_string()),
                        _ => CallTarget::Internal(n.to_string()),
                    },
                    None => CallTarget::Indirect,
                },
                _ => CallTarget::Indirect,
            };
            CallSite {
                at,
                span: ins.span,
                function: module.function_at(at),
                target,
            }
        })
        .collect()
}

fn find_syscalls(module: &Module) -> Vec<SyscallSite> {
    module
        .instructions()
        .filter(|(_, i)| i.mnemonic == Mnemonic::Syscall)
        .map(|(at, ins)| SyscallSite {
            at,
            span: ins.span,
            function: module.function_at(at),
            number: value_of(module, at, 0, 0),
            args: SYSCALL_ARG_REGS.map(|r| value_of(module, at, r, 0)),
        })
        .collect()
}

/// Hardware number of the 64-bit register containing `r`.
fn family(r: &Register) -> Option<u8> {
    match r.kind {
        RegKind::Gpr(n) | RegKind::HighByte(n) => Some(n),
        _ => None,
    }
}

/// Determines the value of GPR `reg` immediately before the item at `at`.
///
/// Scans backwards within the basic block. The scan stops (value unknown)
/// at labels (possible jump targets), unconditional control flow, `call`
/// (clobbers registers) and data. A conditional jump does not stop the
/// scan: on the fall-through path the earlier values still hold. Linux
/// `syscall` preserves all registers except rax, rcx and r11.
pub fn value_of(module: &Module, at: Loc, reg: u8, depth: u32) -> Value {
    if depth > 16 {
        return Value::Unknown;
    }
    let items = &module.sections[at.section].items;
    for i in (0..at.item).rev() {
        let ins = match &items[i] {
            Item::Instr(ins) => ins,
            Item::Set { .. } | Item::Align { .. } => continue,
            _ => return Value::Unknown,
        };
        let here = Loc {
            section: at.section,
            item: i,
        };
        match ins.mnemonic {
            Mnemonic::Jmp | Mnemonic::Ret | Mnemonic::Call => return Value::Unknown,
            Mnemonic::Syscall => {
                if matches!(reg, 0 | 1 | 11) {
                    return Value::Unknown;
                }
                continue;
            }
            _ => {}
        }
        if let Some(v) = effect(module, ins, here, reg, depth) {
            return v;
        }
    }
    Value::Unknown
}

/// `Some(value)` if `ins` writes register `reg`, `None` otherwise.
fn effect(module: &Module, ins: &Instr, here: Loc, reg: u8, depth: u32) -> Option<Value> {
    use Mnemonic::*;
    let dst = match ins.operands.first() {
        Some(Operand::Reg(r)) if family(r) == Some(reg) => Some(*r),
        _ => None,
    };
    let src = ins.operands.get(1);
    let full = |r: &Register| {
        matches!(r.size, Size::Qword | Size::Dword) && matches!(r.kind, RegKind::Gpr(_))
    };
    let truncate = |v: Value, r: &Register| match (v, r.size) {
        (Value::Const(c), Size::Dword) => Value::Const(c as u32 as i64),
        (v, _) => v,
    };

    // Implicit register writes.
    let implicit: &[u8] = match ins.mnemonic {
        Mul | Div | Idiv => &[0, 2],
        Imul if ins.operands.len() == 1 => &[0, 2],
        Cbw | Cwde | Cdqe => &[0],
        Cwd | Cdq | Cqo => &[2],
        Leave => &[4, 5],
        Push | Pop | Pushfq | Popfq => &[4],
        Xchg => {
            let touches = ins
                .operands
                .iter()
                .any(|o| matches!(o, Operand::Reg(r) if family(r) == Some(reg)));
            if touches {
                return Some(Value::Unknown);
            }
            &[]
        }
        _ => &[],
    };
    if implicit.contains(&reg) {
        return Some(Value::Unknown);
    }

    let dst = dst?;
    if !full(&dst) {
        // Partial (8/16-bit) writes merge with the old value.
        return match ins.mnemonic {
            Cmp | Test | Push => None,
            _ => Some(Value::Unknown),
        };
    }
    let prior = |d: u32| value_of(module, here, reg, d);
    Some(match ins.mnemonic {
        Mov => match src? {
            Operand::Imm(e) => truncate(
                module
                    .symbols
                    .eval(e)
                    .map(Value::Const)
                    .unwrap_or_else(|| Value::AbsExpr(e.clone())),
                &dst,
            ),
            Operand::Addr(e) => Value::Address(e.clone()),
            Operand::Reg(r) if full(r) && r.size == dst.size => {
                truncate(value_of(module, here, family(r)?, depth + 1), &dst)
            }
            _ => Value::Unknown,
        },
        Lea => match src? {
            Operand::Mem(m) if m.index.is_none() && (m.is_rip_relative() || m.base.is_none()) => {
                match &m.disp {
                    Some(d) => Value::Address(d.clone()),
                    None => Value::Unknown,
                }
            }
            _ => Value::Unknown,
        },
        Xor | Sub if src == Some(&Operand::Reg(dst)) => Value::Const(0),
        Add | Sub | Inc | Dec => {
            let delta = match (ins.mnemonic, src) {
                (Inc, _) => Some(1),
                (Dec, _) => Some(-1),
                (Add, Some(Operand::Imm(e))) => module.symbols.eval(e),
                (Sub, Some(Operand::Imm(e))) => module.symbols.eval(e).map(i64::wrapping_neg),
                _ => None,
            };
            match (delta, prior(depth + 1)) {
                (Some(d), Value::Const(c)) => truncate(Value::Const(c.wrapping_add(d)), &dst),
                (Some(d), Value::Address(e)) => {
                    Value::Address(Expr::binary(BinaryOp::Add, e, Expr::Int(d as i128)))
                }
                _ => Value::Unknown,
            }
        }
        Cmp | Test | Push => return None,
        _ => Value::Unknown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn module(src: &str) -> Module {
        let p = parse(src);
        assert!(!p.has_errors(), "{:#?}", p.diagnostics);
        let out = lower(&p.program);
        assert!(!out.has_errors(), "{:#?}", out.diagnostics);
        out.module
    }

    #[test]
    fn syscall_values_hello_world() {
        let m = module(
            ".section .rodata\nmsg: .ascii \"hi\\n\"\nlen = . - msg\n.text\n.globl _start\n_start:\n\
             mov rax, 1\nmov rdi, 1\nlea rsi, [rip + msg]\nmov rdx, len\nsyscall\n\
             mov eax, 60\nxor edi, edi\nsyscall\n",
        );
        assert_eq!(
            m.platform.entry,
            Some(Entry {
                symbol: "_start".into(),
                kind: EntryKind::Start
            })
        );
        let [w, e] = &m.platform.syscalls[..] else {
            panic!()
        };
        assert_eq!(w.number, Value::Const(1));
        assert_eq!(w.args[0], Value::Const(1));
        assert_eq!(w.args[1], Value::Address(Expr::symbol("msg")));
        assert_eq!(w.args[2], Value::AbsExpr(Expr::symbol("len")));
        assert_eq!(e.number, Value::Const(60));
        assert_eq!(e.args[0], Value::Const(0));
        // rdx survives the first syscall, rax does not
        assert_eq!(e.args[2], Value::AbsExpr(Expr::symbol("len")));
        assert_eq!(w.function, Some(0));
    }

    #[test]
    fn value_tracking_is_conservative() {
        let m = module(
            "f:\nmov rax, 1\nlbl:\nsyscall\n\
             g:\nmov rax, 2\ncall f\nsyscall\n\
             h:\nmov eax, -1\nmov rbx, rax\nmov rdi, rbx\nadd rdi, 4\nmov al, 3\nsyscall\n\
             k:\nmov rax, 60\ncmp rdi, 0\njne k\nmov dil, 1\nsyscall\nret\n\
             c:\nmov eax, 60\nmov edi, 7\nmov esi, 8\ncmp rbx, 1\ncmovl eax, ebx\nsete dil\nsetne byte ptr [rsi]\nsyscall\nret\n",
        );
        let s = &m.platform.syscalls;
        assert_eq!(s[4].number, Value::Unknown, "cmovcc may write eax");
        assert_eq!(s[4].args[0], Value::Unknown, "setcc writes dil");
        assert_eq!(
            s[4].args[1],
            Value::Const(8),
            "setcc to memory writes no register"
        );
        assert_eq!(s[0].number, Value::Unknown, "label between");
        assert_eq!(s[1].number, Value::Unknown, "call clobbers");
        assert_eq!(s[2].number, Value::Unknown, "partial write");
        assert_eq!(
            s[2].args[0],
            Value::Const(0xFFFF_FFFF + 4),
            "copy + add, zero-extended mov eax"
        );
        assert_eq!(s[3].number, Value::Const(60), "fallthrough of jcc");
        assert_eq!(s[3].args[0], Value::Unknown, "partial write to dil");
    }

    #[test]
    fn functions_and_calls() {
        let m = module(
            ".text\n.globl main\n.type helper, @function\nhelper:\n ret\nsq:\n imul rdi, rdi\n mov rax, rdi\n ret\n\
             main:\n.Lx:\n call helper\n call sq\n call puts@PLT\n call rax\n call qword ptr [rbx]\n jmp .Lx\n",
        );
        let names: Vec<_> = m
            .functions
            .iter()
            .map(|f| (f.name.as_str(), f.reason))
            .collect();
        assert_eq!(
            names,
            [
                ("helper", FunctionReason::TypeDirective),
                ("sq", FunctionReason::CallTarget),
                ("main", FunctionReason::Entry)
            ]
        );
        assert_eq!(m.functions[1].end, m.functions[2].start);
        let t: Vec<_> = m.platform.calls.iter().map(|c| c.target.clone()).collect();
        assert_eq!(
            t,
            [
                CallTarget::Internal("helper".into()),
                CallTarget::Internal("sq".into()),
                CallTarget::External("puts".into()),
                CallTarget::Indirect,
                CallTarget::Indirect
            ]
        );
        assert!(m.platform.calls.iter().all(|c| c.function == Some(2)));
        // `.Lx` is a local label inside main, not a function
        assert!(!m.functions.iter().any(|f| f.name == ".Lx"));
    }
}
