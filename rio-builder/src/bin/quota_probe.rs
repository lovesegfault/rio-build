//! quota_probe — the prjquota classification chain, runnable against a
//! real directory (merged_bug_074's satisfiability witness, WO-S2-2).
//!
//! Drives the PRODUCTION classifier inputs exactly as the executor's
//! result seam does — `quota::status` (usage + hard limit via
//! `FS_IOC_FSGETXATTR` + `quotactl_fd`), the COUPLED node sample
//! (`node_free_bytes` on the quota'd dir itself — the retired vantage
//! the kernel clamps to the project view), the DECOUPLED sample
//! (`node_free_bytes_decoupled` — the unowned-ancestor walk), the
//! clamp detector, and `classify_quota_exhaustion` over both vantage
//! pairs — and prints one `key=value` line each, so a VM test can
//! assert the kernel-coupled truth table that unit tests structurally
//! cannot witness (the prjquota VM probe scenario;
//! `nix/tests/scenarios/quota-probe.nix`).
//!
//! Also a node-debugging tool: run it on any builder node against an
//! emptyDir to see what the classifier would conclude there.
//!
//! Output grammar (one per line, `none` for absent samples):
//!   projid, quota_used, quota_limit, coupled_node_free,
//!   coupled_clamped, decoupled_node_free, classify_coupled,
//!   classify_decoupled
//!
//! `--ensure` (live_063): run the PRODUCTION acquisition face
//! (`quota::ensure_project_quota` — the exact fn `setup_overlay`
//! calls under hostUsers:true) against the dir BEFORE sampling, and
//! prepend `ensure=<existing|minted|unavailable>` to the kv block.
//! This is the R13 feeder for the kubelet-projquota witness's
//! provisioned × hostUsers:true cell: the in-pod invocation stands in
//! for setup_overlay (the setup_overlay→ensure threading itself is
//! pinned at unit level in rio-builder, same tier split as the
//! completion-row threading disclosed in the scenario header).

use std::path::Path;

use rio_builder::quota;

fn fmt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "none".to_string(), |x| x.to_string())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (ensure, dir_arg) = match args.next() {
        Some(a) if a == "--ensure" => (true, args.next()),
        other => (false, other),
    };
    let Some(dir) = dir_arg else {
        eprintln!("usage: quota_probe [--ensure] <dir> [<sibling>]");
        std::process::exit(2);
    };
    let dir = Path::new(&dir);

    if ensure {
        let verdict = quota::ensure_project_quota(dir);
        let label = match verdict {
            quota::ProjQuota::Existing(_) => "existing",
            quota::ProjQuota::Minted(_) => "minted",
            quota::ProjQuota::Unavailable => "unavailable",
        };
        println!("ensure={label}");
    }

    let projid = quota::project_id(dir);
    let status = quota::status(dir).ok().flatten();
    let used = status.map(|q| q.used_bytes);
    let limit = status.and_then(|q| q.hard_limit_bytes);

    // The retired vantage: statvfs of the quota'd dir itself. Under
    // enforced prjquota + PROJINHERIT the kernel clamps this to the
    // project view (f_bavail = limit - used, f_blocks ~= limit).
    let coupled_free = quota::node_free_bytes(dir);
    let coupled_clamped = match (quota::fs_capacity_bytes(dir), limit) {
        (Some((blocks, frsize)), Some(l)) => Some(quota::statvfs_clamped(blocks, frsize, l)),
        _ => None,
    };

    // The decoupled vantage: the first same-device ancestor that is
    // neither project-owned nor clamp-shaped, or the named sibling
    // mount (the in-pod fallback — merged_bug_012). The probe takes
    // an optional second positional path so the kubelet-projquota VM
    // cells can pass the fuse-cache sibling exactly as the executor
    // does in-pod.
    let sibling = args.next();
    let decoupled_free =
        quota::node_free_bytes_decoupled(dir, limit, sibling.as_ref().map(Path::new));

    // The classification fold, exactly as the executor's result seam
    // evaluates it (absent inputs => no attribution).
    let classify = |free: Option<u64>| match (status, free) {
        (Some(q), Some(f)) => quota::classify_quota_exhaustion(q, f),
        _ => false,
    };

    println!("projid={}", fmt(projid));
    println!("quota_used={}", fmt(used));
    println!("quota_limit={}", fmt(limit));
    // D-2: the typed enforcement-posture letter (the same alphabet as
    // the rio_builder_quota_enforcement gauge label).
    println!(
        "enforcement={}",
        quota::QuotaEnforcement::classify(status).label()
    );
    println!("coupled_node_free={}", fmt(coupled_free));
    println!("coupled_clamped={}", fmt(coupled_clamped));
    println!("decoupled_node_free={}", fmt(decoupled_free));
    println!("classify_coupled={}", classify(coupled_free));
    println!("classify_decoupled={}", classify(decoupled_free));
}
