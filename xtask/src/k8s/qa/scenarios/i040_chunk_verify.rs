//! I-040: chunk lost from S3 while manifest claims complete —
//! `rio-cli verify-chunks` must detect it.
//!
//! Destructive: deletes one S3 chunk object, runs verify-chunks,
//! asserts the missing hex appears in the report. Restoring the chunk
//! is non-trivial (would need a re-upload of the owning path), so this
//! mutates real state — declared Exclusive(S3, Postgres) and runs LAST
//! in qa's canonical order. Dev cluster is authorized-destructive.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use sqlx::Row;

use crate::k8s::eks::TF_DIR;
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};
use crate::sh::{self, cmd, shell};

pub struct ChunkVerify;

#[async_trait]
impl Scenario for ChunkVerify {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i040-chunk-verify",
            i_ref: Some(40),
            isolation: Isolation::Exclusive {
                mutates: &[Component::S3, Component::Postgres],
            },
            // Budget for the cold-spawn tail. The seed build is ~15 s
            // when a warm builder is up (full QA run: phase-1 scenarios
            // pre-warm the pool before phase-2 reaches i040), but 298 s
            // when Karpenter has scaled to zero (`--only i040` against
            // an idle cluster — observed 2026-05-14). 300 s gave 0
            // margin for the SQL pick + S3 delete + verify-chunks scan
            // (~30-60 s); 420 s keeps ~60 s slack past the cold tail.
            timeout: Duration::from_secs(420),
        }
    }

    /// Self-setup submits a build (Scheduler read) and `verify-chunks`
    /// holds a gRPC connection to the store for the whole scan (Store
    /// read). Without the explicit Store read, the phase-2 scheduler
    /// runs i040 concurrently with `i039-store-kill-survives` (disjoint
    /// `mutates`: `[S3, Postgres]` vs `[Store]`), and i039's store-kill
    /// drops i040's `verify-chunks` stream mid-scan — observed
    /// 2026-05-14 round 5: `transport error … BrokenPipe … stream
    /// closed because of a broken pipe`. Same `mutates`-only-captures-
    /// destruction footgun the i024/i039/i040 default-`reads` exists
    /// for; this is the second instance for the same scenario pair.
    fn reads(&self) -> &'static [Component] {
        &[Component::Scheduler, Component::Store]
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let Ok(bucket) = crate::tofu::output(TF_DIR, "chunk_bucket_name") else {
            return Ok(Verdict::Skip(
                "chunk_bucket_name tofu output unavailable (k3s?)".into(),
            ));
        };

        // Self-setup: seed a fresh chunked output. The nonce is computed
        // Rust-side (not `builtins.currentTime`) so the local
        // `nix-instantiate` below and `gateway_build`'s internal one
        // agree on the drv path — `currentTime` is wall-clock and the
        // two evals can straddle a second boundary, yielding two
        // different drvs and a `target_out` that was never built.
        //
        // ~305 KiB body (300 lines × ~1040 B) clears `INLINE_THRESHOLD`
        // (256 KiB) with margin so the store's PutPath always takes the
        // chunked CAS path — this scenario is dead if the output inlines.
        //
        // The iteration list is a Rust-generated word literal, NOT
        // `$(busybox seq 1 300)`: a `builtins.derivation` with no PATH
        // env var has an empty PATH in the build sandbox, so `busybox`
        // isn't resolvable inside the script and the command
        // substitution silently returns nothing — the loop runs 0
        // times and `> $out` creates an empty file (112 B of NAR
        // framing, well under the inline threshold). That broken seed
        // shipped for months: the old unscoped chunk-pick (`WHERE
        // refcount=1 ORDER BY created_at DESC LIMIT 1`) didn't care
        // and deleted *some other path's* chunk every run — that was
        // the actual i201-stranded-chunk source. Same wall the smoke
        // author hit (`SMOKE_EXPR`: "stdenv bootstrap busybox lacks
        // dd, $((arith)), AND printf"); same fix (literal word list).
        // `$i` keeps each line — and so each chunk — unique.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let seq: String = (1u32..=300)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let expr = format!(
            r#"{BUSYBOX_LET} builtins.derivation {{
              name = "rio-qa-i040-seed-{nonce}";
              system = "x86_64-linux";
              builder = "${{busybox}}";
              args = ["sh" "-c" "for i in {SEQ}; do echo i040-{nonce}-$i-{CHUNK}; done > $out"];
            }}"#,
            SEQ = seq,
            CHUNK = "x".repeat(1020),
            BUSYBOX_LET = crate::k8s::eks::smoke::BUSYBOX_LET,
        );
        // Pre-instantiate to a fixed drv so we can compute the seed's
        // output path after the build returns — `gateway_build` discards
        // `nix build`'s stdout. Same pattern as iso03.
        let drv = {
            let s = shell()?;
            sh::run_read(cmd!(s, "nix-instantiate --expr {expr}")).await?
        };
        ctx.nix_build_expr_via_gateway(0, &expr).await?;
        let target_out = {
            let s = shell()?;
            sh::run_read(cmd!(s, "nix-store -q --outputs {drv}"))
                .await?
                .lines()
                .next()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("nix-store -q --outputs {drv}: empty"))?
        };

        // Pick a chunk that belongs to the SEED'S OWN manifest. The
        // earlier unscoped pick (`WHERE refcount=1 ORDER BY created_at
        // DESC LIMIT 1`) picked "the most recent single-reference chunk
        // in the whole table" — which after a phase-2 predecessor
        // substitutes a new shared input is *that input's* chunk, not
        // the seed's. Round 7 (2026-05-14) deleted busybox chunk
        // `574e8a43f…`, round-8 i201 then found it stranded (PG row
        // re-INSERTed by a subsequent build, S3 object still gone).
        // Scoping to the seed makes "we never delete another path's
        // chunk" structural.
        //
        // No `manifest_chunks` join table exists — `manifest_data.
        // chunk_list` is the packed binary format from
        // `rio-store/src/manifest.rs` (`r[store.manifest.format]`):
        //   [version: u8 = 1] [entry: 36 B = blake3[32] ++ size_u32_le]*
        // so the chunk hashes are extracted with `substring … FOR 32`
        // over `generate_series`. The "unshared" filter is
        // manifest-reference-based: the candidate hash must appear in
        // NO other manifest's chunk_list (a `position()` scan over
        // manifest_data — fine for a QA probe; every manifest_data row
        // has a manifests parent by FK, so no extra join is needed).
        // Within the seed's own manifest a chunk could theoretically
        // collide with another path's via FastCDC; the nonce in every
        // line makes that astronomically unlikely, but if it happens we
        // want to skip that chunk, not corrupt the colliding path.
        let row = sqlx::query(
            "WITH seed AS (
                SELECT md.store_path_hash, md.chunk_list
                FROM narinfo n
                JOIN manifests m USING (store_path_hash)
                JOIN manifest_data md USING (store_path_hash)
                WHERE n.store_path = $1 AND m.status = 'complete'
             ),
             ch AS (
                SELECT seed.store_path_hash AS seed_hash,
                       substring(seed.chunk_list FROM 2 + 36 * g FOR 32) AS blake3_hash
                FROM seed,
                     generate_series(0, (octet_length(seed.chunk_list) - 1) / 36 - 1) AS g
             )
             SELECT encode(ch.blake3_hash, 'hex') AS h
             FROM ch
             JOIN chunks c USING (blake3_hash)
             WHERE NOT c.deleted
               AND NOT EXISTS (
                   SELECT 1 FROM manifest_data md2
                    WHERE md2.store_path_hash <> ch.seed_hash
                      AND position(ch.blake3_hash IN md2.chunk_list) > 0
               )
             LIMIT 1",
        )
        .bind(&target_out)
        .fetch_optional(ctx.pg())
        .await?;
        let Some(row) = row else {
            return Ok(Verdict::Fail(
                diagnose_missing_chunk(ctx, &target_out).await?,
            ));
        };
        let hex: String = row.try_get("h")?;
        let key = format!("chunks/{}/{hex}", &hex[..2]);

        let s = sh::shell()?;
        sh::try_read(cmd!(
            s,
            "aws s3api delete-object --bucket {bucket} --key {key}"
        ))?;

        // The S3 object is gone — from here, PG cleanup MUST run on
        // every exit path. The pre-2026-05-14 version skipped cleanup
        // when verify-chunks errored (early `return Err`), leaving the
        // PG `chunks` row pointing at a 404 → next round's i201 fails
        // with "stranded chunk". Capture the verdict, cleanup, return.
        let verdict: Result<Verdict> = async {
            // verify-chunks streams missing hex hashes to stdout.
            // CliCtx::run captures stdout; --store-addr is set by CliCtx.
            //
            // Fresh CliCtx, not `ctx.cli`: `reads: [Store]` keeps i040
            // from running *while* i039-store-kill-survives holds the
            // Store write, but i040 still runs *after* it — and the
            // shared `ctx.cli` was opened before phase 2 with a
            // port-forward to the store pod i039 just killed. Observed
            // 2026-05-14 round 7: `transport error … BrokenPipe`.
            let cli = crate::k8s::eks::smoke::CliCtx::open(&ctx.kube, 0, 0).await?;
            let out = match cli.run(&["verify-chunks", "--limit", "0"]) {
                Ok(o) => o,
                Err(e) => {
                    // Some deployments need the limit flag named
                    // differently or don't support it — fall back.
                    let msg = format!("{e:#}");
                    if msg.contains("unexpected argument") {
                        cli.run(&["verify-chunks"])?
                    } else {
                        return Err(e);
                    }
                }
            };
            if out.contains(&hex) {
                Ok(Verdict::Pass)
            } else {
                Ok(Verdict::Fail(format!(
                    "verify-chunks did not report deleted chunk {hex}. Output (first 500B): {}",
                    out.chars().take(500).collect::<String>()
                )))
            }
        }
        .await;

        // Restore PG↔S3 consistency. The S3 object is gone for good
        // (would need a chunked re-upload of the seed); drop the PG
        // `chunks` row so i201's PG-says-exists-S3-says-404 scan
        // doesn't flag it, and drop the seed's narinfo (cascades to
        // manifests → manifest_data → content_index) so the permanently
        // corrupt path doesn't accumulate across rounds. Both warn-and-
        // continue: a partial cleanup is still better than no cleanup.
        if let Err(e) = sqlx::query("DELETE FROM chunks WHERE blake3_hash = decode($1, 'hex')")
            .bind(&hex)
            .execute(ctx.pg())
            .await
        {
            tracing::warn!("i040 cleanup: DELETE FROM chunks {hex}: {e:#}");
        }
        if let Err(e) = sqlx::query("DELETE FROM narinfo WHERE store_path = $1")
            .bind(&target_out)
            .execute(ctx.pg())
            .await
        {
            tracing::warn!("i040 cleanup: DELETE FROM narinfo {target_out}: {e:#}");
        }

        verdict
    }
}

/// Why didn't the seed produce a deletable chunk? Each branch of the
/// chunked-PutPath pipeline (narinfo → manifest → manifest_data →
/// chunks) has a different failure mode and a different fix; the old
/// catch-all "no deletable chunk appeared" message couldn't tell them
/// apart, so the round-8 occurrence had to be re-run with manual SQL.
async fn diagnose_missing_chunk(ctx: &QaCtx, target_out: &str) -> Result<String> {
    let diag = sqlx::query(
        "SELECT m.store_path_hash IS NOT NULL AS has_manifest,
                m.status,
                m.inline_blob IS NOT NULL AS inlined,
                n.nar_size,
                (octet_length(md.chunk_list) - 1) / 36 AS n_chunks
         FROM narinfo n
         LEFT JOIN manifests m USING (store_path_hash)
         LEFT JOIN manifest_data md USING (store_path_hash)
         WHERE n.store_path = $1",
    )
    .bind(target_out)
    .fetch_optional(ctx.pg())
    .await?;
    let why = match diag {
        None => "no narinfo row — output never registered (build no-op'd?)".to_owned(),
        Some(d) => {
            let has_manifest: bool = d.try_get("has_manifest")?;
            let status: Option<String> = d.try_get("status")?;
            let inlined: bool = d.try_get("inlined")?;
            let nar_size: i64 = d.try_get("nar_size")?;
            let n_chunks: Option<i32> = d.try_get("n_chunks")?;
            if !has_manifest {
                "narinfo exists but no manifest — PutPath never started".into()
            } else if status.as_deref() != Some("complete") {
                format!("manifest status={status:?} — PutPath never completed")
            } else if inlined {
                format!(
                    "output was inlined (NAR {nar_size} B < 256 KiB INLINE_THRESHOLD) — \
                     seed body too small to take the chunked-PutPath path"
                )
            } else {
                format!(
                    "manifest is chunked ({} chunks, NAR {nar_size} B) but none is \
                     uniquely referenced — every candidate also appears in another \
                     manifest's chunk_list (CDC collision?)",
                    n_chunks.map_or_else(|| "?".into(), |n| n.to_string()),
                )
            }
        }
    };
    Ok(format!(
        "no uniquely-referenced deletable chunk for seed {target_out}: {why}"
    ))
}
