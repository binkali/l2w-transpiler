# Sum 1..10 with a loop and conditional jumps; exit code = sum (55).
.intel_syntax noprefix

.data
counter: .quad 0

.bss
buffer: .zero 64

.text
.globl _start
_start:
    xor eax, eax
    mov ecx, 1
.Lloop:
    add rax, rcx
    inc rcx
    cmp rcx, 10
    jle .Lloop

    mov qword ptr [rip + counter], rax
    cmp rax, 55
    je .Lok
    mov rdi, 1
    jmp .Lexit
.Lok:
    mov rdi, rax
.Lexit:
    mov rax, 60
    syscall
