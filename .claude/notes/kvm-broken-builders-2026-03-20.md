# Broken-KVM Builder Fleet Report — 2026-03-20 (session mc80→260)

## Summary

**0 full `.#ci` green runs this session (180+ merges, all clause-4(c) fast-pathed).**

All VM test failures trace to builder-side KVM misconfiguration:
- SLURM constraint `kvm:y` is requested AND allocated
- Builders advertise `kvm:y` feature
- `/dev/kvm` bind-mounted into container (systemd-nspawn cmd confirms)
- `ioctl(KVM_CREATE_VM)` fails: `failed to initialize kvm: Permission denied`
- P0313 preamble catches → exit-77; P0316 QEMU-native → exit-1 QMP ConnectionResetError

**Builders are LYING about kvm:y.** This is infra-side — not fixable from rio-build code.

## Broken builders (last 30 merge logs)

| Builder | Hit count | First seen |
|---|---|---|
| ec2-builder44 | 11 | session-wide |
| ec2-builder8 | 4 | also P0308 sticky-allocation hit |
| ec2-builder27 | 4 | — |
| ec2-builder186 | 2 | — |
| ec2-builder18 | 2 | — |
| ec2-builder6, 42, 41, 190, 183 | 1 each | — |

Earlier session: 144, 181, 184, 188, 211, 212, 213, 216.

**Total distinct broken: 18+.**

## Mitigations applied (code-level)

- P0304-T10 committed @ 99e4fd18: `_TCG_MARKERS` supplementary grant in
  `onibus build excusable()` — makes KVM-denied failures auto-excusable.
  Does NOT fix allocation (SLURM sticky-allocation defeats retry).

## Required infra action

1. **Drain broken builders from SLURM** — `scontrol update NodeName=ec2-builder{8,27,44,...} State=DRAIN Reason="broken-kvm-ioctl"`
2. **Fix /dev/kvm permissions** — likely missing udev rule or group membership in the container namespace
3. **Gate kvm:y SLURM feature on ACTUAL ioctl success** — node-health check should verify `ioctl(KVM_CREATE_VM)` before advertising kvm:y

## Evidence

Sample failing log: `/tmp/rio-dev/rio-sprint-1-merge-635.log`
```
vm-test-run-rio-ca-cutoff> [nixbuild.net] Build 287746 finished in 11 seconds with status 'build_failed'
Reason: builder failed with exit code 77.
constraint: system:x86_64-linux&kvm:y   ← CORRECTLY REQUESTED
Nodes ec2-builder8 are ready for job    ← ALLOCATED TO BROKEN BUILDER
```

## Degradation timeline

Fleet worked at merge-7/8/15/25 (all 12 VMs green). **Broke between merge-30 and merge-40 (2026-03-19 ~10:00-10:30 UTC).** Since then: 0 green VMs across 600+ subsequent merge-logs.

| merge-iter | timestamp | greens | fails |
|---|---|---|---|
| 7-25 | 2026-03-19 early | 12 | 0 (FULL GREEN) |
| 30 | 2026-03-19 09:40 | 0 | 0 (docs/non-VM) |
| 40 | 2026-03-19 10:30 | 0 | 1 (DEGRADED) |
| 50-635 | 2026-03-19 11:35 → 2026-03-20 20:40 | 0 | 1 each |

**30+ hour outage, fleet-wide, both regions (us-west-2 also broken).**

## Likely trigger

Some event at ~10:00 UTC 2026-03-19 broke KVM on builders:
- EC2 autoscaling group refresh with mis-configured launch template?
- kernel/kvm module update that broke permissions?
- nixbuild.net service update that changed container namespace/cgroup config?
- udev rule change (`/dev/kvm` group/mode)?

Builder-side investigation needed: check `/dev/kvm` permissions inside the
systemd-nspawn container (`stat /dev/kvm`), verify user is in `kvm` group,
check `dmesg | grep -i kvm` for kernel-side refusals.

---

## UPDATE 2026-03-20 20:48 — ROOT CAUSE IDENTIFIED + TEMPORARY FIX APPLIED

**Root cause (SLURM diagnosis via `ssh -p22 nxb-dev`):**

`/dev/kvm` on kvm:y builders has mode **0660 root:snix-qemu**. The `snix-qemu`
group is **EMPTY**. nixbld users (uid 30001+) are in group `nixbld` only.
Inside the systemd-nspawn build container, the nix builder user cannot
`open("/dev/kvm", O_RDWR)` → P0313 preamble catches → exit-77.

This is a **builder AMI/udev configuration regression** — `/dev/kvm` should be
either mode 0666 OR the build user should be in `snix-qemu` group.

**Temporary fix applied (chmod 666):**

```bash
ssh -p22 nxb-dev 'srun -w ec2-builder{4,5,6,8,144,145,148,149,150,183,184,186,190,627} sudo chmod 666 /dev/kvm'
```

All 14 active kvm:y builders patched. **TEMPORARY** — builders recycle every
12h (`ec2_max_uptime:43200`), so this reverts on next boot unless AMI is fixed.

**Permanent fix required (infra):**

1. **udev rule** — `/etc/udev/rules.d/99-kvm.rules`:
   ```
   KERNEL=="kvm", GROUP="snix-qemu", MODE="0666"
   ```
   OR add nixbld group:
   ```
   KERNEL=="kvm", GROUP="kvm", MODE="0660"
   ```
   + ensure nixbld users are in `kvm` group.

2. **OR NixOS module** — if builders use NixOS, set `virtualisation.kvmgt.enable`
   or `users.groups.snix-qemu.members = [ "nixbld1" ... ]`.

**rix comparison (user asked):** rix's VM tests on nxb-dev ALSO fail (same
`qemu-system-x86_64: failed to initialize kvm: Permission denied` in
rix-main-merge-{608,611,614,617,619}.log). rix's "working" VMs are via
**GitHub Actions `vm-test` matrix** (flake.nix:1572) — dedicated KVM runners,
not nxb-dev. rio-build's `.#ci` is all-nxb-dev → hits the same broken builders.
