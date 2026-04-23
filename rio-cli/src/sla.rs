//! `rio-cli sla` — ADR-023 operator overrides + model status.
//!
//! `override`/`reset`/`status`/`explain`/`export-corpus`/`import-corpus`
//! call the phase-6/7/11 AdminService RPCs. `defaults` is local-only
//! (prints the configured tier ladder via `SlaStatus` on an empty key —
//! server returns `has_fit=false` but the CLI doesn't yet surface
//! config; phase-7 wires `SlaConfig` proto).

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use clap::Subcommand;

use crate::AdminClient;
use rio_proto::types::{
    ExportSlaCorpusRequest, ImportSlaCorpusRequest, ListSlaOverridesRequest, ResetSlaModelRequest,
    SetSlaOverrideRequest, SlaExplainRequest, SlaOverride, SlaStatusRequest,
};

#[derive(Subcommand, Clone)]
pub enum SlaCmd {
    /// Pin a (pname, system?, tenant?) key to a forced tier / cores /
    /// mem. NULL system/tenant are wildcards (most-specific match wins).
    /// Picked up by the next estimator refresh tick (~60s).
    Override {
        pname: String,
        #[arg(long)]
        system: Option<String>,
        #[arg(long)]
        tenant: Option<String>,
        /// Named tier from `[sla].tiers` (e.g. `fast`).
        #[arg(long)]
        tier: Option<String>,
        /// Forced core count. Bypasses the fitted-curve solve entirely.
        #[arg(long)]
        cores: Option<f64>,
        /// Forced memory. Accepts `8Gi`, `512Mi`, or raw bytes.
        #[arg(long)]
        mem: Option<String>,
        /// Expiry as a duration from now (e.g. `2h`, `7d`). Unset =
        /// never expires.
        #[arg(long)]
        ttl: Option<String>,
    },
    /// List non-expired overrides (optionally filtered by pname).
    List {
        #[arg(long)]
        pname: Option<String>,
    },
    /// Delete one override by id (from `sla list`).
    Clear { id: i64 },
    /// Drop all build_samples for one key and evict the cached fit.
    /// Next dispatch falls back to the cold-start probe.
    Reset {
        pname: String,
        #[arg(long, default_value = "x86_64-linux")]
        system: String,
        #[arg(long, default_value = "")]
        tenant: String,
    },
    /// Dump the cached fit for one key (+ any active override).
    Status {
        pname: String,
        #[arg(long, default_value = "x86_64-linux")]
        system: String,
        #[arg(long, default_value = "")]
        tenant: String,
    },
    /// Print the configured tier ladder + probe shape.
    Defaults,
    /// Per-derivation solve trace: re-runs the tier walk in dry-run
    /// mode and prints a candidate table showing why each tier was
    /// accepted or rejected.
    Explain {
        pname: String,
        #[arg(long, default_value = "x86_64-linux")]
        system: String,
        #[arg(long, default_value = "")]
        tenant: String,
    },
    /// Dump every cached fit with n_eff ≥ min_n to a JSON file.
    /// `import-corpus` on a fresh deployment loads this into the
    /// seed-prior table so first-dispatch of a known pname skips the
    /// cold-start probe ladder.
    ExportCorpus {
        #[arg(long)]
        tenant: Option<String>,
        #[arg(long, default_value = "3")]
        min_n: u32,
        #[arg(short)]
        out: PathBuf,
    },
    /// Load a seed corpus (from `export-corpus`, possibly another
    /// cluster) into the seed-prior table. In-memory only — set
    /// `[sla].seed_corpus` for restart-survival.
    ImportCorpus { path: PathBuf },
}

// r[impl cli.cmd.sla]
pub(crate) async fn run(
    as_json: bool,
    client: &mut AdminClient,
    cmd: SlaCmd,
) -> anyhow::Result<()> {
    match cmd {
        SlaCmd::Override {
            pname,
            system,
            tenant,
            tier,
            cores,
            mem,
            ttl,
        } => {
            let mem_bytes = mem.as_deref().map(parse_bytes).transpose()?;
            let expires_at_epoch = ttl.as_deref().map(parse_ttl).transpose()?;
            let req = SetSlaOverrideRequest {
                r#override: Some(SlaOverride {
                    pname,
                    system,
                    tenant,
                    tier,
                    cores,
                    mem_bytes,
                    expires_at_epoch,
                    // Audit stamp. Best-effort — empty if $USER unset.
                    created_by: std::env::var("USER").ok(),
                    ..Default::default()
                }),
            };
            let resp = crate::rpc("SetSlaOverride", async || {
                client.set_sla_override(req.clone()).await
            })
            .await?;
            if as_json {
                return crate::json(&resp);
            }
            println!("override #{} set for {}", resp.id, resp.pname);
            Ok(())
        }
        SlaCmd::List { pname } => {
            let req = ListSlaOverridesRequest { pname };
            let resp = crate::rpc("ListSlaOverrides", async || {
                client.list_sla_overrides(req.clone()).await
            })
            .await?;
            if as_json {
                return crate::json(&resp);
            }
            if resp.overrides.is_empty() {
                println!("(no overrides)");
                return Ok(());
            }
            println!(
                "{:<6} {:<24} {:<16} {:<12} {:<8} {:<8} {:<10}",
                "ID", "PNAME", "SYSTEM", "TENANT", "TIER", "CORES", "MEM"
            );
            for o in &resp.overrides {
                println!(
                    "{:<6} {:<24} {:<16} {:<12} {:<8} {:<8} {:<10}",
                    o.id,
                    o.pname,
                    o.system.as_deref().unwrap_or("*"),
                    o.tenant.as_deref().unwrap_or("*"),
                    o.tier.as_deref().unwrap_or("-"),
                    o.cores.map(|c| format!("{c}")).unwrap_or("-".into()),
                    o.mem_bytes.map(fmt_bytes).unwrap_or("-".into()),
                );
            }
            Ok(())
        }
        SlaCmd::Clear { id } => {
            let req = rio_proto::types::ClearSlaOverrideRequest { id };
            crate::rpc("ClearSlaOverride", async || {
                client.clear_sla_override(req).await
            })
            .await?;
            if as_json {
                return crate::json(&serde_json::json!({"cleared": id}));
            }
            println!("override #{id} cleared");
            Ok(())
        }
        SlaCmd::Reset {
            pname,
            system,
            tenant,
        } => {
            let req = ResetSlaModelRequest {
                pname: pname.clone(),
                system,
                tenant,
            };
            crate::rpc("ResetSlaModel", async || {
                client.reset_sla_model(req.clone()).await
            })
            .await?;
            if as_json {
                return crate::json(&serde_json::json!({"reset": pname}));
            }
            println!("model reset for {pname}; next dispatch uses cold-start probe");
            Ok(())
        }
        SlaCmd::Status {
            pname,
            system,
            tenant,
        } => {
            let req = SlaStatusRequest {
                pname: pname.clone(),
                system: system.clone(),
                tenant,
            };
            let resp =
                crate::rpc("SlaStatus", async || client.sla_status(req.clone()).await).await?;
            if as_json {
                return crate::json(&resp);
            }
            println!("Key:       {pname} ({system})");
            if !resp.has_fit {
                println!("Fit:       (no cached fit — cold-start probe path)");
            } else {
                println!(
                    "Fit:       {} S={:.1}s P={:.1}s Q={:.4} p̄={:.1}",
                    resp.fit_kind, resp.s, resp.p, resp.q, resp.p_bar
                );
                println!(
                    "Mem:       {} p90={}",
                    resp.mem_kind,
                    fmt_bytes_u(resp.mem_p90_bytes)
                );
                println!(
                    "Stats:     n_eff={:.1} span={:.1} σ={:.3} tier={} prior={}",
                    resp.n_eff,
                    resp.span,
                    resp.sigma_resid,
                    resp.tier.as_deref().unwrap_or("-"),
                    if resp.prior_source.is_empty() {
                        "-"
                    } else {
                        &resp.prior_source
                    },
                );
            }
            if let Some(o) = &resp.active_override {
                println!(
                    "Override:  #{} tier={} cores={} mem={} (by {})",
                    o.id,
                    o.tier.as_deref().unwrap_or("-"),
                    o.cores.map(|c| format!("{c}")).unwrap_or("-".into()),
                    o.mem_bytes.map(fmt_bytes).unwrap_or("-".into()),
                    o.created_by.as_deref().unwrap_or("?"),
                );
            }
            Ok(())
        }
        SlaCmd::Defaults => {
            // TODO: phase-7 — surface `[sla].tiers` / `probe` via a
            // proto field on SlaStatusResponse or a dedicated RPC.
            // Phase-6: print a pointer so the subcommand exists.
            anyhow::bail!(
                "not yet implemented (v1.1 phase 7); inspect scheduler.toml [sla] directly"
            )
        }
        SlaCmd::Explain {
            pname,
            system,
            tenant,
        } => {
            let req = SlaExplainRequest {
                pname: pname.clone(),
                system: system.clone(),
                tenant,
            };
            let resp =
                crate::rpc("SlaExplain", async || client.sla_explain(req.clone()).await).await?;
            if as_json {
                return crate::json(&resp);
            }
            println!("Key:       {pname} ({system})");
            println!("Fit:       {}", resp.fit_summary);
            println!("Prior:     {}", resp.prior_source);
            if let Some(o) = &resp.override_applied {
                println!("Override:  {o}");
            }
            if resp.candidates.is_empty() {
                println!("(no candidates — cold start, dispatch uses probe path)");
                return Ok(());
            }
            println!();
            println!(
                "{:<12} {:>8} {:>10} {:<16} FEASIBLE",
                "TIER", "C*", "MEM", "CONSTRAINT"
            );
            for c in &resp.candidates {
                println!(
                    "{:<12} {:>8} {:>10} {:<16} {}",
                    c.tier,
                    c.c_star
                        .map(|v| format!("{v:.2}"))
                        .unwrap_or_else(|| "-".into()),
                    c.mem_bytes.map(fmt_bytes_u).unwrap_or_else(|| "-".into()),
                    c.binding_constraint,
                    if c.feasible { "yes" } else { "no" },
                );
            }
            Ok(())
        }
        SlaCmd::ExportCorpus { tenant, min_n, out } => {
            let req = ExportSlaCorpusRequest { tenant, min_n };
            let resp = crate::rpc("ExportSlaCorpus", async || {
                client.export_sla_corpus(req.clone()).await
            })
            .await?;
            std::fs::write(&out, &resp.json)
                .map_err(|e| anyhow::anyhow!("write {}: {e}", out.display()))?;
            if as_json {
                return crate::json(&serde_json::json!({
                    "entries": resp.entries, "out": out.display().to_string()
                }));
            }
            println!("wrote {} entries to {}", resp.entries, out.display());
            Ok(())
        }
        SlaCmd::ImportCorpus { path } => {
            let json = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
            // v2: send the typed body so server-side range-validation
            // runs on proto-typed fields. Parse failures (e.g. a v1
            // file whose serde shape differs from the proto-serde
            // shape) fall back to the opaque `json` string — the
            // server's `serde_json::from_str::<prior::SeedCorpus>`
            // accepts both via #[serde(default)].
            let corpus = serde_json::from_str::<rio_proto::types::SeedCorpus>(&json).ok();
            let req = ImportSlaCorpusRequest { json, corpus };
            let resp = crate::rpc("ImportSlaCorpus", async || {
                client.import_sla_corpus(req.clone()).await
            })
            .await?;
            if as_json {
                return crate::json(&serde_json::json!({
                    "entries_loaded": resp.entries_loaded,
                    "ref_hw_class": resp.ref_hw_class,
                    "rescale_factor": resp.rescale_factor,
                }));
            }
            println!(
                "loaded {} seeds (ref={}, rescale={:.3})",
                resp.entries_loaded, resp.ref_hw_class, resp.rescale_factor
            );
            Ok(())
        }
    }
}

/// Parse `8Gi` / `512Mi` / `1024` → bytes. Ki/Mi/Gi only (k8s
/// convention); bare integer = bytes.
fn parse_bytes(s: &str) -> anyhow::Result<i64> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix("Gi") {
        (n, 1i64 << 30)
    } else if let Some(n) = s.strip_suffix("Mi") {
        (n, 1i64 << 20)
    } else if let Some(n) = s.strip_suffix("Ki") {
        (n, 1i64 << 10)
    } else {
        (s, 1)
    };
    let n: i64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid --mem {s:?}: expected e.g. 8Gi, 512Mi, or bytes"))?;
    Ok(n * mult)
}

/// Parse `2h` / `7d` / `30m` → Unix-epoch seconds at now()+ttl.
fn parse_ttl(s: &str) -> anyhow::Result<f64> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix('d') {
        (n, 86400u64)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1)
    } else {
        (s, 1)
    };
    let n: u64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid --ttl {s:?}: expected e.g. 2h, 7d, 30m"))?;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs_f64();
    Ok(now + (n * mult) as f64)
}

fn fmt_bytes_u(b: u64) -> String {
    fmt_bytes(b.min(i64::MAX as u64) as i64)
}

fn fmt_bytes(b: i64) -> String {
    if b >= 1 << 30 {
        format!("{:.1}Gi", b as f64 / (1u64 << 30) as f64)
    } else if b >= 1 << 20 {
        format!("{:.1}Mi", b as f64 / (1u64 << 20) as f64)
    } else {
        format!("{b}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_bytes_suffixes() {
        assert_eq!(parse_bytes("8Gi").unwrap(), 8 << 30);
        assert_eq!(parse_bytes("512Mi").unwrap(), 512 << 20);
        assert_eq!(parse_bytes("4096").unwrap(), 4096);
        assert!(parse_bytes("foo").is_err());
    }
    #[test]
    fn fmt_bytes_u_clamps_u64_max() {
        // Server uses u64::MAX as "unbounded" sentinel for
        // Amdahl/Probe+Coupled. fmt_bytes_u clamps before the i64
        // cast; raw `as i64` wraps to -1 and fmt_bytes(-1) prints
        // "-1" — the bug at sla.rs status p90.
        assert!(fmt_bytes_u(u64::MAX).ends_with("Gi"));
        assert!(!fmt_bytes_u(u64::MAX).starts_with('-'));
        // Tripwire documenting WHY direct cast is wrong.
        assert_eq!(fmt_bytes(u64::MAX as i64), "-1");
        assert_eq!(fmt_bytes(-1), "-1");
    }
    #[test]
    fn parse_ttl_suffixes() {
        // Can't assert exact (now()-relative), but the delta is checkable.
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let in_2h = parse_ttl("2h").unwrap();
        assert!((in_2h - now - 7200.0).abs() < 1.0);
        assert!(parse_ttl("nope").is_err());
    }
}
