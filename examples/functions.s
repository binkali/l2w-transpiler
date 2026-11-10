# Functions with different numbers of arguments (System V ABI).
.intel_syntax noprefix

.text
.globl _start

# long add3(long a, long b, long c)
add3:
    push rbp
    mov rbp, rsp
    lea rax, [rdi + rsi]
    add rax, rdx
    pop rbp
    ret

# long scale(long x) = x * 8 + 3
scale:
    lea rax, [rdi*8 + 3]
    ret

_start:
    mov rdi, 1
    mov rsi, 2
    mov rdx, 3
    call add3
    mov rdi, rax
    call scale
    mov rdi, rax
    mov rax, 60
    syscall
