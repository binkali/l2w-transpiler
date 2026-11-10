.intel_syntax noprefix
.text
    mov rax, [rbx*3]
    mov rax, 1
    .weak foo
    mov rcx, [rsp*2 + rax]
