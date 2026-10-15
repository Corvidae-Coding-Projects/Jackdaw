#[cfg(target_os = "linux")]
pub(crate) fn enter_document_worker_sandbox() -> anyhow::Result<()> {
    use libc::{
        BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W, EACCES, PR_SET_NO_NEW_PRIVS,
        PR_SET_SECCOMP, SECCOMP_MODE_FILTER, SECCOMP_RET_ALLOW, SECCOMP_RET_ERRNO,
        SECCOMP_RET_KILL_PROCESS, SYS_accept, SYS_accept4, SYS_bind, SYS_connect, SYS_creat,
        SYS_execve, SYS_execveat, SYS_listen, SYS_open, SYS_openat, SYS_socket, SYS_socketpair,
        c_ushort, sock_filter, sock_fprog,
    };

    const SECCOMP_DATA_NR_OFFSET: u32 = 0;
    const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;

    fn statement(code: u32, value: u32) -> sock_filter {
        sock_filter {
            code: u16::try_from(code).expect("classic-BPF opcode must fit in u16"),
            jt: 0,
            jf: 0,
            k: value,
        }
    }

    fn jump(code: u32, value: u32, on_true: u8, on_false: u8) -> sock_filter {
        sock_filter {
            code: u16::try_from(code).expect("classic-BPF opcode must fit in u16"),
            jt: on_true,
            jf: on_false,
            k: value,
        }
    }

    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xc000_00b7;
    #[cfg(target_arch = "x86")]
    const AUDIT_ARCH: u32 = 0x4000_0003;
    #[cfg(target_arch = "arm")]
    const AUDIT_ARCH: u32 = 0x4000_0028;
    #[cfg(target_arch = "riscv64")]
    const AUDIT_ARCH: u32 = 0xc000_00f3;
    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "x86",
        target_arch = "arm",
        target_arch = "riscv64"
    )))]
    compile_error!("document-worker seccomp audit architecture is not defined");

    let denied_syscalls = [
        SYS_open,
        SYS_openat,
        SYS_creat,
        SYS_socket,
        SYS_socketpair,
        SYS_connect,
        SYS_bind,
        SYS_listen,
        SYS_accept,
        SYS_accept4,
        SYS_execve,
        SYS_execveat,
    ];
    let mut filter = Vec::with_capacity(5 + denied_syscalls.len() * 2);
    filter.push(statement(
        BPF_LD | BPF_W | BPF_ABS,
        SECCOMP_DATA_ARCH_OFFSET,
    ));
    filter.push(jump(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH, 1, 0));
    filter.push(statement(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS));
    filter.push(statement(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_NR_OFFSET));
    for syscall in denied_syscalls {
        filter.push(jump(BPF_JMP | BPF_JEQ | BPF_K, syscall as u32, 0, 1));
        filter.push(statement(
            BPF_RET | BPF_K,
            SECCOMP_RET_ERRNO | EACCES as u32,
        ));
    }
    filter.push(statement(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));

    let program = sock_fprog {
        len: c_ushort::try_from(filter.len())?,
        filter: filter.as_mut_ptr(),
    };
    // SAFETY: `program` points at the live filter slice for the duration of `prctl`; the kernel
    // copies and validates the classic-BPF program before either call returns.
    unsafe {
        if libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if libc::prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &program) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn enter_document_worker_sandbox() -> anyhow::Result<()> {
    anyhow::bail!("document-worker isolation requires Linux seccomp")
}
