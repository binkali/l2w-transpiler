# Hello world using Linux system calls (write + exit).
.intel_syntax noprefix

.section .rodata
msg:    .ascii "Hello, World!\n"
len = . - msg

.text
.globl _start
_start:
    mov rax, 1              # sys_write
    mov rdi, 1              # fd = stdout
    lea rsi, [rip + msg]    # buffer
    mov rdx, len            # count
    syscall

    mov rax, 60             # sys_exit
    xor edi, edi            # status = 0
    syscall
