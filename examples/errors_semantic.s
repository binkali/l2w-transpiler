# Parses fine, but is rejected by the IR stage (semantic errors).
.intel_syntax noprefix
.text
.globl _start
_start:
    mov rax, ebx                # operand size mismatch
    inc [rdi]                   # ambiguous operand size
    add rax, 0x100000000        # immediate does not fit (imm32)
    cpuid                       # unsupported instruction
    jmp 1b                      # no numeric label before
    mov rdx, len                # warning: len defined later -> memory access
.data
msg: .ascii "x"
len = . - msg
