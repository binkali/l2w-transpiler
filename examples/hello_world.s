.intel_syntax noprefix

.data
msg:
    .ascii "Hello, World!\n"
    .equ len, . - msg           # Berechnet die Länge (aktueller Ort '.' minus Startadresse 'msg')

.text
.global _start

_start:
    # sys_write(stdout, msg, len)
    mov rax, 1                  # Syscall 1 = sys_write
    mov rdi, 1                  # File Descriptor 1 = stdout
    lea rsi, [rip + msg]        # RIP-relative Adresse der Nachricht laden
    mov rdx, len                # Länge des Strings
    syscall

    # sys_exit(0)
    mov rax, 60                 # Syscall 60 = sys_exit
    xor rdi, rdi                # rdi auf 0 setzen
    syscall
