# quota-probe — the prjquota satisfiability witness (merged_bug_074,
# WO-S2-2 / OQ-13): a REAL XFS project quota, filled to its hard
# limit, driven through rio-builder's PRODUCTION classifier chain via
# the `quota_probe` bin. Unit truth-tables structurally cannot witness
# kernel-coupled input spaces — this scenario proves, against the
# kernel:
#
#   1. THE CLAMP IS REAL: statvfs taken inside the project view is
#      clamped (`f_blocks ~= hard limit`, `f_bavail = limit - used`)
#      — the `statvfs_clamped` detector fires on the coupled vantage.
#   2. THE DEAD LETTER (the retired sampling's red, evaluated live):
#      with the quota EXHAUSTED, the coupled-vantage classification —
#      both conjuncts sampled from the quota'd dir, exactly the
#      pre-fix executor seam — is FALSE: the clamp forces
#      node_free = limit - used <= slack < headroom whenever the
#      quota conjunct holds. The conjuncts are mutually exclusive;
#      the DiskFull letter could never be emitted where it mattered.
#   3. THE LETTER FIRES: the decoupled-vantage classification (the
#      unowned-ancestor walk) is TRUE on the same kernel state.
#   4. THE PEAK PREMISE: after the fill is deleted (the keep-failed
#      cleanup shape — the daemon deletes a failed build's scratch
#      before the post-daemon one-shot), the one-shot usage collapses
#      and no vantage classifies — the during-build 1Hz peak monitor
#      (executor/monitors.rs) exists precisely because of this cell.
#
# Project setup mirrors kubelet's emptyDir assignment (project id +
# PROJINHERIT via `xfs_quota project -s`, hard limit via `limit -p`)
# on an XFS loopback mounted `-o prjquota`. Single node, no k8s — the
# witness is the kernel/classifier coupling, not the pipeline (the
# floor-bump consumption of the letter is pinned at unit level in
# rio-scheduler, and the wire field family carries it; the
# composition seam is `.fields`-pinned).
#
# Budget (R17): single-VM boot + mkfs + a 256 MiB dd fill — observed
# well under 120s on KVM; 420s leaves builder-variance headroom per
# the catalogued coverage-mode class.
{
  pkgs,
  common,
}:
let
  inherit (common) rio-workspace;
  probe = "${rio-workspace}/bin/quota_probe";
  mib = toString (256 * 1024 * 1024);
in
pkgs.testers.runNixOSTest {
  name = "rio-quota-probe";
  skipTypeCheck = true;
  globalTimeout = 420 + common.covTimeoutHeadroom;

  nodes.machine = {
    environment.systemPackages = [ pkgs.xfsprogs ];
    # 4 GiB sparse loop image on the root disk; the fill writes only
    # the 256 MiB hard limit for real.
    virtualisation.diskSize = 6144;
    virtualisation.memorySize = 1024;
  };

  testScript = ''
    ${common.assertions}

    ${common.kvmCheck}
    start_all()
    machine.wait_for_unit("multi-user.target")

    def probe_kv():
        """Run the production classifier chain against the project dir
        and parse the key=value grammar."""
        out = machine.succeed("${probe} /mnt/quota/build")
        return dict(line.split("=", 1) for line in out.strip().splitlines())

    with subtest("xfs prjquota fixture: loopback + project + hard limit"):
        machine.succeed(
            "truncate -s 4G /var/lib/quota.img",
            "mkfs.xfs -q /var/lib/quota.img",
            "mkdir -p /mnt/quota",
            "mount -o loop,prjquota /var/lib/quota.img /mnt/quota",
            # The kubelet-parity assignment: project id + PROJINHERIT
            # on the build dir, then the enforced hard limit.
            "mkdir -p /mnt/quota/build",
            "xfs_quota -x -c 'project -s -p /mnt/quota/build 42' /mnt/quota",
            "xfs_quota -x -c 'limit -p bhard=256m 42' /mnt/quota",
        )
        out = probe_kv()
        assert out["projid"] == "42", f"project id assigned: {out}"
        assert out["quota_limit"] == "${mib}", f"hard limit enforced: {out}"

    with subtest("the kernel clamp is real and the coupled letter is dead"):
        # Fill the project to its hard limit (dd hits EDQUOT/ENOSPC —
        # expected; the bytes that DID land stay, like a live build's
        # scratch at the exhaustion instant).
        machine.succeed(
            "dd if=/dev/zero of=/mnt/quota/build/fill bs=1M count=512 conv=fsync 2>&1 | tail -2 || true",
            "sync",
        )
        out = probe_kv()
        used = int(out["quota_used"])
        limit = int(out["quota_limit"])
        slack = 64 * 1024 * 1024
        assert used >= limit - slack, f"quota exhausted within the slack band: {out}"
        # (1) the clamp: the quota'd dir's own statvfs is the PROJECT
        # view — the in-tree detector fires on it.
        assert out["coupled_clamped"] == "true", f"statvfs clamped to the project: {out}"
        coupled_free = int(out["coupled_node_free"])
        assert coupled_free <= slack, (
            f"the clamp forces coupled node_free = limit - used <= slack: {out}"
        )
        # (2) THE DEAD LETTER: the retired same-dir conjunct pair is
        # kernel-unsatisfiable — exhausted quota AND >=2GiB 'node'
        # headroom cannot both hold through the clamped vantage.
        assert out["classify_coupled"] == "false", (
            f"the coupled vantage can never classify (the dead letter): {out}"
        )
        # (3) the fix: the decoupled ancestor walk sees the real
        # filesystem headroom and the letter fires.
        decoupled_free = int(out["decoupled_node_free"])
        assert decoupled_free >= 2 * 1024 * 1024 * 1024, (
            f"the unowned ancestor reports the node view: {out}"
        )
        assert out["classify_decoupled"] == "true", (
            f"DiskFull is satisfiable under real exhaustion post-fix: {out}"
        )

    with subtest("the keep-failed cleanup shape motivates the peak monitor"):
        machine.succeed("rm /mnt/quota/build/fill", "sync")
        out = probe_kv()
        assert int(out["quota_used"]) < 64 * 1024 * 1024, (
            f"the one-shot usage collapses after cleanup: {out}"
        )
        assert out["classify_decoupled"] == "false", (
            "no vantage classifies after the scratch is deleted; the "
            f"during-build peak poller is the only honest usage source: {out}"
        )
  '';
}
