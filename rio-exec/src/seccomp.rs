//! The multi-ABI seccomp purity filter.
//!
//! Installed in the sandboxed process immediately before the privilege
//! drop. It is a **purity** filter, not a security boundary: the two
//! things it blocks are operations whose results cannot be represented
//! in the archive format the caller serializes outputs into, so allowing
//! them would mean a build *believes* it produced something (a setuid
//! binary, a file with an xattr/ACL) that the archived output silently
//! does not contain. Failing the operation inside the build — loudly,
//! with an errno the build tooling understands — preserves the failure
//! mode callers depend on instead of silently stripping the attribute
//! after the fact.
//!
//! | syscalls | condition | action |
//! |---|---|---|
//! | `chmod`, `fchmod`, `fchmodat`, `fchmodat2` | mode has `S_ISUID\|S_ISGID` | `EPERM` |
//! | `setxattr`, `lsetxattr`, `fsetxattr` | always | `ENOTSUP` |
//! | anything else | | allow |
//! | unknown `seccomp_data.arch` | | kill |
//!
//! Only the *set* half of the xattr family is filtered: purity requires
//! preventing xattr creation; reading them back is harmless (there are
//! none to read inside the sandbox).
//!
//! # Why the program is hand-assembled
//!
//! The filter must match **every syscall ABI the process can reach**,
//! not just the build target's native one. A process that calls
//! `personality(PER_LINUX32)` (an i686 build on an x86_64 host) issues
//! syscalls the kernel tags `AUDIT_ARCH_I386` with i386 syscall
//! numbers; any process on x86_64 can issue x32 syscalls (numbers with
//! bit 30 set, still tagged `AUDIT_ARCH_X86_64`). A single-ABI filter
//! is silently bypassed by either. This is the multi-arch hole Nix's
//! own sandbox filter closed in NixOS/nix#2719, and it is why the
//! off-the-shelf `seccompiler` crate is unusable here: its generated
//! prologue kills any syscall whose `arch` differs from the one
//! compiled target (which would kill an i686 build's first syscall),
//! and its rule model cannot express per-ABI syscall numbers. So the
//! classic-BPF program is assembled from a per-ABI table by a tiny
//! label-resolving emitter, and the unit tests execute it in a software
//! interpreter against every registered ABI.

use libc::sock_filter;
use nix::errno::Errno;

// ---------------------------------------------------------------------------
// Constants libc 0.2 does not export for linux-gnu.
// ---------------------------------------------------------------------------

/// `AUDIT_ARCH_*` tokens from `include/uapi/linux/audit.h`: the ELF
/// machine number OR'd with `__AUDIT_ARCH_64BIT` (`0x8000_0000`) and
/// `__AUDIT_ARCH_LE` (`0x4000_0000`) as appropriate. Stable kernel UAPI
/// values.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_X86_64: u32 = 62 | 0x8000_0000 | 0x4000_0000; // EM_X86_64
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_I386: u32 = 3 | 0x4000_0000; // EM_386
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH_AARCH64: u32 = 183 | 0x8000_0000 | 0x4000_0000; // EM_AARCH64
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH_ARM: u32 = 40 | 0x4000_0000; // EM_ARM

/// `arch/x86/entry/syscalls/syscall_64.tbl`: x32 syscall numbers are
/// the x86_64 numbers OR'd with this bit (`__X32_SYSCALL_BIT`).
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// `S_ISUID | S_ISGID` (0o4000 | 0o2000). The sticky bit (0o1000) is
/// representable in the output archive and stays allowed.
const SETID_BITS: u32 = 0o6000;

/// Byte offsets into `struct seccomp_data` (`include/uapi/linux/seccomp.h`):
/// `{ int nr; __u32 arch; __u64 instruction_pointer; __u64 args[6]; }`.
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;

/// Offset of the **low 32 bits** of `args[i]`. Classic BPF loads are
/// 32-bit; every bit we test (`SETID_BITS` < 2^16) lives in the low
/// word on a little-endian target.
const fn off_arg_lo(i: u32) -> u32 {
    16 + 8 * i
}

#[cfg(target_endian = "big")]
compile_error!(
    "the seccomp filter's argument loads read the low word of each 64-bit \
     argument at offset 16 + 8*i, which is only the low word on little-endian"
);

// ---------------------------------------------------------------------------
// The per-ABI syscall tables.
// ---------------------------------------------------------------------------

/// One syscall ABI's view of the filtered syscalls: the
/// `seccomp_data.arch` token that selects the table, the
/// `(syscall_nr, index_of_the_mode_argument)` pairs for the
/// chmod family, and the unconditional-`ENOTSUP` xattr-set numbers.
struct AbiTable {
    name: &'static str,
    arch: u32,
    chmod_like: &'static [(u32, u32)],
    xattr_set: &'static [u32],
}

/// The mode argument is `args[1]` for `chmod(path, mode)` and
/// `fchmod(fd, mode)`…
const MODE_ARG_CHMOD: u32 = 1;
/// …and `args[2]` for `fchmodat(dirfd, path, mode, flags)` and
/// `fchmodat2(dirfd, path, mode, flags)`.
const MODE_ARG_CHMODAT: u32 = 2;

/// The ABIs reachable from an x86_64 process.
///
/// Native and x32 share the `AUDIT_ARCH_X86_64` token — x32 is
/// distinguished only by bit 30 of the syscall number — so they share
/// one arch block carrying both number sets. The native numbers come
/// from `libc::SYS_*` (correct by construction for the build target);
/// the i386 numbers are from `arch/x86/entry/syscalls/syscall_32.tbl`.
#[cfg(target_arch = "x86_64")]
const ABIS: &[AbiTable] = &[
    AbiTable {
        name: "x86_64+x32",
        arch: AUDIT_ARCH_X86_64,
        chmod_like: &[
            (libc::SYS_chmod as u32, MODE_ARG_CHMOD),
            (libc::SYS_fchmod as u32, MODE_ARG_CHMOD),
            (libc::SYS_fchmodat as u32, MODE_ARG_CHMODAT),
            (libc::SYS_fchmodat2 as u32, MODE_ARG_CHMODAT),
            (libc::SYS_chmod as u32 | X32_SYSCALL_BIT, MODE_ARG_CHMOD),
            (libc::SYS_fchmod as u32 | X32_SYSCALL_BIT, MODE_ARG_CHMOD),
            (
                libc::SYS_fchmodat as u32 | X32_SYSCALL_BIT,
                MODE_ARG_CHMODAT,
            ),
            (
                libc::SYS_fchmodat2 as u32 | X32_SYSCALL_BIT,
                MODE_ARG_CHMODAT,
            ),
        ],
        xattr_set: &[
            libc::SYS_setxattr as u32,
            libc::SYS_lsetxattr as u32,
            libc::SYS_fsetxattr as u32,
            libc::SYS_setxattr as u32 | X32_SYSCALL_BIT,
            libc::SYS_lsetxattr as u32 | X32_SYSCALL_BIT,
            libc::SYS_fsetxattr as u32 | X32_SYSCALL_BIT,
        ],
    },
    AbiTable {
        name: "i386",
        arch: AUDIT_ARCH_I386,
        // arch/x86/entry/syscalls/syscall_32.tbl (verified against the
        // generated asm/unistd_32.h from linux-headers 6.18.7): chmod=15
        // and fchmod=94 (the v7-inherited block), fchmodat=306 (the
        // 2.6.16 *at block, 295..=307), fchmodat2=452 (the post-6.6
        // unified numbering shared by every ABI), setxattr/lsetxattr/
        // fsetxattr = 226/227/228 (the 2.6.0 xattr block, 226..=237).
        chmod_like: &[
            (15, MODE_ARG_CHMOD),
            (94, MODE_ARG_CHMOD),
            (306, MODE_ARG_CHMODAT),
            (452, MODE_ARG_CHMODAT),
        ],
        xattr_set: &[226, 227, 228],
    },
];

/// The ABIs reachable from an aarch64 process.
///
/// aarch64 uses the asm-generic table (`include/uapi/asm-generic/
/// unistd.h`), which never had a plain `chmod` — only `fchmodat`. The
/// arm32 numbers are from `arch/arm/tools/syscall.tbl` (EABI).
#[cfg(target_arch = "aarch64")]
const ABIS: &[AbiTable] = &[
    AbiTable {
        name: "aarch64",
        arch: AUDIT_ARCH_AARCH64,
        chmod_like: &[
            (libc::SYS_fchmod as u32, MODE_ARG_CHMOD),
            (libc::SYS_fchmodat as u32, MODE_ARG_CHMODAT),
            (libc::SYS_fchmodat2 as u32, MODE_ARG_CHMODAT),
        ],
        xattr_set: &[
            libc::SYS_setxattr as u32,
            libc::SYS_lsetxattr as u32,
            libc::SYS_fsetxattr as u32,
        ],
    },
    AbiTable {
        name: "arm",
        arch: AUDIT_ARCH_ARM,
        // arch/arm/tools/syscall.tbl (EABI): chmod=15, fchmod=94,
        // fchmodat=333 (arm's *at block is 322..=334), fchmodat2=452,
        // setxattr/lsetxattr/fsetxattr=226/227/228.
        chmod_like: &[
            (15, MODE_ARG_CHMOD),
            (94, MODE_ARG_CHMOD),
            (333, MODE_ARG_CHMODAT),
            (452, MODE_ARG_CHMODAT),
        ],
        xattr_set: &[226, 227, 228],
    },
];

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!(
    "rio-exec's seccomp filter has no ABI table for this target architecture; \
     add one to seccomp.rs (and decide which 32-bit sibling ABIs it must cover)"
);

// ---------------------------------------------------------------------------
// A minimal classic-BPF emitter with label-resolved jumps.
// ---------------------------------------------------------------------------

/// A forward-reference label: created with [`Asm::label`], positioned
/// with [`Asm::bind`], referenced as a [`Jump::To`] target. Every label
/// must be bound exactly once, after every instruction that references
/// it, before [`Asm::finish`].
#[derive(Clone, Copy, Debug)]
struct Label(usize);

/// A conditional-jump target: fall through to the next instruction, or
/// jump to a label whose offset is computed at [`Asm::finish`] time.
#[derive(Clone, Copy)]
enum Jump {
    Next,
    To(Label),
}

/// Which 8-bit field of a `sock_filter` a fixup patches.
#[derive(Clone, Copy)]
enum Field {
    Jt,
    Jf,
}

/// The emitter. Jump offsets are never written by hand: conditional
/// jumps name a [`Label`] and the offset is computed (and bounds-checked
/// against BPF's 8-bit relative-jump range) when the program is
/// finished. Adding a syscall or an ABI to the tables above therefore
/// cannot produce a silently-wrong jump — only a panic at filter-build
/// time, which the unit tests exercise on every change.
struct Asm {
    insns: Vec<sock_filter>,
    /// `(instruction index, field, label)` — patched in [`Asm::finish`].
    fixups: Vec<(usize, Field, Label)>,
    /// `label id -> index of the instruction it points at`.
    bound: Vec<Option<usize>>,
}

impl Asm {
    fn new() -> Self {
        Self {
            insns: Vec::new(),
            fixups: Vec::new(),
            bound: Vec::new(),
        }
    }

    /// Allocate a new, unbound label.
    fn label(&mut self) -> Label {
        self.bound.push(None);
        Label(self.bound.len() - 1)
    }

    /// Bind `l` to the next instruction to be emitted.
    fn bind(&mut self, l: Label) {
        // A hard assert, not a debug_assert: a double-bound label is a
        // silently mis-targeted jump in the finished program. This runs
        // once, in the parent, at filter-build time, where panicking is
        // allowed (and the unit tests build the filter on every run).
        assert!(self.bound[l.0].is_none(), "label bound twice");
        self.bound[l.0] = Some(self.insns.len());
    }

    fn raw(&mut self, code: u32, jt: u8, jf: u8, k: u32) {
        self.insns.push(sock_filter {
            code: code as u16,
            jt,
            jf,
            k,
        });
    }

    /// Record a fixup for a label target; `Jump::Next` is offset 0.
    fn jump_field(&mut self, field: Field, j: Jump) -> u8 {
        match j {
            Jump::Next => 0,
            Jump::To(l) => {
                self.fixups.push((self.insns.len(), field, l));
                0 // patched in finish()
            }
        }
    }

    /// `A = *(u32 *)(seccomp_data + off)`
    fn ld_abs(&mut self, off: u32) {
        self.raw(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0, 0, off);
    }

    /// `if (A == k) goto jt else goto jf`
    fn jeq(&mut self, k: u32, jt: Jump, jf: Jump) {
        let t = self.jump_field(Field::Jt, jt);
        let f = self.jump_field(Field::Jf, jf);
        self.raw(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K, t, f, k);
    }

    /// `if (A & k) goto jt else goto jf`
    fn jset(&mut self, k: u32, jt: Jump, jf: Jump) {
        let t = self.jump_field(Field::Jt, jt);
        let f = self.jump_field(Field::Jf, jf);
        self.raw(libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K, t, f, k);
    }

    /// `return k` (a `SECCOMP_RET_*` action word).
    fn ret(&mut self, k: u32) {
        self.raw(libc::BPF_RET | libc::BPF_K, 0, 0, k);
    }

    /// Patch every label reference and return the finished program.
    ///
    /// Panics on an unbound label, a backward jump, or a jump that does
    /// not fit in BPF's 8-bit relative-offset field — all are bugs in
    /// the generator (caught by the unit tests, which build the filter),
    /// not runtime conditions.
    fn finish(self) -> Vec<sock_filter> {
        let Asm {
            mut insns,
            fixups,
            bound,
        } = self;
        for (at, field, label) in fixups {
            let target = bound[label.0].expect("seccomp filter: jump to an unbound label");
            // A conditional jump at index `at` with offset N lands at
            // `at + 1 + N`.
            let rel = target
                .checked_sub(at + 1)
                .expect("seccomp filter: backward jump");
            let rel =
                u8::try_from(rel).expect("seccomp filter: jump offset exceeds BPF's 8-bit range");
            match field {
                Field::Jt => insns[at].jt = rel,
                Field::Jf => insns[at].jf = rel,
            }
        }
        debug_assert!(insns.len() <= libc::BPF_MAXINSNS as usize);
        insns
    }
}

// ---------------------------------------------------------------------------
// The filter itself.
// ---------------------------------------------------------------------------

/// `SECCOMP_RET_ERRNO` with the errno value in the action's data field.
const fn ret_errno(errno: i32) -> u32 {
    libc::SECCOMP_RET_ERRNO | (errno as u32 & libc::SECCOMP_RET_DATA)
}

/// Assemble the filter program for the build target's ABI set.
///
/// One block per [`AbiTable`] entry:
///
/// ```text
///   ld   arch
///   jeq  <abi.arch>, +0, <next block>
///   ld   nr
///   ; per chmod-like syscall (nr, mode_idx):
///   jeq  <nr>, +0, <skip>      ; not this syscall -> next check
///   ld   args[<mode_idx>].lo32 ; clobbers A
///   jset 0o6000, <EPERM>, +0
///   ld   nr                    ; restore A for the next comparison
///   ; <skip> lands here (A still holds nr on this path)
///   ; per xattr-set syscall:
///   jeq  <nr>, <ENOTSUP>, +0
///   ret  ALLOW                 ; nothing matched for this ABI
///   ret  ERRNO(EPERM)          ; <EPERM>
///   ret  ERRNO(ENOTSUP)        ; <ENOTSUP>
///   ; <next block> lands here
/// ```
///
/// followed by a single `ret KILL_PROCESS` reached only when no block
/// claimed the arch token — defense in depth against an ABI we did not
/// enumerate, unreachable for any ABI a process on a supported kernel
/// can actually produce.
///
/// Built once by the parent before forking (it allocates); the forked
/// child only calls [`install`] on the finished slice.
pub(crate) fn build_filter() -> Vec<sock_filter> {
    let mut a = Asm::new();
    for abi in ABIS {
        let next_block = a.label();
        let eperm = a.label();
        let enotsup = a.label();

        // Select this block iff seccomp_data.arch matches.
        a.ld_abs(OFF_ARCH);
        a.jeq(abi.arch, Jump::Next, Jump::To(next_block));

        // A = syscall number for the rest of the block.
        a.ld_abs(OFF_NR);
        for &(nr, mode_arg) in abi.chmod_like {
            let skip = a.label();
            a.jeq(nr, Jump::Next, Jump::To(skip));
            // Matched the syscall: test the mode argument's setid bits.
            // The argument load clobbers A, so the not-setid path must
            // reload the syscall number before the next comparison.
            a.ld_abs(off_arg_lo(mode_arg));
            a.jset(SETID_BITS, Jump::To(eperm), Jump::Next);
            a.ld_abs(OFF_NR);
            a.bind(skip);
        }
        for &nr in abi.xattr_set {
            a.jeq(nr, Jump::To(enotsup), Jump::Next);
        }

        // No filtered syscall matched under this ABI.
        a.ret(libc::SECCOMP_RET_ALLOW);
        a.bind(eperm);
        a.ret(ret_errno(libc::EPERM));
        a.bind(enotsup);
        // ENOTSUP and EOPNOTSUPP are the same value (95) on Linux; libc
        // only exports the latter spelling for linux-gnu.
        a.ret(ret_errno(libc::EOPNOTSUPP));
        a.bind(next_block);
    }
    // seccomp_data.arch matched no registered ABI block.
    a.ret(libc::SECCOMP_RET_KILL_PROCESS);
    a.finish()
}

// ---------------------------------------------------------------------------
// Installation.
// ---------------------------------------------------------------------------

/// Install `prog` as the calling thread's seccomp filter.
///
/// # Preconditions
///
/// The caller must already have set `PR_SET_NO_NEW_PRIVS` (or hold
/// `CAP_SYS_ADMIN` in its user namespace) — the kernel rejects the
/// filter with `EACCES` otherwise. The sandbox child sequence sets it
/// during its hardening step, before calling this.
///
/// # Async-signal-safety
///
/// This runs in the forked child of a multi-threaded process, between
/// `fork` and `exec`: raw syscalls only, no allocation, no panicking on
/// the success path. `prog` must therefore be built (and its `Vec`
/// allocated) by the parent before forking.
pub(crate) fn install(prog: &[sock_filter]) -> Result<(), Errno> {
    // No length assertion here: this must not panic (it runs post-fork),
    // and the kernel already rejects an empty or over-BPF_MAXINSNS
    // program with EINVAL, which surfaces as the Err return.
    let fprog = libc::sock_fprog {
        len: prog.len() as u16,
        // The kernel never writes through this pointer; the `*mut` is an
        // artifact of the C prototype.
        filter: prog.as_ptr().cast_mut(),
    };
    // SAFETY: `fprog` points at `prog.len()` valid, initialized
    // `sock_filter`s and outlives the call; seccomp(2) copies the
    // program into the kernel before returning.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0u32,
            std::ptr::from_ref(&fprog),
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let err = Errno::last();
    if err != Errno::ENOSYS {
        return Err(err);
    }
    // seccomp(2) postdates filter-mode seccomp by a few releases (3.17
    // vs 3.5); fall back to the prctl spelling on ENOSYS.
    // `PR_SET_SECCOMP` = 22 (include/uapi/linux/prctl.h); libc 0.2 does
    // not export it for linux-gnu.
    const PR_SET_SECCOMP: libc::c_int = 22;
    // SAFETY: same pointer validity argument as above; prctl(2)'s arg3
    // is the `sock_fprog` pointer in `SECCOMP_MODE_FILTER` mode.
    let rc = unsafe {
        libc::prctl(
            PR_SET_SECCOMP,
            libc::c_ulong::from(libc::SECCOMP_MODE_FILTER),
            std::ptr::from_ref(&fprog),
            0,
            0,
        )
    };
    if rc == 0 { Ok(()) } else { Err(Errno::last()) }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd as _;

    use super::*;

    /// `PR_SET_NO_NEW_PRIVS` = 38 (`include/uapi/linux/prctl.h`); libc
    /// 0.2 does not export it for linux-gnu.
    const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;

    // -- a software classic-BPF interpreter ------------------------------

    /// A synthetic `struct seccomp_data`.
    struct Data {
        arch: u32,
        nr: u32,
        args: [u64; 6],
    }

    impl Data {
        fn new(arch: u32, nr: u32) -> Self {
            Self {
                arch,
                nr,
                args: [0; 6],
            }
        }

        fn arg(mut self, i: usize, v: u64) -> Self {
            self.args[i] = v;
            self
        }

        /// The 64-byte native-endian layout the kernel hands to a BPF
        /// filter: `{ s32 nr; u32 arch; u64 ip; u64 args[6]; }`.
        fn to_bytes(&self) -> [u8; 64] {
            let mut b = [0u8; 64];
            b[0..4].copy_from_slice(&self.nr.to_ne_bytes());
            b[4..8].copy_from_slice(&self.arch.to_ne_bytes());
            for (i, arg) in self.args.iter().enumerate() {
                b[16 + 8 * i..16 + 8 * i + 8].copy_from_slice(&arg.to_ne_bytes());
            }
            b
        }
    }

    /// Execute `prog` against `data` in software and return the
    /// `SECCOMP_RET_*` action word. Handles exactly the instruction
    /// classes [`build_filter`] emits and panics on anything else, so an
    /// unhandled opcode is loud rather than silently misinterpreted.
    /// This is what lets the i386/x32/arm blocks be tested without a
    /// 32-bit userspace.
    fn run(prog: &[sock_filter], data: &Data) -> u32 {
        let mem = data.to_bytes();
        let mut a: u32 = 0;
        let mut pc: usize = 0;
        // A classic-BPF program with no backward jumps executes at most
        // `len` instructions; the bound makes a non-terminating program
        // a panic instead of a hang.
        for _ in 0..=prog.len() {
            let i = prog
                .get(pc)
                .unwrap_or_else(|| panic!("pc {pc} ran off the end of the program"));
            let code = u32::from(i.code);
            if code == libc::BPF_LD | libc::BPF_W | libc::BPF_ABS {
                let off = i.k as usize;
                assert!(off + 4 <= mem.len(), "ld_abs offset {off} out of range");
                a = u32::from_ne_bytes(mem[off..off + 4].try_into().unwrap());
                pc += 1;
            } else if code == libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K {
                pc += 1 + usize::from(if a == i.k { i.jt } else { i.jf });
            } else if code == libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K {
                pc += 1 + usize::from(if a & i.k != 0 { i.jt } else { i.jf });
            } else if code == libc::BPF_JMP | libc::BPF_JA {
                pc += 1 + i.k as usize;
            } else if code == libc::BPF_RET | libc::BPF_K {
                return i.k;
            } else {
                panic!("unhandled BPF opcode {code:#06x} at pc {pc}");
            }
        }
        panic!("program did not terminate within {} steps", prog.len() + 1);
    }

    const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
    const KILL: u32 = libc::SECCOMP_RET_KILL_PROCESS;
    const EPERM_RET: u32 = ret_errno(libc::EPERM);
    const ENOTSUP_RET: u32 = ret_errno(libc::EOPNOTSUPP);

    // -- table-driven coverage of every ABI block ------------------------

    /// Every chmod-family entry of every registered ABI: setuid and
    /// setgid modes are EPERM, plain modes are allowed, and setid bits
    /// in a *different* argument slot do not trigger the filter.
    #[test]
    fn chmod_family_setid_modes_are_eperm_per_abi() {
        let prog = build_filter();
        for abi in ABIS {
            for &(nr, mode_arg) in abi.chmod_like {
                for mode in [0o4755u64, 0o2644, 0o6777] {
                    let d = Data::new(abi.arch, nr).arg(mode_arg as usize, mode);
                    assert_eq!(
                        run(&prog, &d),
                        EPERM_RET,
                        "{}: nr {nr:#x} mode {mode:o} should be EPERM",
                        abi.name
                    );
                }
                for mode in [0o755u64, 0o644, 0o1777] {
                    let d = Data::new(abi.arch, nr).arg(mode_arg as usize, mode);
                    assert_eq!(
                        run(&prog, &d),
                        ALLOW,
                        "{}: nr {nr:#x} mode {mode:o} should be allowed",
                        abi.name
                    );
                }
                // A setid pattern in a different argument slot must not
                // trigger the filter: the filter must test the *mode*
                // argument, not whatever pointer happens to be there.
                let other = if mode_arg == 1 { 2 } else { 1 };
                let d = Data::new(abi.arch, nr).arg(other, 0o6777);
                assert_eq!(
                    run(&prog, &d),
                    ALLOW,
                    "{}: nr {nr:#x} with setid bits in args[{other}] (not the mode arg) \
                     should be allowed",
                    abi.name
                );
            }
        }
    }

    /// Every xattr-set entry of every registered ABI returns ENOTSUP
    /// regardless of arguments.
    #[test]
    fn xattr_set_family_is_enotsup_per_abi() {
        let prog = build_filter();
        for abi in ABIS {
            for &nr in abi.xattr_set {
                let d = Data::new(abi.arch, nr).arg(1, 0o6777);
                assert_eq!(
                    run(&prog, &d),
                    ENOTSUP_RET,
                    "{}: xattr-set nr {nr:#x} should be ENOTSUP",
                    abi.name
                );
            }
        }
    }

    /// Unfiltered syscalls are allowed under every registered ABI; an
    /// arch token we did not register is killed.
    #[test]
    fn default_allow_and_unknown_arch_kill() {
        let prog = build_filter();
        for abi in ABIS {
            // A number that is not in any table for any ABI.
            let unfiltered = 0x0003_0000;
            assert_eq!(
                run(&prog, &Data::new(abi.arch, unfiltered)),
                ALLOW,
                "{}: an unfiltered syscall must be allowed",
                abi.name
            );
        }
        // AUDIT_ARCH_PPC64 (EM_PPC64=21 | 64BIT) — never registered.
        assert_eq!(run(&prog, &Data::new(21 | 0x8000_0000, 1)), KILL);
        // arch 0 (malformed) is also killed.
        assert_eq!(run(&prog, &Data::new(0, 1)), KILL);
    }

    /// Syscall numbers must be scoped to their arch block: i386's
    /// `chmod` is nr 15, which under `AUDIT_ARCH_X86_64` is
    /// `rt_sigreturn` and must NOT be filtered; x86_64's `chmod`
    /// (nr 90) under `AUDIT_ARCH_I386` is `old_mmap` and must not be
    /// filtered there.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn syscall_numbers_are_scoped_to_their_arch() {
        let prog = build_filter();
        let d = Data::new(AUDIT_ARCH_X86_64, 15).arg(1, 0o4755);
        assert_eq!(run(&prog, &d), ALLOW, "nr 15 under x86_64 is not chmod");
        let d = Data::new(AUDIT_ARCH_I386, 90).arg(1, 0o4755);
        assert_eq!(run(&prog, &d), ALLOW, "nr 90 under i386 is not chmod");
        let d = Data::new(AUDIT_ARCH_I386, 15).arg(1, 0o4755);
        assert_eq!(run(&prog, &d), EPERM_RET, "nr 15 under i386 IS chmod");
    }

    /// An x32-flagged syscall number (bit 30 set, still tagged
    /// `AUDIT_ARCH_X86_64`) hits the filter; without the x32 table
    /// entries it would fall through to ALLOW.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x32_flagged_syscalls_are_filtered() {
        let prog = build_filter();
        let nr = libc::SYS_chmod as u32 | X32_SYSCALL_BIT;
        let d = Data::new(AUDIT_ARCH_X86_64, nr).arg(1, 0o4755);
        assert_eq!(run(&prog, &d), EPERM_RET);
        let nr = libc::SYS_setxattr as u32 | X32_SYSCALL_BIT;
        assert_eq!(run(&prog, &Data::new(AUDIT_ARCH_X86_64, nr)), ENOTSUP_RET);
    }

    /// The mode-argument index is per-syscall: `fchmodat`'s mode is
    /// `args[2]`, and a setid value in `args[1]` (the pathname pointer
    /// slot) must not trigger the filter.
    #[test]
    fn fchmodat_tests_args2_not_args1() {
        let prog = build_filter();
        let abi = &ABIS[0];
        let (nr, mode_arg) = *abi
            .chmod_like
            .iter()
            .find(|(_, idx)| *idx == MODE_ARG_CHMODAT)
            .expect("every ABI table has at least one *at chmod variant");
        assert_eq!(mode_arg, MODE_ARG_CHMODAT);
        assert_eq!(
            run(&prog, &Data::new(abi.arch, nr).arg(1, 0o4755)),
            ALLOW,
            "setid bits in args[1] (the pathname pointer) must not trip the args[2] mode check"
        );
        assert_eq!(
            run(&prog, &Data::new(abi.arch, nr).arg(2, 0o4755)),
            EPERM_RET
        );
    }

    /// Structural sanity: every conditional jump lands inside the
    /// program, the program ends with the unknown-arch kill, and the
    /// instruction count is far below `BPF_MAXINSNS`.
    #[test]
    fn program_is_structurally_valid() {
        let prog = build_filter();
        assert!(
            prog.len() < 256,
            "program grew past the range where intra-block jumps are trivially safe; re-audit"
        );
        for (i, insn) in prog.iter().enumerate() {
            let code = u32::from(insn.code);
            let is_cond_jump =
                (code & 0x07) == libc::BPF_JMP && code != (libc::BPF_JMP | libc::BPF_JA);
            if is_cond_jump {
                assert!(
                    i + 1 + usize::from(insn.jt) < prog.len(),
                    "jt at {i} jumps past the end"
                );
                assert!(
                    i + 1 + usize::from(insn.jf) < prog.len(),
                    "jf at {i} jumps past the end"
                );
            }
        }
        let last = prog.last().expect("program is non-empty");
        assert_eq!(u32::from(last.code), libc::BPF_RET | libc::BPF_K);
        assert_eq!(last.k, KILL);
    }

    // -- the real thing: install the filter in a forked child ------------

    /// Bit set in the child's exit code for each assertion that held.
    const OK_PLAIN_CHMOD: i32 = 1 << 0;
    const OK_SETUID_CHMOD_EPERM: i32 = 1 << 1;
    const OK_SETGID_FCHMOD_EPERM: i32 = 1 << 2;
    const OK_SETXATTR_FAILS: i32 = 1 << 3;
    const OK_ALL: i32 =
        OK_PLAIN_CHMOD | OK_SETUID_CHMOD_EPERM | OK_SETGID_FCHMOD_EPERM | OK_SETXATTR_FAILS;

    /// Fork, install the real filter in the child under
    /// `PR_SET_NO_NEW_PRIVS`, and verify the kernel enforces it: a plain
    /// chmod succeeds, setuid/setgid chmods fail `EPERM`, and setxattr
    /// fails. Unprivileged seccomp under no_new_privs needs no
    /// capabilities, so this runs in a plain `cargo nextest run`.
    #[test]
    fn installed_filter_is_enforced_by_the_kernel() {
        // Everything the child needs is prepared before the fork: the
        // program (the child must not allocate), an open fd, and a
        // NUL-terminated path to a scratch file it can chmod.
        let prog = build_filter();
        let path =
            std::env::temp_dir().join(format!("rio-exec-seccomp-test-{}", std::process::id()));
        let file = std::fs::File::create(&path).expect("create scratch file");
        let mut cpath = std::ffi::OsString::from(&path).into_encoded_bytes();
        cpath.push(0);

        /// Remove the scratch file even if an assertion below panics.
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = Cleanup(path.clone());

        // SAFETY: the child executes only async-signal-safe libc calls
        // (prctl, seccomp, chmod, fchmod, setxattr, _exit) on
        // pre-allocated buffers and exits via `_exit` without returning
        // into Rust.
        match unsafe { nix::unistd::fork() }.expect("fork") {
            nix::unistd::ForkResult::Child => {
                let mut ok = 0i32;
                // SAFETY: raw syscalls on pre-built NUL-terminated
                // buffers; no allocation; the process never returns from
                // `_exit`.
                unsafe {
                    if libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                        libc::_exit(100);
                    }
                    if install(&prog).is_err() {
                        libc::_exit(101);
                    }
                    let p = cpath.as_ptr().cast::<libc::c_char>();
                    // Plain chmod must still work.
                    if libc::chmod(p, 0o644) == 0 {
                        ok |= OK_PLAIN_CHMOD;
                    }
                    // Setuid chmod must fail with EPERM specifically.
                    if libc::chmod(p, 0o4755) == -1 && *libc::__errno_location() == libc::EPERM {
                        ok |= OK_SETUID_CHMOD_EPERM;
                    }
                    // Setgid fchmod must fail with EPERM specifically.
                    if libc::fchmod(file.as_raw_fd(), 0o2755) == -1
                        && *libc::__errno_location() == libc::EPERM
                    {
                        ok |= OK_SETGID_FCHMOD_EPERM;
                    }
                    // setxattr must fail. The filter returns ENOTSUP;
                    // accept any failure errno so a pre-existing LSM
                    // denying it differently does not flake the test —
                    // what matters is that it cannot succeed.
                    if libc::setxattr(
                        p,
                        c"user.rio_exec_test".as_ptr(),
                        b"x".as_ptr().cast(),
                        1,
                        0,
                    ) == -1
                    {
                        ok |= OK_SETXATTR_FAILS;
                    }
                    libc::_exit(ok);
                }
            }
            nix::unistd::ForkResult::Parent { child } => {
                let status = nix::sys::wait::waitpid(child, None).expect("waitpid");
                let nix::sys::wait::WaitStatus::Exited(_, code) = status else {
                    panic!("child did not exit cleanly: {status:?}");
                };
                assert_ne!(code, 100, "child failed to set PR_SET_NO_NEW_PRIVS");
                assert_ne!(code, 101, "child failed to install the seccomp filter");
                assert_eq!(
                    code, OK_ALL,
                    "kernel-enforced filter behavior mask: got {code:#06b}, want {OK_ALL:#06b} \
                     (bit 0: plain chmod allowed; bit 1: setuid chmod EPERM; \
                     bit 2: setgid fchmod EPERM; bit 3: setxattr failed)"
                );
            }
        }
    }
}
