# A small library without system calls or external calls: translatable
# completely by phase 3 (sections, data, addressing, internal calls).
.intel_syntax noprefix

.section .rodata
table:  .quad 1, 1, 2, 3, 5, 8, 13, 21
greeting: .asciz "Hello from l2w"
greeting_len = . - greeting - 1

.data
.globl counter, ptrs
counter: .quad 0
ptrs:    .quad table, greeting

.bss
scratch: .zero 64
.comm shared, 128, 16

.text
.globl fib_at, sum_table, bump, strlen_l2w, max3
.type fib_at, @function

# long fib_at(long i): table lookup with bounds check, -1 if out of range
fib_at:
    cmp rdi, 7
    ja .Lout_of_range
    lea rax, [rip + table]
    mov rax, qword ptr [rax + rdi*8]
    ret
.Lout_of_range:
    mov rax, -1
    ret

# long sum_table(void): loop over the table
sum_table:
    xor eax, eax
    xor ecx, ecx
.Lloop:
    lea rdx, [rip + table]
    add rax, qword ptr [rdx + rcx*8]
    inc rcx
    cmp rcx, 8
    jl .Lloop
    ret

# long bump(long n): counter += n; returns the new value (absolute -> RIP-relative)
bump:
    mov rax, counter
    add rax, rdi
    mov counter, rax
    ret

# long strlen_l2w(const char *s)
strlen_l2w:
    mov rax, rdi
1:  cmp byte ptr [rax], 0
    je 2f
    inc rax
    jmp 1b
2:  sub rax, rdi
    ret

# long max3(long a, long b, long c): uses an internal helper
max3:
    push rbx
    mov rbx, rdx
    call max2
    mov rdi, rax
    mov rsi, rbx
    call max2
    pop rbx
    ret

max2:
    mov rax, rdi
    cmp rsi, rdi
    jle 1f
    mov rax, rsi
1:  ret

greeting_info:
    mov rax, offset greeting
    mov edx, greeting_len
    ret

.section .note.GNU-stack,"",@progbits
